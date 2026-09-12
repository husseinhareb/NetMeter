//! Serve an existing apps.db over a socket, without BPF or privileges.
//!
//! For frontend work: the GUI cannot tell this from the real helper, and it
//! needs no root. The real daemon is the one that measures.
//!
//!     cargo run --example serve -- /tmp/apps.db /tmp/netmeterd.sock

use netmeter_lib::api::ipc::{DaemonStatus, PROBES_EXPECTED};
use netmeterd::server::Server;
use std::sync::{Arc, Mutex};

fn main() {
    let mut args = std::env::args().skip(1);
    let db = args.next().expect("usage: serve <apps.db> <socket>");
    let socket = args.next().expect("usage: serve <apps.db> <socket>");

    let status = Arc::new(Mutex::new(DaemonStatus {
        version: format!("{}-stub", env!("CARGO_PKG_VERSION")),
        probes_attached: PROBES_EXPECTED,
        probes_expected: PROBES_EXPECTED,
        started_at_utc_ms: netmeter_lib::core::time::now_utc_ms(),
        last_flush_utc_ms: None,
        dropped: Default::default(),
    }));

    println!("serving {db} on {socket}");
    Server::new(
        socket.into(),
        db.into(),
        netmeter_lib::core::time::system_timezone(),
        status,
    )
    .run()
    .expect("serve");
}
