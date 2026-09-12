import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./App.css";

type Traffic = { rx_bytes: number; tx_bytes: number };
type Rate = { rx_bytes_per_sec: number | null; tx_bytes_per_sec: number | null };

type Live = {
  total: Rate;
  by_interface: { name: string; rate: Rate; included: boolean }[];
  time_anomaly: boolean;
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
  buckets: { by_interface: { name: string; traffic: Traffic; included: boolean }[] }[];
};

const UNITS = ["B", "KiB", "MiB", "GiB", "TiB"];

function bytes(n: number): string {
  let u = 0;
  while (n >= 1024 && u < UNITS.length - 1) {
    n /= 1024;
    u++;
  }
  return `${n < 10 && u > 0 ? n.toFixed(1) : Math.round(n)} ${UNITS[u]}`;
}

const perSec = (n: number | null) => (n === null ? "–" : `${bytes(n)}/s`);

// True when the helper started after local midnight, so it holds only part
// of today.
function startedToday(startedAtMs: number): boolean {
  const midnight = new Date();
  midnight.setHours(0, 0, 0, 0);
  return startedAtMs > midnight.getTime();
}

export default function App() {
  const [live, setLive] = useState<Live | null>(null);
  const [status, setStatus] = useState<Status | null>(null);
  const [today, setToday] = useState<Series | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [helper, setHelper] = useState<HelperState | null>(null);
  const [apps, setApps] = useState<AppUsage | null>(null);

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
    }, 15000);
    const subs = [
      listen<Live>("network-usage-updated", (e) => setLive(e.payload)),
      listen<Status>("monitor-status-changed", (e) => setStatus(e.payload)),
    ];

    return () => {
      clearInterval(timer);
      subs.forEach((s) => s.then((off) => off()));
    };
  }, []);

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

      <p className="note">Dimmed interfaces are recorded but not counted in the total.</p>

      <h2>By application, today</h2>

      {helper?.state === "not_installed" && (
        <p className="note">
          Per-application usage needs the <code>netmeterd</code> helper, which runs as a
          system service because the kernel does not expose per-process byte counts to
          an unprivileged program. See <code>docs/PER_APP.md</code>.
        </p>
      )}

      {helper?.state === "unreachable" && <p className="error">{helper.message}</p>}

      {/* The interface totals above cover the whole day; the helper only
          knows what happened since it started. Saying so beats letting the
          two numbers sit side by side looking comparable. */}
      {helper?.state === "running" && startedToday(helper.started_at_utc_ms) && (
        <p className="note">
          Counting applications since{" "}
          {new Date(helper.started_at_utc_ms).toLocaleTimeString()} — earlier traffic today
          is in the interface totals above but not attributed below.
        </p>
      )}

      {helper?.state === "running" && helper.probes_attached < helper.probes_expected && (
        <p className="error">
          Only {helper.probes_attached} of {helper.probes_expected} probes attached, so some
          traffic is not counted below.
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
