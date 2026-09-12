"""Print the argument count and signature of kernel functions, read straight
out of /sys/kernel/btf/vmlinux. fentry/fexit programs must declare exactly the
arity BTF reports, or the return value is read from the wrong ctx slot."""
import struct, sys

KIND_STRUCT, KIND_UNION, KIND_ENUM, KIND_FUNC, KIND_FUNC_PROTO = 4, 5, 6, 12, 13
KIND_INT, KIND_ARRAY, KIND_VAR, KIND_DATASEC, KIND_DECL_TAG, KIND_ENUM64 = 1, 3, 14, 15, 17, 19

blob = open("/sys/kernel/btf/vmlinux", "rb").read()
magic, ver, flags, hdr_len, type_off, type_len, str_off, str_len = struct.unpack_from("<HBBIIIII", blob, 0)
assert magic == 0xEB9F, hex(magic)
types_base, strs_base = hdr_len + type_off, hdr_len + str_off

def s(off):
    end = blob.index(b"\0", strs_base + off)
    return blob[strs_base + off : end].decode()

types = [None]  # id 0 is void
pos = types_base
while pos < types_base + type_len:
    name_off, info, size = struct.unpack_from("<III", blob, pos)
    vlen, kind = info & 0xFFFF, (info >> 24) & 0x1F
    body = pos + 12
    extra = {
        KIND_INT: 4, KIND_ARRAY: 12, KIND_STRUCT: 12 * vlen, KIND_UNION: 12 * vlen,
        KIND_ENUM: 8 * vlen, KIND_ENUM64: 12 * vlen, KIND_FUNC_PROTO: 8 * vlen,
        KIND_VAR: 4, KIND_DATASEC: 12 * vlen, KIND_DECL_TAG: 4,
    }.get(kind, 0)
    types.append((kind, s(name_off), size, vlen, body))
    pos = body + extra

def name_of(tid):
    seen = 0
    while tid and seen < 8:
        kind, nm, size, vlen, body = types[tid]
        if nm:
            return nm if kind != 2 else nm + " *"
        if kind == 2:  # PTR
            inner = name_of(size)
            return (inner or "void") + " *"
        tid, seen = size, seen + 1
    return "void"

by_name = {}
for tid, t in enumerate(types):
    if t and t[0] == KIND_FUNC:
        by_name[t[1]] = t

for want in sys.argv[1:]:
    t = by_name.get(want)
    if not t:
        print(f"  {want:24} NOT IN BTF")
        continue
    proto = types[t[2]]
    kind, _, ret_tid, vlen, body = proto
    params = [struct.unpack_from("<II", blob, body + 8 * i) for i in range(vlen)]
    sig = ", ".join(f"{name_of(p[1])} {s(p[0]) or '_'}" for p in params)
    print(f"  {want:24} {vlen} args -> {name_of(ret_tid):12}  ({sig})")
