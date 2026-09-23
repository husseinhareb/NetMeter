//! The GUI follows netmeterd: while it answers, history and live readings come
//! from it and the local engine stays off; when it goes, local sampling
//! resumes. Driven against a fake daemon on a real unix socket.
//!
//! Its own test binary because it sets `NETMETERD_SOCKET`, which is process
//! global.

use netmeter_lib::api::events::{InterfaceRate, LiveUsage, RecordingSink};
use netmeter_lib::api::ipc::{
    read_frame, write_frame, DaemonStatus, LiveInterface, LiveSnapshot, Request, Response,
};
use netmeter_lib::core::types::{DataRate, InterfaceKind, Traffic, UsageSummary};
use netmeter_lib::monitor::EngineShared;
use netmeter_lib::system::state::{AppState, DynSink};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;

fn rate(name: &str, rx: u64, included: bool) -> InterfaceRate {
    InterfaceRate {
        name: name.into(),
        rate: DataRate::UNKNOWN,
        traffic: Traffic::new(rx, 0),
        included,
    }
}

fn snapshot() -> LiveSnapshot {
    let mut s = LiveSnapshot::of(&EngineShared::default());
    s.interfaces = vec![
        LiveInterface {
            name: "wlp7s0".into(),
            kind: InterfaceKind::Wifi,
            mac: None,
        },
        LiveInterface {
            name: "docker0".into(),
            kind: InterfaceKind::Bridge,
            mac: None,
        },
    ];
    // Flags as a daemon with some other policy would have set them: wrong for
    // this user, so the GUI has to redo them.
    s.live = Some(LiveUsage {
        sampled_at_utc_ms: 1_789_000_000_000,
        interval_ms: Some(1_000),
        total: DataRate::UNKNOWN,
        by_interface: vec![rate("wlp7s0", 1_000, false), rate("docker0", 500, true)],
        pending_today: UsageSummary::ZERO,
        time_anomaly: false,
    });
    s
}

fn fake_daemon(socket: PathBuf, db: PathBuf) {
    let listener = UnixListener::bind(&socket).expect("bind");
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let response = match read_frame(&mut stream) {
                Ok(Request::Live) => Response::Live(Box::new(snapshot())),
                Ok(_) => Response::Status(DaemonStatus {
                    version: "fake".into(),
                    probes_attached: 6,
                    probes_expected: 6,
                    started_at_utc_ms: 0,
                    last_flush_utc_ms: None,
                    dropped: Default::default(),
                    interface_db: Some(db.display().to_string()),
                }),
                Err(_) => continue,
            };
            let _ = write_frame(&mut stream, &response);
        }
    });
}

#[test]
fn the_gui_reads_from_the_daemon_while_it_answers_and_samples_locally_after() {
    let dir = tempfile::tempdir().expect("dir");
    let socket = dir.path().join("netmeterd.sock");
    let daemon_db = dir.path().join("daemon.db");
    // SAFETY: set before any thread of this test binary reads it.
    unsafe { std::env::set_var("NETMETERD_SOCKET", &socket) };
    fake_daemon(socket.clone(), daemon_db.clone());

    let sink = RecordingSink::default();
    let state = AppState::new(
        &dir.path().join("data"),
        &dir.path().join("config"),
        DynSink::new(sink.clone()),
    )
    .expect("state");
    let local_db = state.db_path();

    state.follow_daemon();
    assert_eq!(state.db_path(), daemon_db, "history comes from the daemon");
    assert!(state.engine_shared().is_none(), "no second sampler");

    let live = state.live_rates();
    assert_eq!(
        live.total.rx_bytes_per_sec,
        Some(1_000.0),
        "the user's policy counts the NIC and not the bridge"
    );
    assert!(live.by_interface.iter().all(|i| i.included == (i.name == "wlp7s0")));

    state.follow_daemon();
    assert_eq!(
        sink.usage.lock().unwrap().len(),
        1,
        "one event per daemon sample, not per poll"
    );

    std::fs::remove_file(&socket).expect("daemon goes away");
    state.follow_daemon();
    assert_eq!(state.db_path(), local_db, "back to the GUI's own history");
    assert!(state.engine_shared().is_some(), "local sampling resumed");
    state.shutdown();
}
