//! The check on the loopback filter. Needs root, because it loads the probes.
//!
//!     cargo build --example loopback && sudo ./target/debug/examples/loopback
//!
//! Two transfers of the same size over the same kernel path, differing only in
//! the address the socket carries:
//!
//!   * to 127.0.0.1 -- bytes that never reach a NIC, and must not be counted;
//!   * to this host's own LAN address -- the negative control, which proves
//!     the probes still count an ordinary socket rather than having been
//!     silenced altogether.
//!
//! The second case also names the filter's edge: those bytes are routed over
//! `lo` too, and they are counted. The rule is the address, not the route,
//! which is the same rule `docs/ACCOUNTING.md` applies to the interface total.

use libbpf_rs::skel::{OpenSkel, SkelBuilder};
use libbpf_rs::{MapCore, MapFlags};
use netmeterd::skel;
use std::io::{Read, Write};
use std::mem::MaybeUninit;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};

const MIB: usize = 1 << 20;
const PAYLOAD: usize = 64 * MIB;
/// Ample room for the incidental traffic of a running desktop under the same
/// pid, tight enough that 64 MiB leaking through is unmissable.
const SLACK: u64 = 4 * MIB as u64;

fn main() {
    let mut open_object = MaybeUninit::uninit();
    let open = skel::NetmeterdSkelBuilder::default()
        .open(&mut open_object)
        .expect("open the bpf object");
    let skel = match open.load() {
        Ok(s) => s,
        Err(e) if e.kind() == libbpf_rs::ErrorKind::PermissionDenied => {
            eprintln!("loading BPF needs root: sudo ./target/debug/examples/loopback");
            std::process::exit(1);
        }
        Err(e) => panic!("load the bpf program: {e}"),
    };

    let _links = [
        skel.progs.tcp_send.attach().expect("attach tcp_sendmsg"),
        skel.progs.tcp_recv.attach().expect("attach tcp_recvmsg"),
    ];

    // Both ends of each transfer live in this process, so a counted one lands
    // at roughly twice PAYLOAD -- the client's send and the server's receive.
    let counted_loopback = transfer_and_count(&skel, Ipv4Addr::LOCALHOST.into());
    println!("{:<16} {PAYLOAD} bytes moved, {counted_loopback} counted", Ipv4Addr::LOCALHOST);

    let host = host_address().expect("this host has a non-loopback address");
    let counted_host = transfer_and_count(&skel, host);
    println!("{host:<16} {PAYLOAD} bytes moved, {counted_host} counted");

    assert!(
        counted_loopback < SLACK,
        "loopback traffic was counted: {counted_loopback} bytes attributed for a \
         transfer that never reached a NIC"
    );
    assert!(
        counted_host > PAYLOAD as u64,
        "an ordinary socket was not counted: {counted_host} bytes attributed for a \
         {PAYLOAD} byte transfer. The filter is rejecting more than loopback."
    );
    println!("ok");
}

/// Moves PAYLOAD over a socket bound to `addr`, both ways, and returns what
/// the probes attributed to this process while it happened.
fn transfer_and_count(skel: &skel::NetmeterdSkel, addr: IpAddr) -> u64 {
    drain(skel);

    let listener = TcpListener::bind(SocketAddr::new(addr, 0)).expect("bind");
    let target = listener.local_addr().expect("local addr");
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        let mut sink = vec![0u8; 256 * 1024];
        let mut seen = 0;
        while seen < PAYLOAD {
            match sock.read(&mut sink) {
                Ok(0) | Err(_) => break,
                Ok(n) => seen += n,
            }
        }
        sock.write_all(&vec![0u8; MIB]).expect("reply");
    });

    let mut client = TcpStream::connect(target).expect("connect");
    client.write_all(&vec![0u8; PAYLOAD]).expect("send");
    let mut reply = Vec::new();
    client.read_to_end(&mut reply).expect("receive");
    server.join().expect("server thread");

    drain(skel)
}

/// Empties the counter map and returns the bytes it held for this pid. The
/// daemon drains the same way, so a key left behind would show up here too.
fn drain(skel: &skel::NetmeterdSkel) -> u64 {
    let me = std::process::id();
    let map = &skel.maps.counters;
    let mut mine = 0u64;
    for key in map.keys() {
        if let Ok(Some(value)) = map.lookup(&key, MapFlags::ANY) {
            let tgid = u32::from_le_bytes(key[0..4].try_into().unwrap());
            if tgid == me {
                let rx = u64::from_le_bytes(value[0..8].try_into().unwrap());
                let tx = u64::from_le_bytes(value[8..16].try_into().unwrap());
                mine += rx + tx;
            }
        }
        let _ = map.delete(&key);
    }
    mine
}

/// This host's own address, found the way every program does: ask the routing
/// table where a packet to a public address would leave from. A UDP connect
/// sends nothing, so this works with the network unplugged.
fn host_address() -> Option<IpAddr> {
    let probe = UdpSocket::bind("0.0.0.0:0").ok()?;
    probe.connect("8.8.8.8:53").ok()?;
    let addr = probe.local_addr().ok()?.ip();
    (!addr.is_loopback() && !addr.is_unspecified()).then_some(addr)
}
