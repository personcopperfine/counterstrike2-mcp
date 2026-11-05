#!/usr/bin/env python3
"""test_geometry.py - live acceptance for the bridge geometry methods (m2.5+).

Needs Hammer with probe.vmap open (runtime.sh cycle). Mutates and SAVES the map; restore from
/tmp/probe.vmap.before-geometry if needed. Steps (each verified by save + parse of the .vmap):

  node.list                  meshes/entities with nodeIDs
  mesh.create  box A (winding 1) + box B (winding 2)   -> new CMapMesh nodes, welded 8 verts / 6 faces
  view.render                both boxes, to see which winding faces outward
  mesh.setGeometry           box A moved +192 x
  mesh.setMaterial           box A -> toolsnodraw
  undo.undo                  material change reverted?
  node.delete                box B removed
"""
import json, os, sys, time
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.join(HERE, "vmapreader"))
from geo_rpc import Rpc
import vmap_dmx
from cs2mcp_root import APP_DIR  # noqa: E402

VMAP = os.path.join(APP_DIR, "content", "csgo_addons", "mcp_test", "maps", "probe.vmap")
MAT = "materials/dev/reflectivity_30.vmat"
MAT2 = "materials/tools/toolsnodraw.vmat"


def box(x0, y0, z0, s, winding):
    v = [[x0 + dx * s, y0 + dy * s, z0 + dz * s] for dz in (0, 1) for dy in (0, 1) for dx in (0, 1)]
    # outward-facing quads, counter-clockwise seen from outside (winding 1)
    f = [[0, 2, 3, 1], [4, 5, 7, 6], [0, 1, 5, 4], [2, 6, 7, 3], [0, 4, 6, 2], [1, 3, 7, 5]]
    if winding == 2:
        f = [list(reversed(q)) for q in f]
    return v, f


def file_meshes():
    f = vmap_dmx.load(VMAP)
    out = {}
    for e in f.elements:
        if e.class_name != "CMapMesh":
            continue
        md = e.get("meshData")
        pos = None
        for s in (md.get("vertexData").get("streams") or []):
            if s.name == "position:0":
                pos = s.get("data")
        fe = md.get("faceEdgeIndices") or []
        nxt = md.get("edgeNextIndices") or []
        ev = md.get("edgeVertexIndices") or []
        faces = []
        for start in fe:
            loop, e2, guard = [], start, 0
            while True:
                loop.append(ev[e2]); e2 = nxt[e2]; guard += 1
                if e2 == start or guard > 1000:
                    break
            faces.append(loop)
        out[e.get("nodeID")] = {"origin": tuple(e.get("origin")), "verts": len(md.get("vertexEdgeIndices") or []),
                                "faces": faces, "pos": pos, "materials": list(md.get("materials") or [])}
    return out


def save(r):
    res = r.mut("map.save", {})
    assert "result" in res, res
    time.sleep(1)


def main():
    r = Rpc()
    checks = {}
    lst = r.call("node.list", {"class": "mesh"})["result"]
    before = file_meshes()
    print("live meshes:", lst["count"], "file meshes:", len(before))
    checks["list_matches_file"] = sorted(n["nodeID"] for n in lst["nodes"]) == sorted(before)

    va, fa = box(256, -512, 0, 128, 1)
    vb, fb = box(512, -512, 0, 128, 2)
    ra = r.mut("mesh.create", {"vertices": va, "faces": fa, "material": MAT})
    rb = r.mut("mesh.create", {"vertices": vb, "faces": fb, "material": MAT})
    print("create A:", json.dumps(ra)[:300]); print("create B:", json.dumps(rb)[:300])
    ida, idb = ra["result"]["nodeID"], rb["result"]["nodeID"]
    rv = r.call("view.render", {"origin": [448, -900, 300], "angles": [20, 90, 0], "width": 960, "height": 540,
                                "name": "geo_boxes_windings"})
    print("render:", json.dumps(rv.get("result", rv))[:200])
    save(r)
    fm = file_meshes()
    for label, i in (("A", ida), ("B", idb)):
        m = fm.get(i)
        print("file mesh %s nodeID %s: origin %s verts %s faces %s mats %s" % (
            label, i, m and m["origin"], m and m["verts"], m and len(m["faces"]), m and m["materials"]))
    checks["create_persisted"] = ida in fm and idb in fm
    checks["welded"] = checks["create_persisted"] and fm[ida]["verts"] == 8 and len(fm[ida]["faces"]) == 6
    checks["pivot_centre"] = checks["create_persisted"] and all(
        abs(a - b) < 0.01 for a, b in zip(fm[ida]["origin"], (320, -448, 64)))

    moved = [[x + 192, y, z] for x, y, z in va]
    rs = r.mut("mesh.setGeometry", {"nodeID": ida, "vertices": moved, "faces": fa, "material": MAT})
    print("setGeometry:", json.dumps(rs)[:300])
    rm = r.mut("mesh.setMaterial", {"nodeID": ida, "material": MAT2})
    print("setMaterial:", json.dumps(rm)[:300])
    save(r)
    fm = file_meshes()
    m = fm.get(ida)
    print("after move+material: origin %s mats %s" % (m and m["origin"], m and m["materials"]))
    checks["moved"] = bool(m) and abs(m["origin"][0] - 512) < 0.01
    checks["material"] = bool(m) and MAT2 in m["materials"]

    ru = r.mut("undo.undo", {})
    print("undo:", json.dumps(ru)[:300])
    save(r)
    fm = file_meshes()
    m = fm.get(ida)
    print("after undo: origin %s mats %s" % (m and m["origin"], m and m["materials"]))
    checks["undo_material"] = bool(m) and MAT2 not in m["materials"] and abs(m["origin"][0] - 512) < 0.01

    rd = r.mut("node.delete", {"nodeIDs": [idb]})
    print("delete:", json.dumps(rd)[:300])
    save(r)
    fm = file_meshes()
    checks["deleted"] = idb not in fm and ida in fm
    print("checks:", checks)
    print("RESULT:", "PASS" if all(checks.values()) else "FAIL")


if __name__ == "__main__":
    main()
