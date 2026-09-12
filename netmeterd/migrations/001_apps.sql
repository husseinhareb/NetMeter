-- Per-application usage. A separate database from the GUI's: this one is
-- written by a privileged service and holds every user's rows, so it lives in
-- /var/lib rather than a home directory.

CREATE TABLE apps (
    id                INTEGER PRIMARY KEY,
    -- Flatpak id, snap name, or the application name derived from the
    -- executable. Stable across restarts, unlike a pid.
    app_key           TEXT NOT NULL UNIQUE,
    display_name      TEXT NOT NULL,
    exe_path          TEXT,
    first_seen_utc_ms INTEGER NOT NULL,
    last_seen_utc_ms  INTEGER NOT NULL
) STRICT;

-- Time first in the primary key: every query is a range over time, and this
-- makes that range a contiguous scan. WITHOUT ROWID because the key *is* the
-- row -- there is nothing else to index.
CREATE TABLE usage_app_hour (
    hour_start_utc_ms INTEGER NOT NULL,
    uid               INTEGER NOT NULL,
    app_id            INTEGER NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    -- Carried, not derived: the local date an hour belongs to depends on the
    -- timezone that was in force, and recomputing it later would silently
    -- rewrite history across a DST change.
    local_date        TEXT NOT NULL,
    rx_bytes          INTEGER NOT NULL DEFAULT 0,
    tx_bytes          INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (hour_start_utc_ms, uid, app_id)
) STRICT, WITHOUT ROWID;

CREATE INDEX usage_app_hour_by_date ON usage_app_hour (local_date);

CREATE TABLE usage_app_day (
    local_date TEXT NOT NULL,
    uid        INTEGER NOT NULL,
    app_id     INTEGER NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    rx_bytes   INTEGER NOT NULL DEFAULT 0,
    tx_bytes   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (local_date, uid, app_id)
) STRICT, WITHOUT ROWID;

-- Protocol overhead: the bytes on the wire that no application owns, measured
-- as (interface total - attributed). Headers and acknowledgements, mostly.
-- Kept per day so the UI can show a remainder that adds up instead of a gap
-- the user has to explain to themselves.
CREATE TABLE overhead_day (
    local_date TEXT NOT NULL PRIMARY KEY,
    rx_bytes   INTEGER NOT NULL DEFAULT 0,
    tx_bytes   INTEGER NOT NULL DEFAULT 0
) STRICT, WITHOUT ROWID;

CREATE TABLE meta (
    key   TEXT NOT NULL PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;
