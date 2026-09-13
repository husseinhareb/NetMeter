import { useEffect, useState } from "react";
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

type AppUsage = {
  total: AppTraffic[];
  overhead: Traffic;
  uid: number;
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

const UNITS = ["B", "KiB", "MiB", "GiB", "TiB"];

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

function ranges(): Range[] {
  const today = new Date();
  return [
    { label: "Today", granularity: "hour", from: dayKey(today), to: dayKey(today) },
    { label: "Yesterday", granularity: "hour", from: dayKey(daysAgo(1)), to: dayKey(daysAgo(1)) },
    { label: "7 days", granularity: "day", from: dayKey(daysAgo(6)), to: dayKey(today) },
    { label: "30 days", granularity: "day", from: dayKey(daysAgo(29)), to: dayKey(today) },
    { label: "This month", granularity: "day", from: monthKey(today), to: monthKey(today) },
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

function span(from: number, to: number): string {
  const f = new Date(from);
  const t = new Date(to);
  return `${f.toLocaleString()} – ${t.toLocaleString()}`;
}

// True when the helper started after local midnight, so it holds only part
// of today.
function startedToday(startedAtMs: number): boolean {
  const midnight = new Date();
  midnight.setHours(0, 0, 0, 0);
  return startedAtMs > midnight.getTime();
}

function HistoryTable({ series }: { series: Series }) {
  const peak = Math.max(
    1,
    ...series.buckets.map((b) => b.summary.included.rx_bytes + b.summary.included.tx_bytes),
  );
  const offline = series.offline_total.included;
  const hasOffline = offline.rx_bytes > 0 || offline.tx_bytes > 0;

  return (
    <>
      <dl>
        <dt>Total</dt>
        <dd>
          down <span className="down">{bytes(series.total.included.rx_bytes)}</span>
          up <span className="up">{bytes(series.total.included.tx_bytes)}</span>
        </dd>
      </dl>

      <table>
        <tbody>
          {series.buckets.map((b) => {
            const total = b.summary.included.rx_bytes + b.summary.included.tx_bytes;
            // Before NetMeter held any data, a zero is absence rather than an
            // idle period, and it is drawn as neither.
            // Two kinds of blank: before NetMeter held data, and after now.
            // Neither is a zero, and a month view spans both.
            const before =
              series.data_since !== null && dayKey(new Date(b.start_utc_ms)) < series.data_since;
            const future = b.start_utc_ms > Date.now();
            const unknown = before || future;
            return (
              <tr key={b.key} className={unknown ? "excluded" : ""}>
                <td className="bucket">{bucketLabel(b.key, b.start_utc_ms)}</td>
                <td className="barcell">
                  {!unknown && (
                    <span className="bar" style={{ width: `${(100 * total) / peak}%` }} />
                  )}
                </td>
                <td>{future ? "" : before ? "no data" : bytes(b.summary.included.rx_bytes)}</td>
                <td>{unknown ? "" : bytes(b.summary.included.tx_bytes)}</td>
              </tr>
            );
          })}
        </tbody>
      </table>

      {/* Real bytes the kernel counted while nothing was watching. They
          belong to no bucket, so they are reported rather than spread. */}
      {hasOffline && (
        <p className="note">
          Not running
          {series.offline_windows.length > 0 &&
            ` ${span(
              series.offline_windows[0].from_utc_ms,
              series.offline_windows[series.offline_windows.length - 1].to_utc_ms,
            )}`}
          : {bytes(offline.rx_bytes)} down, {bytes(offline.tx_bytes)} up. Not in any bar.
        </p>
      )}
    </>
  );
}

export default function App() {
  const [live, setLive] = useState<Live | null>(null);
  const [status, setStatus] = useState<Status | null>(null);
  const [today, setToday] = useState<Series | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [helper, setHelper] = useState<HelperState | null>(null);
  const [apps, setApps] = useState<AppUsage | null>(null);
  const [range, setRange] = useState<Range>(() => ranges()[0]);
  const [history, setHistory] = useState<Series | null>(null);
  const [autostart, setAutostart] = useState(false);
  const [canInstall, setCanInstall] = useState(false);
  const [config, setConfig] = useState<Config | null>(null);
  const [known, setKnown] = useState<InterfaceInfo[]>([]);
  const [quota, setQuota] = useState<QuotaWarning | null>(null);
  const [installing, setInstalling] = useState(false);

  useEffect(() => {
    const fail = (e: unknown) =>
      setError(typeof e === "object" && e && "message" in e ? String(e.message) : String(e));

    const loadToday = () => invoke<Series>("get_today_usage").then(setToday).catch(fail);

    // Per-application data comes from the privileged helper, which is
    // optional. Its absence is a normal state, not an error to report.
    const loadApps = () =>
      invoke<HelperState>("get_helper_state").then((h) => {
        setHelper(h);
        if (h.state !== "running") {
          setApps(null);
          return;
        }
        const day = new Date();
        const key = `${day.getFullYear()}-${String(day.getMonth() + 1).padStart(2, "0")}-${String(
          day.getDate(),
        ).padStart(2, "0")}`;
        invoke<AppUsage>("get_app_usage", { granularity: "day", from: key, to: key })
          .then(setApps)
          .catch(() => setApps(null));
      });

    invoke<boolean>("get_autostart").then(setAutostart).catch(fail);
    invoke<boolean>("can_install_helper").then(setCanInstall).catch(fail);
    invoke<Config>("get_config").then(setConfig).catch(fail);
    invoke<InterfaceInfo[]>("get_interfaces").then(setKnown).catch(fail);
    invoke<Live>("get_live_rates").then(setLive).catch(fail);
    invoke<Status>("get_monitor_status").then(setStatus).catch(fail);
    loadToday();
    loadApps();

    // The day's totals come from the backend, which is the only place that
    // knows the database's share as well as the unflushed buffer's. Rates
    // arrive on the event, once a second.
    const timer = setInterval(() => {
      loadToday();
      loadApps();
      // `present` comes from what the engine has seen this session, and at
      // mount it has seen nothing yet: fetched once, every interface reads as
      // absent forever.
      invoke<InterfaceInfo[]>("get_interfaces").then(setKnown).catch(fail);
    }, 15000);
    const subs = [
      listen<Live>("network-usage-updated", (e) => setLive(e.payload)),
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

  const todayByName = new Map(
    (today?.buckets[0]?.by_interface ?? []).map((i) => [i.name, i.traffic]),
  );
  const rows = (live?.by_interface ?? []).map((i) => ({
    ...i,
    traffic: todayByName.get(i.name) ?? { rx_bytes: 0, tx_bytes: 0 },
  }));

  return (
    <main>
      <h1>
        NetMeter
        <span className="status">
          {status ? `${status.state} · every ${status.sampling_interval_seconds}s` : "–"}
        </span>
      </h1>

      {error && <p className="error">{error}</p>}

      {quota && (
        <p className="error">
          {quota.percent}% of this month's allowance: {bytes(quota.used_bytes)} of{" "}
          {bytes(quota.limit_bytes)}.{" "}
          <button onClick={() => setQuota(null)}>Dismiss</button>
        </p>
      )}
      {status?.degraded_reason && <p className="error">{status.degraded_reason}</p>}

      <dl>
        <dt>Today</dt>
        <dd>
          down <span className="down">{bytes(today?.total.included.rx_bytes ?? 0)}</span>
          up <span className="up">{bytes(today?.total.included.tx_bytes ?? 0)}</span>
        </dd>
        <dt>Now</dt>
        <dd>
          down <span className="down">{perSec(live?.total.rx_bytes_per_sec ?? null)}</span>
          up <span className="up">{perSec(live?.total.tx_bytes_per_sec ?? null)}</span>
        </dd>
      </dl>

      <table>
        <thead>
          <tr>
            <th>Interface</th>
            <th>Down today</th>
            <th>Up today</th>
            <th>Down now</th>
            <th>Up now</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((i) => (
            <tr key={i.name} className={i.included ? "" : "excluded"}>
              <td>{i.name}</td>
              <td>{bytes(i.traffic.rx_bytes)}</td>
              <td>{bytes(i.traffic.tx_bytes)}</td>
              <td>{perSec(i.rate.rx_bytes_per_sec)}</td>
              <td>{perSec(i.rate.tx_bytes_per_sec)}</td>
            </tr>
          ))}
        </tbody>
      </table>

      <p className="note">Dimmed: recorded, not counted.</p>

      <h2>Settings</h2>

      {config && (
        <>
          <p className="note">
            Counted toward your total. Ticking both a VPN and the interface carrying it
            counts those bytes twice.
          </p>
          <table>
            <thead>
              <tr>
                <th>Counted</th>
                <th>Interface</th>
                <th>Kind</th>
                <th>Present</th>
              </tr>
            </thead>
            <tbody>
              {known.map((i) => (
                <tr key={i.name} className={i.present ? "" : "excluded"}>
                  <td>
                    <input
                      type="checkbox"
                      checked={i.included}
                      onChange={(e) => toggleInterface(i.name, e.target.checked)}
                    />
                  </td>
                  <td>{i.name}</td>
                  <td>{i.kind}</td>
                  <td>{i.present ? "yes" : "no"}</td>
                </tr>
              ))}
            </tbody>
          </table>

          <label className="setting">
            <input
              type="number"
              min={0}
              step={1}
              value={config.quota.monthly_bytes === null ? "" : gib(config.quota.monthly_bytes)}
              placeholder="off"
              onChange={(e) => setQuota_(e.target.value)}
            />
            Monthly allowance in GB. Warns at{" "}
            {config.quota.warn_at_percent.join("% and ")}%. Empty for no limit.
          </label>

          <label className="setting">
            <input
              type="number"
              min={1}
              max={3600}
              value={config.sampling_interval_seconds}
              onChange={(e) => setInterval_(Number(e.target.value))}
            />
            Seconds between samples.
          </label>
        </>
      )}

      <label className="setting">
        <input
          type="checkbox"
          checked={autostart}
          onChange={(e) =>
            invoke<boolean>("set_autostart", { enabled: e.target.checked })
              .then(setAutostart)
              .catch((err) => setError(String(err)))
          }
        />
        Start at login. Closing the window keeps counting; Quit from the tray to stop.
      </label>

      <h2>History</h2>

      <div className="tabs">
        {ranges().map((r) => (
          <button
            key={r.label}
            className={r.label === range.label ? "on" : ""}
            onClick={() => setRange(r)}
          >
            {r.label}
          </button>
        ))}
      </div>

      {history && <HistoryTable series={history} />}

      <h2>By application, today</h2>

      {helper?.state === "not_installed" && (
        <p className="note">
          Needs the <code>netmeterd</code> helper: a system service with{" "}
          <code>CAP_BPF</code>, because the kernel exposes no per-process counters to an
          unprivileged reader.{" "}
          {canInstall && (
            <button
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
        </p>
      )}

      {helper?.state === "unreachable" && <p className="error">{helper.message}</p>}

      {/* The interface totals above cover the whole day; the helper only
          knows what happened since it started. Saying so beats letting the
          two numbers sit side by side looking comparable. */}
      {helper?.state === "running" && startedToday(helper.started_at_utc_ms) && (
        <p className="note">
          Since {new Date(helper.started_at_utc_ms).toLocaleTimeString()}. Earlier traffic
          today is counted above but not attributed here.
        </p>
      )}

      {helper?.state === "running" && helper.probes_attached < helper.probes_expected && (
        <p className="error">
          {helper.probes_attached} of {helper.probes_expected} probes attached; some traffic
          is uncounted.
        </p>
      )}

      {apps && (
        <table>
          <thead>
            <tr>
              <th>Application</th>
              <th>Down</th>
              <th>Up</th>
            </tr>
          </thead>
          <tbody>
            {apps.total.map((a) => (
              <tr key={a.app}>
                <td>{a.app}</td>
                <td>{bytes(a.rx_bytes)}</td>
                <td>{bytes(a.tx_bytes)}</td>
              </tr>
            ))}
            {/* Its own row, not hidden: payload bytes never sum to what the
                interfaces moved, and the difference is headers and acks. */}
            <tr className="excluded">
              <td>protocol overhead</td>
              <td>{bytes(apps.overhead.rx_bytes)}</td>
              <td>{bytes(apps.overhead.tx_bytes)}</td>
            </tr>
          </tbody>
        </table>
      )}

      {apps?.total.length === 0 && (
        <p className="note">Nothing recorded yet — the helper flushes every 30 seconds.</p>
      )}
    </main>
  );
}
