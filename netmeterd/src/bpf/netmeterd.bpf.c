// Per-application byte accounting.
//
// Attached to the protocol operations rather than the socket layer: those are
// reached through sk->sk_prot->sendmsg, so they cannot be inlined away and
// every syscall path -- send, sendmsg, sendfile, splice, io_uring -- has to
// funnel through them. See docs/PER_APP.md for the measurements behind that.
//
// No kernel struct is dereferenced here, so this needs no vmlinux.h and stays
// portable across kernel builds. Pointer arguments are declared void * for
// exactly that reason.

#include <linux/bpf.h>
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>

#define TASK_COMM_LEN 16

struct key {
    __u32 tgid;
    __u32 uid;
};

struct value {
    __u64 rx_bytes;
    __u64 tx_bytes;
    char comm[TASK_COMM_LEN];
};

// Userspace drains and deletes this every tick. `comm` is recorded on first
// sight so a process that exits before the next drain can still be named.
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 16384);
    __type(key, struct key);
    __type(value, struct value);
} counters SEC(".maps");

// Bytes we saw but could not store because the map was full. Printed by the
// daemon: a silent loss in a usage meter is worse than a visible one.
struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, 2);
    __type(key, __u32);
    __type(value, __u64);
} dropped SEC(".maps");

static __always_inline void note_drop(__u32 slot, __u64 n) {
    __u64 *d = bpf_map_lookup_elem(&dropped, &slot);
    if (d)
        __sync_fetch_and_add(d, n);
}

static __always_inline void account(long bytes, int is_rx) {
    if (bytes <= 0)
        return;

    __u64 id = bpf_get_current_pid_tgid();
    struct key k = {
        .tgid = id >> 32,
        .uid = (__u32)bpf_get_current_uid_gid(),
    };

    struct value *v = bpf_map_lookup_elem(&counters, &k);
    if (v) {
        if (is_rx)
            __sync_fetch_and_add(&v->rx_bytes, bytes);
        else
            __sync_fetch_and_add(&v->tx_bytes, bytes);
        return;
    }

    struct value init = {};
    if (is_rx)
        init.rx_bytes = bytes;
    else
        init.tx_bytes = bytes;
    bpf_get_current_comm(&init.comm, sizeof(init.comm));

    if (bpf_map_update_elem(&counters, &k, &init, BPF_NOEXIST))
        note_drop(is_rx ? 0 : 1, bytes);
}

// Send: the return value is the number of bytes accepted.
SEC("fexit/tcp_sendmsg")
int BPF_PROG(tcp_send, void *sk, void *msg, __u64 size, long ret) {
    account(ret, 0);
    return 0;
}

SEC("fexit/udp_sendmsg")
int BPF_PROG(udp_send, void *sk, void *msg, __u64 len, long ret) {
    account(ret, 0);
    return 0;
}

SEC("fexit/udpv6_sendmsg")
int BPF_PROG(udpv6_send, void *sk, void *msg, __u64 len, long ret) {
    account(ret, 0);
    return 0;
}

// Receive: tcp_cleanup_rbuf carries the bytes copied to userspace and fires on
// the splice path as well as recvmsg. It is called more than once per receive,
// so the count must come from `copied`, never from the number of calls.
SEC("fentry/tcp_cleanup_rbuf")
int BPF_PROG(tcp_recv, void *sk, int copied) {
    account(copied, 1);
    return 0;
}

SEC("fexit/udp_recvmsg")
int BPF_PROG(udp_recv, void *sk, void *msg, __u64 len, int flags, long ret) {
    account(ret, 1);
    return 0;
}

SEC("fexit/udpv6_recvmsg")
int BPF_PROG(udpv6_recv, void *sk, void *msg, __u64 len, int flags, long ret) {
    account(ret, 1);
    return 0;
}

char LICENSE[] SEC("license") = "GPL";
