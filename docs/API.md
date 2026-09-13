# Backend API

Everything the frontend can do. It never opens SQLite, never reads
`/proc/net/dev`, never reads `/sys/class/net`.

## Conventions

* **Calendar keys** are zero-padded local strings and ranges are **inclusive on
  both ends**: `"2026"`, `"2026-09"`, `"2026-09-11"`, `"2026-09-11T14"` (that
  last one names a UTC hour).
* **Instants** are `i64` epoch milliseconds UTC, always suffixed `_utc_ms`.
* **Byte counts** are `u64` and never null. An empty period is zeros.
* **Rates** are `number | null`. `null` means "not known yet" — the first
  sample, a zero-length interval, or an interval spanning a suspend — which is a
  different thing from a measured zero.

## Commands

| Command | Returns |
|---|---|
| `get_monitor_status()` | `MonitorStatus` |
| `start_monitor()` | `MonitorStatus` — idempotent |
| `stop_monitor()` | `MonitorStatus` — flushes before returning |
| `get_interfaces()` | `InterfaceInfo[]` |
| `get_live_rates()` | `LiveRates` — from memory, no database |
| `get_usage_series({query})` | `UsageSeries` — the one historical query |
| `get_usage_totals({query})` | `UsageTotals` — the same, without the series |
| `get_today_usage()` | `UsageSeries` for the current local day |
| `get_config()` | `Config` |
| `set_config({config})` | `ConfigApplied` |

### Every view is one `get_usage_series` call

```ts
const series = (q) => invoke('get_usage_series', { query: q })

// Today
series({ granularity: 'day', from: today, to: today })
// Yesterday
series({ granularity: 'day', from: yesterday, to: yesterday })
// Last 7 days, as 7 bars
series({ granularity: 'day', from: sixDaysAgo, to: today })
// Last 30 days
series({ granularity: 'day', from: twentyNineDaysAgo, to: today })
// This month / a previous month
series({ granularity: 'month', from: '2026-09', to: '2026-09' })
// Every month this year, as a series
series({ granularity: 'month', from: '2026-01', to: '2026-12' })
// This year, previous years
series({ granularity: 'year', from: '2024', to: '2026' })
// Custom range, per day
series({ granularity: 'day', from: '2026-03-01', to: '2026-04-15' })
// Today by hour (24 bars, or 23/25 on a DST day)
series({ granularity: 'hour', from: today, to: today })
// Per-interface breakdown of any of the above
series({ granularity: 'day', from: today, to: today, include_breakdown: true })
// Everything observed, including tunnels and bridges (diagnostic)
series({ granularity: 'day', from: today, to: today, scope: 'all' })
```

### `UsageSeries`

```ts
{
  granularity: 'hour' | 'day' | 'month' | 'year',
  timezone: string,                // the zone the day boundaries used
  buckets: [{
    key: string,                   // "2026-09-11"
    start_utc_ms: number,          // carried so a chart can position a bar
    end_utc_ms: number,            // without doing DST arithmetic in JS
    summary: {
      included: { rx_bytes, tx_bytes },   // the usage total
      observed: { rx_bytes, tx_bytes },   // every recorded interface
    },
    by_interface: [{ interface_id, name, kind, included, traffic }],
  }],
  total: { included, observed },
  offline_total: { included, observed },
  offline_windows: [{ from_utc_ms, to_utc_ms, summary }],
  included_interfaces: string[],   // what makes up total.included
  data_since: string | null,       // earliest local date held
  includes_pending: boolean,       // last bucket includes unflushed bytes
  generated_at_utc_ms: number,
}
```

**The series is dense.** Every bucket in the range is present, zero-filled — a
weekend the laptop was off is a zero bar, not a missing one, so the frontend
never reconstructs the local calendar itself. `data_since` lets it grey out
bars from before NetMeter was installed, instead of drawing them as a genuine
zero.

**`included` vs `observed`.** `included` is the headline number and counts each
byte once. `observed` sums every recorded interface and is therefore larger than
reality — a VPN's payload appears twice, a container's traffic three times. See
[ACCOUNTING.md](ACCOUNTING.md). Show `observed` as a diagnostic, never as a
total.

**`offline_*`.** Bytes the kernel counted while NetMeter was not watching. They
are real, but nothing says *when* inside the window they moved, so they are
reported separately rather than dropped into a day. Render them as
"+440 MB while NetMeter was not running (Sep 8 – Sep 11)".

## Events

Subscribe instead of polling. `core:default` in `capabilities/default.json`
already permits this.

| Event | Payload | Rate |
|---|---|---|
| `network-usage-updated` | `LiveUsage` | coalesced, ≤ 1/s by default |
| `interface-added` | `InterfaceChanged` | on change |
| `interface-removed` | `InterfaceChanged` | on change |
| `monitor-status-changed` | `MonitorStatus` | on transition |

`network-usage-updated` carries exactly what `get_live_rates()` returns, so
there is one parser and one code path whether you poll or subscribe.

```ts
// LiveUsage
{
  sampled_at_utc_ms, interval_ms,          // number | null for the interval
  total: DataRate,                         // rate across counted interfaces
  by_interface: [{ name, rate, traffic, included }],
  pending_today: { included, observed },   // NOT the day's total -- see below
  time_anomaly: boolean,                   // byte counts good, timing is not
}
```

**`pending_today` is not today's total.** It is only what is still in memory
awaiting a flush, so it drops back to zero every flush interval. The headline
"today" figure is `get_today_usage().total.included`, which already adds the
pending bytes to the database's.

## `MonitorStatus`

```ts
{
  state: 'stopped' | 'running' | 'degraded' | 'stalled',
  started_at_utc_ms, last_tick_utc_ms, last_flush_utc_ms,   // number | null
  sampling_interval_seconds, interfaces_observed, interfaces_included,
  restarts,        // ticks that panicked and were survived
  parse_errors,    // /proc/net/dev lines that did not parse
  pending,         // bytes held in memory, not yet written
  degraded_reason, // string | null
}
```

`state` is derived from heartbeats, not from a stored flag: `stalled` means no
tick in three intervals. A green light over a dead sampler looks identical to an
idle network, so the status is computed from evidence that the loop is alive.

## `Config`

```jsonc
{
  "sampling_interval_seconds": 5,      // 1..3600
  "flush_interval_seconds": 30,        // 1..3600; larger costs nothing in accuracy
  "database_path": null,               // null = app data dir
  "timezone": null,                    // null = follow the system
  "interface_policy": "physical_only", // | "manual" | "all_except"
  "included_interfaces": [],           // globs; only used by "manual"
  "excluded_interfaces": ["lo"],       // globs; wins under every policy
  "retention": { "hourly_days": 400, "daily_days": 0, "interface_days": 90 },
  "logging_level": "info",
  "event_interval_ms": 1000
}
```

`set_config` validates before writing, applies to the running engine without a
restart, and returns the resolved `included_interfaces` so the UI never
re-implements glob matching. Raising `flush_interval_seconds` reduces disk
writes without costing accuracy — buckets are keyed at sample time, not flush
time.

## Errors

Commands reject with `{ kind, message }`. `kind` is one of `monitor`,
`storage`, `storage_read_only`, `storage_disk_full`, `config`, `bad_request`,
`internal` — so the UI can branch without matching on message text.
