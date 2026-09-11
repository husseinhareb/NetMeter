-- NetMeter schema, version 1.
--
-- Design notes that are load-bearing and must not be "simplified" away:
--
-- * Usage is stored as DELTAS, never as cumulative kernel counters. Deltas are
--   additive, so they survive counter resets, and a row can be updated by
--   accumulation instead of replacement.
--
-- * Every UPSERT below accumulates (`SET rx = rx + excluded.rx`). Replacing
--   instead of accumulating makes each bucket report only the last flush's
--   bytes -- the daily total collapses to near zero while the counters look
--   right, which reads as a display bug and sends you hunting in the wrong
--   layer.
--
-- * Primary keys put the TIME column first. Every query in the product is a
--   date range, optionally filtered by interface, so time-first lets SQLite
--   seek instead of scanning. `WITHOUT ROWID` because these tables are all key
--   plus two integers -- the rowid indirection would double the storage.
--
-- * Day rows are keyed by LOCAL calendar date, resolved once at write time.
--   SQLite cannot do timezone conversion, and a day is not 86400 seconds
--   across DST. Zero-padded ISO dates sort lexicographically exactly as they
--   sort chronologically, so month and year rollups are plain BETWEEN ranges
--   over a prefix -- no LIKE, no date parsing in SQL.

CREATE TABLE IF NOT EXISTS interfaces (
    id            INTEGER PRIMARY KEY,
    name          TEXT    NOT NULL UNIQUE,
    kind          TEXT    NOT NULL,
    -- Nullable: tailscale0 has an empty address file, and a bridge has none
    -- until a member joins. Display metadata only -- never identity, because
    -- WiFi MAC randomization changes it on the same physical NIC.
    mac           TEXT,
    first_seen_utc_ms INTEGER NOT NULL,
    last_seen_utc_ms  INTEGER NOT NULL
);

-- Hour-resolution usage. The tier that grows (24 rows per interface per day),
-- so it is the tier that is pruned.
CREATE TABLE IF NOT EXISTS usage_hour (
    hour_start_utc_ms INTEGER NOT NULL,
    interface_id      INTEGER NOT NULL REFERENCES interfaces(id) ON DELETE CASCADE,
    -- The local calendar date this UTC hour starts in. Denormalised on purpose:
    -- it makes "today" a single indexed equality test.
    local_date        TEXT    NOT NULL,
    rx_bytes          INTEGER NOT NULL DEFAULT 0,
    tx_bytes          INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (hour_start_utc_ms, interface_id)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS ix_usage_hour_local_date
    ON usage_hour (local_date);

-- Day-resolution usage. Written alongside the hour row in the same
-- transaction, not rolled up later -- so a day query never has to union two
-- tables or care whether the rollup job has run. ~40 bytes per interface per
-- day means a decade of four interfaces is well under a megabyte, which is why
-- this tier is kept forever by default.
CREATE TABLE IF NOT EXISTS usage_day (
    local_date   TEXT    NOT NULL,
    interface_id INTEGER NOT NULL REFERENCES interfaces(id) ON DELETE CASCADE,
    rx_bytes     INTEGER NOT NULL DEFAULT 0,
    tx_bytes     INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (local_date, interface_id)
) WITHOUT ROWID;

-- Traffic that the kernel counted while NetMeter was not watching (process
-- closed, or the machine suspended). The counter tells us how many bytes
-- moved, but nothing tells us when inside the window they moved, so they are
-- recorded as a window and reported separately rather than being smeared
-- across days or dumped into the hour the app happened to reopen in.
CREATE TABLE IF NOT EXISTS usage_offline (
    from_utc_ms  INTEGER NOT NULL,
    interface_id INTEGER NOT NULL REFERENCES interfaces(id) ON DELETE CASCADE,
    to_utc_ms    INTEGER NOT NULL,
    rx_bytes     INTEGER NOT NULL DEFAULT 0,
    tx_bytes     INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (from_utc_ms, interface_id)
) WITHOUT ROWID;

-- The last counter reading per interface, so a restart can pick up where it
-- left off. Written in the SAME transaction as the usage rows above: that is
-- what makes a crash lose nothing. If the process dies with unflushed bytes in
-- memory, the stored baseline is equally old, so the next start re-derives
-- exactly those bytes from the kernel's cumulative counter.
--
-- boot_id and ifindex together are the identity guard. Across a reboot the
-- kernel counters restart, and a device recreated under the same name gets a
-- new ifindex; in either case the stored value describes a counter that no
-- longer exists and must not be differenced against.
CREATE TABLE IF NOT EXISTS counter_state (
    interface_id  INTEGER PRIMARY KEY REFERENCES interfaces(id) ON DELETE CASCADE,
    ifindex       INTEGER NOT NULL,
    last_rx_bytes INTEGER NOT NULL,
    last_tx_bytes INTEGER NOT NULL,
    last_seen_utc_ms INTEGER NOT NULL,
    boot_id       TEXT    NOT NULL
) WITHOUT ROWID;

-- Small key/value store for bookkeeping that is not configuration: the date
-- retention last ran, the schema's own provenance.
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;
