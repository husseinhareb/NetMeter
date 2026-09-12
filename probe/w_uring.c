/* 64 MiB through a self-connected TCP socket using io_uring send and recv,
 * to see whether those paths reach sock_sendmsg/sock_recvmsg at all. */
#include <arpa/inet.h>
#include <liburing.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define CHUNK (64 << 10)
#define TOTAL (64ULL << 20)

int main(void) {
    int srv = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = inet_addr("127.0.0.1")};
    if (bind(srv, (struct sockaddr *)&a, sizeof a) || listen(srv, 1)) return perror("bind"), 1;
    socklen_t al = sizeof a;
    getsockname(srv, (struct sockaddr *)&a, &al);

    int tx = socket(AF_INET, SOCK_STREAM, 0);
    if (connect(tx, (struct sockaddr *)&a, sizeof a)) return perror("connect"), 1;
    int rx = accept(srv, NULL, NULL);
    if (rx < 0) return perror("accept"), 1;

    struct io_uring ring;
    if (io_uring_queue_init(64, &ring, 0)) return perror("ring"), 1;

    char *sbuf = malloc(CHUNK), *rbuf = malloc(CHUNK);
    memset(sbuf, 'x', CHUNK);
    unsigned long long sent = 0, got = 0;

    /* One send and one recv in flight at a time: the point is which kernel
     * path runs, not throughput. */
    while (sent < TOTAL) {
        struct io_uring_sqe *s = io_uring_get_sqe(&ring);
        io_uring_prep_send(s, tx, sbuf, CHUNK, 0);
        struct io_uring_sqe *r = io_uring_get_sqe(&ring);
        io_uring_prep_recv(r, rx, rbuf, CHUNK, 0);
        io_uring_submit(&ring);

        for (int i = 0; i < 2; i++) {
            struct io_uring_cqe *c;
            if (io_uring_wait_cqe(&ring, &c)) return perror("cqe"), 1;
            if (c->res < 0) { fprintf(stderr, "op failed: %d\n", c->res); return 1; }
            if (c->user_data == 0 && i == 0) sent += c->res; else got += c->res;
            io_uring_cqe_seen(&ring, c);
        }
        sent += 0;
        if (got >= TOTAL) break;
        sent = got;  /* keep the two sides in lockstep */
    }
    printf("io_uring moved %llu bytes\n", got);
    io_uring_queue_exit(&ring);
    return 0;
}
