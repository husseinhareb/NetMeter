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

export default function App() {
  const [live, setLive] = useState<Live | null>(null);
  const [status, setStatus] = useState<Status | null>(null);
  const [today, setToday] = useState<Series | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const fail = (e: unknown) =>
      setError(typeof e === "object" && e && "message" in e ? String(e.message) : String(e));

    const loadToday = () => invoke<Series>("get_today_usage").then(setToday).catch(fail);

    invoke<Live>("get_live_rates").then(setLive).catch(fail);
    invoke<Status>("get_monitor_status").then(setStatus).catch(fail);
    loadToday();

    // The day's totals come from the backend, which is the only place that
    // knows the database's share as well as the unflushed buffer's. Rates
    // arrive on the event, once a second.
    const timer = setInterval(loadToday, 15000);
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
    </main>
  );
}
