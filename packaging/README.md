# Packaging

`netmeterd.service` and `install-helper.sh` install the per-application
accounting helper. Everything else in NetMeter runs unprivileged and needs
none of this.

    cargo build --release --manifest-path netmeterd/Cargo.toml
    sudo packaging/install-helper.sh

Undo it with `sudo packaging/install-helper.sh --uninstall`.

The GUI does not install the helper itself yet; it detects whether the socket
is there and says so. Running this from the settings screen through `pkexec`,
so the escalation is one polkit prompt rather than a terminal, waits on the
app being packaged — there is no installed path to the script before then.

## What it grants

`CAP_BPF` and `CAP_PERFMON`, and nothing else — no setuid binary, no sudoers
entry. systemd owns `/var/lib/netmeter` (the database) and
`/run/netmeter` (the socket). The unit denies all IP address families, so the
service that counts everyone else's traffic cannot open a network connection
of its own.

Why any of this is necessary, and what it cannot measure, is in
[../docs/PER_APP.md](../docs/PER_APP.md).
