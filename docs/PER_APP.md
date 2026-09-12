# Per-application accounting

How NetMeter attributes traffic to the program that caused it, the way
Windows' *Data usage* page does.

This is a design document. Nothing here is implemented yet.

## The one rule that does not change

**Interface counters stay the ground truth.** `/proc/net/dev` is what the
kernel actually moved on the wire, and it is the number a data cap is measured
against. Per-application figures are an *attribution* of that total, and they
will always sum to less than it.

Everything below is built so the UI can say

```
Today          4.3 GiB
  firefox      2.1 GiB
  spotify      810 MiB
  ...
  unattributed 190 MiB
```

and have every one of those numbers be honest. The remainder is not a rounding
error to be hidden — it is packet headers, ACKs, retransmits, kernel-originated
traffic (ARP, ICMP, DHCP renewals) and anything that moved through a
namespace we do not follow. It gets a row.

## Why this needs a privileged helper

Measured on a developer machine, 2026-09-12, kernel 7.2.4:

| Mechanism | Verdict |
|---|---|
| `/proc/<pid>/net/dev` | Per *namespace*, not per process. Every process in the root netns reports the same system-wide numbers. |
| systemd `IPAccounting` | One bucket for the whole login session. Apps are not in per-app scopes under every desktop — on the test machine firefox, spotify and the terminal all sit in `session-2.scope`. |
| `sock_diag` netlink, TCP | Works unprivileged: real `bytes_acked` / `bytes_received` per socket, with pid mapping for the calling user's own processes. |
| `sock_diag` netlink, UDP | **No byte counters exist.** Queue depths only. QUIC — a large share of modern browser and streaming traffic — is invisible. |
| eBPF | Accurate for TCP and UDP. `/sys/kernel/btf/vmlinux` present, so CO-RE works. |

A TCP-only sampler was considered and rejected: it under-reports exactly the
applications users care most about, and a usage meter that silently loses a
browser's QUIC traffic is worse than one that does not claim per-app numbers at
all.

eBPF requires privilege and there is no way around it: `kernel.unprivileged_bpf_disabled = 2`
on a stock Arch kernel means even a restricted unprivileged load is refused.
Windows reaches the same conclusion — its data usage page is fed by NDU, a
kernel driver.

This is the case NetMeter's own rule -- no root, no raw sockets, no packet
capture "unless absolutely necessary and technically justified" -- exists for.
The justification is that no
unprivileged interface exposes the measurement, and the privilege is confined
to one small program that does nothing else.

## Process boundary

```
  netmeterd (system service, CAP_BPF + CAP_PERFMON, no shell, no network)
     |  loads the BPF program, owns the maps, resolves pid -> app,
     |  accumulates per (uid, app), persists to /var/lib/netmeter/apps.db
     |
     |  AF_UNIX SOCK_SEQPACKET  /run/netmeter/netmeterd.sock  (0660, group netmeter)
     |  peer uid from SO_PEERCRED; a client is only ever served its own uid's rows
     v
  NetMeter GUI (unprivileged, unchanged)
     interface totals as today, per-app breakdown when the socket is there
```

The GUI never loads BPF, never opens `apps.db`, and works exactly as it does
now when the helper is absent — the per-app view shows an "enable" prompt
instead of numbers. This is the `netmeterd` that
[ARCHITECTURE.md](ARCHITECTURE.md) reserved room for, arriving with a reason to
exist.

## The BPF program

Two `fexit` probes, both of which run in the calling process's context, so the
pid is correct for receive as well as send:

| Probe | Counts |
|---|---|
| `fexit/sock_sendmsg` | return value = bytes queued, on success |
| `fexit/sock_recvmsg` | return value = bytes delivered, on success |

Both verified present in the test kernel's symbol table, along with the
narrower `tcp_sendmsg` / `tcp_cleanup_rbuf` / `udp_sendmsg` / `udp_recvmsg`
alternatives, which remain the fallback if the wide probes prove to include
paths we do not want.

Filtering happens in the probe: only `AF_INET` and `AF_INET6` are counted, so
unix-socket chatter between desktop processes never appears.

Maps:

* `counters`: per-CPU hash, key `{tgid, uid}`, value `{rx, tx}`.
* `execs`: hash, key `tgid`, value `{comm, start_time}` — written from
  `tracepoint/sched/sched_process_exec`, so userspace can identify a process
  it never saw running.
* `drain`: ring buffer. `tracepoint/sched/sched_process_exit` pushes a dying
  process's totals before its `counters` entry is deleted.

That last one is why this design beats socket polling on more than just UDP: a
`curl` that runs for 400 ms between two ticks is counted in full, because the
kernel accumulated it at syscall time and the exit hook flushes it. A sampler
that polls sockets every five seconds never sees that process at all.

Userspace drains `counters` on the existing tick and the ring buffer
continuously, applying the same delta rules as `core::statistics` — a counter
that goes backwards is a reset, never negative traffic.

### What it does not count

Stated up front so the "unattributed" row is explainable:

* Packet, IP and TCP headers — these are payload bytes.
* ACKs, retransmits and anything the kernel emits without a process behind it.
* Traffic in other network namespaces (containers) attributed to the container,
  not the app inside it.
* `sendfile`/`splice` and `io_uring` paths must be confirmed to route through
  `sock_sendmsg` on the target kernel; if they do not, they need their own
  probe. This is the first thing to test, not to assume.

## App identity

A pid is useless as a historical key — they recycle within hours. The stored
key is, in order of preference:

1. Flatpak application id, from `/proc/<pid>/root/.flatpak-info` (readable
   because the daemon is privileged).
2. Snap name, from the process's cgroup path.
3. `realpath` of `/proc/<pid>/exe`.
4. `comm` from the exec tracepoint, when the process is already gone.

Display name comes from a `.desktop` lookup on that key, falling back to the
basename. Chromium's forty helper processes share one executable and therefore
collapse into one app, which is the behaviour a user expects.

## Schema

In `apps.db`, owned by the daemon, alongside the same WAL and pragma
configuration as the main database:

```sql
apps(id, app_key UNIQUE, display_name, exe_path, first_seen_utc_ms, last_seen_utc_ms)

usage_app_hour(hour_start_utc_ms, uid, app_id, local_date, rx_bytes, tx_bytes)
  PRIMARY KEY (hour_start_utc_ms, uid, app_id) WITHOUT ROWID

usage_app_day(local_date, uid, app_id, rx_bytes, tx_bytes)
  PRIMARY KEY (local_date, uid, app_id) WITHOUT ROWID
```

Cardinality is the new risk here — the main database has one row per interface
per hour, this one has a row per *application* per hour. Control is by the same
mechanism, one level up: hourly rows are pruned on the existing retention
schedule, and the daily rollup keeps the top N applications per day plus an
`other` row. N of 20 keeps a heavy desktop under a megabyte a year.

## IPC

Length-prefixed JSON over `SOCK_SEQPACKET`, reusing the serde models the Tauri
commands already have. Two requests to begin with:

* `GetAppUsage { from, to, granularity }` → the same `UsageSeries` shape the
  frontend already renders, with apps where interfaces are now.
* `Subscribe` → a per-tick push of live per-app rates.

The socket serves the connecting peer's uid only, taken from `SO_PEERCRED` and
never from anything the client sends.

## Install story

The GUI ships without the helper and says so. "Enable per-app accounting" runs
one `pkexec` that installs a unit file and enables it:

```ini
[Service]
ExecStart=/usr/lib/netmeter/netmeterd
User=netmeter
AmbientCapabilities=CAP_BPF CAP_PERFMON
CapabilityBoundingSet=CAP_BPF CAP_PERFMON
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateNetwork=no
RestrictAddressFamilies=AF_UNIX AF_NETLINK
SystemCallFilter=@system-service
StateDirectory=netmeter
```

No setuid binary, no sudo, no root shell. The escalation is a single
polkit-mediated action the user takes deliberately, and it can be undone with
`systemctl disable --now netmeterd`.

## Build

`libbpf-rs` with a small CO-RE C program, not `aya`: aya's kernel-side crate
needs a nightly Rust toolchain, and the target machine runs distribution stable
with no rustup. `libbpf` 1.7 is already present; `clang` compiles the probe at
build time and a committed `vmlinux.h` keeps the build independent of installed
kernel headers.

The daemon is a second binary in the same Cargo workspace, sharing `core` and
`storage` and importing neither `tauri` nor the GUI's state.

## Order of work

1. A throwaway probe that answers the `sendfile` / `io_uring` question and
   measures the overhead of the two `fexit` hooks under load. Everything else
   depends on those two answers.
2. `netmeterd` with the BPF program and in-memory counters, no persistence,
   printing to stdout. Verifiable against `nethogs` by hand.
3. Persistence and retention.
4. The socket, then the GUI view.
