//! The sampling loop.
//!
//! Owns the writer connection and the in-memory accumulator. Everything it
//! touches outside itself goes through a trait -- [`NetworkStatsProvider`],
//! [`Repository`], [`EventSink`] -- which is what makes the same loop usable
//! inside a standalone daemon.

use super::provider::NetworkStatsProvider;
use super::sampling::{BucketKey, Clocks, OfflineWindow, Sampler};
use crate::api::events::{EventSink, InterfaceChanged, InterfaceRate, LiveUsage};
use crate::core::config::{Config, ResolvedPolicy};
use crate::core::statistics;
use crate::core::types::{
    DataRate, InterfaceKind, MonitorState, MonitorStatus, Traffic, UsageSummary,
};
use crate::storage::repository::{FlushBatch, Repository};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

/// Tunables the engine reads at each tick, so a config change takes effect
/// without a restart.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub sampling_interval: std::time::Duration,
    pub flush_interval: std::time::Duration,
    pub event_interval: std::time::Duration,
    pub timezone: chrono_tz::Tz,
    pub policy: ResolvedPolicy,
    pub retention: crate::core::config::RetentionPolicy,
}

impl From<&Config> for EngineConfig {
    fn from(c: &Config) -> Self {
        Self {
            sampling_interval: c.sampling_interval(),
            flush_interval: c.flush_interval(),
            event_interval: std::time::Duration::from_millis(c.event_interval_ms.max(100) as u64),
            timezone: c.timezone_or_system(),
            policy: c.resolve(),
            retention: c.retention.clone(),
        }
    }
}

/// Traffic buffered in memory, awaiting a flush.
///
/// Shared with the API layer so that "Today" includes bytes not yet written:
/// without this the headline number would sit frozen between flushes and then
/// jump, which reads as a bug even though the total is right.
#[derive(Debug, Default)]
pub struct Pending {
    pub buckets: HashMap<BucketKey, Traffic>,
    pub offline: Vec<OfflineWindow>,
    /// What we know about every interface seen this session, for the API and
    /// for the flush. Bounded each tick to what is present or still buffered.
    pub seen: HashMap<String, SeenInterface>,
}

impl Pending {
    pub fn total(&self) -> Traffic {
        self.buckets.values().copied().sum()
    }

    fn merge(&mut self, other: HashMap<BucketKey, Traffic>) {
        for (k, v) in other {
            *self.buckets.entry(k).or_insert(Traffic::ZERO) += v;
        }
    }

    fn is_empty(&self) -> bool {
        self.buckets.is_empty() && self.offline.is_empty()
    }
}

/// Interface metadata held in memory between flushes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenInterface {
    pub kind: InterfaceKind,
    /// `None` for interfaces with no link-layer address, such as a tunnel.
    pub mac: Option<String>,
}

/// Live state the API can read without touching the engine thread.
#[derive(Debug)]
pub struct EngineShared {
    pub pending: Mutex<Pending>,
    pub live: Mutex<Option<LiveUsage>>,
    started_at_utc_ms: AtomicI64,
    last_tick_utc_ms: AtomicI64,
    last_flush_utc_ms: AtomicI64,
    restarts: AtomicU32,
    parse_errors: AtomicU64,
    interfaces_observed: AtomicU32,
    interfaces_included: AtomicU32,
    running: AtomicBool,
    sampling_interval_seconds: AtomicU32,
    degraded: Mutex<Option<String>>,
}

impl Default for EngineShared {
    fn default() -> Self {
        Self {
            pending: Mutex::new(Pending::default()),
            live: Mutex::new(None),
            started_at_utc_ms: AtomicI64::new(0),
            last_tick_utc_ms: AtomicI64::new(0),
            last_flush_utc_ms: AtomicI64::new(0),
            restarts: AtomicU32::new(0),
            parse_errors: AtomicU64::new(0),
            interfaces_observed: AtomicU32::new(0),
            interfaces_included: AtomicU32::new(0),
            running: AtomicBool::new(false),
            sampling_interval_seconds: AtomicU32::new(5),
            degraded: Mutex::new(None),
        }
    }
}

/// Locks a mutex, recovering from poisoning.
///
/// Poisoning means some earlier holder panicked. Every value guarded here is
/// plain data -- a byte counter, a timestamp -- so the state is still coherent,
/// and refusing to proceed would brick the app for the life of the process.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl EngineShared {
    /// Current status, derived from heartbeats rather than from a stored flag.
    ///
    /// A boolean cannot tell "sampling" from "the thread died an hour ago",
    /// and a green light over a silently dead sampler is the worst failure a
    /// usage meter can have: it is indistinguishable from an idle network.
    pub fn status(&self) -> MonitorStatus {
        let interval_s = self.sampling_interval_seconds.load(Ordering::Relaxed).max(1);
        let running = self.running.load(Ordering::Relaxed);
        let last_tick = self.last_tick_utc_ms.load(Ordering::Relaxed);
        let last_flush = self.last_flush_utc_ms.load(Ordering::Relaxed);
        let now = crate::core::time::now_utc_ms();
        let degraded_reason = lock(&self.degraded).clone();

        let state = if !running {
            MonitorState::Stopped
        } else if last_tick > 0 && now - last_tick > (interval_s as i64 * 1_000) * 3 {
            MonitorState::Stalled
        } else if degraded_reason.is_some() || self.restarts.load(Ordering::Relaxed) > 0 {
            MonitorState::Degraded
        } else {
            MonitorState::Running
        };

        MonitorStatus {
            state,
            started_at_utc_ms: none_if_zero(self.started_at_utc_ms.load(Ordering::Relaxed)),
            last_tick_utc_ms: none_if_zero(last_tick),
            last_flush_utc_ms: none_if_zero(last_flush),
            sampling_interval_seconds: interval_s,
            interfaces_observed: self.interfaces_observed.load(Ordering::Relaxed),
            interfaces_included: self.interfaces_included.load(Ordering::Relaxed),
            restarts: self.restarts.load(Ordering::Relaxed),
            parse_errors: self.parse_errors.load(Ordering::Relaxed),
            pending: lock(&self.pending).total(),
            degraded_reason,
        }
    }

    fn set_degraded(&self, reason: Option<String>) -> bool {
        let mut g = lock(&self.degraded);
        let changed = *g != reason;
        *g = reason;
        changed
    }
}

fn none_if_zero(v: i64) -> Option<i64> {
    (v != 0).then_some(v)
}

/// Messages the loop reacts to between ticks.
enum Signal {
    Shutdown,
    Reconfigure(Box<EngineConfig>),
    FlushNow,
}

/// A running sampling loop.
pub struct Engine {
    shared: Arc<EngineShared>,
    tx: mpsc::Sender<Signal>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Engine {
    /// Start sampling.
    ///
    /// The loop runs on a plain OS thread rather than an async task: it wakes
    /// on a timer, does a few syscalls and sleeps again, so an async runtime
    /// would add machinery without removing any blocking.
    pub fn start<P, R, S>(
        provider: P,
        repository: R,
        sink: Arc<S>,
        config: EngineConfig,
        boot_id: String,
    ) -> Self
    where
        P: NetworkStatsProvider,
        R: Repository,
        S: EventSink,
    {
        let shared = Arc::new(EngineShared::default());
        let (tx, rx) = mpsc::channel();

        shared.running.store(true, Ordering::Relaxed);
        shared
            .started_at_utc_ms
            .store(crate::core::time::now_utc_ms(), Ordering::Relaxed);
        shared.sampling_interval_seconds.store(
            config.sampling_interval.as_secs().max(1) as u32,
            Ordering::Relaxed,
        );

        let worker = {
            let shared = Arc::clone(&shared);
            let sink = Arc::clone(&sink);
            std::thread::Builder::new()
                .name("netmeter-sampler".into())
                .spawn(move || {
                    run_loop(provider, repository, sink, config, boot_id, shared, rx);
                })
                .ok()
        };

        if worker.is_none() {
            tracing::error!("could not spawn the sampling thread");
            shared.running.store(false, Ordering::Relaxed);
        }

        Self {
            shared,
            tx,
            handle: worker,
        }
    }

    pub fn shared(&self) -> Arc<EngineShared> {
        Arc::clone(&self.shared)
    }

    pub fn status(&self) -> MonitorStatus {
        self.shared.status()
    }

    /// Apply a new configuration without restarting.
    pub fn reconfigure(&self, config: EngineConfig) {
        self.shared.sampling_interval_seconds.store(
            config.sampling_interval.as_secs().max(1) as u32,
            Ordering::Relaxed,
        );
        let _ = self.tx.send(Signal::Reconfigure(Box::new(config)));
    }

    /// Ask for an immediate flush; does not wait for it.
    pub fn request_flush(&self) {
        let _ = self.tx.send(Signal::FlushNow);
    }

    /// Signal shutdown and wait for the final flush.
    ///
    /// The loop blocks on the channel rather than sleeping, so it observes this
    /// within microseconds instead of at the end of a tick. The final flush
    /// happens inside the loop, on the thread that owns the connection -- never
    /// from the caller, which would make a second writer.
    pub fn stop(mut self) {
        let _ = self.tx.send(Signal::Shutdown);
        if let Some(h) = self.handle.take() {
            match h.join() {
                Ok(()) => {}
                Err(_) => tracing::error!("sampling thread panicked during shutdown"),
            }
        }
        self.shared.running.store(false, Ordering::Relaxed);
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // A dropped engine still gets to flush: the thread is signalled and
        // joined rather than detached, so buffered bytes are not lost.
        let _ = self.tx.send(Signal::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.shared.running.store(false, Ordering::Relaxed);
    }
}

/// How many consecutive failed flushes before the buffer is collapsed to keep
/// memory flat during a long outage (a full disk, a read-only filesystem).
const COLLAPSE_AFTER_FAILURES: u32 = 20;

fn run_loop<P, R, S>(
    provider: P,
    mut repository: R,
    sink: Arc<S>,
    mut config: EngineConfig,
    boot_id: String,
    shared: Arc<EngineShared>,
    rx: mpsc::Receiver<Signal>,
) where
    P: NetworkStatsProvider,
    R: Repository,
    S: EventSink,
{
    let mut sampler = Sampler::new(
        boot_id,
        config.timezone,
        config.policy.clone(),
        config.flush_interval.as_millis() as i64,
    );

    match repository.baselines() {
        Ok(saved) => {
            let n = sampler.restore(saved);
            if n > 0 {
                tracing::info!(interfaces = n, "resumed counter baselines from this boot");
            }
        }
        Err(e) => tracing::error!(error = %e, "could not read stored baselines; starting fresh"),
    }

    sampler.refresh_interfaces(&provider);
    let mut last_names: Vec<String> = sampler
        .known_interfaces()
        .map(|i| i.name.clone())
        .collect();
    last_names.sort();

    let mut last_flush = std::time::Instant::now();
    let mut last_event = std::time::Instant::now() - config.event_interval;
    let mut ticks: u64 = 0;
    let mut consecutive_flush_failures: u32 = 0;
    let mut last_prune_date = repository.meta("last_prune_date").ok().flatten();

    publish_status(&shared, &sink);

    loop {
        // Block on the channel rather than sleeping: shutdown is observed
        // immediately, and an idle tick costs one timer wake-up.
        match rx.recv_timeout(config.sampling_interval) {
            Ok(signal) => {
                if apply_signal(signal, &mut config, &mut sampler) {
                    break;
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            // The Engine handle was dropped without stop(); flush and exit.
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        ticks += 1;

        // One tick's work, isolated. A panic here -- a slice index in a parser
        // on an unexpected kernel format, an arithmetic overflow in a debug
        // build -- costs this tick and is counted, rather than silently ending
        // monitoring while the GUI still shows a green light. The loop state
        // lives outside the closure, so the next tick resumes with its
        // baselines and its buffered bytes intact.
        //
        // `AssertUnwindSafe` is the honest claim here: the state crossing the
        // boundary is a counter map, a SQLite connection and some timestamps.
        // An interrupted transaction is rolled back by rusqlite's own `Drop`,
        // and an interrupted buffer update leaves bytes either counted or not
        // counted, never half-counted -- the flush reconciles against what
        // actually committed.
        let tick_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_tick(
                &provider,
                &mut repository,
                &sink,
                &config,
                &mut sampler,
                &shared,
                ticks,
                &mut last_names,
                &mut last_flush,
                &mut last_event,
                &mut consecutive_flush_failures,
                &mut last_prune_date,
            )
        }));

        if tick_result.is_err() {
            let n = shared.restarts.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::error!(
                restarts = n,
                "sampling tick panicked; monitoring continues"
            );
            // Back off briefly so a deterministic panic cannot spin the CPU.
            // The signal is *applied*, not merely inspected: dropping a
            // `Reconfigure` that lands in this one-second window would leave
            // the engine counting under the old policy for ever, while the
            // config file and the UI both showed the new one.
            match rx.recv_timeout(std::time::Duration::from_secs(1)) {
                Ok(signal) => {
                    if apply_signal(signal, &mut config, &mut sampler) {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    // Final flush on the thread that owns the connection.
    let now = crate::core::time::now_utc_ms();
    let mut ignored = 0;
    flush(&mut repository, &sampler, &shared, &sink, now, &mut ignored);
    shared.running.store(false, Ordering::Relaxed);
    publish_status(&shared, &sink);
    tracing::info!("sampling stopped");
}

/// Apply one control signal. Returns true when the loop should exit.
///
/// Every path that takes a signal off the channel goes through here, so a
/// signal can never be received and then dropped on the floor.
fn apply_signal(signal: Signal, config: &mut EngineConfig, sampler: &mut Sampler) -> bool {
    match signal {
        Signal::Shutdown => return true,
        Signal::Reconfigure(new) => {
            sampler.set_timezone(new.timezone);
            sampler.set_policy(new.policy.clone());
            *config = *new;
            tracing::info!(
                interval_s = config.sampling_interval.as_secs(),
                "configuration applied"
            );
        }
        // Nothing to do: the caller flushes on its next pass regardless.
        Signal::FlushNow => {}
    }
    false
}

/// Everything one tick does. Extracted so it can be run under `catch_unwind`.
#[allow(clippy::too_many_arguments)]
fn run_tick<P, R, S>(
    provider: &P,
    repository: &mut R,
    sink: &Arc<S>,
    config: &EngineConfig,
    sampler: &mut Sampler,
    shared: &Arc<EngineShared>,
    ticks: u64,
    last_names: &mut Vec<String>,
    last_flush: &mut std::time::Instant,
    last_event: &mut std::time::Instant,
    consecutive_flush_failures: &mut u32,
    last_prune_date: &mut Option<String>,
) where
    P: NetworkStatsProvider,
    R: Repository,
    S: EventSink,
{
        let clocks = crate::system::power::now();

        // --- Read the counters. ---
        let samples = match provider.counters() {
            Ok(s) => s,
            Err(e) => {
                // Skip the tick without touching baselines: the next successful
                // read's delta from the same baseline is still exactly right,
                // so a transient failure costs latency, not bytes.
                tracing::warn!(error = %e, "counter read failed; skipping this tick");
                return;
            }
        };

        // --- Refresh classification only when it can have changed. ---
        let mut names: Vec<String> = samples.iter().map(|s| s.name.clone()).collect();
        names.sort();
        let set_changed = names != *last_names;
        // Every 60 ticks (~5 min at the default interval) catches an interface
        // destroyed and recreated under the same name between two ticks, which
        // leaves the name set unchanged.
        if set_changed || ticks % 60 == 0 {
            sampler.refresh_interfaces(provider);
            *last_names = names;
        }

        let outcome = sampler.sample(samples, clocks);

        // --- Buffer what the tick produced. ---
        //
        // Immediately after `sample`, and before anything else that could
        // fail: `sample` has already advanced the in-memory baselines, so
        // those bytes exist nowhere else. Any panic between the two would lose
        // them permanently, and the tick's `catch_unwind` would report a
        // survived tick that had quietly dropped traffic.
        {
            let mut pending = lock(&shared.pending);

            // Bridge legs are dropped here rather than at query time. A veth is
            // by construction one end of a pair whose traffic is also counted
            // on its bridge and on the physical NIC, so it adds no information;
            // and Docker mints a fresh random name (`vethed62bc1`,
            // `veth0de1e48`, ...) on every container start, so persisting them
            // would add permanent rows on a table kept forever, without bound,
            // on any machine that runs containers. They remain visible live, in
            // `get_interfaces` and the observed totals -- just not on disk.
            pending.merge(
                outcome
                    .buckets
                    .iter()
                    .filter(|(k, _)| {
                        sampler
                            .kind_of(&k.interface)
                            .map(|kind| kind.is_persistable())
                            .unwrap_or(true)
                    })
                    .map(|(k, v)| (k.clone(), *v))
                    .collect(),
            );
            pending.offline.extend(
                outcome
                    .offline
                    .iter()
                    .filter(|w| {
                        sampler
                            .kind_of(&w.interface)
                            .map(|k| k.is_persistable())
                            .unwrap_or(true)
                    })
                    .cloned(),
            );

            for i in sampler.known_interfaces() {
                pending.seen.insert(
                    i.name.clone(),
                    SeenInterface {
                        kind: i.kind,
                        mac: i.mac.clone(),
                    },
                );
            }
            // Forget interfaces that are neither present nor holding buffered
            // bytes. Without this the map -- and the interface rows every flush
            // upserts from it -- would grow with every container restart for
            // the life of the process.
            let live: std::collections::HashSet<&str> = sampler
                .known_interfaces()
                .map(|i| i.name.as_str())
                .collect();
            let buffered: std::collections::HashSet<String> = pending
                .buckets
                .keys()
                .map(|k| k.interface.clone())
                .chain(pending.offline.iter().map(|w| w.interface.clone()))
                .collect();
            pending
                .seen
                .retain(|name, _| live.contains(name.as_str()) || buffered.contains(name));
        }


        // A counter going backwards means the device may have been recreated,
        // so its cached metadata is stale. Cheap to re-read on an event that
        // happens a handful of times a day.
        if !outcome.reset.is_empty() {
            for name in &outcome.reset {
                tracing::info!(interface = %name, "counter reset; re-baselined");
            }
            sampler.refresh_interfaces(provider);
        }

        shared
            .last_tick_utc_ms
            .store(clocks.wall_utc_ms, Ordering::Relaxed);
        // Surfaced so a systematically wrong parser is visible in the status
        // rather than silently dropping interfaces from every total.
        shared
            .parse_errors
            .store(provider.parse_errors(), Ordering::Relaxed);
        shared
            .interfaces_observed
            .store(sampler.known_interfaces().count() as u32, Ordering::Relaxed);
        shared.interfaces_included.store(
            sampler
                .known_interfaces()
                .filter(|i| config.policy.counts(&i.name, i.kind))
                .count() as u32,
            Ordering::Relaxed,
        );

        // --- Interface churn. ---
        for name in &outcome.added {
            let kind = sampler.kind_of(name).unwrap_or(InterfaceKind::Virtual);
            tracing::info!(interface = %name, ?kind, "interface appeared");
            sink.interface_added(&InterfaceChanged {
                name: name.clone(),
                kind,
                included: config.policy.counts(name, kind),
            });
        }
        for name in &outcome.removed {
            let kind = sampler.kind_of(name).unwrap_or(InterfaceKind::Virtual);
            tracing::info!(interface = %name, "interface disappeared");
            sink.interface_removed(&InterfaceChanged {
                name: name.clone(),
                kind,
                included: config.policy.counts(name, kind),
            });
        }

        // --- Publish a coalesced live update. ---
        if last_event.elapsed() >= config.event_interval {
            *last_event = std::time::Instant::now();
            let payload = live_payload(sampler, config, &outcome, clocks, shared);
            *lock(&shared.live) = Some(payload.clone());
            sink.usage(&payload);
        }

        // --- Flush on schedule. ---
        if last_flush.elapsed() >= config.flush_interval {
            *last_flush = std::time::Instant::now();
            let ok = flush(
                repository,
                sampler,
                shared,
                sink,
                clocks.wall_utc_ms,
                consecutive_flush_failures,
            );

            // --- Retention, once per local calendar day. ---
            // Driven by the local date changing rather than by a 24-hour
            // countdown: a laptop that is suspended more than it is awake would
            // never reach a monotonic 24 hours, so retention would silently
            // never run.
            if ok {
                let today =
                    crate::core::time::local_date_key(config.timezone, clocks.wall_utc_ms);
                if last_prune_date.as_deref() != Some(today.as_str())
                    && !outcome.discarded_untrusted_clock
                {
                    match repository.prune(&config.retention, config.timezone) {
                        Ok(report) => {
                            if report != Default::default() {
                                tracing::info!(?report, "retention applied");
                            }
                            let _ = repository.set_meta("last_prune_date", &today);
                            *last_prune_date = Some(today);
                        }
                        Err(e) => tracing::error!(error = %e, "retention failed"),
                    }
                }
            }
        }
}

/// Write the buffer. Returns true on success.
fn flush<R: Repository, S: EventSink>(
    repository: &mut R,
    sampler: &Sampler,
    shared: &Arc<EngineShared>,
    sink: &Arc<S>,
    now_utc_ms: i64,
    failures: &mut u32,
) -> bool {
    // Snapshot rather than take: if the write fails, the bytes are still in the
    // buffer. Taking first and failing afterwards is the classic way this shape
    // of design silently loses 30 seconds of traffic per failed flush.
    let (buckets, offline, interfaces) = {
        let pending = lock(&shared.pending);
        if pending.is_empty() && sampler.baselines().is_empty() {
            return true;
        }
        (
            pending.buckets.clone(),
            pending.offline.clone(),
            pending
                .seen
                .iter()
                .filter(|(_, i)| i.kind.is_persistable())
                .map(|(n, i)| (n.clone(), i.kind, i.mac.clone()))
                .collect::<Vec<_>>(),
        )
    };

    let batch = FlushBatch {
        buckets: buckets.clone(),
        offline: offline.clone(),
        // Written in the same transaction as the usage. That is what makes a
        // crash lose nothing: a stale baseline means the kernel's cumulative
        // counter still holds the difference.
        baselines: sampler.baselines(),
        interfaces,
        at_utc_ms: now_utc_ms,
    };

    match repository.flush(&batch) {
        Ok(()) => {
            *failures = 0;
            {
                let mut pending = lock(&shared.pending);
                // Remove exactly what was written; ticks that landed during the
                // write keep their bytes.
                for (k, written) in &buckets {
                    if let Some(cur) = pending.buckets.get_mut(k) {
                        cur.rx_bytes = cur.rx_bytes.saturating_sub(written.rx_bytes);
                        cur.tx_bytes = cur.tx_bytes.saturating_sub(written.tx_bytes);
                        if cur.is_zero() {
                            pending.buckets.remove(k);
                        }
                    }
                }
                let written: std::collections::HashSet<_> = offline.iter().collect();
                pending.offline.retain(|w| !written.contains(w));
            }
            shared
                .last_flush_utc_ms
                .store(now_utc_ms, Ordering::Relaxed);
            if shared.set_degraded(None) {
                publish_status(shared, sink);
            }
            true
        }
        Err(e) => {
            *failures = failures.saturating_add(1);
            tracing::error!(error = %e, failures = *failures, "flush failed; bytes retained");
            if shared.set_degraded(Some(e.to_string())) {
                publish_status(shared, sink);
            }

            // A long outage must not grow the buffer without bound. Collapsing
            // to day granularity keeps every byte and every calendar date while
            // holding memory flat.
            if *failures >= COLLAPSE_AFTER_FAILURES {
                let mut pending = lock(&shared.pending);
                let collapsed: HashMap<BucketKey, Traffic> =
                    pending.buckets.drain().fold(HashMap::new(), |mut acc, (k, v)| {
                        let day = BucketKey {
                            interface: k.interface,
                            // Midnight-of-day stand-in: the day row is exact,
                            // the hour row becomes "some hour that day".
                            hour_start_utc_ms: k.hour_start_utc_ms
                                - k.hour_start_utc_ms.rem_euclid(86_400_000),
                            local_date: k.local_date,
                        };
                        *acc.entry(day).or_insert(Traffic::ZERO) += v;
                        acc
                    });
                pending.buckets = collapsed;
            }
            false
        }
    }
}

fn live_payload(
    sampler: &Sampler,
    config: &EngineConfig,
    outcome: &super::sampling::SampleOutcome,
    clocks: Clocks,
    shared: &Arc<EngineShared>,
) -> LiveUsage {
    let interval = outcome.interval;
    let mut by_interface: Vec<InterfaceRate> = outcome
        .rates
        .iter()
        .map(|(name, traffic)| {
            let kind = sampler.kind_of(name).unwrap_or(InterfaceKind::Virtual);
            InterfaceRate {
                name: name.clone(),
                rate: interval
                    .map(|d| statistics::rate(*traffic, d))
                    .unwrap_or(DataRate::UNKNOWN),
                traffic: *traffic,
                included: config.policy.counts(name, kind),
            }
        })
        .collect();
    by_interface.sort_by_key(|i| std::cmp::Reverse(i.traffic.total_bytes()));

    let counted: Traffic = by_interface
        .iter()
        .filter(|i| i.included)
        .map(|i| i.traffic)
        .sum();

    // Usage so far today from the buffer alone. The API's `get_usage_series`
    // adds the database's share; this field is the live tile's fast path.
    let today_key = crate::core::time::local_date_key(config.timezone, clocks.wall_utc_ms);
    let mut today = UsageSummary::ZERO;
    {
        let pending = lock(&shared.pending);
        for (k, v) in &pending.buckets {
            if k.local_date != today_key {
                continue;
            }
            today.observed += *v;
            let kind = pending
                .seen
                .get(&k.interface)
                .map(|i| i.kind)
                .unwrap_or(InterfaceKind::Virtual);
            if config.policy.counts(&k.interface, kind) {
                today.included += *v;
            }
        }
    }

    LiveUsage {
        sampled_at_utc_ms: clocks.wall_utc_ms,
        interval_ms: interval.map(|d| d.as_millis() as u64),
        total: interval
            .map(|d| statistics::rate(counted, d))
            .unwrap_or(DataRate::UNKNOWN),
        by_interface,
        today,
        time_anomaly: outcome.time.is_anomalous(),
    }
}

fn publish_status<S: EventSink>(shared: &Arc<EngineShared>, sink: &Arc<S>) {
    sink.status(&shared.status());
}
