# `vmap` fixtures — SAVED CS2 maps + the Python oracle's entity records

All three `.vmap` files are **our own** addon content (`csgo_addons/mcp_test`), not Valve binaries,
so they are committed:

| File | Source | Size |
|---|---|---|
| `probe.vmap` | `workshop/run/app/content/csgo_addons/mcp_test/maps/probe.vmap` | 224 KB |
| `probe_scaled.vmap` | `probe.vmap` with one attribute changed (see below) | 224 KB |
| `probe_conn.vmap` | `probe.vmap` with one `connectionsData` entry added (see below) | 224 KB |
| `defuse_poc.vmap` | `workshop/run/app/content/csgo_addons/mcp_test/maps/defuse_poc.vmap` | 247 KB |

`defuse_poc.vmap` is 247 KB (< 2 MB), so it is included as well (the WP-R brief only said to skip it
above 2 MB).

### `probe_scaled.vmap` (the review-2 M1 fixture)

`probe.vmap` with the **object-properties `scales` of nodeID 3** changed from `[1, 1, 1]` to
`[2, 2, 1]` — 4 bytes (`1.0f -> 2.0f` in two components) at the `scales` value vector, byte offset
115560 (the components' high bytes are 0-based 115562/115563 and 115566/115567). It exists for the
file route's scaled-node refusal: the extraction applies the node's own scales to produce WORLD
vertices, but the bridge's `mesh.setGeometry` resets only the node's angles and pivot, so a rebuild
from such a file would scale the vertices a second time.

The generator is committed next to the fixtures (review-3 NEW-6: this used to live in the
worker's uncommitted scratch): `hammer-mcp/fixtures/vmap/patch_scales.py`. It walks the
attribute-body section exactly like `hammer-mcp/src/vmap/dmx.rs`, so it can find the wanted
element's `scales` vector and rewrite those 3 f32s, and it **reproduces the committed file byte for
byte** (that is what it verifies by default):

```sh
python3 -I hammer-mcp/fixtures/vmap/patch_scales.py           # regenerate + verify
python3 -I hammer-mcp/fixtures/vmap/patch_scales.py --check   # verify only, no write
# -> probe_scaled.vmap reproduced byte for byte: nodeID 3 scales [2.0, 2.0, 1.0] at offset 115560,
#    sha256 a6e6da2f96898c35e275f3d90f9f2a0224826646a0ec8dcacec6952d1e6e4408
```

The general form patches any binary-9 `.vmap`:
`patch_scales.py <src.vmap> <dst.vmap> <nodeID> <sx> <sy> <sz>`.

### `probe_conn.vmap` (the review C1 / FIX-C fixture)

`probe.vmap` with func_buyzone **nodeID 67's `connectionsData`** given one `DmeConnectionData`
entry — every other fixture's `connectionsData` is empty, so the file route's I/O connection path
(what `list_triggers`/`get_entity` read) had no non-`#[ignore]`d cover. The generated map appends
one `DmeConnectionData` DMX element with the installed
`content/csgo_addons/cs_script_demo/maps/cs_script_demo.vmap`'s attribute shape (`outputName`
"OnStartTouch", `targetType` 7, `targetName` "ta_kill_t", `inputName` "Kill", `overrideParam` "",
`delay` 0.0, `timesToFire` -1; the live a3.log wire reports the same) and points node 67's
(previously empty) element array at it.

The generator committed next to the fixtures (`hammer-mcp/fixtures/vmap/patch_connections.py`,
same pattern as `patch_scales.py`) appends the table strings, one element index row and one
element body (DMX references are indices, never byte offsets), and reproduces the committed file
byte for byte; the connection record was verified against the Python oracle:

```sh
python3 -I hammer-mcp/fixtures/vmap/patch_connections.py           # regenerate + verify
python3 -I hammer-mcp/fixtures/vmap/patch_connections.py --check   # verify only, no write
# -> probe_conn.vmap: connectionsData of nodeID 67 -> DmeConnectionData ref 626
```

The general form adds one connection to any binary-9 `.vmap`:
`patch_connections.py <src.vmap> <dst.vmap> <nodeID> <output> <target> <input> <param> <targetType> <delay> <timesToFire>`.
Consumer: `hammer-mcp/tests/triggers_query.rs`
(`file_connections_come_from_connections_data`; the entity still classifies as a non-trigger
under the fixture FGD, so `kind:"solid"` lists it).

The `*.entities.json` goldens are **generated** by the oracle reader, never edited by hand. They
hold one `Entity.to_dict` record per entity — `guid`, `nodeClass`, `nodeID`, `referenceID`,
`classname`, `targetname`, `origin`, `angles`, `scales`, `connections` and the full `keyvalues` map
— sorted by `nodeID` so the checked-in file is stable.

Generator (from the repo root; `find ""` matches every entity, `--json` prints the records; the
helper goes under `target/`, never `/tmp`, which is a RAM tmpfs):

```sh
mkdir -p target/gen
cat > target/gen/sortgolden.py <<'EOF'
import json, sys
d = json.load(sys.stdin)
d.sort(key=lambda e: (e["nodeID"] is None, e["nodeID"]))
json.dump(d, sys.stdout, indent=2)
sys.stdout.write("\n")
EOF
for name in probe defuse_poc; do
  python3 -I bridge/vmapreader/vmap_dmx.py "hammer-mcp/fixtures/vmap/$name.vmap" find "" --json \
    | python3 -I target/gen/sortgolden.py > "hammer-mcp/fixtures/vmap/$name.entities.json"
done
```

`hammer-mcp/tests/vmap_dmx.rs` compares the Rust reader's `Entity::to_json(true)` for every entity
against these files. The oracle is a **dev-time** tool: no python runs inside `cargo test`.

### Mesh goldens (`*.meshes.json`)

The `*.meshes.json` goldens are generated by the POC's mesh extractor (the same
`../idabins/worknotes/mcpserver/vmap_reader.py` the tools port): one `MeshRecord.to_dict`
record per usable `CMapMesh` element, in element order (`nodeID`, raw `origin`/`angles`,
counts, `materials`, 3-decimal `bounds`, plus `vertices` (4 decimals), `faces` and
`faceMaterials`). Both fixtures hold 18 / 35 usable CMapMeshes (all with `angles` `[0,0,0]` and
`scales` `[1,1,1]`, one wedge with two materials — `probe` nodeID 106). Generator (from the repo
root):

```sh
python3 -I - <<'EOF'
import sys, json
sys.path.insert(0, 'idabins/worknotes')            # the POC package (mcpserver.vmap_reader)
sys.path.insert(0, 'bridge/vmapreader')            # the oracle DMX reader (DmxFile)
from mcpserver.vmap_reader import _mesh_from_element
from vmap_dmx import DmxFile
for name in ('probe', 'defuse_poc'):
    f = DmxFile.load(f'hammer-mcp/fixtures/vmap/{name}.vmap')
    records = [r.to_dict() for e in f.elements
               if e.class_name == 'CMapMesh' and (r := _mesh_from_element(e))]
    json.dump(records, open(f'hammer-mcp/fixtures/vmap/{name}.meshes.json', 'w'), indent=1)
    open(f'hammer-mcp/fixtures/vmap/{name}.meshes.json', 'a').write('\n')
EOF
```

`hammer-mcp/tests/vmap_mesh.rs` compares the Rust `MeshRecord::to_json` for every mesh against
these (exact for faces/materials/ids, `1e-9` tolerance for the rounded floats).

Counts at generation time: `probe.vmap` 69 entities / 887 keyvalues, `defuse_poc.vmap` 25 entities /
283 keyvalues. Every keyvalue in both maps is a DMX string attribute (plan §2 fact F1) and every
`connectionsData` array is empty, so these goldens do **not** cover the I/O connection path.

That path is covered instead by the `#[ignore]`d real-tree tests, which read (read-only) two maps
of the installed game and compare against the oracle:

| Map (under the install's `content/`) | Entities | With connections | Node checked |
|---|---|---|---|
| `csgo/maps/templates/template_wingman.vmap` | 40 | 1 | nodeID 73 `logic_auto`: `OnMapSpawn` -> `script.2v2` `RunScriptCode` `"EnableWingman()"` |
| `csgo_addons/cs_script_demo/maps/cs_script_demo.vmap` | 183 | 12 | nodeID 296 `trigger_multiple` "trigger.grenade": `OnTrigger` -> `scriptent` `RunScriptInput` `"GrenadeHit"` |

Both maps stay in the install (Valve content is never copied into the repo; the `entities_file`
test copies the map into its own temp app tree under `target/`). The tests fail loudly when the
game tree is missing rather than skipping:

```sh
cargo test -p hammer-mcp --test vmap_dmx     -- --ignored
cargo test -p hammer-mcp --test entities_file -- --ignored
cargo test -p hammer-mcp --test vmap_mesh    -- --ignored
cargo test -p hammer-mcp --test mesh_file    -- --ignored
```
