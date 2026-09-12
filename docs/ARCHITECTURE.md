# NetMeter backend architecture

One Tauri project, one Cargo crate, five modules with a one-way dependency
chain:

```
measurement  ->  processing  ->  persistence  ->  API  ->  visualization
 monitor/         core/           storage/        api/      (frontend)
```

```
┌──────────────────────────────┐
│   NetMeter Tauri frontend    │   visualises and configures; never collects
└──────────────┬───────────────┘
        Tauri commands + events
┌──────────────▼───────────────┐
│         api/  (DTOs)         │   the only module that knows a GUI exists
└──────┬────────────────┬──────┘
       │                │
┌──────▼──────┐  ┌──────▼───────┐
│  monitor/   │  │   storage/   │   the only module containing SQL
│  (engine)   │─▶│ (repository) │
└──────┬──────┘  └──────┬───────┘
       │                ▼
       │            SQLite (WAL)
       ▼
 /proc/net/dev, /sys/class/net      read-only, unprivileged
```

`core/` sits under all of them and depends on none: domain types plus pure
functions, no Tauri, no SQLite, no syscalls.

| Module | Holds | Must not contain |
|---|---|---|
| `core/` | types, delta/reset math, DST-safe calendar, errors, config | anything I/O |
| `monitor/` | provider trait, `/proc` parser, sysfs classifier, sampler, engine loop | SQL, Tauri |
| `storage/` | pragmas, migrations, repository, aggregation | `/proc`, Tauri |
| `api/` | commands, DTOs, `EventSink` | SQL, `/proc` |
| `system/` | clocks, paths, instance lock, service lifecycle | domain logic |

## 1. Measurement

One `read()` of `/proc/net/dev` per tick returns every interface's counters
together, so readings never skew against each other. Measured cost on the
development machine: **190 µs per read** — 0.004% of one core at a 5 s
interval.

`/sys/class/net` is read only when classification can have changed: the
interface *name set* changed, a counter went backwards (a device may have been
recreated under the same name), or every 60th tick as a backstop.

Not used, deliberately: shell commands, packet capture, raw sockets, netlink.
The kernel's counters are the cheapest and most accurate source, and reading
them needs no privileges — no root, no setuid, no capabilities.

**Parsing.** The separator is the colon, not whitespace: older kernels render
`"%6s:%8llu"` with no space after the colon, so a wide byte count yields
`eth0:1234567890`. `dev_valid_name()` rejects `:` in interface names, so the
first colon is unambiguous. Fields are located by index from the left, never by
character offset — values routinely overflow their `%7llu` width. The file is
read as **bytes**, not a `String`, because `read_to_string` rejects the *whole*
file if any one interface name is not valid UTF-8. Every line is individually
fallible: a bad line costs that interface, never the tick.

## 2. Counter resets and interface changes

Three cases, judged per direction:

| Observation | Credited | Why |
|---|---|---|
| counter advanced | `new - prev` | ordinary traffic |
| counter went **backwards** | `new` | the device was recreated and the counter restarted; whatever preceded the reset is unrecoverable, but `new` is real bytes the kernel has counted on this interface since |
| first ever sighting | `0` | the counter holds pre-NetMeter history, which is not usage we measured |

Crediting `0` on a reset — the intuitive "discard the sample" reading — silently
under-counts every suspend/resume and every dongle replug, on exactly the
physical interfaces the total is made of.

Wrap-around is **not** handled: `/proc/net/dev` renders `rtnl_link_stats64`, so
counters are 64-bit (this machine's `wlp7s0` is already past 2³²). Treating a
decrease as a wrap would invent petabytes the first time an interface is
recreated.

**Identity** is guarded by `(boot_id, ifindex)`, not by name:

* a reboot changes `boot_id`, so stored baselines are discarded — the kernel's
  counters restarted and a stored reading means nothing;
* a device recreated under the same name gets a new `ifindex`, so a replaced USB
  dongle cannot inherit a baseline from a device it never was;
* `ifindex` survives a udev *rename* and MAC randomization, which are the two
  things that would otherwise split one NIC's history in two.

MAC is display metadata only: `tailscale0` has none, bridge MACs derive from
members, and WiFi randomizes per SSID.

## 3. Time

Three clocks, because none answers alone:

| clock | advances while suspended | affected by NTP |
|---|---|---|
| `CLOCK_MONOTONIC` | no | no |
| `CLOCK_BOOTTIME` | yes | no |
| wall clock | yes | yes |

So `BOOTTIME − MONOTONIC` **is** the suspended duration and `wall − BOOTTIME`
**is** the clock step — both measured, neither inferred from a threshold on a
single clock, and neither resting on `Instant`'s suspend behaviour, which `std`
explicitly declines to specify.

* Rates divide by **monotonic** time, so a rate is bytes per second of *awake*
  time and an NTP step cannot look like a traffic spike.
* Bucket keys come from the **wall clock**, the only one that names a date.
* A detected clock step **holds the sample**: baselines are not advanced, so the
  next good tick's delta is still exactly right. Nothing is lost by waiting, and
  no row is minted for a year that has not happened.

Day boundaries come from the IANA database, never from adding 86 400 s. Local
midnight does not always exist — Santiago, Havana and Tehran shift DST *at*
midnight — so `day_start` walks forward to the first instant the zone admits
rather than unwrapping a `MappedLocalTime::None`. Ranges are half-open
`[start, end)`, which makes 23- and 25-hour days fall out of the arithmetic and
guarantees adjacent days neither overlap nor gap.

## 4. Schema

```sql
interfaces   (id, name UNIQUE, kind, mac, first_seen_utc_ms, last_seen_utc_ms)

usage_hour   (hour_start_utc_ms, interface_id, local_date, rx_bytes, tx_bytes)
             PRIMARY KEY (hour_start_utc_ms, interface_id) WITHOUT ROWID

usage_day    (local_date, interface_id, rx_bytes, tx_bytes)
             PRIMARY KEY (local_date, interface_id) WITHOUT ROWID

usage_offline(from_utc_ms, interface_id, to_utc_ms, rx_bytes, tx_bytes)

counter_state(interface_id, ifindex, last_rx_bytes, last_tx_bytes,
              last_seen_utc_ms, boot_id)

meta         (key, value)
```

Five properties are load-bearing:

1. **Deltas, never cumulative counters.** Deltas are additive, so they survive
   resets and a row updates by accumulation.
2. **Every upsert accumulates** (`SET rx = rx + excluded.rx`). Replacing would
   leave each bucket holding only the last flush's bytes — the day total would
   collapse to near zero while the counters still looked right, which reads as a
   display bug and sends you hunting in the wrong layer.
3. **Time column first in every primary key.** Every product query is a date
   range, so this is a seek rather than a scan. Zero-padded ISO dates sort
   lexicographically exactly as they sort chronologically, so month and year
   rollups are `BETWEEN` over a prefix — no `LIKE`, no date parsing in SQL.
4. **Day rows keyed by *local* date, resolved once at write time.** SQLite
   cannot do timezone conversion, and a day is not 86 400 s across DST.
5. **`usage_day` is written alongside `usage_hour` in the same transaction**,
   not rolled up later, so a day query never unions two tables or depends on a
   rollup job having run.

Buckets are keyed at **sample time**, not flush time. The accumulator maps
`(interface, UTC hour, local date) → bytes`, so one flush may write two hours or
two days. Keying at flush time would push up to a whole flush interval across
every midnight — "Today" would start each day wrong, permanently, and invisibly,
because the totals would still add up.

Pragmas: `auto_vacuum=INCREMENTAL`, `journal_mode=WAL`, `synchronous=NORMAL`,
`busy_timeout=5000`, `foreign_keys=ON`, `wal_autocheckpoint=1000`,
`journal_size_limit=8388608` (the one that actually *truncates* the WAL —
without it the `-wal` file sits at its historical high-water mark for ever).

**The order matters.** `auto_vacuum` must be set before any table exists *and*
before `journal_mode=WAL`; setting it afterwards reports success and is
silently ignored, leaving the database at `NONE` so `incremental_vacuum`
reclaims nothing. Verified: WAL-then-`auto_vacuum` gives `PRAGMA auto_vacuum`
= 0, `auto_vacuum`-then-WAL gives 2. There is a test pinning it.

## 5. Crash safety and database growth

**Crash safety.** `counter_state` is written in the *same transaction* as the
usage rows. If the process dies with unflushed bytes in memory, the stored
baseline is equally stale, so the next start re-derives exactly those bytes from
the kernel's cumulative counter. A crash costs attribution precision, not bytes.

The flush **snapshots** the buffer rather than taking it, and removes only what
committed. Taking first and failing afterwards is the classic way this shape of
design loses 30 s of traffic per failed write. After 20 consecutive failures —
a full disk, a read-only filesystem — the buffer collapses to day granularity so
memory stays flat while every byte and every calendar date survives.

Exit paths that flush: `RunEvent::Exit`, `SIGTERM`/`SIGINT` (a handler that does
nothing but one atomic store), `Engine::stop`, and `Drop`. `SIGTERM` matters
most — it is what logout and reboot send, and it is when the unflushed traffic
is largest.

**Growth.**

| tier | retention | size |
|---|---|---|
| `usage_hour` | 400 days | 24 rows/interface/day |
| `usage_day` | forever | ~40 bytes/interface/day |
| `usage_offline` | with day rows | one row per gap |

With three or four counted interfaces that is well under 10 MB after a decade.
Two further bounds: **bridge legs are never persisted** (Docker mints a fresh
random `vethXXXXXXX` per container start, so they would grow a forever-table
without bound), and interfaces that are gone and hold no history are garbage
collected.

Retention cutoffs are anchored to `MAX()` of the data, **never to `now()`**. A
clock stepped forward a year would otherwise delete every row on the next
startup — and there is no source to re-derive it from, because the kernel keeps a
cumulative counter, not a history. The daily pass is triggered by the local date
*changing*, not by a 24-hour countdown, which a mostly-suspended laptop would
never reach.

**Traffic that cannot be placed in time** — the app was closed for three days,
or the machine slept for eight hours — goes to `usage_offline` with the window it
happened in, and is reported as `offline_total` separately from any bucket.
Dumping it into the reopening hour would make "Today" show three days of usage;
spreading it evenly would be inventing a plausible shape. Neither is honest, so
NetMeter reports the window and lets the UI say so.

## 6. Becoming a systemd service

> Per-application accounting needs a privileged helper and is specified
> separately in [PER_APP.md](PER_APP.md). It is the first concrete reason for
> `netmeterd` to exist.

The engine already takes its three collaborators as traits:

```rust
Engine::start(
    provider:   impl NetworkStatsProvider,   // LinuxNetworkStatsProvider
    repository: impl Repository,             // SqliteRepository
    sink:       Arc<impl EventSink>,         // TauriEventSink today
    config, boot_id,
)
```

Nothing in `monitor/` or `storage/` mentions Tauri; the single Tauri-aware type
is `TauriEventSink` in `lib.rs`, ~20 lines. So the transition is:

```
today                          later
─────                          ─────
NetMeter GUI                   systemd --user
  └─ Engine (in-process)         └─ netmeterd  ── Engine + SqliteRepository
       └─ TauriEventSink                            └─ SocketEventSink
                                 NetMeter GUI ── local IPC ──┘
```

Add a `[[bin]] netmeterd` to the same `Cargo.toml`, construct the same `Engine`
with a socket-backed `EventSink`, and point the GUI at the socket. The
monitoring logic does not change.

The single-writer problem is already solved: an `flock` on
`<data_dir>/netmeter.lock` is held for the process lifetime, and the GUI refuses
to start its engine when it cannot take it. Two writers would not corrupt
anything — WAL serializes them — they would *silently double every byte*, in the
one table kept forever. The kernel releases an `flock` however the process dies,
so there is no stale-lock case.

## 7. Status

`MonitorStatus` is derived from heartbeats, never from a stored boolean. A flag
cannot tell "sampling" from "the thread died an hour ago", and a green light over
a dead sampler is the worst failure a usage meter can have: it is
indistinguishable from an idle network. `Stalled` when no tick in 3 intervals,
`Degraded` when writes are failing or a tick has panicked, plus `parse_errors`
so a systematically wrong parser is visible rather than silent — that count is
read from the provider through the `NetworkStatsProvider` trait each tick, not
stored and forgotten.

Each tick runs under `catch_unwind`. A panic costs that tick, increments
`restarts`, forces `Degraded`, and backs off for a second; the loop state lives
outside the closure, so the next tick resumes with its baselines and its
buffered bytes intact. The bytes a tick produced are moved into the shared
buffer immediately after `sample()` — before anything else that could fail —
because `sample()` has already advanced the in-memory baselines and those bytes
then exist nowhere else.
