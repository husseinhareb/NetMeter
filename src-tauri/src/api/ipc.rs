//! What the GUI and `netmeterd` say to each other.
//!
//! Per-application accounting needs a privileged helper (see
//! `docs/PER_APP.md`), so that data arrives over a local socket instead of
//! from this process. These types live here, in the crate both sides share,
//! rather than in the daemon: the GUI must not depend on a crate that pulls
//! in libbpf, and neither side may guess at the other's wire format.
//!
//! One JSON message per frame, length prefixed. `SOCK_STREAM` rather than the
//! `SOCK_SEQPACKET` first sketched: a year of daily rows for twenty
//! applications is over half a megabyte, which does not fit in a datagram,
//! and a length prefix costs four bytes.

use crate::core::types::Granularity;
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

/// Where the daemon listens. Under `/run`, so it disappears on reboot rather
/// than leaving a stale socket to connect to.
pub const SOCKET_PATH: &str = "/run/netmeter/netmeterd.sock";

/// Frames larger than this are refused rather than allocated.
pub const MAX_FRAME: u32 = 8 << 20;

/// How many probes a complete installation attaches. Fewer means some traffic
/// is not being counted, which the GUI must say out loud rather than imply
/// totality.
pub const PROBES_EXPECTED: usize = 6;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Per-application usage over a range of calendar keys, inclusive.
    AppUsage {
        granularity: Granularity,
        from: String,
        to: String,
    },
    /// Whether the helper is alive and what it knows.
    Status,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    AppUsage(AppUsage),
    Status(DaemonStatus),
    Error { kind: String, message: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppUsage {
    pub granularity: Granularity,
    /// The zone the day boundaries were computed in.
    pub timezone: String,
    /// One entry per bucket in the range, in order, zero-filled: a day the
    /// machine was off is a zero bucket, not a missing one.
    pub buckets: Vec<AppBucket>,
    /// Every application in the range, largest first.
    pub total: Vec<AppTraffic>,
    /// Wire bytes no application owned over the whole range -- headers and
    /// acknowledgements. Its own row, so the parts add up to what the
    /// interfaces actually moved instead of leaving a gap.
    pub overhead: Traffic,
    /// Earliest local date the daemon holds, or `None` if it holds nothing.
    pub data_since: Option<String>,
    /// Whose rows these are. Taken from the connection's peer credentials,
    /// never from the request.
    pub uid: u32,
    pub generated_at_utc_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppBucket {
    pub key: String,
    pub start_utc_ms: i64,
    pub end_utc_ms: i64,
    pub apps: Vec<AppTraffic>,
    /// Zero at hour granularity: overhead is measured per day, and splitting
    /// it across hours would be inventing a distribution.
    pub overhead: Traffic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppTraffic {
    pub app: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Traffic {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub version: String,
    pub probes_attached: usize,
    pub probes_expected: usize,
    pub started_at_utc_ms: i64,
    pub last_flush_utc_ms: Option<i64>,
    /// Bytes the kernel counted but could not store because the map was full.
    pub dropped: Traffic,
}

/// Write one length-prefixed JSON frame.
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, value: &T) -> io::Result<()> {
    let body = serde_json::to_vec(value).map_err(io::Error::other)?;
    let len: u32 = body
        .len()
        .try_into()
        .map_err(|_| io::Error::other("frame too large to encode"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

/// Read one length-prefixed JSON frame.
///
/// The length is checked against [`MAX_FRAME`] before anything is allocated,
/// so a hostile or corrupt header cannot ask for a gigabyte.
pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> io::Result<T> {
    let mut header = [0u8; 4];
    r.read_exact(&mut header)?;
    let len = u32::from_le_bytes(header);
    if len > MAX_FRAME {
        return Err(io::Error::other(format!("frame of {len} bytes refused")));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_survives_a_round_trip() {
        let req = Request::AppUsage {
            granularity: Granularity::Day,
            from: "2026-09-01".into(),
            to: "2026-09-12".into(),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &req).expect("write");
        let back: Request = read_frame(&mut buf.as_slice()).expect("read");
        assert_eq!(back, req);
    }

    #[test]
    fn an_oversized_header_is_refused_before_allocating() {
        let mut buf = (MAX_FRAME + 1).to_le_bytes().to_vec();
        buf.extend_from_slice(b"{}");
        let err = read_frame::<_, Request>(&mut buf.as_slice()).expect_err("must refuse");
        assert!(err.to_string().contains("refused"), "{err}");
    }

    #[test]
    fn a_truncated_frame_is_an_error_not_a_hang() {
        let mut buf = 100u32.to_le_bytes().to_vec();
        buf.extend_from_slice(b"{\"type\":\"status\"}");
        assert!(read_frame::<_, Request>(&mut buf.as_slice()).is_err());
    }
}
