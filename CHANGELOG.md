# Hardwave Analyser — Changelog

## v1.0.19 — Stability & performance (2026-07-07)

- Fixed a freeze of up to 5 seconds when removing the plugin or closing a project — plugin shutdown no longer waits out the connection-retry timer.
- Lower CPU impact on the audio thread: the FFT engine now runs with preallocated buffers and the analysis data is shared with the UI without copying, so heavy sessions stay smooth.
- Crash reporting no longer stalls audio: reports upload in the background, and repeated identical crashes are sent only once per minute.
- Fixed a rare crash in the plugin's local data server when logging non-ASCII debug text.
- License metadata corrected to GPL-3.0 (matching the earlier relicense).


## v1.0.18 — Preset state persistence (2026-05-20)

- Preset state now persists across DAW reloads. The Rust `HardwaveAnalyserParams` struct has a `#[persist = "preset_state"]` field that nih-plug serialises into the DAW project, then re-injects on load. Your custom band layouts, color themes, and scale choices survive a DAW close → reopen.
- New `GET /init` endpoint on the packet server replaces the unreliable init-script globals injection. The webview now polls a known endpoint for initial state instead of waiting for a one-shot script eval that the wry timing made flaky.
- New `GET /debug/` endpoint exposes packet-server state for frontend probes (was a black box during the preset debugging work).
- `POST /state` body is now read as a raw JSON object, not a double-serialised string — fixes the silent state-save no-ops.

(v1.0.10–v1.0.17 were intermediate debug iterations toward this release. The customer-visible behaviour change is described above.)

## v1.0.9 and earlier

The pre-1.0.18 history is documented in the git log; this changelog
starts capturing customer-facing bullets from v1.0.18 onward. The
Discord-changelog auto-poster reads from this file — bullets at the
top of each release section appear in the announcement embed.
