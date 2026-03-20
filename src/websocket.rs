//! WebSocket client for streaming audio data to Hardwave Suite

use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use parking_lot::Mutex;
use tungstenite::protocol::WebSocket;
use tungstenite::{Message, client::IntoClientRequest};

use crate::protocol::AudioPacket;

/// WebSocket client that runs in a background thread
pub struct WebSocketClient {
    /// Sender for audio packets — None until start() is called
    packet_sender: Option<Sender<AudioPacket>>,

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
        Self {
            packet_sender: None,
            shutdown: Arc::new(AtomicBool::new(false)),
            thread_handle: None,
            server_port: Arc::new(Mutex::new(9847u16)),
        }
    }

    /// Start the background connection thread. Safe to call multiple times —
    /// only the first call spawns the thread.
    pub fn start(&mut self) {
        if self.thread_handle.is_some() {
            return;
        }

        let (packet_sender, packet_receiver) = bounded::<AudioPacket>(32);
        self.packet_sender = Some(packet_sender);

        let shutdown_clone = Arc::clone(&self.shutdown);
        let port_clone = Arc::clone(&self.server_port);

        self.thread_handle = Some(thread::spawn(move || {
            Self::connection_loop(packet_receiver, shutdown_clone, port_clone);
        }));
    }

    /// Update the server port
    pub fn set_port(&self, port: i32) {
        let mut p = self.server_port.lock();
        *p = port as u16;
    }

    /// Send an audio packet (non-blocking). No-op before start() is called.
    pub fn send(&self, packet: AudioPacket) {
        if let Some(sender) = &self.packet_sender {
            let _ = sender.try_send(packet);
        }
    }

    /// Background connection loop
    fn connection_loop(
        receiver: Receiver<AudioPacket>,
        shutdown: Arc<AtomicBool>,
        server_port: Arc<Mutex<u16>>,
    ) {
        let mut reconnect_delay = Duration::from_millis(100);
        let max_reconnect_delay = Duration::from_secs(5);

        while !shutdown.load(Ordering::Relaxed) {
            let port = *server_port.lock();

            match Self::try_connect(port) {
                Ok(mut socket) => {
                    reconnect_delay = Duration::from_millis(100);
                    Self::handle_connection(&mut socket, &receiver, &shutdown, &server_port, port);
                    // If the port changed, reconnect immediately without backoff.
                    if *server_port.lock() != port {
                        reconnect_delay = Duration::from_millis(100);
                        continue;
                    }
                }
                Err(_) => {}
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

        // Use tungstenite's built-in handshake which validates Sec-WebSocket-Accept
        // per RFC 6455, preventing connections to rogue local services.
        let url = format!("ws://127.0.0.1:{}/", port);
        let request = url.into_client_request().map_err(|_| ())?;
        let (socket, _response) = tungstenite::client(request, stream).map_err(|_| ())?;
        Ok(socket)
    }

    /// Handle an active connection. Returns when the connection drops, the port
    /// changes (so the caller reconnects on the new port), or shutdown is signalled.
    fn handle_connection(
        socket: &mut WebSocket<TcpStream>,
        receiver: &Receiver<AudioPacket>,
        shutdown: &Arc<AtomicBool>,
        server_port: &Arc<Mutex<u16>>,
        connected_port: u16,
    ) {
        let mut last_heartbeat = std::time::Instant::now();
        let heartbeat_interval = Duration::from_secs(1);

        while !shutdown.load(Ordering::Relaxed) {
            // Reconnect immediately if the port was changed by the user.
            if *server_port.lock() != connected_port {
                return;
            }
            match receiver.try_recv() {
                Ok(packet) => {
                    let data = packet.to_bytes();
                    if socket.send(Message::Binary(data)).is_err() {
                        return;
                    }
                    if socket.flush().is_err() {
                        return;
                    }
                }
                Err(TryRecvError::Empty) => {
                    if last_heartbeat.elapsed() >= heartbeat_interval {
                        let heartbeat = AudioPacket::new_heartbeat(0, 0);
                        let data = heartbeat.to_bytes();
                        if socket.send(Message::Binary(data)).is_err() {
                            return;
                        }
                        if socket.flush().is_err() {
                            return;
                        }
                        last_heartbeat = std::time::Instant::now();
                    }
                }
                Err(TryRecvError::Disconnected) => {
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
