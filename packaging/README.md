# Packaging

## Arch Linux

    cd packaging
    makepkg -si

`netmeter-git` builds the latest commit on `main` from GitHub, so push first:
uncommitted or unpushed work is not in it. The AUR repository holds
`PKGBUILD`, `.SRCINFO` and `netmeter.install` from this directory; after
changing the PKGBUILD, regenerate `.SRCINFO` with
`makepkg --printsrcinfo > .SRCINFO`.

The package enables and starts `netmeterd.service`: the daemon does all measuring and recording, from boot, whether or not anyone is logged
in, and the GUI (`netmeter`, in the application menu) only displays it.
Without the daemon the GUI falls back to sampling for itself while open.

A helper installed by hand earlier has to go first, or pacman refuses the
file conflict and the unit in `/etc` shadows the packaged one:

    sudo packaging/install-helper.sh --uninstall

History in `/var/lib/netmeter` survives both that and `pacman -R`.

## Without a package

`netmeterd.service` and `install-helper.sh` install the daemon by hand:

    cargo build --release --manifest-path netmeterd/Cargo.toml
    sudo packaging/install-helper.sh

Undo it with `sudo packaging/install-helper.sh --uninstall`. Installed by the
package, the same script (run by the GUI through `pkexec`) only enables or
disables the service.

## What it grants

`CAP_BPF` and `CAP_PERFMON`, and nothing else — no setuid binary, no sudoers
entry. systemd owns `/var/lib/netmeter` (the database) and
`/run/netmeter` (the socket). The unit denies all IP address families, so the
service that counts everyone else's traffic cannot open a network connection
of its own.

Why any of this is necessary, and what it cannot measure, is in
[../docs/PER_APP.md](../docs/PER_APP.md).

## Building the app

    npm run tauri build

Produces a `.deb` and an `.rpm` under `src-tauri/target/release/bundle/`.
They carry the GUI only; on Arch use the `PKGBUILD` above.

AppImage is deliberately not a target. Building one on a current Arch host
fails twice over: linuxdeploy bundles a 2024 `strip` that cannot read the
`.relr.dyn` sections modern binutils emit, and Tauri's GTK plugin expects
`/usr/lib/gdk-pixbuf-2.0/2.10.0`, a path gdk-pixbuf 2.44 no longer ships.
Neither is NetMeter's to fix, and an AppImage linked against a bleeding-edge
glibc would not run on the older systems AppImages exist to serve. If one is
ever wanted, build it in a container on an old base image, which is how
portable AppImages are made anyway.
