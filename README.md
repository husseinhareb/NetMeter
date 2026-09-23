# NetMeter

NetMeter is a Linux desktop application that measures how much data your computer uses, in total and per application. Built with the Tauri framework, it pairs a Rust backend with a TypeScript/React frontend, and a small system daemon, `netmeterd`, that does the measuring from boot whether or not the window is open. It reads the kernel's own counters and never captures packets or looks at their contents.

## Description

The app is organised around four main sections:

### Overview

The state of the machine at a glance:

- **Status**: whether counting is running, and how often it samples
- **Last 30 days**: download, upload and total
- **Today**: download, upload and total so far, including traffic not yet written to disk
- **Current speed**: live download and upload rates
- **Live bandwidth**: a graph of the last 60 seconds
- **Network interfaces**: per-interface download, upload and total for today, with live rates. Interfaces that are recorded but not counted toward your total are dimmed.

### Applications

Every application that used the network, with its download, upload and total. The list can be filtered by name and sorted by any column, and each application shows its own icon from the installed `.desktop` files.

Time ranges:

- Today
- Yesterday
- 7 days
- 30 days
- This month
- Last month
- This year
- Custom range

Clicking an application opens its timeline for the selected range. A **protocol overhead** row shows what the interfaces moved that no application owns (packet headers, acknowledgements, retransmits), so the rows add up to the real total instead of leaving an unexplained gap. The view exports to CSV and JSON.

### History

Usage over time for the same ranges as Applications, as a chart and a detail table. Day and month bars can be clicked to drill down into them. Traffic that moved while nothing was counting is reported separately, because nothing says *when* inside that window it happened, and days before NetMeter was installed read "no data" rather than zero. The view exports to CSV and JSON.

### Settings

- **Counted interfaces**: which interfaces count toward your total
- **Usage limits**: a monthly allowance, with desktop notifications at 80% and 100%
- **Sampling**: the interval used when the app samples by itself (without the daemon)
- **Startup**: start at login
- **Background behavior**: closing the window keeps NetMeter in the tray; Quit from the tray menu

## How it counts

### The daemon

`netmeterd` runs as a system service from boot. It records interface totals every 5 seconds, and per-application usage through six eBPF probes on the kernel's TCP and UDP send and receive operations. It runs with `CAP_BPF` and `CAP_PERFMON` and nothing else (no setuid binary, no root shell), cannot open a network connection of its own, and serves the app over a local socket that only returns the calling user's own per-application rows.

The app only displays what the daemon recorded. Without the daemon, it falls back to sampling interface totals by itself while it is running, and per-application usage is unavailable.

### Which interfaces count

Summing every interface reports roughly three times reality: a VPN's payload is counted again on the interface carrying the encrypted packets, and a container's traffic is counted on the veth, the bridge and the NIC. NetMeter records every interface but counts only physical ones (Ethernet, WiFi, mobile broadband) toward your total by default. Loopback is never counted.

### Accuracy

- Counter resets, interfaces appearing and disappearing, reboots, suspend and clock changes are all handled explicitly. When a delta cannot be trusted, the sample is discarded rather than guessed at.
- Per-application figures are payload bytes, so they always sum to less than the interfaces moved. The difference is its own row.
- Traffic between two local programs (loopback) is not counted, since it never reaches a network.
- One read of `/proc/net/dev` every few seconds costs about 190 µs. A year of four interfaces is roughly 2.4 MB of database.

## Configuration

The app creates a configuration file at `~/.config/com.shtam.netmeter/config.json`. It can be managed from the Settings page or edited by hand. The config persists:

- the counted and excluded interfaces (glob patterns such as `veth*` are accepted);
- the monthly allowance, its warning thresholds, and whether it counts download, upload or both;
- the sampling and flush intervals;
- the timezone used for day boundaries (the system's by default);
- how long hourly history, daily history and unused interfaces are kept (400 days, forever and 90 days by default);
- the logging level.

Data lives in:

| What | Where |
|---|---|
| Interface and per-application history (daemon) | `/var/lib/netmeter/` |
| Interface history (app, when running without the daemon) | `~/.local/share/com.shtam.netmeter/netmeter.db` |

## Installation

### Arch Linux (AUR)

```bash
git clone https://aur.archlinux.org/netmeter.git
cd netmeter
makepkg -si
```

Or with an AUR helper:

```bash
yay -S netmeter
```

The package installs the app and the daemon, and enables and starts `netmeterd.service`. History in `/var/lib/netmeter` is kept when the package is removed.

### Other distributions

Build the app and the daemon from source (see below), then install the daemon as a system service:

```bash
cargo build --release --manifest-path netmeterd/Cargo.toml
sudo packaging/install-helper.sh
```

Remove it with `sudo packaging/install-helper.sh --uninstall`. The daemon needs a kernel with BTF (`/sys/kernel/btf/vmlinux`), which every mainstream distribution kernel has.

### Resolving Dependency Errors

If the app fails to start with a missing shared-library error such as:

```
error while loading shared libraries: libjavascriptcoregtk-4.1.so
```

install the WebKit2GTK 4.1 package for your distribution:

| Distribution | Command |
|---|---|
| Arch Linux | `sudo pacman -S webkit2gtk-4.1` |
| Debian/Ubuntu | `sudo apt install libwebkit2gtk-4.1-dev` |
| Fedora/RHEL | `sudo dnf install webkit2gtk4.1-devel` |
| Gentoo | `sudo emerge --ask net-libs/webkit-gtk:4.1` |
| Void Linux | `sudo xbps-install -S webkit2gtk-devel` |

### Fixing NVIDIA GPU Errors

If you use an NVIDIA GPU and encounter errors like:

```
GBM-DRV error (nv_gbm_create_device_native): nv_common_gbm_create_device failed
Failed to create GBM buffer of size 800x600: Permission denied
```

add these environment variables to your shell config (`.bashrc`, `.zshrc`, or `config.fish`):

```bash
export WEBKIT_DISABLE_DMABUF_RENDERER=1
export LIBGL_ALWAYS_SOFTWARE=1
export QT_XCB_FORCE_SOFTWARE_OPENGL=1
```

For fish:

```fish
set -Ux WEBKIT_DISABLE_DMABUF_RENDERER 1
set -Ux LIBGL_ALWAYS_SOFTWARE 1
set -Ux QT_XCB_FORCE_SOFTWARE_OPENGL 1
```

## Building from Source

Building needs Rust, Node.js and npm, and `clang` for the daemon's eBPF program.

1. **Clone the repository**:

    ```bash
    git clone https://github.com/husseinhareb/NetMeter
    cd NetMeter
    ```

2. **Install frontend dependencies**:

    ```bash
    npm install
    ```

3. **Run in development mode**:

    ```bash
    npm run tauri dev
    ```

4. **Build a release binary**:

    ```bash
    npm run tauri build
    ```

5. **Build the daemon**:

    ```bash
    cargo build --release --manifest-path netmeterd/Cargo.toml
    ```

### Project layout

- `src/`: the frontend (React)
- `src-tauri/`: the app backend: `monitor/` (kernel counters), `core/` (domain logic), `storage/` (SQLite), `api/` (Tauri commands, the daemon client), `system/` (clocks, paths, service lifecycle)
- `netmeterd/`: the daemon, its eBPF program and its socket server. It links the app's crate with Tauri switched off, so both record interfaces with the same code.
- `packaging/`: systemd unit, PKGBUILD, desktop entry

### Tests

- `cargo test --manifest-path src-tauri/Cargo.toml --features test-support`: the app backend. `tests/engine.rs` drives the sampler through counter resets, interfaces vanishing and returning, reboots and restarts; `tests/live_kernel.rs` checks invariants that must hold on any Linux machine; `tests/resources.rs` measures the cost claims above.
- `cargo test --manifest-path netmeterd/Cargo.toml`: the daemon's storage and socket server, without loading BPF or needing root.

## Changelog

### v0.1.0
- **feat**: interface totals from `/proc/net/dev`, with only physical interfaces counted by default
- **feat**: per-application usage through eBPF, with a protocol overhead row
- **feat**: `netmeterd` records from boot; the app only displays it
- **feat**: Overview, Applications, History and Settings pages
- **feat**: history ranges from today to a year, custom ranges, drill-down, CSV and JSON export
- **feat**: monthly allowance with desktop notifications
- **feat**: application icons from installed `.desktop` files
- **feat**: tray icon, close-to-tray, start at login
- **feat**: AUR package (`netmeter`)

## Contributing

Contributions are welcome.

1. Fork the repository.
2. Create a branch: `git checkout -b feature/YourFeature`
3. Commit your changes: `git commit -m 'Add some feature'`
4. Push to the branch: `git push origin feature/YourFeature`
5. Submit a pull request.

Please run both test suites and `cargo clippy --all-targets` in `src-tauri/` and `netmeterd/` before opening a PR.

## Licence

This project is licensed under the [Apache License 2.0](https://github.com/husseinhareb/NetMeter/blob/main/LICENSE).
