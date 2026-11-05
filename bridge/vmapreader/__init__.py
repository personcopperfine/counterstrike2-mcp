"""vmapreader: importable package wrapper around :mod:`vmap_dmx`.

The bridge tests (`bridge/test_*.py`) put `bridge/vmapreader/` on `sys.path`
and import ``vmap_dmx``; ``load(path)`` is the entry point.  The reader can also
be used standalone (``python3 bridge/vmapreader/vmap_dmx.py FILE list``).

Interface consumed by the MCP adapter:

    load(path) -> DmxFile
        DmxFile.entities -> Sequence[Entity]
        Entity.id / .classname / .origin / .angles / .keyvalues / .guid

Only reads live here; writes are the bridge's or the file route's job.
"""

from .vmap_dmx import (  # noqa: F401
    DmxElement,
    DmxError,
    DmxFile,
    Entity,
    load,
    parse,
)

__all__ = ["DmxError", "DmxElement", "DmxFile", "Entity", "load", "parse"]
