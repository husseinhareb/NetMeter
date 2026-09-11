# Tauri + React + Typescript

This template should help get you started developing with Tauri, React and Typescript in Vite.

## Recommended IDE Setup

- [VS Code](https://code.visualstudio.com/) + [Tauri](https://marketplace.visualstudio.com/items?itemName=tauri-apps.tauri-vscode) + [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)

## Backend

The Rust backend lives in `src-tauri/` as one crate with a one-way dependency
chain: `monitor/` (kernel counters) → `core/` (pure domain logic) →
`storage/` (SQLite) → `api/` (Tauri commands and events). `system/` holds
process concerns: clocks, paths, the single-instance lock, service lifecycle.

* [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — how measurement, attribution,
  storage and retention work, and how the monitor becomes a systemd service.
* [docs/ACCOUNTING.md](docs/ACCOUNTING.md) — which interfaces count toward your
  usage total, and why summing them all reports ~3× reality.
* [docs/API.md](docs/API.md) — the commands and events the frontend uses.

```sh
cd src-tauri
cargo test --features test-support   # unit + integration + live-kernel tests
cargo clippy --all-targets --features test-support
```

Measurement needs no privileges: it reads `/proc/net/dev` and `/sys/class/net`
only. No root, no setuid, no packet capture, no raw sockets.
