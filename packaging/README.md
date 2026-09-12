# Packaging

`netmeterd.service` and `install-helper.sh` install the per-application
accounting helper. Everything else in NetMeter runs unprivileged and needs
none of this.

    cargo build --release --manifest-path netmeterd/Cargo.toml
    sudo packaging/install-helper.sh

The GUI runs the same script through `pkexec` when you enable per-application
usage from its settings, so the escalation is one polkit prompt rather than a
root shell. Undo it with `sudo packaging/install-helper.sh --uninstall`.

## What it grants

`CAP_BPF` and `CAP_PERFMON`, and nothing else — no setuid binary, no sudoers
entry. systemd owns `/var/lib/netmeter` (the database) and
`/run/netmeter` (the socket). The unit denies all IP address families, so the
service that counts everyone else's traffic cannot open a network connection
of its own.

Why any of this is necessary, and what it cannot measure, is in
[../docs/PER_APP.md](../docs/PER_APP.md).
