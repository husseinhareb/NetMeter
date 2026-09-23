# Rejected / Refuted Candidates — NetMeter bug hunt

These were investigated and ruled out at the 6-point confidence bar.
Recorded here so they are not re-raised. Each lists the claim, why it does
not hold, and the evidence.

---

## R-01 — Offline-window double-count
**Claim:** Bytes counted while offline get credited both as an
`usage_offline` window *and* into a normal bucket on the next restart, so
totaling over a period double-counts the gap.

**Refuted.** `Sampler::attribute` (sampling.rs:366–402) routes a long gap
(`gap_ms > max_offline_ms`, or `crash_window`-crossed) *only* to
`out.offline` — it never also calls `push_bucket`. And the baseline is
re-baselined to `now` in the same `sample()` call (sampling.rs:337–344) after
the delta is taken, so the *next* tick differs from the new baseline, not the
old one. No second credit exists.

---

## R-02 — Bucket-key mismatch at month/year granularity (hour vs day rows)
**Claim:** At `Month`/`Year` granularity a query's `substr(local_date,1,?3)`
("YYYY-MM") would not match dense-series bucket keys, or a "trailing-dash"
`"YYYY-MM-"` mismatch would drop data.

**Refuted.** Resolution mapping is strict (commands.rs:81–84):
`Granularity::Hour => Resolution::Hour`; Day/Week/Month/Year all `=>
Resolution::Day`. Hour rows are keyed by full UTC key `key_len=13`
("YYYY-MM-DDTHH"); day/month/year rows use `local_date` truncated to `key_len`
7/4. The `Month` arm uses `Resolution::Day` with `key_len=7`, so *both* the
SQL `substr(local_date,1,7)` and the dense `local_date_key(..7)` yield
"YYYY-MM". Hour resolution is never mixed with day rows in any query, and no
trailing-dash key is ever produced. No mismatch path.

---

## R-03 — Orphaned `counter_state` after interface GC
**Claim:** Retention deletes `interfaces` rows (repository.rs:518–523) and
leaves dangling `counter_state` rows (keyed by `interface_id`), so `baselines()`
JOIN could miss them or leave orphans.

**Refuted.** Schema (migrations/001_init.sql:92):
`counter_state.interface_id INTEGER PRIMARY KEY REFERENCES interfaces(id) ON
DELETE CASCADE` — deleting the interface row cascades to `counter_state` (and
to `usage_hour/day/offline`, all `ON DELETE CASCADE`). Retention only deletes
interfaces that are *also* absent from all three `usage_*` tables, so there is
nothing to orphan. The `baselines()` JOIN (repository.rs:402) is safe.

---

## R-04 — `SampleOutcome.rates` holds raw deltas, not bytes/s
**Claim:** `rates[name]` is a raw `Traffic` (delta bytes) even when `interval`
is `None` (suspended / first tick), and something renders it as a rate.

**Refuted.** `live_payload` (engine.rs:862–876) treats `rates[name]` as raw
traffic and computes the rate *only* when `interval` is `Some`:
`rate: interval.map(|d| statistics::rate(*traffic, d)).unwrap_or(DataRate::UNKNOWN)`.
On suspend / first tick `out.interval` is explicitly set to `None`
(sampling.rs:261–264, 245–248), so no divide-by-zero and no bogus rate. The
raw `Traffic` is also surfaced as the `traffic` field separately. Intended
design, not a bug.

---

## R-05 — `ifindex == 0` hole in the device-identity guard
**Claim:** `sampling.rs:307` accepts a prior baseline when
`s.ifindex == 0 || prev.ifindex == 0`, so a recreated device (same name, new
device) whose ifindex read fails would bypass the guard and produce a spurious
delta.

**Refuted (not a certain bug).** `ifindex` is read from
`/sys/class/net/<name>/ifindex` with `unwrap_or(0)` (collector.rs:105, 131);
the real-system integration test asserts `ifindex > 0` for every interface
(collector.rs:383) — on any live kernel the file exists for every interface in
`/proc/net/dev`, and a recreated device *updates* (not removes) that file with
its new ifindex. The `0` sentinel is a defensive fallback for a read race
that is not a reliable way to observe device recreation, and the fallback
("unknown → trust the delta") is a reasonable choice. Not triggerable as a
reproducible spurious-delta bug at 6 points.

---

## Notes / low-confidence (not bug-bar)
- **Hour view is always UTC** regardless of configured tz (dense Hour tier uses
  UTC `hour_start_ms`); Day/Month/Year are local. This is a display
  convention (hour buckets are kernel-time, day buckets are local), not a data
  integrity bug.
- **Tz-change edge:** `local_date` is resolved at write time; changing tz
  later could misalign historical `local_date` vs new-tz bounds/keys. Not a
  reproducible in-range bug; design assumes stable tz (see schema note
  001_init.sql:20–24).
