//! WebSocket client for streaming audio data to Hardwave Suite

use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError};
use parking_lot::Mutex;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tungstenite::protocol::WebSocket;
use tungstenite::{Message, client::IntoClientRequest, handshake::client::generate_key};

use crate::protocol::AudioPacket;

/// Connection state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Error,
}

/// WebSocket client that runs in a background thread
pub struct WebSocketClient {
    /// Sender for audio packets
    packet_sender: Sender<AudioPacket>,

    /// Current connection state
    state: Arc<Mutex<ConnectionState>>,

    /// Flag to signal shutdown
    shutdown: Arc<AtomicBool>,

    /// Background thread handle
    thread_handle: Option<JoinHandle<()>>,

    /// Current server port
    server_port: Arc<Mutex<u16>>,
}

impl WebSocketClient {
    /// Create a new WebSocket client. Does NOT start the connection thread yet —
    /// call `start()` after the plugin is initialised to avoid blocking DAW
    /// plugin scans.
    pub fn new() -> Self {
        let (packet_sender, _packet_receiver) = bounded::<AudioPacket>(32);
        let state = Arc::new(Mutex::new(ConnectionState::Disconnected));
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_port = Arc::new(Mutex::new(9847u16));

        Self {
            packet_sender,
            state,
            shutdown,
            thread_handle: None,
            server_port,
        }
    }

    /// Start the background connection thread. Safe to call multiple times —
    /// only the first call spawns the thread.
    pub fn start(&mut self) {
        if self.thread_handle.is_some() {
            return;
        }

        let (packet_sender, packet_receiver) = bounded::<AudioPacket>(32);
        self.packet_sender = packet_sender;

        let state_clone = Arc::clone(&self.state);
        let shutdown_clone = Arc::clone(&self.shutdown);
        let port_clone = Arc::clone(&self.server_port);

        self.thread_handle = Some(thread::spawn(move || {
            Self::connection_loop(packet_receiver, state_clone, shutdown_clone, port_clone);
        }));
    }

    /// Update the server port
    pub fn set_port(&self, port: i32) {
        let mut p = self.server_port.lock();
        *p = port as u16;
    }

    /// Get the current connection state
    pub fn connection_state(&self) -> ConnectionState {
        *self.state.lock()
    }

    /// Check if connected
    pub fn is_connected(&self) -> bool {
        self.connection_state() == ConnectionState::Connected
    }

    /// Send an audio packet (non-blocking)
    pub fn send(&self, packet: AudioPacket) {
        // Don't block the audio thread - drop packets if queue is full
        let _ = self.packet_sender.try_send(packet);
    }

    /// Background connection loop
    fn connection_loop(
        receiver: Receiver<AudioPacket>,
        state: Arc<Mutex<ConnectionState>>,
        shutdown: Arc<AtomicBool>,
        server_port: Arc<Mutex<u16>>,
    ) {
        let mut reconnect_delay = Duration::from_millis(100);
        let max_reconnect_delay = Duration::from_secs(5);

        while !shutdown.load(Ordering::Relaxed) {
            // Get current port
            let port = *server_port.lock();

            // Try to connect
            *state.lock() = ConnectionState::Connecting;

            match Self::try_connect(port) {
                Ok(mut socket) => {
                    *state.lock() = ConnectionState::Connected;
                    reconnect_delay = Duration::from_millis(100);

                    // Handle connection
                    Self::handle_connection(&mut socket, &receiver, &state, &shutdown);
                }
                Err(_) => {
                    *state.lock() = ConnectionState::Disconnected;
                }
            }

            // Wait before reconnecting
            if !shutdown.load(Ordering::Relaxed) {
                thread::sleep(reconnect_delay);
                reconnect_delay = (reconnect_delay * 2).min(max_reconnect_delay);
            }
        }
    }

    /// Try to establish a WebSocket connection
    fn try_connect(port: u16) -> Result<WebSocket<TcpStream>, ()> {
        let addr = format!("127.0.0.1:{}", port);

        // Connect with timeout
        let stream = TcpStream::connect_timeout(
            &addr.parse().map_err(|_| ())?,
            Duration::from_secs(2),
        )
        .map_err(|_| ())?;

        stream.set_nonblocking(false).ok();
        stream.set_read_timeout(Some(Duration::from_millis(100))).ok();
        stream.set_write_timeout(Some(Duration::from_millis(100))).ok();

        // Perform WebSocket handshake manually
        let key = generate_key();
        let request = format!(
            "GET / HTTP/1.1\r\n\
             Host: 127.0.0.1:{}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {}\r\n\
             Sec-WebSocket-Version: 13\r\n\
             \r\n",
            port, key
        );

        let mut stream_clone = stream.try_clone().map_err(|_| ())?;
        stream_clone.write_all(request.as_bytes()).map_err(|_| ())?;

        // Read response
        let mut response = [0u8; 1024];
        let mut total_read = 0;
        loop {
            let n = stream_clone.read(&mut response[total_read..]).map_err(|_| ())?;
            if n == 0 {
                return Err(());
            }
            total_read += n;
            // Check for end of headers
            if total_read >= 4 && &response[total_read - 4..total_read] == b"\r\n\r\n" {
                break;
            }
            if total_read >= response.len() {
                return Err(());
            }
        }

        // Verify response contains 101 Switching Protocols
        let response_str = std::str::from_utf8(&response[..total_read]).map_err(|_| ())?;
        if !response_str.contains("101") || !response_str.to_lowercase().contains("upgrade") {
            return Err(());
        }

        // Create WebSocket from the stream
        let socket = WebSocket::from_raw_socket(stream_clone, tungstenite::protocol::Role::Client, None);
        Ok(socket)
    }

    /// Handle an active connection
    fn handle_connection(
        socket: &mut WebSocket<TcpStream>,
        receiver: &Receiver<AudioPacket>,
        state: &Arc<Mutex<ConnectionState>>,
        shutdown: &Arc<AtomicBool>,
    ) {
        let mut last_heartbeat = std::time::Instant::now();
        let heartbeat_interval = Duration::from_secs(1);

        while !shutdown.load(Ordering::Relaxed) {
            // Check for incoming packets to send
            match receiver.try_recv() {
                Ok(packet) => {
                    let data = packet.to_bytes();
                    if socket.send(Message::Binary(data)).is_err() {
                        *state.lock() = ConnectionState::Disconnected;
                        return;
                    }
                    // Flush to ensure data is sent
                    if socket.flush().is_err() {
                        *state.lock() = ConnectionState::Disconnected;
                        return;
                    }
                }
                Err(TryRecvError::Empty) => {
                    // No packet available, check if we need to send heartbeat
                    if last_heartbeat.elapsed() >= heartbeat_interval {
                        let heartbeat = AudioPacket::new_heartbeat(0, 0);
                        let data = heartbeat.to_bytes();
                        if socket.send(Message::Binary(data)).is_err() {
                            *state.lock() = ConnectionState::Disconnected;
                            return;
                        }
                        if socket.flush().is_err() {
                            *state.lock() = ConnectionState::Disconnected;
                            return;
                        }
                        last_heartbeat = std::time::Instant::now();
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    // Channel closed, exit
                    return;
                }
            }

            // Small sleep to avoid busy-waiting
            thread::sleep(Duration::from_millis(1));
        }
    }
}

impl Default for WebSocketClient {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for WebSocketClient {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}
