#!/usr/bin/env python3
"""Give one `CMapEntity` of a binary-9 DMX .vmap an I/O `connectionsData` entry.

This is the generator of the committed `probe_conn.vmap` fixture (review-rust.md
C1 test support): every checked-in `.vmap` fixture has only empty `connectionsData`
arrays, so the file route's I/O connection path had no non-`#[ignore]`d cover. The
generated map is `probe.vmap` with exactly one added DMX element and one non-empty
attribute value:

* one `DmeConnectionData` element appended to the element index, with the seven
  attributes the real writer emits (read off the installed
  `content/csgo_addons/cs_script_demo/maps/cs_script_demo.vmap`, the file the
  `#[ignore]`d real-tree connection tests read; the live a3.log wire reports the
  same shape): `outputName` "OnStartTouch", `targetType` 7, `targetName`
  "ta_kill_t", `inputName` "Kill", `overrideParam` "", `delay` 0.0 (f32),
  `timesToFire` -1;
* func_buyzone nodeID 67's (previously empty) `connectionsData` element array gets
  a count of 1 plus a reference to that element.

Nothing else in the file changes: the patch appends table strings, one index row
and one element body (a trailing byte run), and rewrites two i32 counters — every
DMX reference is a table/array index, never a byte offset, so inserting and
appending bytes keeps the file well-formed, and regenerating is byte-reproducible
(like `patch_scales.py`).

Run it from anywhere:

    python3 -I hammer-mcp/fixtures/vmap/patch_connections.py                # regenerate + verify
    python3 -I hammer-mcp/fixtures/vmap/patch_connections.py --check        # verify only, no write

The general form adds one connection to any binary-9 `.vmap`:

    patch_connections.py <src.vmap> <dst.vmap> <nodeID> <output> <target> <input> <param> <targetType> <delay> <timesToFire>
"""
import hashlib
import os
import struct
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_SRC = os.path.join(HERE, "probe.vmap")
DEFAULT_DST = os.path.join(HERE, "probe_conn.vmap")
# probe.vmap's tested buyzone brush entity (the file-route connection test's node).
DEFAULT_NODE = 67
DEFAULT_CONN = ("OnStartTouch", "ta_kill_t", "Kill", "", 7, 0.0, -1)

# The attribute list of one connection element, in the real writer's order (the
# installed cs_script_demo.vmap's element), with the DMX type ids the Rust decoder
# reads: 5 = string (a table index inside an element body), 2 = int, 3 = float.
ATTRS = [
    ("outputName", 5), ("targetType", 2), ("targetName", 5), ("inputName", 5),
    ("overrideParam", 5), ("delay", 3), ("timesToFire", 2),
]
# A fixed GUID for the synthesized element (kept stable so the fixture regenerates
# byte for byte). The generator asserts it does not occur in the file already.
NEW_GUID = bytes(
    [0x4D, 0xC6, 0x13, 0x37, 0xD0, 0xC0, 0x4E, 0xD0, 0xA5, 0x51, 0x10, 0x02, 0xC0, 0x47, 0xE1, 0x1A]
)


class R:
    """The DMX walker: same counts, same value sizes, same string forms as
    `hammer-mcp/src/vmap/dmx.rs` (`parse`)."""

    def __init__(self, data, pos):
        self.data, self.pos = data, pos

    def i32(self):
        v = struct.unpack_from("<i", self.data, self.pos)[0]
        self.pos += 4
        return v

    def u8(self):
        v = self.data[self.pos]
        self.pos += 1
        return v

    def f32s(self, n):
        vals = struct.unpack_from(f"<{n}f", self.data, self.pos)
        self.pos += 4 * n
        return list(vals)

    def le(self, n):
        v = self.data[self.pos:self.pos + n]
        self.pos += n
        return v

    def u64(self):
        v = struct.unpack_from("<Q", self.data, self.pos)[0]
        self.pos += 8
        return v

    def raw_str(self):
        end = self.data.index(0, self.pos)
        s = bytes(self.data[self.pos:end]).decode("utf-8", "replace")
        self.pos = end + 1
        return s

    def element_ref(self):
        i = self.i32()
        if i == -2:  # REF_STUB: the referenced guid as a raw NUL-terminated string
            self.raw_str()
        return i

    def array_with_count_pos(self, t):
        """`count i32` + items; returns the byte offset of the count."""
        pos = self.pos
        n = self.i32()
        if n < 0 or self.pos + n > len(self.data):
            raise RuntimeError(f"array count {n}")
        if t == 33:
            for _ in range(n):
                self.element_ref()
        elif t == 37:
            for _ in range(n):
                self.raw_str()
        elif t == 38:
            for _ in range(n):
                ln = self.i32()
                if ln != -1:
                    self.pos += ln
        elif t in (39, 40):
            self.pos += 4 * n
        else:
            s = t - 32
            if s == 0 or s > 16:
                raise RuntimeError(f"array type {t}")
            for _ in range(n):
                self.scalar_size(s, True)
        return pos

    def array(self, t):
        self.array_with_count_pos(t)

    def scalar_size(self, t, prefix):
        if t == 1:
            self.element_ref()
        elif t in (2, 7):
            self.i32()
        elif t == 3:
            self.f32s(1)
        elif t == 4:
            self.u8()
        elif t == 5:
            if prefix:
                self.raw_str()
            else:
                self.i32()
        elif t == 6:
            ln = self.i32()
            self.pos += ln
        elif t == 8:
            self.le(4)
        elif t == 9:
            self.f32s(2)
        elif t in (10, 12):
            self.f32s(3)
        elif t == 11 or t == 13:
            self.f32s(4)
        elif t == 14:
            self.f32s(16)
        elif t == 15:
            self.u64()
        elif t == 16:
            self.u8()
        else:
            raise RuntimeError(f"scalar type {t}")


def locate(data, want_node):
    """Find the pieces the patch rewrites.

    Returns `(pos_string_count, strings_end, strings, pos_element_count,
    index_rows_start, index_rows_end, element_count, pos_conn_count, node_guids)`;
    `pos_conn_count` is `None` when the entity (or its connectionsData
    element-array attribute) was not found. `node_guids` is every element index
    row's guid for the duplicate-guid assertion.
    """
    end = data.index(0, 0)
    header = bytes(data[:end]).decode()
    assert header.startswith("<!-- dmx encoding binary 9"), header
    r = R(data, end + 1)

    for _ in range(r.i32()):           # prefix elements
        for _ in range(r.i32()):
            r.raw_str()                # attribute name (raw string)
            t = r.u8()
            if 33 <= t <= 48:
                r.array(t)
            else:
                r.scalar_size(t, True)

    pos_string_count = r.pos
    string_count = r.i32()
    strings = []
    for _ in range(string_count):
        strings.append(r.raw_str())
    strings_end = r.pos

    pos_element_count = r.pos
    element_count = r.i32()
    index_rows_start = r.pos
    rows = []
    guids = set()
    for _ in range(element_count):
        class_idx = r.i32()
        r.i32()                        # element-name string index
        guid = bytes(r.le(16))
        guids.add(guid)
        rows.append(class_idx)
    index_rows_end = r.pos

    pos_conn_count = None
    node_id = None
    for idx in range(element_count):
        klass = strings[rows[idx]]
        node_id = None
        for _ in range(r.i32()):
            name_idx = struct.unpack_from("<i", r.data, r.pos)[0]
            name = strings[name_idx]
            r.pos += 4
            t = r.u8()
            base = r.pos
            value = None
            if 33 <= t <= 48:
                value = r.array_with_count_pos(t)
            else:
                if t == 2:
                    value = struct.unpack_from("<i", r.data, base)[0]
                r.scalar_size(t, False)
            if klass == "CMapEntity" and name == "nodeID" and t == 2:
                node_id = value
            if (klass == "CMapEntity" and node_id == want_node
                    and name == "connectionsData" and t == 33):
                pos_conn_count = base
    if pos_conn_count is None:
        raise RuntimeError(
            f"no CMapEntity with nodeID {want_node} carrying a connectionsData "
            "element-array attribute"
        )
    return (pos_string_count, strings_end, strings, pos_element_count,
            index_rows_start, index_rows_end, element_count, pos_conn_count,
            guids)


def patch(src, dst, want_node, connection):
    """Write `dst` = `src` + one DmeConnectionData on `want_node`."""
    output, target, input_name, param, target_type, delay, times = connection
    data = open(src, "rb").read()
    if NEW_GUID in data:
        sys.exit(f"the synthesized guid already occurs in {src}")

    (pos_string_count, strings_end, strings, pos_element_count, rows_start,
     rows_end, element_count, pos_conn_count, guids) = locate(data, want_node)
    if NEW_GUID in guids:
        sys.exit("the synthesized guid collides with an element guid")

    # The new table strings: the element class, the attribute names, and the string
    # attribute values (each its own table slot).
    new_strings = (["DmeConnectionData"]
                   + [name for name, _ in ATTRS]
                   + [output, target, input_name, param])
    base = len(strings)
    idx = {name: base + i for i, name in enumerate(new_strings)}
    value_idx = {
        "outputName": base + 1 + len(ATTRS) + 0,
        "targetName": base + 1 + len(ATTRS) + 1,
        "inputName": base + 1 + len(ATTRS) + 2,
        "overrideParam": base + 1 + len(ATTRS) + 3,
    }

    new_ref = element_count  # the appended element's index
    # index row: class-name string index, element-name string index, 16 guid bytes
    row = (struct.pack("<i", idx["DmeConnectionData"])
           + struct.pack("<i", idx["DmeConnectionData"])
           + NEW_GUID)

    body = struct.pack("<i", len(ATTRS))
    values = {
        "outputName": output, "targetType": target_type, "targetName": target,
        "inputName": input_name, "overrideParam": param, "delay": delay,
        "timesToFire": times,
    }
    for name, t in ATTRS:
        body += struct.pack("<i", idx[name])
        body += struct.pack("<B", t)
        if t == 5:
            body += struct.pack("<i", value_idx[name])
        elif t == 2:
            body += struct.pack("<i", values[name])
        elif t == 3:
            body += struct.pack("<f", values[name])
        else:
            raise RuntimeError(f"attribute type {t} not supported")

    out = bytearray()
    out += data[:pos_string_count]
    out += struct.pack("<i", len(strings) + len(new_strings))
    out += data[pos_string_count + 4:strings_end]
    out += b"".join(text.encode("utf-8") + b"\x00" for text in new_strings)
    out += data[strings_end:pos_element_count]
    out += struct.pack("<i", element_count + 1)
    out += data[pos_element_count + 4:rows_end]
    out += row
    out += data[rows_end:pos_conn_count]
    out += struct.pack("<i", 1)            # connectionsData count 0 -> 1
    out += struct.pack("<i", new_ref)      # the DmeConnectionData element ref
    out += data[pos_conn_count + 4:]       # the rest, verbatim
    out += body                            # the new element's body (last in file)

    open(dst, "wb").write(bytes(out))
    return new_ref


def sha256(path):
    return hashlib.sha256(open(path, "rb").read()).hexdigest()


def main(argv):
    check = "--check" in argv
    argv = [a for a in argv if a != "--check"]
    if argv:
        if len(argv) != 10:
            sys.exit("usage: patch_connections.py [--check] "
                     "[src dst nodeID output target input param targetType delay timesToFire]")
        src, dst, node = argv[0], argv[1], int(argv[2])
        conn = (argv[3], argv[4], argv[5], argv[6], int(argv[7]),
                float(argv[8]), int(argv[9]))
        ref = patch(src, dst, node, conn)
        print(f"patched connectionsData of nodeID {node} at ref {ref}")
        return

    if check:
        regenerated_path = DEFAULT_DST + ".regen"
        patch(DEFAULT_SRC, regenerated_path, DEFAULT_NODE, DEFAULT_CONN)
        regenerated = open(regenerated_path, "rb").read()
        os.unlink(regenerated_path)
    else:
        ref = patch(DEFAULT_SRC, DEFAULT_DST, DEFAULT_NODE, DEFAULT_CONN)
        print(f"probe_conn.vmap: connectionsData of nodeID {DEFAULT_NODE} -> "
              f"DmeConnectionData ref {ref}")
        regenerated = open(DEFAULT_DST, "rb").read()
    committed = open(DEFAULT_DST, "rb").read()
    if regenerated != committed:
        sys.exit(
            f"{DEFAULT_DST} does not match: {len(regenerated)} bytes regenerated, "
            f"{len(committed)} committed"
        )
    print(
        f"probe_conn.vmap reproduced byte for byte: sha256 {sha256(DEFAULT_DST)}"
    )


if __name__ == "__main__":
    main(sys.argv[1:])
