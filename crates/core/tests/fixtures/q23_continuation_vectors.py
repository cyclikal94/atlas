#!/usr/bin/env python3
"""BE-Q23 DA-H1/Q6 golden continuation vectors (test-only).

Computes the `d1` cursors and `d2` continuations that the deterministic core and HTTP
fixtures must receive, with Python's standard-library `hmac`/`hashlib` only, so the expected
values do not come from the Rust signer under test. The framing follows contract v1 and
`crates/core/src/sync_cursor.rs`:

    mac(domain, payload) = HMAC-SHA256(key, f"{domain}\n{actor}\n{len(device)}:{device}\n{payload}")
    d1 = f"d1.{boundary:016x}.{expiry:016x}" + "." + hex(mac("atlas-delta-v1", d1))
    header = f"d2.{boundary:016x}.{expiry:016x}.{revision:016x}.{index:08x}"
    position = (pass << 16) | ordinal              # pass 0 = upsert, 1 = removal
    tag = hex(mac("atlas-delta-v2", f"{header}.{position:08x}"))
    pad = first four bytes of mac("atlas-delta-v2-siv", tag), big-endian
    d2 = f"{header}.{position ^ pad:08x}.{tag}"

Run with `python3 -I crates/core/tests/fixtures/q23_continuation_vectors.py`; it prints the
Rust constants pasted into the two tests. The inputs below are the tests' declared fixtures.
"""
import hashlib
import hmac

KEY = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0"
READER = "00000000-0000-0000-0000-0000000000b0"
T = 1_788_868_800
EXPIRY = T + 90 * 86400
BOUNDARY = 2
REVISION = 3


def mac(device, domain, payload):
    message = f"{domain}\n{READER}\n{len(device)}:{device}\n{payload}".encode()
    return hmac.new(KEY.encode(), message, hashlib.sha256).hexdigest()


def d1(device, boundary):
    payload = f"d1.{boundary:016x}.{EXPIRY:016x}"
    return f"{payload}.{mac(device, 'atlas-delta-v1', payload)}"


def d2(device, index, removal, ordinal):
    header = f"d2.{BOUNDARY:016x}.{EXPIRY:016x}.{REVISION:016x}.{index:08x}"
    position = (int(removal) << 16) | ordinal
    tag = mac(device, "atlas-delta-v2", f"{header}.{position:08x}")
    pad = int(mac(device, "atlas-delta-v2-siv", tag)[:8], 16)
    return f"{header}.{position ^ pad:08x}.{tag}"


VECTORS = {
    "reader": [
        ("START", lambda d: d1(d, 2)),
        ("DONE", lambda d: d1(d, 3)),
        ("I1_U4", lambda d: d2(d, 1, False, 4)),
        ("I1_U5", lambda d: d2(d, 1, False, 5)),
        ("I1_U6", lambda d: d2(d, 1, False, 6)),
        ("I1_U7", lambda d: d2(d, 1, False, 7)),
        ("I2_U7", lambda d: d2(d, 2, False, 7)),
        ("I2_U8", lambda d: d2(d, 2, False, 8)),
        ("I2_U9", lambda d: d2(d, 2, False, 9)),
        ("I2_U10", lambda d: d2(d, 2, False, 10)),
        ("I3_R4", lambda d: d2(d, 3, True, 4)),
        ("I3_R5", lambda d: d2(d, 3, True, 5)),
        ("I3_U10", lambda d: d2(d, 3, False, 10)),
    ],
    "phone": [
        ("START", lambda d: d1(d, 2)),
        ("DONE", lambda d: d1(d, 3)),
        ("I1_U4", lambda d: d2(d, 1, False, 4)),
        ("I1_U5", lambda d: d2(d, 1, False, 5)),
        ("I2_U7", lambda d: d2(d, 2, False, 7)),
        ("I2_U8", lambda d: d2(d, 2, False, 8)),
        ("I3_R4", lambda d: d2(d, 3, True, 4)),
        ("I3_R5", lambda d: d2(d, 3, True, 5)),
    ],
}

if __name__ == "__main__":
    for device, vectors in VECTORS.items():
        print(f"// device {device!r}")
        for name, make in vectors:
            print(f'const {name}: &str = "{make(device)}";')
