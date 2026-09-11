//! The seam between "how Linux reports counters" and everything above it.

use crate::core::errors::MonitorError;
use crate::core::types::{CounterSample, NetworkInterface};

/// A source of network interface statistics.
///
/// Abstracted for two reasons, both real: tests need to drive counter resets,
/// interface churn and overflow deterministically (no test can make a kernel
/// reset a NIC on cue), and a future `netmeterd` may want to read the same
/// counters through a different mechanism without the sampling logic noticing.
pub trait NetworkStatsProvider: Send + 'static {
    /// Every interface currently present, with its classification and state.
    ///
    /// Relatively expensive (one `/sys` traversal), so the engine calls this
    /// only when the interface set changes rather than on every tick.
    fn interfaces(&self) -> Result<Vec<NetworkInterface>, MonitorError>;

    /// Cumulative RX/TX byte counters for every interface, in one shot.
    ///
    /// Batched deliberately: `/proc/net/dev` renders all interfaces in a single
    /// read, so asking per-interface would multiply the syscalls for no gain
    /// and would also skew the readings against each other in time.
    fn counters(&self) -> Result<Vec<CounterSample>, MonitorError>;

    /// Lines this provider has had to discard because they did not parse.
    ///
    /// On the trait rather than only on the Linux implementation so the engine
    /// can publish it in `MonitorStatus`: a parser that is systematically wrong
    /// about a kernel would otherwise drop interfaces from every total in
    /// complete silence.
    fn parse_errors(&self) -> u64 {
        0
    }
}

/// A provider driven entirely from in-memory data, for tests.
///
/// Production code never constructs this -- it is compiled only for tests and
/// for anyone writing tests against NetMeter's engine.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Default, Clone)]
pub struct FakeNetworkStatsProvider {
    inner: std::sync::Arc<std::sync::Mutex<FakeState>>,
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Default)]
struct FakeState {
    interfaces: Vec<NetworkInterface>,
    counters: Vec<CounterSample>,
    /// When set, the next call fails -- for testing that a transient read
    /// failure neither crashes the loop nor corrupts a baseline.
    fail_next: Option<String>,
    /// When set, the next call panics -- for testing that one bad tick is
    /// survived rather than ending monitoring silently.
    panic_next: bool,
    interface_calls: usize,
    counter_calls: usize,
}

#[cfg(any(test, feature = "test-support"))]
impl FakeNetworkStatsProvider {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Define the interface as present, with the given counters.
    pub fn set(&self, iface: NetworkInterface, rx: u64, tx: u64) {
        let mut s = self.lock();
        let sample = CounterSample {
            name: iface.name.clone(),
            ifindex: iface.ifindex,
            counters: crate::core::types::NetworkCounters {
                rx_bytes: rx,
                tx_bytes: tx,
            },
        };
        match s.counters.iter_mut().find(|c| c.name == iface.name) {
            Some(c) => *c = sample,
            None => s.counters.push(sample),
        }
        match s.interfaces.iter_mut().find(|i| i.name == iface.name) {
            Some(i) => *i = iface,
            None => s.interfaces.push(iface),
        }
    }

    /// Advance an interface's counters, as ordinary traffic would.
    pub fn advance(&self, name: &str, rx: u64, tx: u64) {
        let mut s = self.lock();
        if let Some(c) = s.counters.iter_mut().find(|c| c.name == name) {
            c.counters.rx_bytes = c.counters.rx_bytes.saturating_add(rx);
            c.counters.tx_bytes = c.counters.tx_bytes.saturating_add(tx);
        }
    }

    /// Force the counters to an absolute value, for reset and overflow tests.
    pub fn set_counters(&self, name: &str, rx: u64, tx: u64) {
        let mut s = self.lock();
        if let Some(c) = s.counters.iter_mut().find(|c| c.name == name) {
            c.counters.rx_bytes = rx;
            c.counters.tx_bytes = tx;
        }
    }

    /// Remove an interface, as unplugging or `docker compose down` would.
    pub fn remove(&self, name: &str) {
        let mut s = self.lock();
        s.counters.retain(|c| c.name != name);
        s.interfaces.retain(|i| i.name != name);
    }

    /// Rename an interface in place, keeping its ifindex -- what udev does.
    pub fn rename(&self, from: &str, to: &str) {
        let mut s = self.lock();
        if let Some(c) = s.counters.iter_mut().find(|c| c.name == from) {
            c.name = to.to_string();
        }
        if let Some(i) = s.interfaces.iter_mut().find(|i| i.name == from) {
            i.name = to.to_string();
        }
    }

    pub fn fail_next(&self, msg: &str) {
        self.lock().fail_next = Some(msg.to_string());
    }

    /// Make the next `counters()` call panic.
    pub fn panic_next(&self) {
        self.lock().panic_next = true;
    }

    pub fn interface_calls(&self) -> usize {
        self.lock().interface_calls
    }

    pub fn counter_calls(&self) -> usize {
        self.lock().counter_calls
    }
}

#[cfg(any(test, feature = "test-support"))]
impl NetworkStatsProvider for FakeNetworkStatsProvider {
    fn interfaces(&self) -> Result<Vec<NetworkInterface>, MonitorError> {
        let mut s = self.lock();
        s.interface_calls += 1;
        if let Some(m) = s.fail_next.take() {
            return Err(MonitorError::Format {
                path: "fake".into(),
                detail: m,
            });
        }
        Ok(s.interfaces.clone())
    }

    fn counters(&self) -> Result<Vec<CounterSample>, MonitorError> {
        let mut s = self.lock();
        s.counter_calls += 1;
        if std::mem::take(&mut s.panic_next) {
            drop(s);
            panic!("simulated panic inside a sampling tick");
        }
        if let Some(m) = s.fail_next.take() {
            return Err(MonitorError::Format {
                path: "fake".into(),
                detail: m,
            });
        }
        Ok(s.counters.clone())
    }
}
