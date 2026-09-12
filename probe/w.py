"""One process, one pid: sends 64 MiB to itself over loopback and reads it back.

Traced with set_ftrace_pid on this pid, so every hit in the trace is ours.
Phase decides which send path the kernel takes.
"""
import os, socket, sys, tempfile, threading

SIZE = 64 << 20
CHUNK = 64 << 10
phase = sys.argv[1]

if phase == "udp":
    rx = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    rx.bind(("127.0.0.1", 0))
    tx = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    tx.connect(rx.getsockname())

    def drain():
        got = 0
        while got < SIZE:
            got += len(rx.recv(CHUNK))

    t = threading.Thread(target=drain, daemon=True)
    t.start()
    buf = b"x" * 8192
    for _ in range(SIZE // len(buf)):
        tx.send(buf)
    t.join(30)
    sys.exit(0)

srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", 0))
srv.listen(1)
tx = socket.create_connection(srv.getsockname())
rx, _ = srv.accept()

def drain():
    got = 0
    while got < SIZE:
        b = rx.recv(CHUNK)
        if not b:
            break
        got += len(b)

t = threading.Thread(target=drain, daemon=True)
t.start()

if phase == "write":
    buf = b"x" * CHUNK
    for _ in range(SIZE // CHUNK):
        tx.sendall(buf)
elif phase == "sendfile":
    with tempfile.NamedTemporaryFile() as f:
        f.write(b"x" * CHUNK * 16)
        f.flush()
        for _ in range(SIZE // (CHUNK * 16)):
            f.seek(0)
            tx.sendfile(f, 0, CHUNK * 16)
else:
    sys.exit("unknown phase")

t.join(30)
