# Hardwave Analyser

VST3/CLAP plugin that streams real-time audio analysis to its embedded webview UI — spectrum, spectrogram, phase correlation, LUFS metering, kick analysis, and more. Free, with optional integration into the [Hardwave Suite](https://hardwave.studio) desktop app.

## Download

Get the latest release from the [Releases page](../../releases).

| Platform | Download |
|----------|----------|
| Windows x64 | [hardwave-analyser-windows-x64.zip](../../releases/latest/download/hardwave-analyser-windows-x64.zip) |
| macOS Intel | [hardwave-analyser-macos-x64.zip](../../releases/latest/download/hardwave-analyser-macos-x64.zip) |
| macOS Apple Silicon | [hardwave-analyser-macos-arm64.zip](../../releases/latest/download/hardwave-analyser-macos-arm64.zip) |
| Linux x64 | [hardwave-analyser-linux-x64.zip](../../releases/latest/download/hardwave-analyser-linux-x64.zip) |

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

1. In your DAW, add **Hardwave Analyser** to any track or master channel
2. Open the plugin window — the analyser UI loads automatically
3. Sign in with your Hardwave Studios account (free)

The plugin passes audio through unchanged — it only analyzes and visualizes.

## Features

- **Zero-latency pass-through** — no processing delay
- **256-band log spectrum** with peak hold, freeze, slope, and 5 themes
- **Spectrogram view** with perceptual color mapping
- **K-weighted LUFS** — Momentary, Short-term, Integrated, plus LRA
- **True-peak metering** with 4× oversampling
- **Phase correlation** + **Lissajous vectorscope**
- **Oscilloscope** with zero-crossing trigger
- **Kick analysis** — fundamental frequency, musical note, and Sub/Punch/Tail energy ratios (powers the KickForge workflow)
- **Beginner & Advanced UI modes** with first-run setup
- **Presets** — save, load, set default
- **Export** — PNG snapshots and CSV spectrum data

## Building from Source

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Clone and build
git clone https://github.com/Dishairano/hardwave-analyser-vst.git
cd hardwave-analyser-vst
cargo xtask bundle hardwave-analyser --release

# Plugins are in target/bundled/
```

Or use the installer script:
```bash
./install.sh
```

## Technical Details

- **Framework:** [nih-plug](https://github.com/robbert-vdh/nih-plug)
- **UI runtime:** [wry](https://github.com/tauri-apps/wry) WebView (loads `analyser.hardwavestudios.com/vst/analyser`)
- **FFT:** 8192-point with Welch's method (multi-segment overlapping) and configurable window (Hann / Blackman-Harris / Kaiser)
- **Display bands:** 256 logarithmic bands (5 Hz – 20 kHz)
- **Update rate:** 60–144 Hz (configurable)
- **Audio packet:** 4096 raw FFT bins + 512-sample waveform per channel + scalars

## License

MIT License — see [LICENSE](LICENSE) for details.
