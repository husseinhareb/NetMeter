import { useEffect, useState, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
} from "@tauri-apps/plugin-notification";
import "./App.css";

type Traffic = { rx_bytes: number; tx_bytes: number };
type Rate = { rx_bytes_per_sec: number | null; tx_bytes_per_sec: number | null };

type Live = {
  total: Rate;
  by_interface: { name: string; rate: Rate; included: boolean }[];
  time_anomaly: boolean;
};

type InterfaceInfo = {
  name: string;
  kind: string;
  present: boolean;
  included: boolean;
};

type Config = {
  sampling_interval_seconds: number;
  quota: {
    monthly_bytes: number | null;
    warn_at_percent: number[];
    counts: "both" | "download" | "upload";
  };
  interface_policy: "physical_only" | "manual" | "all_except";
  included_interfaces: string[];
  excluded_interfaces: string[];
  [key: string]: unknown;
};

type QuotaWarning = {
  month: string;
  percent: number;
  used_bytes: number;
  limit_bytes: number;
};

type Status = {
  state: string;
  sampling_interval_seconds: number;
  degraded_reason: string | null;
};

type AppTraffic = { app: string; rx_bytes: number; tx_bytes: number };

type HelperState =
  | {
      state: "running";
      probes_attached: number;
      probes_expected: number;
      started_at_utc_ms: number;
    }
  | { state: "not_installed" }
  | { state: "unreachable"; message: string };

type AppBucket = {
  key: string;
  start_utc_ms: number;
  end_utc_ms: number;
  apps: AppTraffic[];
  overhead: Traffic;
};

type AppUsage = {
  granularity?: Granularity;
  timezone?: string;
  buckets: AppBucket[];
  total: AppTraffic[];
  overhead: Traffic;
  data_since?: string | null;
  uid?: number;
  generated_at_utc_ms?: number;
};

type Series = {
  total: { included: Traffic; observed: Traffic };
  buckets: {
    key: string;
    start_utc_ms: number;
    summary: { included: Traffic; observed: Traffic };
    by_interface: { name: string; traffic: Traffic; included: boolean }[];
  }[];
  offline_total: { included: Traffic; observed: Traffic };
  offline_windows: { from_utc_ms: number; to_utc_ms: number }[];
  data_since: string | null;
};

type Granularity = "hour" | "day" | "month" | "year";

type Range = { label: string; granularity: Granularity; from: string; to: string };

type Page = "overview" | "settings" | "history" | "apps";

type SortKey = "app" | "rx" | "tx" | "total";

const UNITS = ["B", "KiB", "MiB", "GiB", "TiB"];

// Sums traffic across rows. Extras let the protocol overhead join the same total
// when the table is showing it.
function sumTraffic(...groups: Traffic[][]): Traffic & { total_bytes: number } {
  const sum = groups.flat().reduce(
    (acc, t) => {
      acc.rx_bytes += t.rx_bytes;
      acc.tx_bytes += t.tx_bytes;
      return acc;
    },
    { rx_bytes: 0, tx_bytes: 0 },
  );
  return { ...sum, total_bytes: sum.rx_bytes + sum.tx_bytes };
}

function bytes(n: number): string {
  let u = 0;
  while (n >= 1024 && u < UNITS.length - 1) {
    n /= 1024;
    u++;
  }
  return `${n < 10 && u > 0 ? n.toFixed(1) : Math.round(n)} ${UNITS[u]}`;
}

const gib = (bytes: number) => Math.round(bytes / 1e9);

const perSec = (n: number | null) => (n === null ? "–" : `${bytes(n)}/s`);

// Both halves or neither: a sum built from a missing sample is not a rate.
const sumRate = (r: Rate | undefined) =>
  r && r.rx_bytes_per_sec !== null && r.tx_bytes_per_sec !== null
    ? r.rx_bytes_per_sec + r.tx_bytes_per_sec
    : null;

// Calendar keys are built from a local Date so day arithmetic crosses DST the
// way the calendar does. Which buckets those keys actually cover is the
// backend's decision, not this file's.
const pad = (n: number) => String(n).padStart(2, "0");
const dayKey = (d: Date) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
const monthKey = (d: Date) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}`;

function daysAgo(n: number): Date {
  const d = new Date();
  d.setDate(d.getDate() - n);
  return d;
}

function lastMonthDate(): Date {
  const d = new Date();
  d.setDate(1);
  d.setMonth(d.getMonth() - 1);
  return d;
}

function ranges(): Range[] {
  const today = new Date();
  const lastMonth = lastMonthDate();
  return [
    { label: "Today", granularity: "hour", from: dayKey(today), to: dayKey(today) },
    { label: "Yesterday", granularity: "hour", from: dayKey(daysAgo(1)), to: dayKey(daysAgo(1)) },
    { label: "7 days", granularity: "day", from: dayKey(daysAgo(6)), to: dayKey(today) },
    { label: "30 days", granularity: "day", from: dayKey(daysAgo(29)), to: dayKey(today) },
    { label: "This month", granularity: "day", from: monthKey(today), to: monthKey(today) },
    { label: "Last month", granularity: "day", from: monthKey(lastMonth), to: monthKey(lastMonth) },
    {
      label: "This year",
      granularity: "month",
      from: String(today.getFullYear()),
      to: String(today.getFullYear()),
    },
  ];
}

// An hour key names a UTC hour ("2026-09-12T14"), so it cannot be shown as
// written: at UTC+2 the first bucket of the local day reads "22". Both the
// label and the date comparison come from the instant instead.
function bucketLabel(key: string, startUtcMs: number): string {
  if (key.includes("T")) return `${pad(new Date(startUtcMs).getHours())}:00`;
  return key;
}

// The same instant, shortened to fit under a chart column.
const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

function tickLabel(key: string, startUtcMs: number): string {
  if (key.includes("T")) return pad(new Date(startUtcMs).getHours());
  const parts = key.split("-");
  if (parts.length === 3) return String(Number(parts[2]));
  if (parts.length === 2) return MONTHS[Number(parts[1]) - 1] ?? key;
  return key;
}

function span(from: number, to: number): string {
  const f = new Date(from);
  const t = new Date(to);
  return `${f.toLocaleString()} – ${t.toLocaleString()}`;
}

function downloadFile(content: string, filename: string, mimeType: string) {
  const blob = new Blob([content], { type: mimeType });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  document.body.removeChild(a);
  URL.revokeObjectURL(url);
}

function exportHistoryCsv(series: Series, rangeLabel: string) {
  const rows = [
    ["Period", "Download Bytes", "Upload Bytes", "Total Bytes", "Download Formatted", "Upload Formatted", "Total Formatted"],
    ...series.buckets.map((b) => [
      b.key,
      b.summary.included.rx_bytes,
      b.summary.included.tx_bytes,
      b.summary.included.rx_bytes + b.summary.included.tx_bytes,
      bytes(b.summary.included.rx_bytes),
      bytes(b.summary.included.tx_bytes),
      bytes(b.summary.included.rx_bytes + b.summary.included.tx_bytes),
    ]),
    [
      "TOTAL",
      series.total.included.rx_bytes,
      series.total.included.tx_bytes,
      series.total.included.rx_bytes + series.total.included.tx_bytes,
      bytes(series.total.included.rx_bytes),
      bytes(series.total.included.tx_bytes),
      bytes(series.total.included.rx_bytes + series.total.included.tx_bytes),
    ],
  ];
  const csv = rows.map((r) => r.map((cell) => `"${String(cell).replace(/"/g, '""')}"`).join(",")).join("\n");
  downloadFile(csv, `netmeter-history-${rangeLabel.toLowerCase().replace(/[^a-z0-9]+/g, "_")}.csv`, "text/csv;charset=utf-8");
}

function exportHistoryJson(series: Series, rangeLabel: string) {
  downloadFile(JSON.stringify(series, null, 2), `netmeter-history-${rangeLabel.toLowerCase().replace(/[^a-z0-9]+/g, "_")}.json`, "application/json");
}

function exportAppsCsv(apps: AppUsage, rangeLabel: string) {
  const rows = [
    ["Application", "Download Bytes", "Upload Bytes", "Total Bytes", "Download Formatted", "Upload Formatted", "Total Formatted"],
    ...apps.total.map((a) => [
      a.app,
      a.rx_bytes,
      a.tx_bytes,
      a.rx_bytes + a.tx_bytes,
      bytes(a.rx_bytes),
      bytes(a.tx_bytes),
      bytes(a.rx_bytes + a.tx_bytes),
    ]),
    [
      "protocol overhead",
      apps.overhead.rx_bytes,
      apps.overhead.tx_bytes,
      apps.overhead.rx_bytes + apps.overhead.tx_bytes,
      bytes(apps.overhead.rx_bytes),
      bytes(apps.overhead.tx_bytes),
      bytes(apps.overhead.rx_bytes + apps.overhead.tx_bytes),
    ],
  ];
  const csv = rows.map((r) => r.map((cell) => `"${String(cell).replace(/"/g, '""')}"`).join(",")).join("\n");
  downloadFile(csv, `netmeter-apps-${rangeLabel.toLowerCase().replace(/[^a-z0-9]+/g, "_")}.csv`, "text/csv;charset=utf-8");
}

function exportAppsJson(apps: AppUsage, rangeLabel: string) {
  downloadFile(JSON.stringify(apps, null, 2), `netmeter-apps-${rangeLabel.toLowerCase().replace(/[^a-z0-9]+/g, "_")}.json`, "application/json");
}

/* ---------------------------------------------------------------- icons -- */

// 16px Fluent-weight line icons. Inline so the bundle stays dependency-free.
const ICONS: Record<string, string> = {
  overview: "M2.8 11a5.2 5.2 0 1 1 10.4 0M8 11l2.7-3.6",
  settings: "M2 4.6h11.5M2 11.4h11.5",
  history: "M8 4.6V8l2.4 1.5",
  apps: "",
  wifi: "M1.9 6.1a8.7 8.7 0 0 1 12.2 0M4.3 8.7a5.3 5.3 0 0 1 7.4 0",
  ethernet: "M5.6 4.4V2.4M10.4 4.4V2.4M5.5 12.8v-2.4M10.5 12.8v-2.4",
  wwan: "M3.2 12.6v-2.2M6.4 12.6V8M9.6 12.6V5.4M12.8 12.6V3",
  vpn: "M8 1.9 13 3.7v4c0 2.9-2 5-5 6.3-3-1.3-5-3.4-5-6.3v-4z",
  bridge: "M5.6 8h4.8",
  loopback: "M12.6 8a4.6 4.6 0 1 1-1.7-3.55M8.1 4.1l2.9.5.4-2.9",
  virtual: "",
  down: "M8 3.2v8.4M4.7 8.4 8 11.8l3.3-3.4",
  up: "M8 12.8V4.4M4.7 7.8 8 4.4l3.3 3.4",
  total: "M5.2 3.2v7.6M3 8.6l2.2 2.2 2.2-2.2M10.8 12.8V5.2M8.6 7.4l2.2-2.2 2.2 2.2",
  info: "M8 7.3v4.1M8 4.8h.01",
  warn: "M8 2.7 14 13.4H2zM8 6.6V10M8 11.9h.01",
  search: "M10.6 10.6 14 14",
  chevron: "M4.2 6.4 8 10.2l3.8-3.8",
  back: "M9.5 3.5L5 8l4.5 4.5",
  calendar: "M2.5 4.5h11M2.5 2.5v11h11v-11zM5 1.5v2M11 1.5v2",
  download_file: "M8 2.5v7.5M5.5 7.5l2.5 2.5 2.5-2.5M3 13.5h10",
  close: "M4.5 4.5l7 7M11.5 4.5l-7 7",
};

const CIRCLES: Record<string, [number, number, number][]> = {
  overview: [[8, 11, 0.85]],
  settings: [
    [5.8, 4.6, 1.9],
    [10.2, 11.4, 1.9],
  ],
  history: [[8, 8, 5.6]],
  wifi: [[8, 11.5, 0.9]],
  search: [[7, 7, 4.4]],
};

const RECTS: Record<string, [number, number, number, number, number][]> = {
  apps: [
    [2.4, 2.4, 4.6, 4.6, 1.2],
    [9, 2.4, 4.6, 4.6, 1.2],
    [2.4, 9, 4.6, 4.6, 1.2],
    [9, 9, 4.6, 4.6, 1.2],
  ],
  ethernet: [[2.4, 4.4, 11.2, 6, 1.4]],
  bridge: [
    [1.4, 5.6, 4.2, 4.8, 1.2],
    [10.4, 5.6, 4.2, 4.8, 1.2],
  ],
  virtual: [[2.4, 2.4, 11.2, 11.2, 2]],
};

function Icon({ name, className }: { name: string; className?: string }) {
  return (
    <svg
      className={`icon${className ? ` ${className}` : ""}`}
      viewBox="0 0 16 16"
      width="16"
      height="16"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.3"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      focusable="false"
    >
      {(RECTS[name] ?? []).map(([x, y, w, h, r], n) => (
        <rect key={n} x={x} y={y} width={w} height={h} rx={r} />
      ))}
      {(CIRCLES[name] ?? []).map(([cx, cy, r], n) => (
        <circle key={n} cx={cx} cy={cy} r={r} />
      ))}
      {ICONS[name] && <path d={ICONS[name]} />}
    </svg>
  );
}

// Interface kinds come from the backend as snake_case; anything new lands on
// the neutral glyph rather than disappearing.
const KIND_ICON: Record<string, string> = {
  ethernet: "ethernet",
  wifi: "wifi",
  wwan: "wwan",
  loopback: "loopback",
  vpn: "vpn",
  bridge: "bridge",
  enslaved: "bridge",
  vlan: "bridge",
  virtual: "virtual",
};

// Module-level icon cache and subscription registry, matching hw-monitor's architecture.
const iconCache = new Map<string, string | null>();
const iconListeners = new Map<string, Set<(src: string | null) => void>>();
const iconInFlight = new Set<string>();

function notifyIconListeners(name: string, result: string | null) {
  iconListeners.get(name)?.forEach((listener) => listener(result));
}

function subscribeIcon(name: string, onResult: (src: string | null) => void) {
  if (!iconListeners.has(name)) {
    iconListeners.set(name, new Set());
  }

  const currentListeners = iconListeners.get(name)!;
  currentListeners.add(onResult);

  return () => {
    currentListeners.delete(onResult);
    if (currentListeners.size === 0) {
      iconListeners.delete(name);
    }
  };
}

function fetchProcessIcon(name: string): void {
  if (iconCache.has(name) || iconInFlight.has(name)) {
    return;
  }

  iconInFlight.add(name);

  invoke<string | null>("get_process_icon", { name })
    .then((result) => {
      iconCache.set(name, result);
      notifyIconListeners(name, result);
    })
    .catch(() => {
      iconCache.set(name, null);
      notifyIconListeners(name, null);
    })
    .finally(() => {
      iconInFlight.delete(name);
    });
}

function useProcessIconSrc(name: string): string | null {
  const [src, setSrc] = useState<string | null>(() => iconCache.get(name) ?? null);

  useEffect(() => {
    if (!name) {
      setSrc(null);
      return;
    }

    setSrc(iconCache.get(name) ?? null);
    const unsubscribe = subscribeIcon(name, setSrc);
    fetchProcessIcon(name);
    return unsubscribe;
  }, [name]);

  return src;
}

function ProcessIcon({
  name,
  size = 16,
  className,
}: {
  name: string;
  size?: number;
  className?: string;
}) {
  const realSrc = useProcessIconSrc(name);
  const [imgFailed, setImgFailed] = useState(false);

  useEffect(() => {
    setImgFailed(false);
  }, [realSrc]);

  if (realSrc && !imgFailed) {
    return (
      <img
        src={realSrc}
        width={size}
        height={size}
        className={`process-icon ${className ?? ""}`}
        onError={() => setImgFailed(true)}
        alt=""
        aria-hidden="true"
      />
    );
  }

  // Nothing on disk: most of these are CLI tools and daemons that ship no icon.
  // A per-name letter chip keeps the rows distinguishable at a glance.
  const letter = (name.match(/[a-z0-9]/i)?.[0] ?? "?").toUpperCase();
  let hash = 0;
  for (let i = 0; i < name.length; i++) hash = (hash * 31 + name.charCodeAt(i)) | 0;

  return (
    <span
      className={`process-icon letter ${className ?? ""}`}
      style={{ width: size, height: size, fontSize: Math.round(size * 0.62), ["--chip-hue" as string]: Math.abs(hash) % 360 }}
      aria-hidden="true"
    >
      {letter}
    </span>
  );
}

/* ------------------------------------------------------------ primitives -- */

function Banner({
  tone,
  children,
}: {
  tone: "error" | "warn" | "info";
  children: ReactNode;
}) {
  return (
    <div className={`banner ${tone}`} role={tone === "error" ? "alert" : undefined}>
      <Icon name={tone === "info" ? "info" : "warn"} className="banner-icon" />
      <div className="banner-body">{children}</div>
    </div>
  );
}

function Meter({
  label,
  value,
  dir,
}: {
  label: string;
  value: string;
  dir: "down" | "up" | "total";
}) {
  return (
    <div className="row">
      <span className={`row-icon ${dir}`}>
        <Icon name={dir} />
      </span>
      <span className="row-title">{label}</span>
      <span className={`row-value num ${dir}`}>{value}</span>
    </div>
  );
}

function SortHeader({
  label,
  col,
  sort,
  onSort,
  align,
}: {
  label: string;
  col: SortKey;
  sort: { key: SortKey; dir: "asc" | "desc" } | null;
  onSort: (k: SortKey) => void;
  align?: "num";
}) {
  const on = sort?.key === col;
  return (
    <th className={align === "num" ? "num" : ""} aria-sort={on ? (sort!.dir === "asc" ? "ascending" : "descending") : "none"}>
      <button className={`sorter${on ? " on" : ""}`} onClick={() => onSort(col)}>
        {label}
        <Icon name="chevron" className={`sort-arrow${on && sort!.dir === "asc" ? " up" : ""}${on ? "" : " off"}`} />
      </button>
    </th>
  );
}

/* ---------------------------------------------------------------- charts -- */

function UsageChart({
  series,
  canDrill,
  onDrillDown,
}: {
  series: Series;
  canDrill?: boolean;
  onDrillDown?: (colKey: string, startUtcMs: number) => void;
}) {
  const cols = series.buckets.map((b) => {
    // Before NetMeter held any data, a zero is absence rather than an idle
    // period, and it is drawn as neither.
    // Two kinds of blank: before NetMeter held data, and after now. Neither is
    // a zero, and a month view spans both.
    const before =
      series.data_since !== null && dayKey(new Date(b.start_utc_ms)) < series.data_since;
    const future = b.start_utc_ms > Date.now();
    return {
      key: b.key,
      startUtcMs: b.start_utc_ms,
      label: bucketLabel(b.key, b.start_utc_ms),
      tick: tickLabel(b.key, b.start_utc_ms),
      rx: b.summary.included.rx_bytes,
      tx: b.summary.included.tx_bytes,
      total: b.summary.included.rx_bytes + b.summary.included.tx_bytes,
      before,
      future,
      unknown: before || future,
    };
  });
  // A range with nothing in it gets a sentence, not an axis scaled to one byte.
  const measured = Math.max(0, ...cols.map((c) => c.total));
  if (measured === 0) {
    return (
      <p className="chart-empty">
        {cols.every((c) => c.future)
          ? "Nothing measured yet for this range."
          : "No usage recorded in this range."}
      </p>
    );
  }
  const peak = measured;
  // Room for roughly a dozen ticks before they collide.
  const every = Math.max(1, Math.ceil(cols.length / 12));

  return (
    <div className="chart" role="img" aria-label={`Usage per period, peak ${bytes(peak)}`}>
      <div className="chart-scale" aria-hidden="true">
        <span>{bytes(peak)}</span>
        <span>{bytes(peak / 2)}</span>
        <span>0</span>
      </div>
      <div className="chart-plot">
        <div className="chart-grid" aria-hidden="true">
          <i />
          <i />
          <i />
        </div>
        <div className="cols">
          {cols.map((c, n) => {
            const isClickable = Boolean(canDrill && !c.unknown);
            return (
              <div
                key={c.key}
                className={`col${c.unknown ? " blank" : ""}${isClickable ? " clickable" : ""}`}
                tabIndex={isClickable ? 0 : undefined}
                role={isClickable ? "button" : undefined}
                aria-label={isClickable ? `Inspect ${c.label}` : undefined}
                data-tip={
                  c.unknown
                    ? `${c.label} · ${c.future ? "not yet" : "no data"}`
                    : `${c.label} · ↓ ${bytes(c.rx)} · ↑ ${bytes(c.tx)}${
                        isClickable ? " · click to inspect" : ""
                      }`
                }
                onClick={() => {
                  if (isClickable && onDrillDown) {
                    onDrillDown(c.key, c.startUtcMs);
                  }
                }}
                onKeyDown={(e) => {
                  if (isClickable && onDrillDown && (e.key === "Enter" || e.key === " ")) {
                    e.preventDefault();
                    onDrillDown(c.key, c.startUtcMs);
                  }
                }}
              >
                <div className="stack">
                  <i className="s-tx" style={{ height: `${(100 * c.tx) / peak}%` }} />
                  <i className="s-rx" style={{ height: `${(100 * c.rx) / peak}%` }} />
                </div>
                <span className="col-tick">{n % every === 0 ? c.tick : ""}</span>
              </div>
            );
          })}
        </div>
      </div>
      <div className="legend">
        <span>
          <i className="key rx" /> Download
        </span>
        <span>
          <i className="key tx" /> Upload
        </span>
      </div>
    </div>
  );
}

function LiveSpeedChart({ history }: { history: { rx: number; tx: number }[] }) {
  const MAX = 60;
  const padded = Array(Math.max(0, MAX - history.length))
    .fill({ rx: 0, tx: 0 })
    .concat(history);

  const peak = Math.max(1024, ...padded.map((p) => Math.max(p.rx, p.tx)));
  const width = 600;
  const height = 120;
  const plotH = 104;

  const getX = (i: number) => (i / (MAX - 1)) * width;
  const getY = (val: number) => Math.max(4, plotH - (val / peak) * (plotH - 8));

  const rxCoords = padded.map((p, i) => `${getX(i).toFixed(1)},${getY(p.rx).toFixed(1)}`);
  const txCoords = padded.map((p, i) => `${getX(i).toFixed(1)},${getY(p.tx).toFixed(1)}`);

  const rxArea = `M 0,${plotH} L ${rxCoords.join(" L ")} L ${width},${plotH} Z`;
  const txArea = `M 0,${plotH} L ${txCoords.join(" L ")} L ${width},${plotH} Z`;

  const rxLine = `M ${rxCoords.join(" L ")}`;
  const txLine = `M ${txCoords.join(" L ")}`;

  const last = padded[padded.length - 1] ?? { rx: 0, tx: 0 };

  return (
    <div className="live-chart-container">
      <div className="live-chart-header">
        <div className="live-chart-peak">
          <span>Peak: {perSec(peak)}</span>
          <span className="live-chart-scale-mid">{perSec(peak / 2)}</span>
          <span>0 B/s</span>
        </div>
        <div className="live-chart-legend">
          <span className="legend-item rx">
            <i className="key rx" /> Down: {perSec(last.rx)}
          </span>
          <span className="legend-item tx">
            <i className="key tx" /> Up: {perSec(last.tx)}
          </span>
        </div>
      </div>
      <div className="live-chart-svg-wrap">
        <svg
          viewBox={`0 0 ${width} ${height}`}
          preserveAspectRatio="none"
          className="live-chart-svg"
          aria-hidden="true"
        >
          <defs>
            <linearGradient id="rxGrad" x1="0" y1="0" x2="0" y2="1">
              <stop offset="0%" stopColor="var(--rx)" stopOpacity="0.32" />
              <stop offset="100%" stopColor="var(--rx)" stopOpacity="0.0" />
            </linearGradient>
            <linearGradient id="txGrad" x1="0" y1="0" x2="0" y2="1">
              <stop offset="0%" stopColor="var(--tx)" stopOpacity="0.32" />
              <stop offset="100%" stopColor="var(--tx)" stopOpacity="0.0" />
            </linearGradient>
          </defs>
          <line x1="0" y1="8" x2={width} y2="8" stroke="var(--stroke)" strokeDasharray="3 3" />
          <line x1="0" y1={plotH / 2} x2={width} y2={plotH / 2} stroke="var(--stroke)" strokeDasharray="3 3" />
          <line x1="0" y1={plotH} x2={width} y2={plotH} stroke="var(--stroke)" />

          <path d={rxArea} fill="url(#rxGrad)" />
          <path d={txArea} fill="url(#txGrad)" />

          <path d={rxLine} fill="none" stroke="var(--rx)" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" />
          <path d={txLine} fill="none" stroke="var(--tx)" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" />

          <text x="4" y={height - 3} fill="var(--text-3)" fontSize="10" fontFamily="inherit">60s ago</text>
          <text x={width / 2} y={height - 3} fill="var(--text-3)" fontSize="10" textAnchor="middle" fontFamily="inherit">30s ago</text>
          <text x={width - 4} y={height - 3} fill="var(--text-3)" fontSize="10" textAnchor="end" fontFamily="inherit">Now</text>
        </svg>
      </div>
    </div>
  );
}

function AppTimelineChart({
  appName,
  buckets,
  onClose,
}: {
  appName: string;
  buckets: AppBucket[];
  onClose: () => void;
}) {
  const cols = buckets.map((b) => {
    const found = b.apps.find((a) => a.app === appName);
    const rx = found?.rx_bytes ?? 0;
    const tx = found?.tx_bytes ?? 0;
    return {
      key: b.key,
      label: bucketLabel(b.key, b.start_utc_ms),
      tick: tickLabel(b.key, b.start_utc_ms),
      rx,
      tx,
      total: rx + tx,
    };
  });

  const totalRx = cols.reduce((sum, c) => sum + c.rx, 0);
  const totalTx = cols.reduce((sum, c) => sum + c.tx, 0);
  const peak = Math.max(1, ...cols.map((c) => c.total));
  const every = Math.max(1, Math.ceil(cols.length / 12));

  return (
    <div className="card app-detail-card">
      <div className="app-detail-header">
        <div className="app-detail-title">
          <ProcessIcon name={appName} size={18} />
          <span className="app-detail-badge mono">{appName}</span>
          <span className="app-detail-stats">
            <span>↓ {bytes(totalRx)}</span>
            <span>↑ {bytes(totalTx)}</span>
            <span className="strong">Total: {bytes(totalRx + totalTx)}</span>
          </span>
        </div>
        <button className="btn" onClick={onClose} aria-label="Close app detail">
          <Icon name="close" /> Close
        </button>
      </div>

      <div className="chart" role="img" aria-label={`Usage for ${appName}, peak ${bytes(peak)}`}>
        <div className="chart-scale" aria-hidden="true">
          <span>{bytes(peak)}</span>
          <span>{bytes(peak / 2)}</span>
          <span>0</span>
        </div>
        <div className="chart-plot">
          <div className="chart-grid" aria-hidden="true">
            <i />
            <i />
            <i />
          </div>
          <div className="cols">
            {cols.map((c, n) => (
              <div
                key={c.key}
                className="col"
                data-tip={`${c.label} · ↓ ${bytes(c.rx)} · ↑ ${bytes(c.tx)}`}
              >
                <div className="stack">
                  <i className="s-tx" style={{ height: `${(100 * c.tx) / peak}%` }} />
                  <i className="s-rx" style={{ height: `${(100 * c.rx) / peak}%` }} />
                </div>
                <span className="col-tick">{n % every === 0 ? c.tick : ""}</span>
              </div>
            ))}
          </div>
        </div>
        <div className="legend">
          <span>
            <i className="key rx" /> Download
          </span>
          <span>
            <i className="key tx" /> Upload
          </span>
        </div>
      </div>
    </div>
  );
}

function HistoryDetail({ series }: { series: Series }) {
  return (
    <div className="card flush">
      <table className="tbl">
        <thead>
          <tr>
            <th>Period</th>
            <th className="num">Download</th>
            <th className="num">Upload</th>
            <th className="num">Total</th>
          </tr>
        </thead>
        <tbody>
          {series.buckets.map((b) => {
            const before =
              series.data_since !== null && dayKey(new Date(b.start_utc_ms)) < series.data_since;
            const future = b.start_utc_ms > Date.now();
            const unknown = before || future;
            return (
              <tr key={b.key} className={unknown ? "dim" : ""}>
                <td className="mono">{bucketLabel(b.key, b.start_utc_ms)}</td>
                <td className="num">
                  {future ? (
                    <span className="empty">—</span>
                  ) : before ? (
                    <span className="empty">no data</span>
                  ) : (
                    bytes(b.summary.included.rx_bytes)
                  )}
                </td>
                <td className="num">
                  {unknown ? (
                    <span className="empty">—</span>
                  ) : (
                    bytes(b.summary.included.tx_bytes)
                  )}
                </td>
                <td className="num strong">
                  {unknown ? (
                    <span className="empty">—</span>
                  ) : (
                    bytes(b.summary.included.rx_bytes + b.summary.included.tx_bytes)
                  )}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

/* ------------------------------------------------------------------- app -- */

const NAV: { id: Page; label: string; icon: string }[] = [
  { id: "overview", label: "Overview", icon: "overview" },
  { id: "apps", label: "Applications", icon: "apps" },
  { id: "history", label: "History", icon: "history" },
  { id: "settings", label: "Settings", icon: "settings" },
];

export default function App() {
  const [live, setLive] = useState<Live | null>(null);
  const [rateHistory, setRateHistory] = useState<{ rx: number; tx: number }[]>([]);
  const [status, setStatus] = useState<Status | null>(null);
  const [last30Days, setLast30Days] = useState<Series | null>(null);
  const [today, setToday] = useState<Series | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [helper, setHelper] = useState<HelperState | null>(null);
  const [apps, setApps] = useState<AppUsage | null>(null);
  const [selectedApp, setSelectedApp] = useState<string | null>(null);
  const [range, setRange] = useState<Range>(() => ranges()[0]);
  const [drillStack, setDrillStack] = useState<Range[]>([]);
  const [isCustomHistory, setIsCustomHistory] = useState(false);
  const [customHistoryFrom, setCustomHistoryFrom] = useState(() => dayKey(daysAgo(7)));
  const [customHistoryTo, setCustomHistoryTo] = useState(() => dayKey(new Date()));
  const [customHistoryGran, setCustomHistoryGran] = useState<Granularity>("day");

  const [appRange, setAppRange] = useState<Range>(() => ranges()[0]);
  const [isCustomApps, setIsCustomApps] = useState(false);
  const [customAppsFrom, setCustomAppsFrom] = useState(() => dayKey(daysAgo(7)));
  const [customAppsTo, setCustomAppsTo] = useState(() => dayKey(new Date()));
  const [customAppsGran, setCustomAppsGran] = useState<Granularity>("day");

  const [history, setHistory] = useState<Series | null>(null);
  const [autostart, setAutostart] = useState(false);
  const [canInstall, setCanInstall] = useState(false);
  const [config, setConfig] = useState<Config | null>(null);
  const [known, setKnown] = useState<InterfaceInfo[]>([]);
  const [quota, setQuota] = useState<QuotaWarning | null>(null);
  const [installing, setInstalling] = useState(false);
  const [page, setPage] = useState<Page>("overview");
  const [appQuery, setAppQuery] = useState("");
  const [appSort, setAppSort] = useState<{ key: SortKey; dir: "asc" | "desc" } | null>(null);

  useEffect(() => {
    const fail = (e: unknown) =>
      setError(typeof e === "object" && e && "message" in e ? String(e.message) : String(e));

    const loadToday = () => invoke<Series>("get_today_usage").then(setToday).catch(fail);
    const loadLast30Days = () => {
      const today = new Date();
      invoke<Series>("get_usage_series", {
        query: {
          granularity: "day",
          from: dayKey(daysAgo(29)),
          to: dayKey(today),
          scope: "included",
          include_breakdown: false,
        },
      })
        .then(setLast30Days)
        .catch(fail);
    };

    const loadHelper = () =>
      invoke<HelperState>("get_helper_state")
        .then(setHelper)
        .catch((e) => setHelper({ state: "unreachable", message: String(e) }));

    invoke<boolean>("get_autostart").then(setAutostart).catch(fail);
    invoke<boolean>("can_install_helper").then(setCanInstall).catch(fail);
    invoke<Config>("get_config").then(setConfig).catch(fail);
    invoke<InterfaceInfo[]>("get_interfaces").then(setKnown).catch(fail);
    invoke<Live>("get_live_rates")
      .then((l) => {
        setLive(l);
        if (l.total.rx_bytes_per_sec !== null || l.total.tx_bytes_per_sec !== null) {
          setRateHistory([
            { rx: l.total.rx_bytes_per_sec ?? 0, tx: l.total.tx_bytes_per_sec ?? 0 },
          ]);
        }
      })
      .catch(fail);
    invoke<Status>("get_monitor_status").then(setStatus).catch(fail);
    loadToday();
    loadLast30Days();
    loadHelper();

    // The day's totals come from the backend, which is the only place that
    // knows the database's share as well as the unflushed buffer's. Rates
    // arrive on the event, once a second.
    const timer = setInterval(() => {
      loadToday();
      loadLast30Days();
      loadHelper();
      // `present` comes from what the engine has seen this session, and at
      // mount it has seen nothing yet: fetched once, every interface reads as
      // absent forever.
      invoke<InterfaceInfo[]>("get_interfaces").then(setKnown).catch(fail);
    }, 15000);
    const subs = [
      listen<Live>("network-usage-updated", (e) => {
        setLive(e.payload);
        const rx = e.payload.total.rx_bytes_per_sec ?? 0;
        const tx = e.payload.total.tx_bytes_per_sec ?? 0;
        setRateHistory((prev) => {
          const next = [...prev, { rx, tx }];
          return next.length > 60 ? next.slice(next.length - 60) : next;
        });
      }),
      listen<Status>("monitor-status-changed", (e) => setStatus(e.payload)),
      // The banner is only seen if the window is open, and the point of a
      // quota warning is the moment it is not.
      listen<QuotaWarning>("quota-warning", async (e) => {
        setQuota(e.payload);
        try {
          const allowed =
            (await isPermissionGranted()) || (await requestPermission()) === "granted";
          if (allowed) {
            sendNotification({
              title: `NetMeter: ${e.payload.percent}% of this month's allowance`,
              body: `${bytes(e.payload.used_bytes)} of ${bytes(e.payload.limit_bytes)} used.`,
            });
          }
        } catch {
          // A desktop without a notification daemon is not a failure worth
          // showing; the banner still says it.
        }
      }),
    ];

    return () => {
      clearInterval(timer);
      subs.forEach((s) => s.then((off) => off()));
    };
  }, []);

  useEffect(() => {
    let live = true;
    const load = () => {
      if (helper?.state !== "running") {
        setApps(null);
        return;
      }
      invoke<AppUsage>("get_app_usage", {
        granularity: appRange.granularity,
        from: appRange.from,
        to: appRange.to,
      })
        .then((res) => live && setApps(res))
        .catch(() => live && setApps(null));
    };

    load();
    const timer = setInterval(load, 15000);
    return () => {
      live = false;
      clearInterval(timer);
    };
  }, [appRange, helper?.state]);

  useEffect(() => {
    let live = true;
    const load = () =>
      invoke<Series>("get_usage_series", {
        query: {
          granularity: range.granularity,
          from: range.from,
          to: range.to,
          scope: "included",
          include_breakdown: false,
        },
      })
        .then((s) => live && setHistory(s))
        .catch(() => live && setHistory(null));

    load();
    // On the same cadence as the rest of the page. Fetching only on a range
    // change left today's bars minutes behind the headline number.
    const timer = setInterval(load, 15000);
    return () => {
      // A range change mid-flight must not let the old answer overwrite the new.
      live = false;
      clearInterval(timer);
    };
  }, [range]);

  // Editing the list switches the policy to manual: ticking a box means "count
  // exactly these", which is what `manual` is. The backend resolves the globs
  // and hands back the interfaces it settled on.
  function apply(next: Config) {
    invoke<{ config: Config; included_interfaces: string[] }>("set_config", { config: next })
      .then((applied) => {
        setConfig(applied.config);
        setKnown((list) =>
          list.map((i) => ({ ...i, included: applied.included_interfaces.includes(i.name) })),
        );
      })
      .catch((err) => setError(String(err)));
  }

  function toggleInterface(name: string, counted: boolean) {
    if (!config) return;
    const current = new Set(known.filter((i) => i.included).map((i) => i.name));
    if (counted) current.add(name);
    else current.delete(name);
    apply({
      ...config,
      interface_policy: "manual",
      included_interfaces: [...current],
      excluded_interfaces: [],
    });
  }

  // GB as the user means it: 1 GB = 1000^3, the unit a carrier prints on a
  // bill, not the 1024-based one the byte formatter uses elsewhere.
  function setQuota_(text: string) {
    if (!config) return;
    const gb = text.trim() === "" ? null : Number(text);
    if (gb !== null && (!Number.isFinite(gb) || gb < 0)) return;
    apply({
      ...config,
      quota: { ...config.quota, monthly_bytes: gb === null ? null : Math.round(gb * 1e9) },
    });
  }

  function setInterval_(seconds: number) {
    if (!config || seconds < 1 || seconds > 3600) return;
    apply({ ...config, sampling_interval_seconds: seconds });
  }

  // What the helper accounts for: applications plus the protocol overhead it
  // measured. Anything the interfaces moved beyond that is time it was not
  // watching.
  const accounted = apps ? sumTraffic(apps.total, [apps.overhead]).total_bytes : 0;
  const interfacesToday = today
    ? today.total.included.rx_bytes + today.total.included.tx_bytes
    : 0;
  const unattributedToday = apps ? Math.max(0, interfacesToday - accounted) : 0;

  const todayByName = new Map(
    (today?.buckets[0]?.by_interface ?? []).map((i) => [i.name, i.traffic]),
  );
  const rows = (live?.by_interface ?? []).map((i) => ({
    ...i,
    traffic: todayByName.get(i.name) ?? { rx_bytes: 0, tx_bytes: 0 },
  }));

  const running = status?.state === "running";
  const degraded = status?.state === "degraded" || status?.state === "stalled";
  const statusText = status
    ? `${status.state[0].toUpperCase()}${status.state.slice(1)} · every ${status.sampling_interval_seconds}s`
    : "Connecting…";

  // Sorting is a view concern: with no column chosen the backend's own order
  // stands, so the default ranking is never silently replaced.
  const q = appQuery.trim().toLowerCase();
  const appRows = (apps?.total ?? []).filter((a) => a.app.toLowerCase().includes(q));
  if (appSort) {
    const sign = appSort.dir === "asc" ? 1 : -1;
    const key = appSort.key;
    const value = (a: AppTraffic) =>
      key === "rx" ? a.rx_bytes : key === "tx" ? a.tx_bytes : a.rx_bytes + a.tx_bytes;
    appRows.sort((a, b) =>
      key === "app" ? sign * a.app.localeCompare(b.app) : sign * (value(a) - value(b)),
    );
  }
  const appPeak = Math.max(1, ...(apps?.total ?? []).map((a) => a.rx_bytes + a.tx_bytes));
  // Overhead is kept per day, so an hourly range has none to show and the row
  // would read a flat zero rather than nothing. Today's shortfall is already
  // named by the unattributed banner above the table.
  const gran = isCustomApps ? customAppsGran : appRange.granularity;
  const showOverhead = gran !== "hour" && "protocol overhead".includes(q);
  // Totals what the table is showing, overhead included only when its row is,
  // so a filter narrows the sum instead of contradicting it.
  const appTotal = sumTraffic(appRows, showOverhead && apps ? [apps.overhead] : []);

  const sortBy = (key: SortKey) =>
    setAppSort((s) =>
      s?.key === key
        ? s.dir === "desc"
          ? { key, dir: "asc" }
          : null
        : { key, dir: "desc" },
    );

  const banners = (
    <>
      {error && <Banner tone="error">{error}</Banner>}
      {quota && (
        <Banner tone="warn">
          <span>
            {quota.percent}% of this month's allowance: {bytes(quota.used_bytes)} of{" "}
            {bytes(quota.limit_bytes)}.
          </span>
          <button className="btn" onClick={() => setQuota(null)}>
            Dismiss
          </button>
        </Banner>
      )}
      {status?.degraded_reason && <Banner tone="warn">{status.degraded_reason}</Banner>}
    </>
  );

  return (
    <div className="shell">
      <nav className="sidebar" aria-label="Sections">
        <div className="brand">
          <div className="brand-name">NetMeter</div>
          <div className="brand-status" data-tip={status?.degraded_reason ?? undefined}>
            <span
              className={`dot ${running ? "ok" : degraded ? "warn" : "off"}`}
              aria-hidden="true"
            />
            <span className="brand-status-text">{statusText}</span>
          </div>
        </div>
        <ul className="nav">
          {NAV.map((n) => (
            <li key={n.id}>
              <button
                className={`nav-item${page === n.id ? " on" : ""}`}
                aria-current={page === n.id ? "page" : undefined}
                data-tip={n.label}
                onClick={() => setPage(n.id)}
              >
                <span className="nav-rail" aria-hidden="true" />
                <Icon name={n.icon} className="nav-icon" />
                <span className="nav-label">{n.label}</span>
              </button>
            </li>
          ))}
        </ul>
      </nav>

      <main className="content">
        {page === "overview" && (
          <div className="page">
            <h1>Overview</h1>
            {banners}

            <h2>Status</h2>
            <div className="card">
              <div className="row">
                <span className="row-icon">
                  <Icon name="overview" />
                </span>
                <span className="row-main">
                  <span className="row-title">Monitoring</span>
                  <span className="row-desc">
                    {status?.degraded_reason ??
                      "Counting from the kernel's own interface counters."}
                  </span>
                </span>
                <span className="row-value">
                  <span
                    className={`dot ${running ? "ok" : degraded ? "warn" : "off"}`}
                    aria-hidden="true"
                  />
                  {statusText}
                </span>
              </div>
            </div>

            <h2>Last 30 days</h2>
            <div className="card">
              <Meter
                label="Download"
                dir="down"
                value={bytes(last30Days?.total.included.rx_bytes ?? 0)}
              />
              <Meter
                label="Upload"
                dir="up"
                value={bytes(last30Days?.total.included.tx_bytes ?? 0)}
              />
              <Meter
                label="Total"
                dir="total"
                value={bytes(
                  (last30Days?.total.included.rx_bytes ?? 0) +
                    (last30Days?.total.included.tx_bytes ?? 0),
                )}
              />
            </div>

            <h2>Today</h2>
            <div className="card">
              <Meter label="Download" dir="down" value={bytes(today?.total.included.rx_bytes ?? 0)} />
              <Meter label="Upload" dir="up" value={bytes(today?.total.included.tx_bytes ?? 0)} />
              <Meter
                label="Total"
                dir="total"
                value={bytes(
                  (today?.total.included.rx_bytes ?? 0) + (today?.total.included.tx_bytes ?? 0),
                )}
              />
            </div>

            <h2>Current speed</h2>
            <div className="card">
              <Meter label="Download" dir="down" value={perSec(live?.total.rx_bytes_per_sec ?? null)} />
              <Meter label="Upload" dir="up" value={perSec(live?.total.tx_bytes_per_sec ?? null)} />
              <Meter label="Total" dir="total" value={perSec(sumRate(live?.total))} />
            </div>

            <h2>Live bandwidth (last 60s)</h2>
            <div className="card chart-card">
              <LiveSpeedChart history={rateHistory} />
            </div>

            <h2>Network interfaces</h2>
            <div className="card flush scroll-x">
              <table className="tbl">
                <thead>
                  <tr>
                    <th>Interface</th>
                    <th className="num">Down today</th>
                    <th className="num">Up today</th>
                    <th className="num">Total today</th>
                    <th className="num">Down now</th>
                    <th className="num">Up now</th>
                  </tr>
                </thead>
                <tbody>
                  {rows.map((i) => (
                    <tr key={i.name} className={i.included ? "" : "dim"}>
                      <td className="mono">{i.name}</td>
                      <td className="num">{bytes(i.traffic.rx_bytes)}</td>
                      <td className="num">{bytes(i.traffic.tx_bytes)}</td>
                      <td className="num strong">
                        {bytes(i.traffic.rx_bytes + i.traffic.tx_bytes)}
                      </td>
                      <td className="num">{perSec(i.rate.rx_bytes_per_sec)}</td>
                      <td className="num">{perSec(i.rate.tx_bytes_per_sec)}</td>
                    </tr>
                  ))}
                  {rows.length === 0 && (
                    <tr>
                      <td colSpan={6} className="placeholder">
                        Waiting for the first sample…
                      </td>
                    </tr>
                  )}
                </tbody>
              </table>
            </div>
            <p className="hint">Dimmed: recorded, not counted.</p>
          </div>
        )}

        {page === "settings" && (
          <div className="page">
            <h1>Settings</h1>
            {banners}

            {config && (
              <>
                <h2>Counted interfaces</h2>
                <p className="section-desc">
                  Counted toward your total. Ticking both a VPN and the interface carrying it
                  counts those bytes twice.
                </p>
                <div className="card">
                  {known.map((i) => (
                    <label key={i.name} className={`row tappable${i.present ? "" : " dim"}`}>
                      <span className="row-icon" data-tip={i.kind}>
                        <Icon name={KIND_ICON[i.kind] ?? "virtual"} />
                      </span>
                      <span className="row-main">
                        <span className="row-title mono">{i.name}</span>
                        <span className="row-desc">
                          {i.kind} · {i.present ? "present" : "not present"}
                        </span>
                      </span>
                      <input
                        type="checkbox"
                        className="switch"
                        checked={i.included}
                        aria-label={`Count ${i.name}`}
                        onChange={(e) => toggleInterface(i.name, e.target.checked)}
                      />
                    </label>
                  ))}
                  {known.length === 0 && <div className="row placeholder">Reading interfaces…</div>}
                </div>

                <h2>Usage limits</h2>
                <div className="card">
                  <div className="row">
                    <span className="row-main">
                      <span className="row-title">Monthly allowance</span>
                      <span className="row-desc">
                        Monthly allowance in GB. Warns at {config.quota.warn_at_percent.join("% and ")}%.
                        Empty for no limit.
                      </span>
                    </span>
                    <span className="field">
                      <input
                        type="number"
                        className="input"
                        min={0}
                        step={1}
                        value={
                          config.quota.monthly_bytes === null ? "" : gib(config.quota.monthly_bytes)
                        }
                        placeholder="off"
                        aria-label="Monthly allowance in GB"
                        onChange={(e) => setQuota_(e.target.value)}
                      />
                      <span className="unit">GB</span>
                    </span>
                  </div>
                </div>

                <h2>Sampling</h2>
                <div className="card">
                  <div className="row">
                    <span className="row-main">
                      <span className="row-title">Sampling interval</span>
                      <span className="row-desc">Seconds between samples.</span>
                    </span>
                    <span className="field">
                      <input
                        type="number"
                        className="input"
                        min={1}
                        max={3600}
                        value={config.sampling_interval_seconds}
                        aria-label="Seconds between samples"
                        onChange={(e) => setInterval_(Number(e.target.value))}
                      />
                      <span className="unit">sec</span>
                    </span>
                  </div>
                </div>
              </>
            )}

            <h2>Startup</h2>
            <div className="card">
              <label className="row tappable">
                <span className="row-main">
                  <span className="row-title">Start at login</span>
                  <span className="row-desc">Begin counting as soon as the session starts.</span>
                </span>
                <input
                  type="checkbox"
                  className="switch"
                  checked={autostart}
                  aria-label="Start at login"
                  onChange={(e) =>
                    invoke<boolean>("set_autostart", { enabled: e.target.checked })
                      .then(setAutostart)
                      .catch((err) => setError(String(err)))
                  }
                />
              </label>
            </div>

            <h2>Background behavior</h2>
            <div className="card">
              <div className="row">
                <span className="row-icon">
                  <Icon name="info" />
                </span>
                <span className="row-main">
                  <span className="row-title">Keeps running in the tray</span>
                  <span className="row-desc">
                    Closing the window keeps counting; Quit from the tray to stop.
                  </span>
                </span>
              </div>
            </div>
          </div>
        )}

        {page === "history" && (
          <div className="page">
            <h1>History</h1>
            {banners}

            <div className="selector-wrap">
              <div className="selector" role="tablist" aria-label="Time range">
                {ranges().map((r) => (
                  <button
                    key={r.label}
                    role="tab"
                    aria-selected={!isCustomHistory && drillStack.length === 0 && r.label === range.label}
                    className={
                      !isCustomHistory && drillStack.length === 0 && r.label === range.label
                        ? "on"
                        : ""
                    }
                    onClick={() => {
                      setIsCustomHistory(false);
                      setDrillStack([]);
                      setRange(r);
                    }}
                  >
                    {r.label}
                  </button>
                ))}
                <button
                  role="tab"
                  aria-selected={isCustomHistory}
                  className={isCustomHistory ? "on" : ""}
                  onClick={() => setIsCustomHistory(true)}
                >
                  Custom…
                </button>
              </div>

              <div className="export-group">
                <button
                  className="btn btn-icon"
                  title="Export this view to CSV"
                  disabled={!history}
                  onClick={() => history && exportHistoryCsv(history, range.label)}
                >
                  <Icon name="download_file" />
                  <span>CSV</span>
                </button>
                <button
                  className="btn btn-icon"
                  title="Export this view to JSON"
                  disabled={!history}
                  onClick={() => history && exportHistoryJson(history, range.label)}
                >
                  <Icon name="download_file" />
                  <span>JSON</span>
                </button>
              </div>
            </div>

            {isCustomHistory && (
              <div className="custom-range-card">
                <span className="custom-range-field">
                  <span>From:</span>
                  <input
                    type="date"
                    className="input date-input"
                    value={customHistoryFrom}
                    onChange={(e) => setCustomHistoryFrom(e.target.value)}
                  />
                </span>
                <span className="custom-range-field">
                  <span>To:</span>
                  <input
                    type="date"
                    className="input date-input"
                    value={customHistoryTo}
                    onChange={(e) => setCustomHistoryTo(e.target.value)}
                  />
                </span>
                <span className="custom-range-field">
                  <span>Step:</span>
                  <select
                    className="select-input"
                    value={customHistoryGran}
                    onChange={(e) => setCustomHistoryGran(e.target.value as Granularity)}
                  >
                    <option value="hour">Hour</option>
                    <option value="day">Day</option>
                    <option value="month">Month</option>
                  </select>
                </span>
                <button
                  className="btn primary"
                  disabled={
                    !customHistoryFrom || !customHistoryTo || customHistoryFrom > customHistoryTo
                  }
                  onClick={() => {
                    setDrillStack([]);
                    setRange({
                      label: `${customHistoryFrom} to ${customHistoryTo}`,
                      granularity: customHistoryGran,
                      from: customHistoryFrom,
                      to: customHistoryTo,
                    });
                  }}
                >
                  Apply
                </button>
              </div>
            )}

            {drillStack.length > 0 && (
              <div className="drill-bar">
                <button
                  className="btn drill-back-btn"
                  onClick={() => {
                    const prev = drillStack[drillStack.length - 1];
                    setDrillStack((s) => s.slice(0, -1));
                    setRange(prev);
                  }}
                >
                  <Icon name="back" />
                  <span>Back to {drillStack[drillStack.length - 1].label}</span>
                </button>
                <span className="drill-title">
                  Inspecting {range.label} ({range.granularity === "hour" ? "hourly" : "daily"}{" "}
                  breakdown)
                </span>
              </div>
            )}

            {history ? (
              <>
                <h2>Total</h2>
                <div className="card">
                  <Meter label="Download" dir="down" value={bytes(history.total.included.rx_bytes)} />
                  <Meter label="Upload" dir="up" value={bytes(history.total.included.tx_bytes)} />
                  <Meter
                    label="Total"
                    dir="total"
                    value={bytes(
                      history.total.included.rx_bytes + history.total.included.tx_bytes,
                    )}
                  />
                </div>

                <h2>Usage over time</h2>
                <div className="card chart-card">
                  <UsageChart
                    series={history}
                    canDrill={range.granularity === "day" || range.granularity === "month"}
                    onDrillDown={(colKey) => {
                      if (range.granularity === "day") {
                        setDrillStack((s) => [...s, range]);
                        setRange({
                          label: colKey,
                          granularity: "hour",
                          from: colKey,
                          to: colKey,
                        });
                      } else if (range.granularity === "month") {
                        setDrillStack((s) => [...s, range]);
                        setRange({
                          label: colKey,
                          granularity: "day",
                          from: colKey,
                          to: colKey,
                        });
                      }
                    }}
                  />
                </div>

                {/* Real bytes the kernel counted while nothing was watching.
                    They belong to no bucket, so they are reported rather than
                    spread. */}
                {(history.offline_total.included.rx_bytes > 0 ||
                  history.offline_total.included.tx_bytes > 0) && (
                  <Banner tone="warn">
                    Not running
                    {history.offline_windows.length > 0 &&
                      ` ${span(
                        history.offline_windows[0].from_utc_ms,
                        history.offline_windows[history.offline_windows.length - 1].to_utc_ms,
                      )}`}
                    : {bytes(history.offline_total.included.rx_bytes)} down,{" "}
                    {bytes(history.offline_total.included.tx_bytes)} up. Not in any bar.
                  </Banner>
                )}

                <h2>Detail</h2>
                <HistoryDetail series={history} />
              </>
            ) : (
              <div className="card">
                <div className="row placeholder">No history for this range.</div>
              </div>
            )}
          </div>
        )}

        {page === "apps" && (
          <div className="page">
            <h1>Applications</h1>
            {banners}

            <div className="selector-wrap">
              <div className="selector" role="tablist" aria-label="Time range">
                {ranges().map((r) => (
                  <button
                    key={r.label}
                    role="tab"
                    aria-selected={!isCustomApps && r.label === appRange.label}
                    className={!isCustomApps && r.label === appRange.label ? "on" : ""}
                    onClick={() => {
                      setIsCustomApps(false);
                      setAppRange(r);
                    }}
                  >
                    {r.label}
                  </button>
                ))}
                <button
                  role="tab"
                  aria-selected={isCustomApps}
                  className={isCustomApps ? "on" : ""}
                  onClick={() => setIsCustomApps(true)}
                >
                  Custom…
                </button>
              </div>

              <div className="export-group">
                <button
                  className="btn btn-icon"
                  title="Export applications to CSV"
                  disabled={!apps}
                  onClick={() => apps && exportAppsCsv(apps, appRange.label)}
                >
                  <Icon name="download_file" />
                  <span>CSV</span>
                </button>
                <button
                  className="btn btn-icon"
                  title="Export applications to JSON"
                  disabled={!apps}
                  onClick={() => apps && exportAppsJson(apps, appRange.label)}
                >
                  <Icon name="download_file" />
                  <span>JSON</span>
                </button>
              </div>
            </div>

            {isCustomApps && (
              <div className="custom-range-card">
                <span className="custom-range-field">
                  <span>From:</span>
                  <input
                    type="date"
                    className="input date-input"
                    value={customAppsFrom}
                    onChange={(e) => setCustomAppsFrom(e.target.value)}
                  />
                </span>
                <span className="custom-range-field">
                  <span>To:</span>
                  <input
                    type="date"
                    className="input date-input"
                    value={customAppsTo}
                    onChange={(e) => setCustomAppsTo(e.target.value)}
                  />
                </span>
                <span className="custom-range-field">
                  <span>Step:</span>
                  <select
                    className="select-input"
                    value={customAppsGran}
                    onChange={(e) => setCustomAppsGran(e.target.value as Granularity)}
                  >
                    <option value="hour">Hour</option>
                    <option value="day">Day</option>
                    <option value="month">Month</option>
                  </select>
                </span>
                <button
                  className="btn primary"
                  disabled={!customAppsFrom || !customAppsTo || customAppsFrom > customAppsTo}
                  onClick={() => {
                    setAppRange({
                      label: `${customAppsFrom} to ${customAppsTo}`,
                      granularity: customAppsGran,
                      from: customAppsFrom,
                      to: customAppsTo,
                    });
                  }}
                >
                  Apply
                </button>
              </div>
            )}

            {helper?.state === "not_installed" && (
              <Banner tone="info">
                <span>
                  Needs the <code>netmeterd</code> helper: a system service with{" "}
                  <code>CAP_BPF</code>, because the kernel exposes no per-process counters to an
                  unprivileged reader.
                </span>
                {canInstall && (
                  <button
                    className="btn primary"
                    disabled={installing}
                    onClick={() => {
                      setInstalling(true);
                      invoke<HelperState>("install_helper")
                        .then(setHelper)
                        .catch((err) => setError(String(err)))
                        .finally(() => setInstalling(false));
                    }}
                  >
                    {installing ? "Installing…" : "Install helper"}
                  </button>
                )}
              </Banner>
            )}

            {helper?.state === "unreachable" && <Banner tone="error">{helper.message}</Banner>}

            {/* A real gap, measured: what the interfaces moved today minus what
                the helper accounted for. Unlike interface counters there is no
                kernel total to recover a per-application gap from, so it is
                reported. */}
            {appRange.label === "Today" && unattributedToday > 1 << 20 && (
              <Banner tone="info">
                {bytes(unattributedToday)} today is not attributed below: the helper was not
                running for part of the day.
              </Banner>
            )}

            {helper?.state === "running" && helper.probes_attached < helper.probes_expected && (
              <Banner tone="error">
                {helper.probes_attached} of {helper.probes_expected} probes attached; some traffic
                is uncounted.
              </Banner>
            )}

            {selectedApp && apps?.buckets && (
              <AppTimelineChart
                appName={selectedApp}
                buckets={apps.buckets}
                onClose={() => setSelectedApp(null)}
              />
            )}

            {apps && (
              <>
                <div className="toolbar">
                  <span className="search">
                    <Icon name="search" className="search-icon" />
                    <input
                      className="input"
                      type="search"
                      value={appQuery}
                      placeholder="Filter applications"
                      aria-label="Filter applications"
                      onChange={(e) => setAppQuery(e.target.value)}
                    />
                  </span>
                  <span className="toolbar-count">
                    {appRows.length} of {apps.total.length}
                  </span>
                </div>

                <div className="card flush">
                  <table className="tbl">
                    <thead>
                      <tr>
                        <SortHeader label="Application" col="app" sort={appSort} onSort={sortBy} />
                        <SortHeader label="Down" col="rx" sort={appSort} onSort={sortBy} align="num" />
                        <SortHeader label="Up" col="tx" sort={appSort} onSort={sortBy} align="num" />
                        <SortHeader label="Total" col="total" sort={appSort} onSort={sortBy} align="num" />
                      </tr>
                    </thead>
                    <tbody>
                      {appRows.map((a) => (
                        <tr
                          key={a.app}
                          className={`tappable-row${selectedApp === a.app ? " selected" : ""}`}
                          onClick={() => setSelectedApp(selectedApp === a.app ? null : a.app)}
                          role="button"
                          tabIndex={0}
                          onKeyDown={(e) => {
                            if (e.key === "Enter" || e.key === " ") {
                              e.preventDefault();
                              setSelectedApp(selectedApp === a.app ? null : a.app);
                            }
                          }}
                        >
                          <td>
                            <span
                              className="share"
                              style={{
                                width: `${(100 * (a.rx_bytes + a.tx_bytes)) / appPeak}%`,
                              }}
                              aria-hidden="true"
                            />
                            <span className="share-label">
                              <ProcessIcon name={a.app} size={16} />
                              <span className="app-name">{a.app}</span>
                              {selectedApp === a.app && (
                                <span className="active-pill">timeline</span>
                              )}
                            </span>
                          </td>
                          <td className="num">{bytes(a.rx_bytes)}</td>
                          <td className="num">{bytes(a.tx_bytes)}</td>
                          <td className="num strong">{bytes(a.rx_bytes + a.tx_bytes)}</td>
                        </tr>
                      ))}
                      {appRows.length === 0 && apps.total.length > 0 && (
                        <tr>
                          <td colSpan={4} className="placeholder">
                            No application matches “{appQuery}”.
                          </td>
                        </tr>
                      )}
                      {apps.total.length === 0 && (
                        <tr>
                          <td colSpan={4} className="placeholder">
                            No application traffic recorded for this range.
                          </td>
                        </tr>
                      )}
                    </tbody>
                    {/* Its own row where there is one: payload bytes never sum
                        to what the interfaces moved, and the difference is
                        headers and acks. */}
                    <tfoot>
                      {showOverhead && (
                        <tr className="dim">
                          <td>protocol overhead</td>
                          <td className="num">{bytes(apps.overhead.rx_bytes)}</td>
                          <td className="num">{bytes(apps.overhead.tx_bytes)}</td>
                          <td className="num strong">
                            {bytes(apps.overhead.rx_bytes + apps.overhead.tx_bytes)}
                          </td>
                        </tr>
                      )}
                      {apps.total.length > 0 && (
                        <tr className="total-row">
                          <td>
                            Total
                            {q && <span className="dim"> · {appRows.length} shown</span>}
                          </td>
                          <td className="num">{bytes(appTotal.rx_bytes)}</td>
                          <td className="num">{bytes(appTotal.tx_bytes)}</td>
                          <td className="num strong">{bytes(appTotal.total_bytes)}</td>
                        </tr>
                      )}
                    </tfoot>
                  </table>
                </div>
                <p className="hint">Tip: Click an application row to view its timeline.</p>
              </>
            )}

            {apps?.total.length === 0 && (
              <p className="hint">
                {appRange.label === "Today"
                  ? "Nothing recorded yet — the helper flushes every 30 seconds."
                  : "No application traffic recorded in this range."}
              </p>
            )}
          </div>
        )}
      </main>
    </div>
  );
}
