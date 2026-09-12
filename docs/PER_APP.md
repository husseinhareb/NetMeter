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

### Where to attach, measured rather than assumed

An earlier draft of this document proposed `sock_sendmsg` and `sock_recvmsg` —
two probes, both in process context, covering every protocol. An ftrace probe
on kernel 7.2.4 (2026-09-12) showed that would have been wrong. Each case moved
64 MiB through a loopback socket as a single traced pid:

| Workload | Call chain observed |
|---|---|
| `send()` / `sendall()` | `tcp_sendmsg <-__sys_sendto` — **no `sock_sendmsg` at all** |
| `sendfile()` | `sock_sendmsg <-splice_to_socket` → `tcp_sendmsg` |
| io_uring send | `sock_sendmsg <-io_send` → `tcp_sendmsg` |
| UDP send | `udp_sendmsg <-__sys_sendto` — again no `sock_sendmsg` |
| receive (all cases) | `sock_recvmsg <-__sys_recvfrom`, `<-io_recv`, `<-sock_read_iter` |

`sock_sendmsg` is inlined into `__sys_sendto` in this build, so the symbol
survives only for the callers compiled elsewhere — splice and io_uring. A probe
there would have counted a file server and an io_uring client while reporting
zero for the ordinary `send()` that most programs use.

Inlining is a property of the kernel build, not of the kernel version, so
`sock_recvmsg` being intact here proves nothing about the next machine.

**The attach points are therefore the protocol operations.** `tcp_sendmsg` and
friends are reached indirectly through `sk->sk_prot->sendmsg`, so they can
never be inlined away and every upper path must funnel through them — which is
exactly what the table shows: all three TCP send workloads hit `tcp_sendmsg`,
whatever route they took to get there.

| Probe | Counts | Covers |
|---|---|---|
| `fexit/tcp_sendmsg` | return value | TCP send, IPv4 and IPv6 |
| `fexit/udp_sendmsg`, `fexit/udpv6_sendmsg` | return value | UDP send, QUIC included |
| `fentry/tcp_cleanup_rbuf` | `copied` argument | TCP receive, `recvmsg` and splice both |
| `fexit/udp_recvmsg`, `fexit/udpv6_recvmsg` | return value | UDP receive |

`tcp_cleanup_rbuf` is chosen over the simpler `tcp_recvmsg` because it also
fires on the splice path. It is called more than once per receive — 2498 calls
for 1504 receives in the trace — so bytes must come from its `copied`
argument; counting calls would be meaningless.

The IPv6 UDP pair is the one line of the table the probe did not exercise; its
test is the existing workload with an `AF_INET6` socket.

All six run in the calling process's context, so the pid is right for receive
as well as send.

### Cost

Tracing seven functions through 64 MiB of loopback traffic was free at the
resolution of the test: 0.04 s either way. ftrace's per-call cost is higher
than a BPF `fexit` hook, so that is a ceiling, and overhead is not a reason to
narrow the probe set.

### Maps

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

* Packet, IP and TCP headers — these are payload bytes handed to or taken from
  the protocol, not frames on the wire.
* ACKs, retransmits and anything the kernel emits without a process behind it.
* Traffic in other network namespaces (containers), which belongs to the
  container rather than the app inside it.
* Protocols other than TCP and UDP — raw sockets, ICMP from `ping`. Rare
  enough on a desktop to live in the remainder.

The syscall layer no longer appears on this list: `write`, `send`, `sendmsg`,
`sendfile`, `splice` and io_uring were all measured to funnel through the
protocol operations above.

## Measured: how much can actually be attributed

Two minutes of ordinary desktop use, kernel 7.2.4, 2026-09-12:

| | On the wire | Attributed | Remainder |
|---|---|---|---|
| Download | 23.0 MiB | 21.4 MiB | **7.3%**, steady |
| Upload | 1.4 MiB | 1.0 MiB | **30%**, falling from 78% |

The download remainder is header overhead and behaves like it: ~15,500
segments at 52-66 bytes of TCP/IP/Ethernet each accounts for about 1.0 MB of
the 1.7 MiB gap, with DNS, ARP, IPv6 router advertisements, retransmits and the
splice receive path making up the rest. It does not drift.

**The upload remainder is mostly acknowledgements for the download**, which is
why it starts near 78% and falls as real upload accumulates. Downloading
21 MiB produces roughly 7,700 delayed ACKs at ~66 bytes on the wire -- about
500 KB, against the ~440 KB observed. Those bytes have no process behind them;
no accounting mechanism at any layer can attribute them to an application.

Two consequences for the UI, both now measured rather than guessed:

* The remainder row is not a rounding error to hide. On an upload-light
  session it is a third of the upstream total, and a user who sees per-app
  numbers summing to 70% of their upstream needs the reason named, not
  smoothed away. Call it *protocol overhead*, since that is what it was
  measured to be.
* Per-application figures are payload. They should never be presented as the
  number a data cap is measured against; the interface total stays the
  headline, exactly as the first section of this document requires.

## App identity

A pid is useless as a historical key — they recycle within hours. The stored
key is, in order of preference:

1. Flatpak application id, from `/proc/<pid>/root/.flatpak-info` (readable
   because the daemon is privileged).
2. Snap name, from the process's cgroup path.
3. The first *specific* name found at the process, or up to four parents above
   it — see below.
4. The executable's basename, then `comm`, for a process already gone.

A name is specific unless the executable is a **runtime** (electron, node,
python, java, wine's preloaders, Steam's reaper) or a bare **version number**.
For those the process cannot name an application, and the two cases differ:

* A runtime is named by its first argument that looks like a path:
  `/usr/lib/electron43/electron /usr/lib/obsidian/app.asar` is `obsidian`. Its
  own *path* is not consulted — walking that named a Proton game `i386-unix`,
  after a directory inside wine.
* A version-numbered executable *is* the application, installed under its own
  name, so its path is walked: `/opt/teams/2.1.269/2.1.269` is `teams`, and
  `.../claude/versions/2.1.269` is `claude` rather than `versions`.
* When neither yields anything the parent is asked. That is what names
  Electron's `--type=zygote` helpers after the app they belong to, and a
  Proton game after Steam instead of after `wine64-preloader`.

Two traps found by running it against live processes, both now under test:

* **`/proc/<pid>/cmdline` is not always NUL-separated.** Electron and Chrome
  rewrite their own argv into one space-separated blob, so Obsidian's app path
  arrived inside argument zero and was never seen.
* **An argument is not a path just because it is not a flag.** `bash -c` puts
  shell code there, and a process was duly named after a redirect target
  inside its own script. An argument now needs a `/` and no whitespace.

Chromium's forty helper processes share one executable and collapse into one
app, which is the behaviour a user expects. `examples/identify.rs` prints what
these rules make of any live pid; it is how the cases above were found.

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

1. ~~A probe for the attach points and their overhead.~~ Done, 2026-09-12;
   it moved the probe set from the socket layer to the protocol layer and
   found the overhead immaterial.
2. ~~`netmeterd` with the BPF program and in-memory counters.~~ Done,
   2026-09-12. Two bugs worth remembering: declaring an `int`-returning
   kernel function's return value as `long` in `BPF_PROG` reads the register's
   undefined upper half, which showed up as totals inflated by exact multiples
   of 2^32; and `tcp_cleanup_rbuf` passes a per-call *running total*, so
   summing it squares the download figure. Both produced confident,
   well-formatted, wildly wrong tables, which is why the daemon now refuses to
   trust itself when attribution exceeds the wire total.
3. Persistence and retention.
4. The socket, then the GUI view.
