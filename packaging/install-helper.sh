#!/bin/sh
# Install the per-application accounting helper as a system service.
#
# Run with sudo (or pkexec):
#
#     sudo packaging/install-helper.sh [path-to-netmeterd]
#
# Undo with:  sudo packaging/install-helper.sh --uninstall
#
# No setuid binary and no root shell: the service gets CAP_BPF and
# CAP_PERFMON from systemd and nothing else. See docs/PER_APP.md.
set -eu

UNIT=/etc/systemd/system/netmeterd.service
LIBDIR=/usr/lib/netmeter
HERE=$(cd "$(dirname "$0")" && pwd)

if [ "$(id -u)" -ne 0 ]; then
    echo "This installs a system service and must run as root." >&2
    exit 1
fi

# Run from an installed package: the binary and the unit belong to pacman,
# so the only thing left to do is switch the service on or off.
if [ "$HERE" = "$LIBDIR" ] && [ -f /usr/lib/systemd/system/netmeterd.service ]; then
    if [ "${1:-}" = "--uninstall" ]; then
        systemctl disable --now netmeterd.service
    else
        systemctl enable netmeterd.service
        systemctl restart netmeterd.service
    fi
    exit 0
fi

if [ "${1:-}" = "--uninstall" ]; then
    systemctl disable --now netmeterd.service 2>/dev/null || true
    rm -f "$UNIT" "$LIBDIR/netmeterd"
    rmdir "$LIBDIR" 2>/dev/null || true
    systemctl daemon-reload
    echo "Removed. /var/lib/netmeter is left in place; delete it to discard history."
    exit 0
fi

BINARY=${1:-$HERE/../netmeterd/target/release/netmeterd}
if [ ! -x "$BINARY" ]; then
    echo "No netmeterd binary at $BINARY" >&2
    echo "Build it first:  cargo build --release --manifest-path netmeterd/Cargo.toml" >&2
    exit 1
fi

# A kernel with unprivileged BPF disabled is the normal case and is fine --
# the service is privileged. A kernel without BTF is not: the probes are
# CO-RE and will not load.
if [ ! -r /sys/kernel/btf/vmlinux ]; then
    echo "This kernel has no BTF (/sys/kernel/btf/vmlinux), so the probes cannot load." >&2
    exit 1
fi

install -d -m 0755 "$LIBDIR"
install -m 0755 "$BINARY" "$LIBDIR/netmeterd"
install -m 0644 "$HERE/netmeterd.service" "$UNIT"

systemctl daemon-reload
systemctl enable netmeterd.service
# restart, not `enable --now`: on a machine where the service is already
# running, `--now` leaves the old binary in place and the upgrade does
# nothing at all.
systemctl restart netmeterd.service

echo
echo "Installed. Check it with:"
echo "    systemctl status netmeterd"
echo "    journalctl -u netmeterd -n 20"
