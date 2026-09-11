# Interface accounting

Summing every interface in `/proc/net/dev` does not produce a total of
anything. On the development machine it produces a number roughly **three times
larger than reality**, because the same bytes appear on several interfaces.

NetMeter therefore reports two different numbers, and only one of them is a
total:

| | meaning |
|---|---|
| **`included`** | Bytes on the interfaces the user's policy counts. **This is the usage total.** Each byte appears once. |
| **`observed`** | Bytes on every interface NetMeter recorded. Larger than reality by construction. A diagnostic, never a headline. |

## Why the naive sum is wrong

A container downloading 1 MB from the internet increments, on this machine:

```
vethed62bc1   +1 MB   (the container's end of the veth pair)
br-ee189c1b10a3 +1 MB (the bridge it is attached to)
wlp7s0        +1 MB   (the NIC the NAT'd packet actually leaves on)
```

One megabyte left the machine. A naive sum reports three. Add a VPN and it
reports four, because the tunnel counts the payload and the NIC counts the
encrypted carrier.

Loopback makes it worse: `lo` on this machine sits at 1.08 GB, which is more
than the entire Tailscale interface, and represents exactly zero network usage.

## The rule

**Physical is something NetMeter proves, never something it falls back to.**

The `device` symlink in `/sys/class/net/<name>/` points at the entry in the
device tree backing the interface. It exists for real hardware and for nothing
else. So:

```
no `device` symlink  ->  virtual  ->  excluded from the total, whatever else it looks like
`device` + `wireless/` -> WiFi
`device`, otherwise    -> Ethernet (or WWAN, by DEVTYPE)
```

Inverting the test this way is what makes in-kernel WireGuard safe. `wg0` has
no `wireless/`, no `bridge/`, no `brport`, no `master`, no `tun_flags` (that
file comes from the TUN driver; WireGuard registers its own rtnl link type),
and its `type` is 65534 rather than the loopback 772. Under a "physical unless
proven otherwise" classifier it would be counted alongside the NIC carrying its
own encrypted packets -- double-counting the entire tunnel. Under this rule it
is virtual, because it has no `device` link, and an unrecognised future tunnel
type will be too.

## Per-class treatment

| Class | Detected by | Where its bytes also appear | Default |
|---|---|---|---|
| **Ethernet** | `device`, no `wireless/` | nowhere -- this is the wire | **counted** |
| **WiFi** | `device` + `wireless/` or `DEVTYPE=wlan` | nowhere | **counted** |
| **WWAN / mobile** | `device` + `DEVTYPE=wwan`, or `wwan*`/`wwp*`/`rmnet*` name | nowhere | **counted** |
| **USB tether / RNDIS** | `device` present | nowhere -- it really is the uplink, and usually the metered one | **counted** (as Ethernet) |
| **Loopback** | `type == 772` | never left the machine | excluded |
| **VPN: TUN/TAP** (OpenVPN) | `tun_flags` | payload re-counted on the carrier NIC | excluded |
| **VPN: WireGuard** | no `device`, `type == 65534` | same | excluded |
| **VPN: Tailscale** | `tun_flags`, `type == 65534` | same | excluded |
| **PPP** | `ppp*` name | same | excluded |
| **Bridge** (`docker0`, `br-*`, `virbr0`) | `bridge/` or `DEVTYPE=bridge` | traffic to the internet is re-counted on the NIC | excluded |
| **Bridge leg / veth / bond slave** | `brport` or `master` | counted again on its master *and* on the NIC | excluded, **and not persisted** |
| **VLAN** (`eth0.100`) | `DEVTYPE=vlan`, or `iflink != ifindex` with a dotted name | every frame also traverses the parent NIC | excluded |
| **Anything else** | -- | unknown | excluded |

### Container traffic, precisely

* **Container to internet**: appears on the veth, the bridge and the NIC. The
  NIC's count is the truthful one, and it is the one NetMeter reports.
* **Container to container** on the same bridge: appears on the veths and the
  bridge, and *never* touches the NIC. It is therefore **not** in the usage
  total -- correctly, because it never left the machine, exactly like loopback.

### Bridge legs are not persisted

Bridge legs are dropped at the storage boundary rather than merely excluded from
the total. Docker assigns a fresh random name on every container start -- there
are four live on this machine right now -- so persisting them would add
permanent rows, without bound, to a table kept forever. They stay visible in
`get_interfaces` and in live rates; they simply never reach disk.

## When the default under-counts

The default counts the physical uplink, so it is right for a data cap on a
mobile hotspot, a home connection, or a tethered phone. It under-reports in one
case: **traffic that leaves over an interface NetMeter cannot see as physical**
-- for example a corporate VPN that is itself the metered link while the
underlying carrier is unmetered. That is what the escape hatch is for.

## The escape hatch

```jsonc
{
  "interface_policy": "manual",       // physical_only (default) | manual | all_except
  "included_interfaces": ["wg0"],     // globs; only consulted by "manual"
  "excluded_interfaces": ["lo"]       // globs; applied under every policy
}
```

`excluded_interfaces` always wins, under every policy, so it is a rule the user
can rely on rather than a hint. `all_except` still refuses loopback, because
those bytes never touched a network.

Because usage is stored **per interface**, changing this policy re-interprets
all existing history rather than only affecting future measurements. Storing
only a pre-summed billable total would have baked today's defaults in
permanently.
