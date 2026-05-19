# Analyser VST

## Project Structure
- **Rust VST plugin**: This directory (`src/`, `Cargo.toml`) — VST3/CLAP spectrum analyser
- **Webview (Next.js)**: `/opt/hardwave-projects/studio/apps/analyser/` — the web UI that runs inside the plugin
- **Shared packages**: `/opt/hardwave-projects/studio/packages/` — shared UI components, analyser-engine, etc.

## How It Works
The Rust plugin (built with `nih-plug`) hosts a webview that loads the Next.js app. The webview communicates with the Rust DSP backend via a message bridge.

## Deploy
When you deploy from the orchestrator:
1. Rust changes → pushed to GitHub → CI builds the VST binary
2. Webview changes → synced to vst-web01 → deploy.sh builds and propagates to web02, web03

You can edit files in both this directory AND the webview directory. Use absolute paths for webview files.

## Engineering Principles
- **Quality over speed**: Deep, clean, easily expandable code. Not fast shipping.
- **Performance**: Every allocation matters — pool buffers, avoid garbage, measure.
- **Simplicity**: Prefer the straightforward solution. Fancy ≠ good.

## Git Workflow
After completing any set of changes, ALWAYS commit and push to GitHub. Do not leave changes only in the local working directory.

CI only triggers on version tags (`v*`), not plain master pushes. To trigger a build, bump the version in `Cargo.toml` and push a tag (e.g. `v0.9.8`).

## Branding
Never mention Claude, Anthropic, or any AI tooling in commit messages, release notes, code comments, or any user-facing content.
