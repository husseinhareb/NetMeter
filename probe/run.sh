#!/bin/bash
# Which kernel send/receive paths would a per-app BPF probe on sock_sendmsg /
# sock_recvmsg actually see? Uses ftrace, so nothing is compiled or loaded.
# Read-only: no counters are altered, no traffic is generated beyond loopback.
set -u
T=/sys/kernel/tracing
P="$(cd "$(dirname "$0")" && pwd)"
FUNCS="sock_sendmsg sock_recvmsg tcp_sendmsg tcp_cleanup_rbuf udp_sendmsg udp_recvmsg skb_consume_udp"

[ -w $T/current_tracer ] || { echo "run me as root"; exit 1; }

cleanup() {
  echo nop      > $T/current_tracer 2>/dev/null
  echo          > $T/set_ftrace_filter 2>/dev/null
  echo          > $T/set_ftrace_pid 2>/dev/null
  echo 0        > $T/options/function-fork 2>/dev/null
  echo 1408     > $T/buffer_size_kb 2>/dev/null
}
trap cleanup EXIT

echo "=== attach points known to this kernel ($(uname -r)) ==="
for f in $FUNCS; do
  grep -qx "$f" $T/available_filter_functions && echo "  yes  $f" || echo "  NO   $f"
done

echo 8192 > $T/buffer_size_kb
echo > $T/set_ftrace_filter
for f in $FUNCS; do echo "$f" >> $T/set_ftrace_filter; done
echo function > $T/current_tracer
echo 1 > $T/options/function-fork

case_run() {
  local name="$1"; shift
  echo > $T/trace
  echo 1 > $T/tracing_on
  local start=$(date +%s%3N)
  # The subshell puts its own pid in the filter, then becomes the workload,
  # so every traced hit belongs to this case and nothing else on the machine.
  bash -c "echo \$\$ > $T/set_ftrace_pid; exec $*" >/dev/null 2>&1
  local ms=$(( $(date +%s%3N) - start ))
  echo 0 > $T/tracing_on
  echo
  echo "--- $name  (${ms} ms for 64 MiB) ---"
  grep -oP '(?<=: )\w+ <-\w+' $T/trace | sort | uniq -c | sort -rn | head -12
  echo > $T/set_ftrace_pid
}

case_run "write() / sendall"  "python3 $P/w.py write"
case_run "sendfile()"         "python3 $P/w.py sendfile"
case_run "UDP send/recv"      "python3 $P/w.py udp"
case_run "io_uring send/recv" "$P/uringsend"

echo
echo "=== cost of tracing these 7 functions ==="
echo 0 > $T/tracing_on
u=$( { time -p python3 $P/w.py write; } 2>&1 | awk '/^real/{print $2}')
echo 1 > $T/tracing_on
echo $$ > $T/set_ftrace_pid
t=$( { time -p python3 $P/w.py write; } 2>&1 | awk '/^real/{print $2}')
echo 0 > $T/tracing_on
echo "  64 MiB untraced: ${u}s   traced: ${t}s"
echo
echo "(ftrace function tracing is heavier than a BPF fexit hook; treat this as a ceiling)"
