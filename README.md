# Nucleus

[![Nucleus](https://img.shields.io/badge/Nucleus-Intent--Aware%20Workspace-blueviolet)](https://github.com/sohithvishnu/Nucleus)
[![CI](https://github.com/sohithvishnu/Nucleus/actions/workflows/run_tests.yml/badge.svg)](https://github.com/sohithvishnu/Nucleus/actions)

**Nucleus** is a high-performance, intent-aware developer environment built on top of [Zed](https://zed.dev). Engineered for extreme speed and intelligence, Nucleus extends standard code editing with a **Passive Observer Engine**, real-time **Intent Classification**, **Engine Telemetry Dashboards**, and streamlined **Agent UI Tooling**.

---

## Key Features & Tool Design Beyond Zed

### 🧠 Intent Engine & Passive Observer (`nucleus_intent`)
Nucleus includes an inline, low-overhead passive observer that tracks editor, buffer, terminal, and diagnostic activity to infer what you are working on in real time:
- **Developer Intent Classifier**: Detects developer context automatically (`Debugging`, `Implementing`, `Refactoring`, `Exploring`, `Reviewing`, `Testing`, `Documenting`, `Configuring`, `Planning`, `AgentAssisted`, `Idle`).
- **Telemetry & Ground-Truth Logging**: Formats and streams raw activity events and intent predictions into structured JSONL logs (`~/.nucleus/logs/`).
- **Non-Intrusive Feedback Nudges**: Minimalist toast notifications (`FeedbackNudgeToast`) collect user feedback on predicted intents to continuously tune model confidence.

### 📊 Engine Panel Telemetry Dashboard (`engine_panel`)
A dedicated workspace dock panel for real-time observability into your workflow and intent classification:
- **Live State Overview**: Displays current inferred intent, confidence scores, top active files, recent edit burst windows, and diagnostic proximity metrics.
- **Log Viewer & Inspector**: Live-tails streaming session events and browses past session logs with filtering by date and event type.

### 🤖 Enhanced Agent UI & Workspace Tooling
- **Chat Clusters**: Re-architected project chat grouping in the Agent panel for organizing multi-task AI workflows.
- **Popover Thread Switcher**: Clean popover thread navigation that replaces static sidebar columns to maximize code viewing area.
- **Improved AI Parsing & Permission Handling**: Fine-grained handling of stream parsing and tool permission denials.

### 🎨 Nucleus Design & System Polish
- **Nucleus Theme**: Tailored high-contrast dark theme with dynamic OS title bar color matching.
- **Refined Dock & Panel Contrast**: Active dock panel styling with high-contrast icon rendering.
- **Markdown & Git Enhancements**: Direct line-linking in markdown previews and improved staging awareness when discarding git changes.

---

## Installation & Packaging

### macOS

#### 1. Quick Build & Install (Local Application)
Build the release binary, package it into a `.app` bundle, ad-hoc sign it, and install directly into `/Applications`:
```bash
./script/bundle-mac -i
```

#### 2. Create Standalone `.dmg` Installer
To build a shareable macOS `.dmg` installer:
```bash
./script/bundle-mac
```
The output `.dmg` will be saved to:
`target/<architecture>-apple-darwin/release/Zed-<arch>.dmg` (or `target/aarch64-apple-darwin/release/Zed-aarch64.dmg`).

#### 3. Fast Debug Build & Auto-Launch
```bash
./script/bundle-mac -d -i -o
```

### Linux & Windows
- **Linux**: Run `./script/bundle-linux` or `./script/install.sh`.
- **Windows**: Run `powershell ./script/bundle-windows.ps1`.

---

## Terminal CLI Setup

Link the bundled CLI binary to your local bin path to use `zed` / `nucleus` from your terminal:

```bash
mkdir -p ~/.local/bin
ln -sf "/Applications/Zed.app/Contents/MacOS/cli" ~/.local/bin/zed
```
*(Ensure `~/.local/bin` is present in your shell's `$PATH`).*

---

## Development & Building

### Prerequisites
- [Rust toolchain](https://rustup.rs/) (managed via `rust-toolchain.toml`)
- Xcode Command Line Tools (on macOS: `xcode-select --install`)

### Building from Source
```bash
# Build all binaries
cargo build --release --package zed --package cli

# Run tests
cargo test
```

---

## Licensing & Attribution

Nucleus is built upon the open-source foundation of Zed.
- Primary Source Code: **GPL-3.0-or-later**
- Components & Tooling: **Apache-2.0** where designated

License compliance for dependencies is managed via [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) and specified in `script/licenses/zed-licenses.toml`.
