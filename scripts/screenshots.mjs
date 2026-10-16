#!/usr/bin/env node
// Screenshot every page of NetMeter at several window sizes, light and dark,
// into screenshots/.
//
// The frontend runs in headless Chromium (Vite dev server) with Tauri's
// invoke() mocked. The mock answers from this machine's real databases in
// /var/lib/netmeter, read here with node:sqlite, so any range the UI asks for
// gets real numbers. Driven over the DevTools protocol; no npm dependencies.
//
// Usage: npm run screenshots [-- --sizes 390x844,1280x800] [--out dir]
//
// Fonts and scrollbars differ slightly from WebKitGTK, the real webview.

import { spawn, spawnSync } from "node:child_process";
import { DatabaseSync } from "node:sqlite";
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const argv = process.argv.slice(2);
const option = (name, fallback) => {
  const i = argv.indexOf(name);
  return i === -1 ? fallback : argv[i + 1];
};
const outDir = option("--out", join(root, "screenshots"));
const SIZES = option("--sizes", "390x844,800x700,1280x800,1920x1080,2560x1440")
  .split(",")
  .map((s) => s.split("x").map(Number));
const DATA = option("--data", "/var/lib/netmeter");
const PORT = 1431;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ── Fixtures ──────────────────────────────────────────────────────────────

function rows(file, sql) {
  const db = new DatabaseSync(join(DATA, file), { readOnly: true });
  try {
    return db.prepare(sql).all().map((r) => ({ ...r }));
  } finally {
    db.close();
  }
}

const me = process.getuid();
const visible = `(uid = ${me} OR uid < 1000 OR uid = 65534)`;
const config = JSON.parse(
  readFileSync(join(process.env.HOME, ".config/com.shtam.netmeter/config.json"), "utf8"),
);
const fixtures = {
  config,
  interfaces: rows("netmeter.db", "SELECT id, name, kind FROM interfaces"),
  day: rows("netmeter.db", "SELECT local_date, interface_id, rx_bytes, tx_bytes FROM usage_day"),
  hour: rows(
    "netmeter.db",
    "SELECT hour_start_utc_ms, local_date, interface_id, rx_bytes, tx_bytes FROM usage_hour",
  ),
  appDay: rows(
    "apps.db",
    `SELECT d.local_date, a.app_key AS app, d.rx_bytes, d.tx_bytes
       FROM usage_app_day d JOIN apps a ON a.id = d.app_id WHERE ${visible.replaceAll("uid", "d.uid")}`,
  ),
  appHour: rows(
    "apps.db",
    `SELECT h.hour_start_utc_ms, a.app_key AS app, h.rx_bytes, h.tx_bytes
       FROM usage_app_hour h JOIN apps a ON a.id = h.app_id WHERE ${visible.replaceAll("uid", "h.uid")}`,
  ),
  overhead: rows("apps.db", "SELECT local_date, rx_bytes, tx_bytes FROM overhead_day"),
};

// Runs in the page: answers each command the way the backend would, from the
// rows above. Approximate where exactness does not change the layout.
const pageSetup = `(() => {
  const FIX = ${JSON.stringify(fixtures)};
  const pad = (n) => String(n).padStart(2, "0");
  const dayKey = (d) => d.getFullYear() + "-" + pad(d.getMonth() + 1) + "-" + pad(d.getDate());
  const included = (name) => FIX.config.included_interfaces.includes(name);
  const ifName = Object.fromEntries(FIX.interfaces.map((i) => [i.id, i]));

  // Every bucket between two calendar keys, as [key, startMs, endMs, matches(date or hourMs)].
  function buckets(granularity, from, to) {
    const out = [];
    const start = new Date(from.length === 4 ? from + "-01-01T00:00" : from.length === 7 ? from + "-01T00:00" : from + "T00:00");
    const end = new Date(to.length === 4 ? (+to + 1) + "-01-01T00:00" : to.length === 7 ? to + "-01T00:00" : to + "T00:00");
    if (to.length === 7) end.setMonth(end.getMonth() + 1);
    if (to.length === 10) end.setDate(end.getDate() + 1);
    for (let d = new Date(start); d < end; ) {
      const s = new Date(d);
      if (granularity === "hour") d.setHours(d.getHours() + 1);
      else if (granularity === "day") d.setDate(d.getDate() + 1);
      else if (granularity === "month") d.setMonth(d.getMonth() + 1);
      else d.setFullYear(d.getFullYear() + 1);
      const key = granularity === "hour" ? s.toISOString().slice(0, 13)
        : granularity === "day" ? dayKey(s)
        : granularity === "month" ? dayKey(s).slice(0, 7) : String(s.getFullYear());
      out.push({ key, start: s.getTime(), end: d.getTime() });
    }
    return out;
  }
  const inBucket = (b, granularity, date, hourMs) =>
    granularity === "hour" ? hourMs >= b.start && hourMs < b.end : date.startsWith(b.key);

  const add = (t, r) => { t.rx_bytes += r.rx_bytes; t.tx_bytes += r.tx_bytes; return t; };
  const zero = () => ({ rx_bytes: 0, tx_bytes: 0 });

  function series({ granularity, from, to }) {
    const src = granularity === "hour" ? FIX.hour : FIX.day;
    const total = { included: zero(), observed: zero() };
    const out = buckets(granularity, from, to).map((b) => {
      const by = {};
      const summary = { included: zero(), observed: zero() };
      for (const r of src) {
        if (!inBucket(b, granularity, r.local_date, r.hour_start_utc_ms)) continue;
        const i = ifName[r.interface_id];
        if (!i || i.kind === "loopback") continue;
        by[i.name] ??= { name: i.name, traffic: zero(), included: included(i.name) };
        add(by[i.name].traffic, r);
        add(summary.observed, r);
        if (included(i.name)) add(summary.included, r);
      }
      add(total.included, summary.included);
      add(total.observed, summary.observed);
      return { key: b.key, start_utc_ms: b.start, summary, by_interface: Object.values(by) };
    });
    const since = FIX.day.map((r) => r.local_date).sort()[0] ?? null;
    return { total, buckets: out, offline_total: { included: zero(), observed: zero() }, offline_windows: [], data_since: since };
  }

  function appUsage({ granularity, from, to }) {
    const src = granularity === "hour" ? FIX.appHour : FIX.appDay;
    const totals = {};
    const overhead = zero();
    const days = buckets("day", from, to).map((b) => b.key);
    for (const r of FIX.overhead) if (days.includes(r.local_date)) add(overhead, r);
    const sorted = (m) => Object.entries(m).map(([app, t]) => ({ app, ...t }))
      .sort((a, b) => b.rx_bytes + b.tx_bytes - (a.rx_bytes + a.tx_bytes));
    const out = buckets(granularity, from, to).map((b) => {
      const m = {};
      for (const r of src) {
        if (!inBucket(b, granularity, r.local_date ?? "", r.hour_start_utc_ms)) continue;
        add((m[r.app] ??= zero()), r);
        add((totals[r.app] ??= zero()), r);
      }
      return { key: b.key, start_utc_ms: b.start, end_utc_ms: b.end, apps: sorted(m), overhead: zero() };
    });
    return { granularity, buckets: out, total: sorted(totals), overhead, data_since: null };
  }

  // A plausible live tick: today's busiest apps at a few hundred KiB/s.
  function liveApps() {
    const today = appUsage({ granularity: "day", from: dayKey(new Date()), to: dayKey(new Date()) });
    const apps = today.total.slice(0, 6).map((a, n) => ({
      app: a.app,
      rx_bytes: Math.round(4e6 / (n + 1) * (0.5 + Math.random())),
      tx_bytes: Math.round(3e5 / (n + 1) * (0.5 + Math.random())),
    }));
    return { interval_ms: 5000, sampled_at_utc_ms: Date.now(), apps };
  }

  function live() {
    const rx = 2e6 * (0.3 + Math.random()), tx = 2e5 * (0.3 + Math.random());
    return {
      total: { rx_bytes_per_sec: rx, tx_bytes_per_sec: tx },
      by_interface: FIX.interfaces.filter((i) => i.kind !== "virtual").map((i) => ({
        name: i.name,
        included: included(i.name),
        rate: included(i.name) ? { rx_bytes_per_sec: rx, tx_bytes_per_sec: tx } : { rx_bytes_per_sec: 0, tx_bytes_per_sec: 0 },
      })),
      time_anomaly: false,
    };
  }

  const params = new URLSearchParams(location.search);
  const helperState = params.get("helper") ?? "running";
  const listeners = [];
  setInterval(() => {
    for (const { event, id } of listeners)
      if (event === "network-usage-updated") window["_" + id]?.({ event, id: 0, payload: live() });
  }, 250);

  let callbackId = 0;
  window.__TAURI_INTERNALS__ = {
    async invoke(cmd, args = {}) {
      const today = dayKey(new Date());
      switch (cmd) {
        case "get_config": return FIX.config;
        case "get_interfaces": return FIX.interfaces.filter((i) => i.kind !== "virtual").map((i) => ({
          name: i.name, kind: i.kind, present: i.kind !== "vpn" || i.name === "tailscale0", included: included(i.name),
        }));
        case "get_monitor_status": return { state: "running", sampling_interval_seconds: 5, degraded_reason: null };
        case "get_today_usage": return series({ granularity: "day", from: today, to: today });
        case "get_usage_series": return series(args.query);
        case "get_live_rates": return live();
        case "get_helper_state": return helperState === "running"
          ? { state: "running", probes_attached: 6, probes_expected: 6, started_at_utc_ms: Date.now() - 3.6e6 }
          : { state: helperState };
        case "get_app_usage":
          if (helperState !== "running") throw { kind: "not_installed", message: "not running" };
          return appUsage(args);
        case "get_live_apps": return helperState === "running" ? liveApps() : null;
        case "get_autostart": return true;
        case "can_install_helper": return true;
        case "get_process_icon": return null;
        case "plugin:event|listen": listeners.push({ event: args.event, id: args.handler }); return listeners.length;
        case "plugin:notification|is_permission_granted": return true;
        default: return null;
      }
    },
    transformCallback(fn) { const id = ++callbackId; window["_" + id] = fn; return id; },
    unregisterCallback() {},
    convertFileSrc: (p) => p,
    metadata: { currentWindow: { label: "main" }, currentWebview: { windowLabel: "main", label: "main" } },
  };
  window.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener() {} };
  if (params.get("theme")) localStorage.setItem("theme", params.get("theme"));

  const isVisible = (e) => e.getClientRects().length > 0;
  window.__ui = {
    find(text) {
      const hits = [...document.querySelectorAll("body *")].filter((e) => isVisible(e) && e.textContent.trim() === text);
      return hits.find((e) => !hits.some((o) => o !== e && e.contains(o))) ?? null;
    },
    click(text) {
      const el = text.startsWith("css:") ? document.querySelector(text.slice(4)) : this.find(text);
      if (!el) return false;
      el.scrollIntoView({ block: "center" });
      el.click();
      return true;
    },
    // Height the window needs for the scrolling pane to show everything.
    fullHeight() {
      const panes = [...document.querySelectorAll("body *")].filter((e) => /(auto|scroll)/.test(getComputedStyle(e).overflowY));
      return Math.max(innerHeight, ...panes.map((e) => e.scrollHeight - e.clientHeight + innerHeight));
    },
    // Anything wider than the window, and the widest offender.
    overflow() {
      const w = document.documentElement.clientWidth;
      // Inside a sideways-scrolling container is by design, not overflow.
      const scrolls = (e) => e.closest(".scroll-x") !== null;
      const wide = [...document.querySelectorAll("body *")].filter((e) => isVisible(e) && !scrolls(e) && e.getBoundingClientRect().right > w + 1);
      const leaf = wide.filter((e) => !wide.some((o) => o !== e && e.contains(o)));
      return leaf.slice(0, 3).map((e) => e.tagName.toLowerCase() + "." + [...e.classList].join(".") + " right=" + Math.round(e.getBoundingClientRect().right));
    },
  };
})();`;

// ── Processes ─────────────────────────────────────────────────────────────

const children = [];
process.on("exit", () => children.forEach((c) => c.kill("SIGTERM")));
process.on("SIGINT", () => process.exit(130));

const vite = spawn("npx", ["vite", "--port", String(PORT), "--strictPort"], { cwd: root, stdio: "ignore" });
children.push(vite);

const browserBin = [process.env.CHROME, "chromium", "google-chrome-stable", "google-chrome"].find(
  (b) => b && spawnSync("which", [b]).status === 0,
);
if (!browserBin) throw new Error("No Chromium found; set CHROME=/path/to/chrome");
const profile = mkdtempSync(join(tmpdir(), "netmeter-shots-"));
process.on("exit", () => {
  try {
    rmSync(profile, { recursive: true, force: true });
  } catch {
    // Chromium still exiting; tmpfs clears it.
  }
});
const browser = spawn(
  browserBin,
  ["--headless=new", "--disable-gpu", "--no-first-run", `--user-data-dir=${profile}`, "--remote-debugging-port=0", "about:blank"],
  { stdio: ["ignore", "ignore", "pipe"] },
);
children.push(browser);

const wsUrl = await new Promise((resolve, reject) => {
  let buf = "";
  browser.stderr.on("data", (d) => {
    buf += d;
    const m = buf.match(/DevTools listening on (ws:\/\/\S+)/);
    if (m) resolve(m[1]);
  });
  browser.on("exit", (code) => reject(new Error(`${browserBin} exited (${code}): ${buf}`)));
});

for (let i = 0; ; i++) {
  try {
    if ((await fetch(`http://localhost:${PORT}`)).ok) break;
  } catch {
    // not up yet
  }
  if (i > 60) throw new Error("Vite dev server did not start");
  await sleep(500);
}

// ── DevTools protocol ─────────────────────────────────────────────────────

const ws = new WebSocket(wsUrl);
await new Promise((r) => ws.addEventListener("open", r, { once: true }));
let msgId = 0;
const pending = new Map();
ws.addEventListener("message", ({ data }) => {
  const msg = JSON.parse(data);
  if (msg.id && pending.has(msg.id)) {
    const { resolve, reject } = pending.get(msg.id);
    pending.delete(msg.id);
    if (msg.error) reject(new Error(msg.error.message));
    else resolve(msg.result);
  } else if (msg.method === "Runtime.exceptionThrown") {
    console.warn("  page error:", msg.params.exceptionDetails.exception?.description ?? msg.params.exceptionDetails.text);
  }
});
const cdp = (method, params = {}, sessionId) =>
  new Promise((resolve, reject) => {
    const id = ++msgId;
    pending.set(id, { resolve, reject });
    ws.send(JSON.stringify({ id, method, params, sessionId }));
  });

const { targetId } = await cdp("Target.createTarget", { url: "about:blank" });
const { sessionId } = await cdp("Target.attachToTarget", { targetId, flatten: true });
const page = (method, params) => cdp(method, params, sessionId);
await page("Page.enable");
await page("Runtime.enable");
await page("Page.addScriptToEvaluateOnNewDocument", { source: pageSetup });

const evaluate = async (expression) => {
  const { result, exceptionDetails } = await page("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true });
  if (exceptionDetails) throw new Error(exceptionDetails.exception?.description ?? exceptionDetails.text);
  return result.value;
};

// ── Steps ─────────────────────────────────────────────────────────────────

mkdirSync(outDir, { recursive: true });
for (const f of readdirSync(outDir)) if (f.endsWith(".png")) rmSync(join(outDir, f));

const slug = (s) => s.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "");
const problems = [];

let size = [0, 0];
const resize = (w, h) =>
  page("Emulation.setDeviceMetricsOverride", { width: w, height: h, deviceScaleFactor: 1, mobile: w < 600 });

async function capture(name) {
  await sleep(700);
  const overflow = await evaluate("__ui.overflow()");
  if (overflow.length) problems.push(`${name}: ${overflow.join(", ")}`);
  // The app scrolls inside its main pane, so grow the window to fit it.
  await resize(size[0], await evaluate("__ui.fullHeight()"));
  await sleep(300);
  const { data } = await page("Page.captureScreenshot", { format: "png" });
  await resize(...size);
  writeFileSync(join(outDir, `${slug(name)}.png`), Buffer.from(data, "base64"));
  console.log(`  ${slug(name)}.png`);
}

async function open(query) {
  await page("Page.navigate", { url: `http://localhost:${PORT}/?${query}` });
  await sleep(1800);
}

const click = async (text) => {
  if (!(await evaluate(`__ui.click(${JSON.stringify(text)})`))) console.warn(`  skip: "${text}" not found`);
  await sleep(600);
};

for (const [w, h] of SIZES) {
  size = [w, h];
  await resize(w, h);
  for (const theme of ["light", "dark"]) {
    const tag = `${w}x${h}-${theme}`;
    await open(`theme=${theme}`);
    await capture(`${tag}-overview`);
    await click("Applications");
    await capture(`${tag}-apps-today`);
    await click("30 days");
    await capture(`${tag}-apps-30-days`);
    await click("css:.tappable-row");
    await capture(`${tag}-apps-timeline`);
    await click("Custom\u2026");
    await capture(`${tag}-apps-custom`);
    await click("History");
    await capture(`${tag}-history-today`);
    await click("This year");
    await capture(`${tag}-history-year`);
    await click("Settings");
    await capture(`${tag}-settings`);
  }
  await open("theme=light&helper=not_installed");
  await click("Applications");
  await capture(`${w}x${h}-apps-no-helper`);
}

console.log(problems.length ? `\nWider than the window:\n  ${problems.join("\n  ")}` : "\nNo horizontal overflow.");
process.exit(0);
