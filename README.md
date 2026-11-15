# OJTpulse / DinoBuds

A lightweight, frameless, always-on-top Rust + Tauri v2 + React + Tailwind CSS floating desktop HUD combining **OJT Time & Accomplishment Tracking**, **Live AI Agent & Benchmark Monitoring**, and a **System RAM Reaper** in a single dark-mode window.

---

## Core Modules

### 1. OJT Pulse (Automated DTR & Journal Generator)
- **Live Shift Clock (`Time In` / `Time Out`)**: Tracks your active shift timer in real time (with a live header pill) and records completed shifts into persistent local storage (`%USERPROFILE%\.dinobuds\ojt-state.json`).
- **486-Hour Progress Tracker**: Calculates rendered hours, remaining hours, estimated shifts remaining (8h/day), and projected weekday completion date.
- **Zero-Friction Activity Capture**:
  - Scans today's `git log --since=midnight --oneline --no-merges` commits across your watched repositories without spawning console windows.
  - Parses completed (`- [x]`) engineering tasks from recent Antigravity `task.md` checklists.
  - Supports manual accomplishment bullets directly from the widget.
- **One-Click Markdown & Obsidian Export**: Generates formal Daily/Weekly/Complete DTR tables and accomplishment logs, copies them straight to your clipboard, and optionally writes Markdown files into your Obsidian vault folder.

### 2. Agents HUD (Antigravity & Axiom Auto-Discovery)
- **Zero-Config Watcher**: Automatically discovers running Antigravity sessions (`%USERPROFILE%\.gemini\antigravity\brain\`) and Axiom benchmark runs without manual setup.
- **Animated Pixel Dinos**: Visual status states (`Brachiosaurus` running, pulsing red `T-Rex` waiting permission, `Dino Egg` queued, `Triceratops` completed) with optional 8-bit synth alerts and a compact `Active | Done | All` filter.
- **Windows UI Automation Action Bar**: Uses `UIAutomationClient` to invoke native Antigravity approval buttons (`Submit ⏎`, `Always allow`, `Skip`) cleanly.

### 3. RAM Reaper (Dev Process Monitor)
- Live system memory usage readout powered by `sysinfo`.
- Lists heavy dev processes (`node`, `tsc`, `cargo`, `rustc`, `vite`, etc.) with per-process termination and one-click orphan Node process sweeping.

---

## Tech Stack
- **Backend**: Rust, Tauri v2, `serde` / `serde_json`, `sysinfo`, `tiny_http`
- **Frontend**: React 19, TypeScript, Vite, Tailwind CSS v4, `lucide-react`

---

## Local Development

```bash
npm install
npm run tauri dev
```

To build the release executable (`src-tauri/target/release/dinobuds.exe`):

```bash
npx tauri build --no-bundle
```
