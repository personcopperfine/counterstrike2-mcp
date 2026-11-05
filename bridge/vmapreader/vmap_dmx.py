#!/usr/bin/env python3
"""vmap_dmx.py - stdlib-only reader for Source 2 DMX files (CS2 ``.vmap``),
encoding ``binary`` version 9.

The reader decodes the whole element/attribute tree that Hammer writes into a
``.vmap`` (``format vmap 40``): the v9 prefix attributes, the string table, the
element index (class / name / GUID) and every attribute body.  On top of that
tree it exposes the map-specific view the MCP server needs for
``list/find/get_entity``: entities (nodes that own an ``entity_properties``
``EditGameClassProps`` child), their FGD keyvalues, ``nodeID``, ``origin`` /
``angles`` and the node hierarchy.

Usage (CLI)::

    python3 vmap_dmx.py FILE info
    python3 vmap_dmx.py FILE list [--class CMapEntity] [--limit N] [--json]
    python3 vmap_dmx.py FILE find <pattern> [--json]
    python3 vmap_dmx.py FILE get <targetname|nodeID|guid> [--json]

Library::

    import vmap_dmx
    dm = vmap_dmx.load("probe.vmap")
    for ent in dm.entities:
        print(ent.node_id, ent.classname, ent.targetname, ent.origin)

Format ground truth (see README.md in this directory):

* ``worknotes/documentation`` - docs/data/vmap-format.md (task 20 static notes)
* upstream ``ValveResourceFormat/Datamodel.NET`` ``Codecs/Binary.cs``
  (fetched 2026-10-06; the exact v9 wire format)
* the files themselves, parsed byte-exactly to EOF and golden-compared with
  ``dmxconvert -oe keyvalues2`` dumps (test_dmx.py).

No third-party imports; Python 3.8+.
"""

from __future__ import annotations

import argparse
import json
import os
import struct
import sys
import uuid
from collections import Counter
from pathlib import Path
from typing import Any, Dict, Iterator, List, Optional, Sequence, Tuple, Union

__all__ = [
    "DmxError",
    "DmxElement",
    "DmxFile",
    "Entity",
    "load",
    "parse",
]

# ---------------------------------------------------------------------------
# binary v9 constants
# ---------------------------------------------------------------------------

BINARY_VERSION = 9

# scalar id = position + 1 in Datamodel.NET Binary.cs SupportedAttributes[9]
SCALAR_TYPE_NAMES = {
    1: "element",
    2: "int",
    3: "float",
    4: "bool",
    5: "string",
    6: "binary",
    7: "time",
    8: "color",
    9: "vector2",
    10: "vector3",
    11: "vector4",
    12: "qangle",
    13: "quaternion",
    14: "matrix",
    15: "uint64",
    16: "byte",
}
ARRAY_TYPE_OFFSET = 32  # v9: array id = scalar id + 32
ARRAY_TYPE_NAMES = {k + ARRAY_TYPE_OFFSET: v for k, v in SCALAR_TYPE_NAMES.items()}

# element-reference sentinels (Datamodel.NET Binary.cs ReadElement)
REF_NULL = -1
REF_STUB = -2  # followed by 36 ASCII GUID chars + NUL

# classes that may be the v9 "prefix attributes stored as an element" duplicate
PREFIX_ELEMENT_CLASSES = ("DmElement", "$prefix_element$")

# DMX attribute types that must not appear in binary version 9 (double et al
# exist only in other encodings); kept here so error messages are specific.
_UNSUPPORTED_V9_IDS = set(range(17, 33))


class DmxError(Exception):
    """Malformed or unsupported DMX file."""


class _StubRef:
    """Placeholder for a ``-2`` element reference, resolved after parse."""

    __slots__ = ("guid",)

    def __init__(self, guid: str) -> None:
        self.guid = guid


# ---------------------------------------------------------------------------
# low-level cursor
# ---------------------------------------------------------------------------


class _Reader:
    __slots__ = ("data", "pos", "size")

    def __init__(self, data: bytes, pos: int = 0) -> None:
        self.data = data
        self.pos = pos
        self.size = len(data)

    def _need(self, n: int) -> None:
        if self.pos + n > self.size:
            raise DmxError(
                "truncated DMX stream: want %d byte(s) at offset %d, file has %d"
                % (n, self.pos, self.size)
            )

    def u8(self) -> int:
        self._need(1)
        v = self.data[self.pos]
        self.pos += 1
        return v

    def i32(self) -> int:
        self._need(4)
        (v,) = struct.unpack_from("<i", self.data, self.pos)
        self.pos += 4
        return v

    def u64(self) -> int:
        self._need(8)
        (v,) = struct.unpack_from("<Q", self.data, self.pos)
        self.pos += 8
        return v

    def f32(self) -> float:
        self._need(4)
        (v,) = struct.unpack_from("<f", self.data, self.pos)
        self.pos += 4
        return v

    def f32n(self, n: int) -> Tuple[float, ...]:
        return tuple(self.f32() for _ in range(n))

    def bytes(self, n: int) -> bytes:
        if n < 0:
            raise DmxError("negative byte count %d at offset %d" % (n, self.pos))
        self._need(n)
        v = self.data[self.pos : self.pos + n]
        self.pos += n
        return v

    def raw_str(self) -> str:
        """NUL-terminated UTF-8 string (raw form used by v9 prefix/arrays).

        Invalid bytes survive via ``surrogateescape`` so a future writer can
        round-trip them; the CLI re-encodes output with backslashreplace.
        """
        end = self.data.find(b"\x00", self.pos)
        if end < 0:
            raise DmxError("unterminated string at offset %d" % self.pos)
        v = self.data[self.pos : end].decode("utf-8", "surrogateescape")
        self.pos = end + 1
        return v

    def guid(self) -> str:
        """16 raw bytes -> canonical GUID string (NET Guid byte order)."""
        b = self.bytes(16)
        return str(uuid.UUID(bytes_le=b))


# ---------------------------------------------------------------------------
# element model
# ---------------------------------------------------------------------------


class DmxElement:
    """One DMX element: class name, element name, GUID and attribute dict.

    Attribute values are plain Python values (``str``/``int``/``float``/
    ``bool``/``bytes``/tuple/list) except element references, which are
    ``DmxElement`` instances, and element arrays, which are lists of
    ``DmxElement``/``None``.
    """

    __slots__ = (
        "index",
        "class_name",
        "name",
        "guid",
        "attributes",
        "attr_types",
        "stub",
        "parent",
        "parent_attribute",
        "referenced_by",
    )

    def __init__(
        self,
        index: int,
        class_name: str,
        name: str,
        guid: str,
        stub: bool = False,
    ) -> None:
        self.index = index
        self.class_name = class_name
        self.name = name
        self.guid = guid
        self.stub = stub
        self.attributes: Dict[str, Any] = {}
        self.attr_types: Dict[str, str] = {}
        self.parent: Optional["DmxElement"] = None
        self.parent_attribute: Optional[str] = None
        self.referenced_by: List[Tuple["DmxElement", str]] = []

    # -- attribute access ---------------------------------------------------

    def get(self, name: str, default: Any = None) -> Any:
        return self.attributes.get(name, default)

    def type_of(self, name: str) -> Optional[str]:
        return self.attr_types.get(name)

    def get_str(self, name: str, default: Optional[str] = None) -> Optional[str]:
        """Attribute as a string (the form entity keyvalues are stored in)."""
        v = self.attributes.get(name)
        if v is None:
            return default
        if isinstance(v, str):
            return v
        return _value_to_text(v)

    def child(self, name: str) -> Optional["DmxElement"]:
        """Element-valued attribute; returns None for missing/null/array."""
        v = self.attributes.get(name)
        return v if isinstance(v, DmxElement) else None

    @property
    def children(self) -> List["DmxElement"]:
        """The ``children`` element array (empty when absent)."""
        v = self.attributes.get("children")
        if isinstance(v, list):
            return [e for e in v if isinstance(e, DmxElement)]
        return []

    def descendants(self) -> Iterator["DmxElement"]:
        """Depth-first walk of ``children`` (the map node hierarchy)."""
        for child in self.children:
            yield child
            yield from child.descendants()

    def refs(self) -> Iterator[Tuple[str, "DmxElement"]]:
        """All (attribute name, element) references out of this element."""
        for name, v in self.attributes.items():
            if isinstance(v, DmxElement):
                yield name, v
            elif isinstance(v, list):
                for item in v:
                    if isinstance(item, DmxElement):
                        yield name, item

    def __repr__(self) -> str:  # pragma: no cover - debug helper
        return "<DmxElement #%d %s name=%r guid=%s %d attr(s)>" % (
            self.index,
            self.class_name,
            self.name,
            self.guid,
            len(self.attributes),
        )


class Entity:
    """A map entity: a node plus its ``entity_properties`` keyvalues.

    ``properties`` is the ``EditGameClassProps`` child (the keyvalue bag);
    ``keyvalues`` are its attributes minus the synthetic ``id``.  Node fields
    (``origin`` etc.) come from the node itself, matching Hammer's split
    between transform attributes and FGD properties.
    """

    __slots__ = ("node", "properties", "file", "_keyvalues", "_classname", "_targetname")

    def __init__(
        self, node: DmxElement, properties: Optional[DmxElement], file: "DmxFile"
    ) -> None:
        self.node = node
        self.properties = properties
        self.file = file
        # cached once: find/sort/lookup touch these a lot on big maps
        if properties is None:
            self._keyvalues: Dict[str, Any] = {}
        else:
            self._keyvalues = {
                k: v for k, v in properties.attributes.items() if k != "id"
            }
        self._classname = _kv_text(self._keyvalues.get("classname"))
        self._targetname = _kv_text(self._keyvalues.get("targetname"))

    # -- identity -----------------------------------------------------------

    @property
    def guid(self) -> str:
        return self.node.guid

    @property
    def node_class(self) -> str:
        return self.node.class_name

    @property
    def node_id(self) -> Optional[int]:
        v = self.node.get("nodeID")
        return v if isinstance(v, int) else None

    @property
    def id(self) -> Optional[int]:
        """MCP-facing id (the ``nodeID``; ``.guid`` is the durable handle)."""
        return self.node_id

    @property
    def reference_id(self) -> Optional[int]:
        """The uint64 ``referenceID`` when the file carries one."""
        v = self.node.get("referenceID")
        return v if isinstance(v, int) else None

    @property
    def keyvalues(self) -> Dict[str, Any]:
        """The entity_properties attributes (cached; treat as read-only)."""
        return self._keyvalues

    @property
    def node_attributes(self) -> Dict[str, Any]:
        """Raw attributes of the map node (origin/angles/nodeID/…)."""
        return self.node.attributes

    @property
    def classname(self) -> str:
        return self._classname

    @property
    def targetname(self) -> str:
        return self._targetname

    @property
    def connections(self) -> List[Dict[str, Any]]:
        """Entity I/O from the node's ``connectionsData`` (DmeConnectionData).

        Each dict has the connection element's attributes
        (``outputName``/``targetName``/``inputName``/``delay``/…); all
        sampled maps have empty arrays, so this path is not golden-tested [?].
        """
        v = self.node.get("connectionsData")
        out: List[Dict[str, Any]] = []
        if isinstance(v, list):
            for el in v:
                if isinstance(el, DmxElement):
                    out.append(
                        {k: x for k, x in el.attributes.items() if k != "id"}
                    )
        return out

    # -- node transform -----------------------------------------------------

    @property
    def origin(self) -> Optional[Tuple[float, float, float]]:
        return _as_vec3(self.node.get("origin"))

    @property
    def angles(self) -> Optional[Tuple[float, float, float]]:
        return _as_vec3(self.node.get("angles"))

    @property
    def scales(self) -> Optional[Tuple[float, float, float]]:
        return _as_vec3(self.node.get("scales"))

    # -- helpers ------------------------------------------------------------

    def matches(self, query: str) -> bool:
        """Case-insensitive substring match for find-style lookups."""
        q = query.lower()
        if q in self.classname.lower():
            return True
        if q in self.targetname.lower():
            return True
        if str(self.node_id) == q:
            return True
        if self.guid.lower().startswith(q):
            return True
        return False

    def keyvalues_as_text(self) -> Dict[str, str]:
        """Keyvalues with every value coerced to text (MCP get_entity shape)."""
        return {k: _value_to_text(v) for k, v in self.keyvalues.items()}

    def to_dict(self, with_keyvalues: bool = True) -> Dict[str, Any]:
        d: Dict[str, Any] = {
            "guid": self.guid,
            "nodeClass": self.node_class,
            "nodeID": self.node_id,
            "referenceID": self.reference_id,
            "classname": self.classname,
            "targetname": self.targetname,
            "origin": _round_vec(self.origin),
            "angles": _round_vec(self.angles),
            "scales": _round_vec(self.scales),
            "connections": [
                {k: _json_value(v) for k, v in c.items()} for c in self.connections
            ],
        }
        if with_keyvalues:
            d["keyvalues"] = {
                k: _json_value(v) for k, v in self.keyvalues.items()
            }
        return d

    def __repr__(self) -> str:  # pragma: no cover - debug helper
        return "<Entity nodeID=%s %s targetname=%r>" % (
            self.node_id,
            self.classname,
            self.targetname,
        )


def _as_vec3(v: Any) -> Optional[Tuple[float, float, float]]:
    if isinstance(v, (tuple, list)) and len(v) == 3:
        return (float(v[0]), float(v[1]), float(v[2]))
    return None


def _round_f32(x: float) -> float:
    """float32 -> ~9 significant digits (JSON-friendly, visually stable)."""
    return float("%.9g" % x)


def _round_vec(v: Optional[Tuple[float, ...]]) -> Optional[List[float]]:
    if v is None:
        return None
    return [_round_f32(x) for x in v]


def _kv_text(v: Any) -> str:
    if isinstance(v, str):
        return v
    if v is None:
        return ""
    return _value_to_text(v)


def _value_to_text(v: Any) -> str:
    """Format a DME value the way keyvalues2 text and the FGD would."""
    if v is None:
        return ""
    if isinstance(v, bool):
        return "1" if v else "0"
    if isinstance(v, str):
        return v
    if isinstance(v, int):
        return str(v)
    if isinstance(v, float):
        return repr(v)
    if isinstance(v, (tuple, list)):
        return " ".join(_value_to_text(x) for x in v)
    if isinstance(v, bytes):
        return v.hex()
    if isinstance(v, DmxElement):
        return "<element %s %s>" % (v.class_name, v.guid)
    return str(v)


def _json_value(v: Any, _depth: int = 0) -> Any:
    """JSON-safe conversion for element-valued keyvalues."""
    if v is None or isinstance(v, (bool, int, float, str)):
        return v
    if isinstance(v, bytes):
        return {"__binary__": len(v)}
    if isinstance(v, (tuple, list)):
        return [_json_value(x, _depth + 1) for x in v]
    if isinstance(v, DmxElement):
        if _depth > 4:
            return {"class": v.class_name, "guid": v.guid}
        return {
            "class": v.class_name,
            "guid": v.guid,
            "name": v.name,
            "attributes": {
                k: _json_value(x, _depth + 1) for k, x in v.attributes.items()
            },
        }
    return str(v)


# ---------------------------------------------------------------------------
# file model
# ---------------------------------------------------------------------------


class DmxFile:
    """A parsed DMX document (the ``.vmap`` tree plus map-specific views)."""

    def __init__(self, path: Optional[str] = None) -> None:
        self.path = path
        self.header: str = ""
        self.encoding: str = ""
        self.encoding_version: int = 0
        self.format: str = ""
        self.format_version: int = 0
        self.size: Optional[int] = None
        self.mtime_ns: Optional[int] = None
        self.prefix_attributes: Dict[str, Any] = {}
        self.prefix_element: Optional[DmxElement] = None
        self.elements: List[DmxElement] = []
        self.by_guid: Dict[str, DmxElement] = {}
        self.by_index: List[DmxElement] = []
        self._entities: Optional[List[Entity]] = None

    @property
    def is_vmap(self) -> bool:
        return self.format == "vmap"

    # -- construction -------------------------------------------------------

    @classmethod
    def load(cls, path: Union[str, Path]) -> "DmxFile":
        """Read + parse a file, failing if it changes while being read.

        A concurrent Hammer save can otherwise produce a silently truncated
        tree; the size/mtime check turns that into a clean :class:`DmxError`.
        """
        p = Path(path)
        before = os.stat(p)
        with open(p, "rb") as fh:
            data = fh.read()
        after = os.stat(p)
        if (before.st_size, before.st_mtime_ns) != (after.st_size, after.st_mtime_ns):
            raise DmxError("file changed while reading (Hammer saving?): %s" % p)
        dm = cls.parse(data, path=str(p))
        dm.size = after.st_size
        dm.mtime_ns = after.st_mtime_ns
        return dm

    @classmethod
    def parse(cls, data: bytes, path: Optional[str] = None) -> "DmxFile":
        if not isinstance(data, bytes):
            data = bytes(data)
        dm = cls(path=path)
        r = _Reader(data)

        header_end = data.find(b"\x00")
        if header_end < 0:
            raise DmxError("no NUL after DMX header (not a DMX file?)")
        header = data[:header_end].decode("utf-8", "surrogateescape")
        dm.header = header
        if not header.startswith("<!-- dmx "):
            raise DmxError("bad DMX header: %r" % header)
        parts = header[8:].split()
        if len(parts) < 5 or parts[0] != "encoding":
            raise DmxError("bad DMX header: %r" % header)
        dm.encoding = parts[1]
        try:
            dm.encoding_version = int(parts[2])
            dm.format = parts[4]
            dm.format_version = int(parts[5])
        except (IndexError, ValueError) as exc:
            raise DmxError("bad DMX header: %r" % header) from exc
        if dm.encoding != "binary":
            raise DmxError(
                "unsupported encoding %r (only binary is implemented)" % dm.encoding
            )
        if dm.encoding_version != BINARY_VERSION:
            raise DmxError(
                "unsupported binary encoding version %d (this reader implements v%d)"
                % (dm.encoding_version, BINARY_VERSION)
            )
        r.pos = header_end + 1

        # -- v9 prefix elements --------------------------------------------
        for pidx in range(r.i32()):
            attr_count = r.i32()
            attrs: Dict[str, Any] = {}
            for _ in range(attr_count):
                name = r.raw_str()
                # The element index does not exist yet here; v9 prefix
                # attributes are binary/string/string[]/int[] in practice.
                attrs[name] = _decode_value(
                    r, r.u8(), prefix=True, strings=(), elements=()
                )
            if pidx == 0:
                dm.prefix_attributes = attrs

        # -- string table ---------------------------------------------------
        string_count = r.i32()
        if string_count < 0 or string_count > len(data):
            raise DmxError("implausible string table size %d" % string_count)
        strings = [r.raw_str() for _ in range(string_count)]

        # -- element index --------------------------------------------------
        element_count = r.i32()
        if element_count < 0 or element_count > len(data):
            raise DmxError("implausible element count %d" % element_count)
        for i in range(element_count):
            class_name = strings[_index(strings, r.i32(), "class name")]
            name = strings[_index(strings, r.i32(), "element name")]
            guid = r.guid()
            el = DmxElement(i, class_name, name, guid)
            dm.elements.append(el)

        # -- attribute bodies ----------------------------------------------
        for el in dm.elements:
            attr_count = r.i32()
            for _ in range(attr_count):
                attr_name = strings[_index(strings, r.i32(), "attribute name")]
                type_id = r.u8()
                el.attributes[attr_name] = _decode_value(
                    r, type_id, prefix=False, strings=strings, elements=dm.elements
                )
                el.attr_types[attr_name] = _type_name(type_id)

        if r.pos != len(data):
            raise DmxError(
                "parsed %d bytes but file is %d (unexpected trailing data)"
                % (r.pos, len(data))
            )

        # -- post-processing ------------------------------------------------
        dm._build_guid_index()
        dm._resolve_stubs()
        dm.by_index = dm.elements
        dm._find_prefix_element()
        dm._link_parents()
        return dm

    # -- post-processing ----------------------------------------------------

    def _build_guid_index(self) -> None:
        """GUID -> element, failing closed on duplicates."""
        for el in self.elements:
            prev = self.by_guid.get(el.guid)
            if prev is not None:
                raise DmxError(
                    "duplicate element GUID %s (elements %d and %d)"
                    % (el.guid, prev.index, el.index)
                )
            self.by_guid[el.guid] = el

    def _resolve_stubs(self) -> None:
        """Replace ``-2`` stub placeholders with real elements (or a stub)."""
        by_guid = self.by_guid

        def fix(v: Any) -> Any:
            if isinstance(v, _StubRef):
                el = by_guid.get(v.guid)
                if el is not None:
                    return el
                return DmxElement(-1, "", "", v.guid, stub=True)
            if isinstance(v, list):
                return [fix(x) for x in v]
            return v

        for el in self.elements:
            el.attributes = {k: fix(v) for k, v in el.attributes.items()}

    def _find_prefix_element(self) -> None:
        """v9 stores the prefix attributes as an unreferenced element.

        Its class is ``DmElement`` in files written by newer Hammer builds and
        ``$prefix_element$`` in older ones (see README).  Detection is by class,
        empty name and identical attribute-name set, as upstream does.
        """
        if len(self.elements) < 2 or not self.prefix_attributes:
            return
        dup = self.elements[1]
        if dup.class_name not in PREFIX_ELEMENT_CLASSES:
            return
        if dup.name != "":
            return
        if list(dup.attributes.keys()) != list(self.prefix_attributes.keys()):
            return
        self.prefix_element = dup

    def _link_parents(self) -> None:
        """Owner links for the map hierarchy.

        ``parent`` is a *tree* parent: a node referenced by a ``children``
        element array, or by a single-element attribute (``world``,
        ``entity_properties``, ``meshData``, …).  Other references — e.g.
        ``CVisibilityMgr.nodes``, selection-set ``children`` inside
        ``selectedObjects``, camera/plug arrays — are not tree edges and are
        recorded in ``referenced_by`` instead, so a hidden-flag manager never
        becomes an entity's parent.
        """
        tree_edges: List[Tuple[DmxElement, DmxElement, str]] = []
        single_edges: List[Tuple[DmxElement, DmxElement, str]] = []
        for el in self.elements:
            for name, v in el.attributes.items():
                if isinstance(v, DmxElement):
                    v.referenced_by.append((el, name))
                    single_edges.append((el, v, name))
                elif isinstance(v, list):
                    for item in v:
                        if isinstance(item, DmxElement):
                            item.referenced_by.append((el, name))
                            if name == "children":
                                tree_edges.append((el, item, name))
        for parent, child, name in tree_edges:
            _set_parent(child, parent, name)
        for parent, child, name in single_edges:
            _set_parent(child, parent, name)

    # -- map views ----------------------------------------------------------

    @property
    def root(self) -> Optional[DmxElement]:
        for el in self.elements:
            if el.class_name.endswith("RootElement"):
                return el
        for el in self.elements:
            if el is not self.prefix_element:
                return el
        return self.elements[0] if self.elements else None

    @property
    def world(self) -> Optional[DmxElement]:
        root = self.root
        if root is None:
            return None
        v = root.get("world")
        if isinstance(v, DmxElement):
            return v
        for el in self.elements:
            if el.class_name == "CMapWorld":
                return el
        return None

    @property
    def entities(self) -> List[Entity]:
        if self._entities is None:
            self._entities = [
                Entity(el, props, self)
                for el in self.elements
                for props in (el.get("entity_properties"),)
                if isinstance(props, DmxElement)
            ]
        return self._entities

    def iter_entities(self) -> Iterator[Entity]:
        return iter(self.entities)

    def world_entities(self) -> List[Entity]:
        """Entities reachable from the world tree (worldspawn + descendants)."""
        world = self.world
        if world is None:
            return list(self.entities)
        reachable = {world.guid}
        for node in world.descendants():
            reachable.add(node.guid)
        return [e for e in self.entities if e.guid in reachable]

    def orphan_entities(self) -> List[Entity]:
        """Entities with props that are not reachable from the world tree.

        Should be empty for a well-formed map; a non-empty list means the file
        carries a leftover node (e.g. a deleted entity's shell).  The MCP
        ``list`` should decide whether to surface these.
        """
        reachable = {e.guid for e in self.world_entities()}
        return [e for e in self.entities if e.guid not in reachable]

    def entities_by_class(self) -> Dict[str, List[Entity]]:
        out: Dict[str, List[Entity]] = {}
        for ent in self.entities:
            out.setdefault(ent.classname, []).append(ent)
        return out

    def counts_by_class(self) -> Counter:
        return Counter(el.class_name for el in self.elements)

    def find_entities(
        self,
        query: str,
        limit: Optional[int] = None,
        classname: Optional[str] = None,
    ) -> List[Entity]:
        """Substring search over classname/targetname/nodeID/guid, exact first."""
        q = query.lower()
        hits = [e for e in self.entities if e.matches(q)]
        if classname is not None:
            hits = [e for e in hits if e.classname == classname]
        hits.sort(
            key=lambda e: (
                0
                if e.targetname.lower() == q
                else 1
                if e.classname.lower() == q
                else 2,
                e.node_id if e.node_id is not None else 1 << 30,
            )
        )
        if limit is not None:
            hits = hits[:limit]
        return hits

    def get_entity(self, selector: str) -> Optional[Entity]:
        """Resolve one entity: exact targetname, nodeID, unique GUID prefix,
        or unique classname (in that order).

        Returns None for an empty selector and for ambiguous GUID-prefix or
        classname matches (use :meth:`find_entities` to list candidates).
        Exact targetname/nodeID matches return the first hit — those are the
        stable selectors the MCP server should use; GUIDs are the durable
        handle across saves, nodeID may be renumbered.
        """
        s = selector.strip()
        if not s:
            return None
        for ent in self.entities:
            if ent.targetname and ent.targetname == s:
                return ent
        if s.lstrip("-").isdigit():
            nid = int(s)
            for ent in self.entities:
                if ent.node_id == nid:
                    return ent
        sl = s.lower()
        guid_hits = [e for e in self.entities if e.guid.lower().startswith(sl)]
        if len(guid_hits) == 1:
            return guid_hits[0]
        if guid_hits:
            return None  # ambiguous prefix
        class_hits = [e for e in self.entities if e.classname == s]
        if len(class_hits) == 1:
            return class_hits[0]
        return None

    def stats(self) -> Dict[str, Any]:
        return {
            "path": self.path,
            "header": self.header,
            "format": self.format,
            "format_version": self.format_version,
            "elements": len(self.elements),
            "entities": len(self.entities),
            "orphan_entities": len(self.orphan_entities()),
            "classes": dict(self.counts_by_class().most_common()),
        }


def _set_parent(child: DmxElement, parent: DmxElement, attr: str) -> None:
    if child.parent is None and child is not parent:
        child.parent = parent
        child.parent_attribute = attr


def _index(strings: Sequence[str], idx: int, what: str) -> int:
    if not 0 <= idx < len(strings):
        raise DmxError("%s index %d out of range (table has %d)" % (what, idx, len(strings)))
    return idx


def _type_name(type_id: int) -> str:
    if type_id in SCALAR_TYPE_NAMES:
        return SCALAR_TYPE_NAMES[type_id]
    if type_id in ARRAY_TYPE_NAMES:
        return ARRAY_TYPE_NAMES[type_id] + "_array"
    return "unknown_%d" % type_id


# ---------------------------------------------------------------------------
# value decoding (binary v9)
# ---------------------------------------------------------------------------


def _decode_value(
    r: _Reader,
    type_id: int,
    *,
    prefix: bool,
    strings: Sequence[str],
    elements: Sequence[DmxElement],
) -> Any:
    if type_id in SCALAR_TYPE_NAMES:
        return _decode_scalar(r, type_id, prefix=prefix, strings=strings, elements=elements)
    if type_id in ARRAY_TYPE_NAMES:
        return _decode_array(r, type_id, strings=strings, elements=elements)
    if type_id in _UNSUPPORTED_V9_IDS:
        raise DmxError(
            "attribute type id %d is not part of binary encoding v9" % type_id
        )
    raise DmxError("unknown attribute type id %d at offset %d" % (type_id, r.pos))


def _decode_scalar(
    r: _Reader,
    type_id: int,
    *,
    prefix: bool,
    strings: Sequence[str],
    elements: Sequence[DmxElement],
) -> Any:
    if type_id == 1:  # element
        return _read_element_ref(r, elements)
    if type_id == 2:  # int
        return r.i32()
    if type_id == 3:  # float
        return r.f32()
    if type_id == 4:  # bool
        return bool(r.u8())
    if type_id == 5:  # string: raw in v9 prefixes, table index elsewhere
        return r.raw_str() if prefix else strings[_index(strings, r.i32(), "string")]
    if type_id == 6:  # binary
        return r.bytes(r.i32())
    if type_id == 7:  # time (int32 ticks of 1/10000 s)
        return r.i32()
    if type_id == 8:  # color RGBA
        return tuple(r.bytes(4))
    if type_id == 9:
        return r.f32n(2)
    if type_id == 10:
        return r.f32n(3)
    if type_id == 11:
        return r.f32n(4)
    if type_id == 12:
        return r.f32n(3)
    if type_id == 13:
        return r.f32n(4)
    if type_id == 14:
        return r.f32n(16)
    if type_id == 15:
        return r.u64()
    if type_id == 16:
        return r.u8()
    raise DmxError("unhandled scalar type id %d" % type_id)


def _decode_array(
    r: _Reader, type_id: int, *, strings: Sequence[str], elements: Sequence[DmxElement]
) -> Any:
    count = r.i32()
    if count < 0 or count > len(r.data):
        raise DmxError("implausible array count %d at offset %d" % (count, r.pos))
    if type_id == 33:  # element[]
        return [_read_element_ref(r, elements) for _ in range(count)]
    if type_id == 37:  # string[] (raw NUL-terminated items)
        return [r.raw_str() for _ in range(count)]
    if type_id == 38:  # binary[] (length + bytes per item, -1 = null)
        out = []
        for _ in range(count):
            n = r.i32()
            out.append(None if n == -1 else r.bytes(n))
        return out
    if type_id == 39:  # time[]
        return [r.i32() for _ in range(count)]
    if type_id == 40:  # color[]
        return [tuple(r.bytes(4)) for _ in range(count)]
    sid = type_id - ARRAY_TYPE_OFFSET
    if sid not in SCALAR_TYPE_NAMES:
        raise DmxError("unknown array type id %d" % type_id)
    return [
        _decode_scalar(r, sid, prefix=True, strings=strings, elements=elements)
        for _ in range(count)
    ]


def _read_element_ref(r: _Reader, elements: Sequence[DmxElement]) -> Any:
    idx = r.i32()
    if idx == REF_NULL:
        return None
    if idx == REF_STUB:
        return _StubRef(r.raw_str())
    if idx < 0:
        raise DmxError("negative element reference %d at offset %d" % (idx, r.pos))
    if idx >= len(elements):
        raise DmxError("element reference %d out of range at offset %d" % (idx, r.pos))
    return elements[idx]


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def _fmt_origin(v: Optional[Tuple[float, float, float]]) -> str:
    if v is None:
        return "-"
    return "(%g, %g, %g)" % v


def cmd_info(dm: DmxFile, args: argparse.Namespace) -> int:
    st = dm.stats()
    if args.json:
        print(json.dumps(st, indent=2))
        return 0
    print("path:      %s" % (dm.path or "<memory>"))
    print("header:    %s" % dm.header)
    print("elements:  %d" % st["elements"])
    print("entities:  %d" % st["entities"])
    if st["orphan_entities"]:
        print("orphans:   %d (not reachable from the world tree)" % st["orphan_entities"])
    print("prefix attrs: %s" % ", ".join(sorted(dm.prefix_attributes)))
    print("classes:")
    for cls, n in dm.counts_by_class().most_common():
        print("  %6d  %s" % (n, cls))
    return 0


def cmd_list(dm: DmxFile, args: argparse.Namespace) -> int:
    ents = dm.entities
    if args.classname:
        ents = [e for e in ents if e.classname == args.classname]
    if args.limit:
        ents = ents[: args.limit]
    if args.json:
        print(json.dumps([e.to_dict(with_keyvalues=False) for e in ents], indent=2))
        return 0
    for e in ents:
        print(
            "nodeID=%-5s %-32s targetname=%-24r origin=%s"
            % (e.node_id, e.classname, e.targetname, _fmt_origin(e.origin))
        )
    print("(%d entities)" % len(ents))
    return 0


def cmd_find(dm: DmxFile, args: argparse.Namespace) -> int:
    hits = dm.find_entities(args.pattern, limit=args.limit)
    if args.json:
        print(json.dumps([e.to_dict() for e in hits], indent=2))
        return 0 if hits else 1
    for e in hits:
        print(
            "nodeID=%-5s %-32s targetname=%-24r origin=%s"
            % (e.node_id, e.classname, e.targetname, _fmt_origin(e.origin))
        )
    print("(%d match%s)" % (len(hits), "" if len(hits) == 1 else "es"))
    return 0 if hits else 1


def cmd_get(dm: DmxFile, args: argparse.Namespace) -> int:
    ent = dm.get_entity(args.selector)
    if ent is None:
        print("no entity matches %r" % args.selector, file=sys.stderr)
        return 1
    if args.json:
        print(json.dumps(ent.to_dict(), indent=2))
        return 0
    print("guid:      %s" % ent.guid)
    print("nodeClass: %s" % ent.node_class)
    print("nodeID:    %s" % ent.node_id)
    print("classname: %s" % ent.classname)
    print("targetname: %s" % ent.targetname)
    print("origin:    %s" % _fmt_origin(ent.origin))
    print("angles:    %s" % _fmt_origin(ent.angles))
    print("keyvalues:")
    for k in sorted(ent.keyvalues):
        print("  %-28s %s" % (k, _value_to_text(ent.keyvalues[k])))
    return 0


def _build_arg_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="vmap_dmx.py",
        description="Read a CS2 .vmap (DMX binary v9) and enumerate its entities.",
    )
    p.add_argument("file", help="path to a .vmap file")
    sub = p.add_subparsers(dest="cmd", required=True)

    s = sub.add_parser("info", help="file/class summary")
    s.add_argument("--json", action="store_true")
    s.set_defaults(func=cmd_info)

    s = sub.add_parser("list", help="list entities")
    s.add_argument("--class", dest="classname", default=None, metavar="CLASS")
    s.add_argument("--limit", type=int, default=None)
    s.add_argument("--json", action="store_true")
    s.set_defaults(func=cmd_list)

    s = sub.add_parser("find", help="substring search over classname/targetname/nodeID/guid")
    s.add_argument("pattern")
    s.add_argument("--limit", type=int, default=None)
    s.add_argument("--json", action="store_true")
    s.set_defaults(func=cmd_find)

    s = sub.add_parser("get", help="print one entity (targetname, nodeID, guid or unique class)")
    s.add_argument("selector")
    s.add_argument("--json", action="store_true")
    s.set_defaults(func=cmd_get)
    return p


def main(argv: Optional[Sequence[str]] = None) -> int:
    # strings may carry surrogateescape bytes; never crash while printing
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(errors="backslashreplace")
    args = _build_arg_parser().parse_args(argv)
    try:
        dm = load(args.file)
    except (OSError, DmxError) as exc:
        print("error: %s" % exc, file=sys.stderr)
        return 2
    return args.func(dm, args)


def load(path: Union[str, Path]) -> DmxFile:
    """Parse a ``.vmap`` file and return its :class:`DmxFile` tree."""
    return DmxFile.load(path)


def parse(data: bytes, path: Optional[str] = None) -> DmxFile:
    """Parse in-memory DMX binary v9 bytes."""
    return DmxFile.parse(data, path=path)


if __name__ == "__main__":
    raise SystemExit(main())
