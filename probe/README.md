# Attach-point probe

Answers one question before any eBPF code is written: which kernel functions
does a per-application accounting probe have to attach to in order to see every
send and receive path?

    sudo bash probe/run.sh

Uses ftrace only — nothing is compiled into the kernel, no BPF is loaded, and
the tracer is reset on exit. The workloads move 64 MiB over loopback as a
single traced pid, so the result is not polluted by whatever else is running.

Results and the conclusions drawn from them are in
[../docs/PER_APP.md](../docs/PER_APP.md). Re-run it on any machine whose kernel
build differs; inlining changes which symbols survive.
