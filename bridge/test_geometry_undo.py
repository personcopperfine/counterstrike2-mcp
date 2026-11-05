#!/usr/bin/env python3
"""test_geometry_undo.py - undo/redo-side and round-trip checks for the geometry methods (m2.5+).

All verified by save + parse. Restore the map from /tmp/probe.vmap.before-geometry afterwards.
  1. create C, undo                -> C gone
  2. create D, setGeometry (move), undo -> D back at its first position
  3. delete D, undo                -> D present again
  4. create E with faceMaterials   -> two materials, top face uses the second
  5. setGeometry on an ORIGINAL mesh with its own geometry read from the file (world coords)
     -> same vertex positions (world) and face count after save
"""
import os, sys, time
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from geo_rpc import Rpc
from test_geometry import box, file_meshes, save, MAT, MAT2


def world_geom(m):
    ox, oy, oz = m["origin"]
    return [[p[0] + ox, p[1] + oy, p[2] + oz] for p in m["pos"]], m["faces"]


def main():
    r = Rpc()
    checks = {}
    # 1
    vc, fc = box(0, -900, 0, 64, 1)
    idc = r.mut("mesh.create", {"vertices": vc, "faces": fc, "material": MAT})["result"]["nodeID"]
    print("undo create:", r.mut("undo.undo", {}).get("result"))
    save(r)
    checks["undo_create"] = idc not in file_meshes()
    # 2
    vd, fd = box(-512, -900, 0, 64, 1)
    idd = r.mut("mesh.create", {"vertices": vd, "faces": fd, "material": MAT})["result"]["nodeID"]
    r.mut("mesh.setGeometry", {"nodeID": idd, "vertices": [[x, y + 300, z] for x, y, z in vd], "faces": fd, "material": MAT})
    print("undo setGeometry:", r.mut("undo.undo", {}).get("result"))
    save(r)
    m = file_meshes().get(idd)
    print("D after undo:", m and m["origin"], m and m["verts"], m and len(m["faces"]))
    checks["undo_set_geometry"] = bool(m) and abs(m["origin"][1] - (-868)) < 0.01 and m["verts"] == 8
    # 3
    r.mut("node.delete", {"nodeIDs": [idd]})
    print("undo delete:", r.mut("undo.undo", {}).get("result"))
    save(r)
    checks["undo_delete"] = idd in file_meshes()
    # 4
    ve, fe = box(-800, -900, 0, 64, 1)
    mats = [None] * 6
    mats[1] = MAT2          # face 1 = top (z = 64)
    re = r.mut("mesh.create", {"vertices": ve, "faces": fe, "material": MAT, "faceMaterials": mats})
    print("create E:", re.get("result"))
    ide = re["result"]["nodeID"]
    save(r)
    m = file_meshes().get(ide)
    print("E materials:", m and m["materials"])
    checks["face_materials"] = bool(m) and sorted(m["materials"]) == sorted([MAT, MAT2])
    # 5
    fm = file_meshes()
    orig = fm.get(3)
    wv, wf = world_geom(orig)
    mats0 = orig["materials"]
    rs = r.mut("mesh.setGeometry", {"nodeID": 3, "vertices": wv, "faces": wf, "material": mats0[0]})
    print("setGeometry original:", rs.get("result") or rs.get("error"))
    save(r)
    after = file_meshes().get(3)
    wv2, wf2 = world_geom(after)
    same_pts = sorted(tuple(round(c, 2) for c in p) for p in wv) == sorted(tuple(round(c, 2) for c in p) for p in wv2)
    print("original mesh: verts %d->%d faces %d->%d positions identical: %s" % (
        len(wv), len(wv2), len(wf), len(wf2), same_pts))
    checks["roundtrip_original"] = same_pts and len(wf) == len(wf2)
    print("checks:", checks)
    print("RESULT:", "PASS" if all(checks.values()) else "FAIL")


if __name__ == "__main__":
    main()
