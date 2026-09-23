// Per-application byte accounting.
//
// Attached to the protocol operations rather than the socket layer: those are
// reached through sk->sk_prot->sendmsg, so they cannot be inlined away and
// every syscall path -- send, sendmsg, sendfile, splice, io_uring -- has to
// funnel through them. See docs/PER_APP.md for the measurements behind that.
//
// The socket is dereferenced for one purpose only: telling loopback apart from
// the wire. The fields come from partial struct declarations rather than
// vmlinux.h, so CO-RE relocates every offset against the running kernel and
// this file still needs no generated header. fexit already requires the
// kernel's BTF, so nothing new is asked of the kernel.

#include <linux/bpf.h>
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>
#include <bpf/bpf_core_read.h>
#include <bpf/bpf_endian.h>

#define TASK_COMM_LEN 16
#define AF_INET 2
#define AF_INET6 10

// Only the fields read below have to be named; CO-RE fixes up where they
// actually live. skc_daddr and skc_rcv_saddr sit inside an anonymous union in
// the kernel, which libbpf sees through when it matches by name.
//
// in6_addr is the exception that carries no preserve_access_index: it exists
// to give skc_v6_daddr a struct type, because CO-RE checks a field's kind
// against the kernel's and an array would not match. Its own member is never
// read -- with the attribute, touching it would emit a relocation for a name
// the kernel's in6_addr does not have (it keeps its bytes in `in6_u`), and
// the program is rejected at load with "invalid CO-RE relocation".
struct in6_addr {
    __u8 bytes[16];
};

struct sock_common {
    __u32 skc_daddr;
    __u32 skc_rcv_saddr;
    unsigned short skc_family;
    struct in6_addr skc_v6_daddr;
    struct in6_addr skc_v6_rcv_saddr;
} __attribute__((preserve_access_index));

struct sock {
    struct sock_common __sk_common;
} __attribute__((preserve_access_index));

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

// Loopback carries bytes that never reach a NIC. docs/ACCOUNTING.md excludes
// `lo` from the interface total for exactly that reason, and an attribution of
// that total has to apply the same rule or it reports more traffic than the
// machine moved -- on a box running local servers, several times more.
static __always_inline int v4_loopback(__u32 be_addr) {
    return (bpf_ntohl(be_addr) >> 24) == 127;
}

// ::1, and the v4-mapped ::ffff:127.0.0.0/8 a dual-stack listener sees.
static __always_inline int v6_loopback(const __u8 a[16]) {
    __u8 head = 0;
    for (int i = 0; i < 10; i++)
        head |= a[i];
    if (head)
        return 0;
    if (a[10] == 0xff && a[11] == 0xff)
        return a[12] == 127;
    if (a[10] | a[11] | a[12] | a[13] | a[14])
        return 0;
    return a[15] == 1;
}

// Either end being loopback settles it: a connected socket has both, and one
// bound to 127.0.0.1 is local whatever it is talking to.
//
// ponytail: an unconnected UDP socket bound to 0.0.0.0 has neither address,
// so datagrams it sends to 127.0.0.1 -- a stub resolver being the realistic
// case -- are still counted. Reading the destination out of the msghdr is the
// upgrade if that ever amounts to anything measurable.
static __always_inline int is_loopback(struct sock *sk) {
    if (!sk)
        return 0;

    unsigned short family = BPF_CORE_READ(sk, __sk_common.skc_family);
    if (family == AF_INET) {
        return v4_loopback(BPF_CORE_READ(sk, __sk_common.skc_daddr)) ||
               v4_loopback(BPF_CORE_READ(sk, __sk_common.skc_rcv_saddr));
    }
    if (family == AF_INET6) {
        __u8 daddr[16], saddr[16];
        BPF_CORE_READ_INTO(&daddr, sk, __sk_common.skc_v6_daddr);
        BPF_CORE_READ_INTO(&saddr, sk, __sk_common.skc_v6_rcv_saddr);
        return v6_loopback(daddr) || v6_loopback(saddr);
    }
    return 0;
}

static __always_inline void account(struct sock *sk, long bytes, int is_rx) {
    if (bytes <= 0 || is_loopback(sk))
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

    if (bpf_map_update_elem(&counters, &k, &init, BPF_NOEXIST)) {
        // Either the map is full, or another CPU created this key between our
        // lookup and here. Try once more before calling the bytes lost.
        v = bpf_map_lookup_elem(&counters, &k);
        if (v) {
            if (is_rx)
                __sync_fetch_and_add(&v->rx_bytes, bytes);
            else
                __sync_fetch_and_add(&v->tx_bytes, bytes);
        } else {
            note_drop(is_rx ? 0 : 1, bytes);
        }
    }
}

// Send: the return value is the number of bytes accepted.
SEC("fexit/tcp_sendmsg")
int BPF_PROG(tcp_send, struct sock *sk, void *msg, __u64 size, int ret) {
    account(sk, ret, 0);
    return 0;
}

SEC("fexit/udp_sendmsg")
int BPF_PROG(udp_send, struct sock *sk, void *msg, __u64 len, int ret) {
    account(sk, ret, 0);
    return 0;
}

SEC("fexit/udpv6_sendmsg")
int BPF_PROG(udpv6_send, struct sock *sk, void *msg, __u64 len, int ret) {
    account(sk, ret, 0);
    return 0;
}

// Receive: the return value is the bytes copied by this one call.
//
// tcp_cleanup_rbuf was measured first and rejected. It fires more than once
// per recvmsg -- 2498 calls for 1504 receives -- and its `copied` argument is
// that call's running total, not each fragment, so summing it inflates
// download quadratically. The cost of using tcp_recvmsg instead is the splice
// receive path, which lands in the unattributed remainder.
SEC("fexit/tcp_recvmsg")
int BPF_PROG(tcp_recv, struct sock *sk, void *msg, __u64 len, int flags, int ret) {
    account(sk, ret, 1);
    return 0;
}

SEC("fexit/udp_recvmsg")
int BPF_PROG(udp_recv, struct sock *sk, void *msg, __u64 len, int flags, int ret) {
    account(sk, ret, 1);
    return 0;
}

SEC("fexit/udpv6_recvmsg")
int BPF_PROG(udpv6_recv, struct sock *sk, void *msg, __u64 len, int flags, int ret) {
    account(sk, ret, 1);
    return 0;
}

char LICENSE[] SEC("license") = "GPL";
