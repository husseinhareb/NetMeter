//! The wire total, for comparison only.
//!
//! ponytail: duplicates the GUI's `/proc/net/dev` reader in ~30 lines rather
//! than depending on that crate, which would drag Tauri into a privileged
//! process. When persistence lands, `core` and `monitor` get factored into a
//! shared crate and this file goes away.

use std::fs;

/// Sum of RX/TX on interfaces that have a real device behind them, which is
/// the same rule `docs/ACCOUNTING.md` uses for the GUI's headline number.
pub fn physical_totals() -> (u64, u64) {
    let Ok(raw) = fs::read_to_string("/proc/net/dev") else {
        return (0, 0);
    };

    let mut rx = 0u64;
    let mut tx = 0u64;
    for line in raw.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if !fs::metadata(format!("/sys/class/net/{name}/device")).is_ok() {
            continue;
        }
        let mut f = rest.split_whitespace();
        let (Some(r), Some(t)) = (f.next(), f.nth(7)) else {
            continue;
        };
        rx += r.parse::<u64>().unwrap_or(0);
        tx += t.parse::<u64>().unwrap_or(0);
    }
    (rx, tx)
}
