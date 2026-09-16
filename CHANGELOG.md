# Hardwave Analyser — Changelog

## v1.0.22

Bug fixes
• the Windows version is built on Windows again, so it stops crashing hosts during playback and when loading a project: it was cross-compiled from Linux and that build was quietly corrupt
• typing a value into Refresh Rate, FFT size or Window works, instead of the plug-in refusing its own reading
• typing a value into any other control works too, and what you type no longer drifts in the last decimal
• a damaged project no longer takes your DAW down with it when the plug-in loads its settings
• your settings reach the host when a project opens, so the DAW stops showing and automating the values you had before

Improvements
• crash reports from our own testing no longer reach the dashboard, so a real crash is not buried under our noise

## v1.0.21 — Settings reach the host, damaged projects stay safe (2026-09-16)

- Reopening a project restored the Analyser's settings, but the plug-in never told the host to re-read them, so the DAW could keep showing and automating stale values. It now asks for a rescan straight after loading.
- A corrupt or foreign saved state made the plug-in ask for an impossible amount of memory, and the failed request took the whole host down. It now refuses the state and carries on.
- Crash reports from our own test runs no longer reach the crash dashboard.

## v1.0.20 — Preset persistence everywhere + true peak (2026-07-07)

- Preset state, custom layouts and themes now survive DAW reloads on macOS and Linux too — previously this only worked on Windows.
- The subscription status flag now reaches the UI on every platform, not just Windows.
- The plugin now sends genuine 4×-oversampled true peak to the UI — the TP readout becomes a real dBTP measurement (UI update rolling out alongside).
- Older Hardwave Suite versions keep working with the new data format.


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
