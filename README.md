# NetMeter

A network usage monitor for Linux that reads the kernel's own interface
counters. It never captures packets and never looks at their contents.

* **Accurate over time.** Counter resets, interface changes, reboots, suspend
  and clock steps are all handled explicitly. When a delta cannot be trusted,
  the sample is discarded rather than guessed at.
* **Honest about what it knows.** Traffic that moved while NetMeter was not
  running is reported separately, because nothing says *when* inside that
  window it happened. Days before installation read "no data", not zero.
* **Cheap.** One read of `/proc/net/dev` every few seconds — about 190 µs, or
  0.004% of a core. A year of four interfaces is roughly 2.4 MB of database.
* **Unprivileged.** The app needs no root, no setuid and no raw sockets.
  Per-application accounting is opt-in and runs in a separate helper.

## Building

    npm install
    npm run tauri dev          # development
    npm run tauri build        # .deb and .rpm in src-tauri/target/release/bundle

On Arch, `makepkg` with [packaging/PKGBUILD](packaging/PKGBUILD).

## Which interfaces count

Summing every interface reports roughly three times reality: a VPN's payload
is counted again on the interface carrying the encrypted packets, and a
container's traffic is counted on the veth, the bridge and the NIC. NetMeter
records every interface but counts only physical ones toward your total by
default, and the Settings table lets you change that per interface.

[docs/ACCOUNTING.md](docs/ACCOUNTING.md) has the full table.

## Per-application usage

Optional, and it needs a privileged helper: Linux exposes no per-process byte
counters to an unprivileged reader. `netmeterd` attaches six eBPF probes to the
kernel's protocol operations, runs as a system service with `CAP_BPF` and
`CAP_PERFMON` and nothing else, and serves the GUI over a local socket that
only ever returns the calling user's own rows.

    cargo build --release --manifest-path netmeterd/Cargo.toml
    sudo packaging/install-helper.sh

Per-application figures are payload bytes, so they never sum to what the
interfaces moved — the difference is headers and acknowledgements, shown as its
own row rather than hidden. [docs/PER_APP.md](docs/PER_APP.md) explains why, and
what it cannot attribute.

## Layout

    src/                  frontend (React)
    src-tauri/            the app: core, monitor, storage, api, system
    netmeterd/            the privileged per-application helper
    probe/                throwaway kernel probes, kept because they re-run
    packaging/            systemd unit, PKGBUILD, desktop entry

The backend is one crate with a one-way dependency chain: `monitor/` (kernel
counters) → `core/` (pure domain logic) → `storage/` (SQLite) → `api/` (Tauri
commands and events). `system/` holds process concerns: clocks, paths, the
single-instance lock, service lifecycle. Tauri sits behind a feature flag, so
the helper links the same crate without it.

* [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — how measurement, attribution,
  storage and retention work, and how the monitor becomes a systemd service.
* [docs/ACCOUNTING.md](docs/ACCOUNTING.md) — which interfaces count, and why
  summing them all reports ~3× reality.
* [docs/API.md](docs/API.md) — the commands and events the frontend uses.
* [docs/PER_APP.md](docs/PER_APP.md) — per-application accounting: why it needs
  privilege, and what it can never attribute.

## Tests

    cargo test --manifest-path src-tauri/Cargo.toml --features test-support
    cargo test --manifest-path netmeterd/Cargo.toml

The interesting ones are not unit tests: `tests/engine.rs` drives the sampler
through counter resets, interfaces vanishing and returning, reboots and
restarts; `tests/live_kernel.rs` asserts invariants that must hold on any Linux
machine; `tests/resources.rs` measures that the claims above are true.
