//! Interface classification from `/sys/class/net`.
//!
//! The rule that drives everything here: **physical is something we prove, not
//! something we fall back to.** Classifying an unrecognized interface as
//! physical would put it in the default usage total, and the most likely
//! unrecognized interface is a new kind of tunnel -- exactly the thing that
//! must not be counted twice.
//!
//! In-kernel WireGuard is the case that makes this concrete. `wg0` has no
//! `wireless/` directory, no `bridge/`, no `brport`, no `master`, no
//! `tun_flags` (that file comes from the TUN driver, and WireGuard registers
//! its own rtnl link type), and `type` is 65534 rather than the loopback 772.
//! Under a "physical by default" classifier it would be counted alongside the
//! NIC carrying its encrypted packets -- double-counting the whole tunnel.
//!
//! The `device` symlink is the signal that settles it: it points at the entry
//! in the device tree that backs the interface, so it exists for real hardware
//! and for nothing else.

use crate::core::types::{InterfaceKind, InterfaceState};
use std::path::Path;

/// Where the kernel exposes per-interface attributes.
pub const SYS_CLASS_NET: &str = "/sys/class/net";

/// `ARPHRD_LOOPBACK` from `<linux/if_arp.h>`.
const ARPHRD_LOOPBACK: u32 = 772;

/// Facts read from sysfs for one interface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SysfsFacts {
    /// A `device` symlink exists: the interface is backed by real hardware.
    pub has_device: bool,
    /// A `wireless` directory exists, or `DEVTYPE=wlan`.
    pub is_wireless: bool,
    /// A `bridge` directory exists, or `DEVTYPE=bridge`.
    pub is_bridge: bool,
    /// `brport` or `master` exists: this interface is a leg of a bridge or bond.
    pub is_enslaved: bool,
    /// `tun_flags` exists: created by the TUN/TAP driver.
    pub is_tun: bool,
    /// `DEVTYPE` from `uevent`, if present.
    pub devtype: Option<String>,
    /// `type`: the ARPHRD_* link-layer type.
    pub arp_type: Option<u32>,
    pub ifindex: Option<u32>,
    /// `iflink` differs from `ifindex` for a stacked device (VLAN, macvlan).
    pub iflink: Option<u32>,
    pub mac: Option<String>,
    pub operstate: InterfaceState,
}

/// Classify an interface from its sysfs facts and its name.
///
/// Order matters: each rule below is checked before the ones that would
/// otherwise also match.
pub fn classify(name: &str, f: &SysfsFacts) -> InterfaceKind {
    // Loopback first: it is backed by no device, so the virtual branch would
    // otherwise have to special-case it anyway.
    if f.arp_type == Some(ARPHRD_LOOPBACK) || name == "lo" {
        return InterfaceKind::Loopback;
    }

    // --- Physical: must be proven. ---
    if f.has_device {
        if f.is_wireless || f.devtype.as_deref() == Some("wlan") {
            return InterfaceKind::Wifi;
        }
        if matches!(f.devtype.as_deref(), Some("wwan" | "wwan_ctrl"))
            || is_wwan_name(name)
        {
            return InterfaceKind::Wwan;
        }
        // A USB tether or RNDIS phone also lands here, and correctly so: that
        // traffic really does leave the machine exactly once, over this
        // interface, and it is very often the metered connection.
        return InterfaceKind::Ethernet;
    }

    // --- Everything without a device link is virtual. Label it usefully. ---

    // Enslaved before bridge: a veth end has `brport`, and we want to say
    // "bridge leg", not "bridge".
    if f.is_enslaved {
        return InterfaceKind::Enslaved;
    }
    if f.is_bridge || f.devtype.as_deref() == Some("bridge") {
        return InterfaceKind::Bridge;
    }
    // A stacked device: `iflink` naming a different interface means frames are
    // handed to a parent, which also counts them.
    if f.devtype.as_deref() == Some("vlan")
        || (matches!((f.ifindex, f.iflink), (Some(a), Some(b)) if a != b)
            && name.contains('.'))
    {
        return InterfaceKind::Vlan;
    }
    if f.is_tun || is_vpn_name(name) {
        return InterfaceKind::Vpn;
    }
    // WireGuard and other rtnl link types register with no link-layer address
    // (ARPHRD_NONE, 65534). Combined with "no device link", that is a tunnel.
    if f.arp_type == Some(65534) {
        return InterfaceKind::Vpn;
    }

    // Unknown and virtual: excluded from the usage total, which is the safe
    // direction.
    InterfaceKind::Virtual
}

/// Name-based fallbacks, used only after the sysfs signals have been exhausted.
/// Names are a hint, never the primary evidence.
fn is_vpn_name(name: &str) -> bool {
    const PREFIXES: [&str; 7] = ["tun", "tap", "wg", "ppp", "ipsec", "nordlynx", "proton"];
    PREFIXES.iter().any(|p| name.starts_with(p)) || name.starts_with("tailscale")
}

fn is_wwan_name(name: &str) -> bool {
    const PREFIXES: [&str; 4] = ["wwan", "wwp", "rmnet", "qmimux"];
    PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Read the sysfs attributes of one interface.
///
/// Every read is individually fallible and individually ignored on failure: an
/// interface can be torn down between the `readdir` and these reads, and that
/// must degrade the classification, not the tick.
pub fn read_facts(root: &Path, name: &str) -> SysfsFacts {
    let dir = root.join(name);
    let exists = |f: &str| dir.join(f).exists();
    let read = |f: &str| -> Option<String> {
        std::fs::read_to_string(dir.join(f))
            .ok()
            .map(|s| s.trim().to_string())
    };

    let uevent = read("uevent").unwrap_or_default();
    let devtype = uevent.lines().find_map(|l| {
        l.strip_prefix("DEVTYPE=")
            .map(|v| v.trim().to_ascii_lowercase())
    });

    let mac = read("address").filter(|m| {
        // `tailscale0` has an empty address file; bridges report all-zero
        // before a member joins. Neither is a usable address.
        !m.is_empty() && m != "00:00:00:00:00:00"
    });

    SysfsFacts {
        has_device: exists("device"),
        is_wireless: exists("wireless") || exists("phy80211"),
        is_bridge: exists("bridge"),
        is_enslaved: exists("brport") || exists("master"),
        is_tun: exists("tun_flags"),
        devtype,
        arp_type: read("type").and_then(|v| v.parse().ok()),
        ifindex: read("ifindex").and_then(|v| v.parse().ok()),
        iflink: read("iflink").and_then(|v| v.parse().ok()),
        mac,
        operstate: read("operstate")
            .map(|s| InterfaceState::parse(&s))
            .unwrap_or(InterfaceState::Unknown),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Facts as observed on the development machine, so these tests pin real
    /// kernel output rather than an imagined shape.
    fn wifi() -> SysfsFacts {
        SysfsFacts {
            has_device: true,
            is_wireless: true,
            devtype: Some("wlan".into()),
            arp_type: Some(1),
            ifindex: Some(3),
            iflink: Some(3),
            mac: Some("50:ee:32:aa:7b:60".into()),
            operstate: InterfaceState::Up,
            ..Default::default()
        }
    }

    fn ethernet() -> SysfsFacts {
        SysfsFacts {
            has_device: true,
            arp_type: Some(1),
            ifindex: Some(2),
            iflink: Some(2),
            mac: Some("30:56:0f:28:53:ea".into()),
            operstate: InterfaceState::Down,
            ..Default::default()
        }
    }

    #[test]
    fn physical_interfaces_are_recognized() {
        assert_eq!(classify("wlp7s0", &wifi()), InterfaceKind::Wifi);
        assert_eq!(classify("enp8s0", &ethernet()), InterfaceKind::Ethernet);
    }

    #[test]
    fn a_down_ethernet_port_is_still_ethernet() {
        // enp8s0 on this machine is down with zero counters. Its kind must not
        // depend on link state, or it would flip category on every cable plug.
        let mut f = ethernet();
        f.operstate = InterfaceState::Down;
        assert_eq!(classify("enp8s0", &f), InterfaceKind::Ethernet);
    }

    #[test]
    fn loopback_is_recognized_by_arp_type() {
        let f = SysfsFacts {
            arp_type: Some(772),
            ifindex: Some(1),
            operstate: InterfaceState::Unknown,
            ..Default::default()
        };
        assert_eq!(classify("lo", &f), InterfaceKind::Loopback);
        // Even if somebody renames it, the ARPHRD type still decides.
        assert_eq!(classify("loop-renamed", &f), InterfaceKind::Loopback);
    }

    #[test]
    fn tailscale_tun_is_a_vpn() {
        // Real values from this machine: ARPHRD_NONE, tun_flags, no MAC.
        let f = SysfsFacts {
            is_tun: true,
            arp_type: Some(65534),
            ifindex: Some(4),
            iflink: Some(4),
            mac: None,
            operstate: InterfaceState::Unknown,
            ..Default::default()
        };
        assert_eq!(classify("tailscale0", &f), InterfaceKind::Vpn);
    }

    #[test]
    fn kernel_wireguard_is_a_vpn_despite_having_no_tun_flags() {
        // The regression this classifier exists for: in-kernel wireguard
        // exposes none of the usual virtual markers. If it ever classified as
        // physical, its payload would be counted twice.
        let f = SysfsFacts {
            has_device: false,
            is_wireless: false,
            is_tun: false,
            is_bridge: false,
            is_enslaved: false,
            devtype: None,
            arp_type: Some(65534),
            ifindex: Some(11),
            iflink: Some(11),
            mac: None,
            operstate: InterfaceState::Unknown,
        };
        assert_eq!(classify("wg0", &f), InterfaceKind::Vpn);
        // ...and with an unhelpful name it is still not physical.
        assert_eq!(classify("mytunnel", &f), InterfaceKind::Vpn);
    }

    #[test]
    fn bridges_and_their_legs_are_distinguished() {
        let bridge = SysfsFacts {
            is_bridge: true,
            devtype: Some("bridge".into()),
            arp_type: Some(1),
            ifindex: Some(5),
            mac: Some("da:df:a2:bf:7f:88".into()),
            operstate: InterfaceState::Up,
            ..Default::default()
        };
        assert_eq!(classify("br-ee189c1b10a3", &bridge), InterfaceKind::Bridge);
        assert_eq!(classify("docker0", &bridge), InterfaceKind::Bridge);

        let veth = SysfsFacts {
            is_enslaved: true,
            arp_type: Some(1),
            ifindex: Some(7),
            iflink: Some(6),
            mac: Some("ba:06:ce:ee:19:b1".into()),
            operstate: InterfaceState::Up,
            ..Default::default()
        };
        assert_eq!(classify("vethed62bc1", &veth), InterfaceKind::Enslaved);
    }

    #[test]
    fn vlan_subinterfaces_are_recognized() {
        let by_devtype = SysfsFacts {
            devtype: Some("vlan".into()),
            arp_type: Some(1),
            ifindex: Some(12),
            iflink: Some(2),
            ..Default::default()
        };
        assert_eq!(classify("eth0.100", &by_devtype), InterfaceKind::Vlan);

        let by_shape = SysfsFacts {
            arp_type: Some(1),
            ifindex: Some(12),
            iflink: Some(2),
            ..Default::default()
        };
        assert_eq!(classify("enp8s0.42", &by_shape), InterfaceKind::Vlan);
    }

    #[test]
    fn usb_tether_counts_as_physical_because_it_really_is_the_uplink() {
        let f = SysfsFacts {
            has_device: true,
            arp_type: Some(1),
            ifindex: Some(9),
            mac: Some("aa:bb:cc:dd:ee:ff".into()),
            operstate: InterfaceState::Up,
            ..Default::default()
        };
        assert_eq!(classify("enp0s20u1", &f), InterfaceKind::Ethernet);
    }

    #[test]
    fn mobile_broadband_is_classified_as_wwan() {
        let f = SysfsFacts {
            has_device: true,
            devtype: Some("wwan".into()),
            arp_type: Some(1),
            ifindex: Some(13),
            ..Default::default()
        };
        assert_eq!(classify("wwan0", &f), InterfaceKind::Wwan);
        assert_eq!(
            classify("wwp0s20f0u2", &SysfsFacts { has_device: true, ..Default::default() }),
            InterfaceKind::Wwan
        );
    }

    #[test]
    fn an_unknown_virtual_interface_is_never_counted_by_default() {
        // The safety property: whatever the kernel invents next, it does not
        // silently join the usage total.
        let f = SysfsFacts {
            arp_type: Some(999),
            ifindex: Some(42),
            ..Default::default()
        };
        let kind = classify("futurenet0", &f);
        assert_eq!(kind, InterfaceKind::Virtual);
        assert!(!kind.is_physical());
    }

    #[test]
    fn empty_and_zero_macs_are_not_reported_as_addresses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let iface = root.join("tailscale0");
        std::fs::create_dir_all(&iface).expect("mkdir");
        std::fs::write(iface.join("address"), "\n").expect("write");
        std::fs::write(iface.join("type"), "65534\n").expect("write");
        std::fs::write(iface.join("ifindex"), "4\n").expect("write");
        std::fs::write(iface.join("operstate"), "unknown\n").expect("write");
        std::fs::write(iface.join("tun_flags"), "0x1002\n").expect("write");

        let f = read_facts(root, "tailscale0");
        assert_eq!(f.mac, None);
        assert!(f.is_tun);
        assert_eq!(f.arp_type, Some(65534));
        assert_eq!(f.ifindex, Some(4));
        assert_eq!(classify("tailscale0", &f), InterfaceKind::Vpn);
    }

    #[test]
    fn missing_sysfs_files_degrade_instead_of_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Nothing exists at all: the facts come back empty and classification
        // still returns something safe.
        let f = read_facts(dir.path(), "ghost0");
        assert_eq!(f, SysfsFacts::default());
        assert_eq!(classify("ghost0", &f), InterfaceKind::Virtual);
    }

    #[test]
    fn uevent_devtype_is_parsed_from_a_multiline_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let iface = dir.path().join("docker0");
        std::fs::create_dir_all(&iface).expect("mkdir");
        std::fs::write(iface.join("uevent"), "DEVTYPE=bridge\nINTERFACE=docker0\nIFINDEX=6\n")
            .expect("write");
        let f = read_facts(dir.path(), "docker0");
        assert_eq!(f.devtype.as_deref(), Some("bridge"));
        assert_eq!(classify("docker0", &f), InterfaceKind::Bridge);
    }

    #[test]
    fn classification_on_the_real_machine_is_sane() {
        // Not an assertion about *this* machine's interfaces -- it asserts the
        // invariant that holds on every machine: nothing without a device link
        // is ever counted as physical.
        let root = Path::new(SYS_CLASS_NET);
        if !root.exists() {
            return;
        }
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let f = read_facts(root, &name);
            let kind = classify(&name, &f);
            if kind.is_physical() {
                assert!(
                    f.has_device,
                    "{name} classified physical without a device link"
                );
            }
        }
    }
}
