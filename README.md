# Hardwave Bridge

VST3/CLAP plugin that streams audio from your DAW to [Hardwave Suite](https://hardwave.studio) for real-time spectrum analysis, phase correlation, and level metering.

## Download

Get the latest release from the [Releases page](../../releases).

| Platform | Download |
|----------|----------|
| Windows x64 | [hardwave-bridge-windows-x64.zip](../../releases/latest/download/hardwave-bridge-windows-x64.zip) |
| macOS Intel | [hardwave-bridge-macos-x64.zip](../../releases/latest/download/hardwave-bridge-macos-x64.zip) |
| macOS Apple Silicon | [hardwave-bridge-macos-arm64.zip](../../releases/latest/download/hardwave-bridge-macos-arm64.zip) |
| Linux x64 | [hardwave-bridge-linux-x64.zip](../../releases/latest/download/hardwave-bridge-linux-x64.zip) |

## Installation

1. Download the zip for your platform
2. Extract the contents
3. Copy the plugins to your plugin folders:

| Platform | VST3 Location | CLAP Location |
|----------|---------------|---------------|
| Windows | `C:\Program Files\Common Files\VST3` | `C:\Program Files\Common Files\CLAP` |
| macOS | `~/Library/Audio/Plug-Ins/VST3` | `~/Library/Audio/Plug-Ins/CLAP` |
| Linux | `~/.vst3` | `~/.clap` |

4. Rescan plugins in your DAW if needed

## Usage

1. Open **Hardwave Suite** desktop app
2. Go to **Analyser** and select **VST** as the audio source
3. In your DAW, add **Hardwave Bridge** to your master channel
4. The connection happens automatically on port 9847

The plugin passes audio through unchanged - it only analyzes and streams the data.

## Features

- **Zero latency** - Pure pass-through, no processing delay
- **64-band spectrum** - Logarithmic frequency analysis (20Hz - 20kHz)
- **Stereo metering** - Peak, RMS, and phase correlation
- **Auto-reconnect** - Automatically reconnects if Hardwave Suite restarts
- **Low overhead** - ~20Hz update rate, ~500 bytes per packet

## Building from Source

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Clone and build
git clone https://github.com/hardwave-studios/hardwave-bridge.git
cd hardwave-bridge
cargo xtask bundle hardwave-bridge --release

# Plugins are in target/bundled/
```

Or use the installer script:
```bash
./install.sh
```

## Technical Details

- **Framework:** [nih-plug](https://github.com/robbert-vdh/nih-plug)
- **Protocol:** Binary WebSocket on port 9847
- **FFT Size:** 4096 samples
- **Update Rate:** ~20Hz
- **Packet Size:** ~536 bytes

## License

MIT License - see [LICENSE](LICENSE) for details.
