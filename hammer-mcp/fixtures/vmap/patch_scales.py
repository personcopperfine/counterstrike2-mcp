#!/usr/bin/env python3
"""Patch one CMapMesh element's `scales` attribute in a binary-9 DMX .vmap.

This is the generator of the committed `probe_scaled.vmap` fixture (review-3 NEW-6): the file
route's scaled-node refusal (review-2 M1) needs a SAVED map whose mesh carries an object-properties
scale, and `probe_scaled.vmap` is `probe.vmap` with exactly that one attribute changed — 4 bytes,
`1.0f -> 2.0f` for two components, at the `scales` value vector of nodeID 3 (`[2, 2, 1]`).

Run it from anywhere:

    python3 -I hammer-mcp/fixtures/vmap/patch_scales.py                 # regenerate + verify
    python3 -I hammer-mcp/fixtures/vmap/patch_scales.py --check         # verify only, no write

With no arguments (or `--check`) it reads `probe.vmap` next to this script, writes
`probe_scaled.vmap` next to it and then compares the result byte for byte with the committed file
(exactly one exit-unclean case: a file that does not match). The explicit form is still available:

    python3 -I patch_scales.py <src.vmap> <dst.vmap> <nodeID> <sx> <sy> <sz>

Walking the attribute-body section exactly like `hammer-mcp/src/vmap/dmx.rs` does (same counts,
same value sizes, same printf-style attribute names) so the byte offset of the wanted element's
`scales` value vector can be computed, then rewriting those 3 f32s in place. Nothing else in the
file changes, so regenerating is byte-reproducible (the committed file's sha256 is printed for
comparison).
"""
import hashlib
import os
import struct
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_SRC = os.path.join(HERE, "probe.vmap")
DEFAULT_DST = os.path.join(HERE, "probe_scaled.vmap")
# probe.vmap's room shell (the mesh the tool tests address); its own scales are [1, 1, 1] there.
DEFAULT_NODE = 3
DEFAULT_SCALES = [2.0, 2.0, 1.0]


class R:
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

    def u64(self):
        v = struct.unpack_from("<Q", self.data, self.pos)[0]
        self.pos += 8
        return v

    def f32s(self, n):
        vals = struct.unpack_from(f"<{n}f", self.data, self.pos)
        self.pos += 4 * n
        return list(vals)

    def le(self, n):
        v = self.data[self.pos:self.pos + n]
        self.pos += n
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
        return -2 if i == -2 else i

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

    def array(self, t):
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


def scales_offset(data, want_node):
    """The byte offset of `want_node`'s `scales` f32 triple, or None."""
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

    strings = []
    for _ in range(r.i32()):
        strings.append(r.raw_str())

    elem_classes = []
    for _ in range(r.i32()):           # element index rows
        elem_classes.append(r.i32())   # class-name string index (e. names we skip)
        r.i32()
        r.le(16)

    found = None
    for idx in range(len(elem_classes)):
        klass = strings[elem_classes[idx]]
        attrs = {}
        for _ in range(r.i32()):       # attribute bodies of element `idx`
            name = strings[r.i32()]
            t = r.u8()
            base = r.pos
            value = None
            if 33 <= t <= 48:
                r.array(t)
            else:
                if t == 2:             # Int: read it now (the element is identified by nodeID)
                    value = struct.unpack_from("<i", r.data, base)[0]
                r.scalar_size(t, False)
            if klass == "CMapMesh":
                attrs[name] = (t, base, value)
        if klass == "CMapMesh" and attrs.get("nodeID", (0, 0, None))[2] == want_node \
                and "scales" in attrs and attrs["scales"][0] == 10:
            found = attrs["scales"][1]
    return found


def patch(src, dst, want_node, scales):
    """Write `dst` = `src` with `want_node`'s `scales` replaced; returns the byte offset."""
    data = bytearray(open(src, "rb").read())
    offset = scales_offset(data, want_node)
    if offset is None:
        sys.exit(f"no scales found for a CMapMesh with nodeID {want_node} in {src}")
    struct.pack_into("<3f", data, offset, *scales)
    open(dst, "wb").write(bytes(data))
    return offset


def sha256(path):
    return hashlib.sha256(open(path, "rb").read()).hexdigest()


def main(argv):
    check = "--check" in argv
    argv = [a for a in argv if a != "--check"]
    if argv:
        if len(argv) != 6:
            sys.exit("usage: patch_scales.py [--check] [src dst nodeID sx sy sz]")
        src, dst, node = argv[0], argv[1], int(argv[2])
        scales = [float(x) for x in argv[3:6]]
        offset = patch(src, dst, node, scales)
        print(f"patched scales at {offset}: {scales}")
        return

    if check:
        # recompute into memory (no write) and compare with the committed file
        data = bytearray(open(DEFAULT_SRC, "rb").read())
        offset = scales_offset(data, DEFAULT_NODE)
        if offset is None:
            sys.exit(f"no scales found for nodeID {DEFAULT_NODE} in {DEFAULT_SRC}")
        struct.pack_into("<3f", data, offset, *DEFAULT_SCALES)
        regenerated = bytes(data)
    else:
        offset = patch(DEFAULT_SRC, DEFAULT_DST, DEFAULT_NODE, DEFAULT_SCALES)
        regenerated = open(DEFAULT_DST, "rb").read()
    committed = open(DEFAULT_DST, "rb").read()
    if regenerated != committed:
        sys.exit(
            f"{DEFAULT_DST} does not match: {len(regenerated)} bytes regenerated, "
            f"{len(committed)} committed"
        )
    print(
        f"probe_scaled.vmap reproduced byte for byte: nodeID {DEFAULT_NODE} scales "
        f"{DEFAULT_SCALES} at offset {offset}, sha256 {sha256(DEFAULT_DST)}"
    )


if __name__ == "__main__":
    main(sys.argv[1:])
