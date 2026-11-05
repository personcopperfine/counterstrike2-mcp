"""geo_rpc.py - tiny client for the bridge's geometry methods (tests/experiments)."""
import json, os, socket
import sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))  # noqa: E402
from cs2mcp_root import PROJECT_ROOT  # noqa: E402

DISC = os.path.join(PROJECT_ROOT,
                    "workshop/run/app/compatdata/730/pfx/drive_c/users/steamuser/AppData/Local/cs2mcpbridge/session.json")


class Rpc:
    def __init__(self, timeout=120):
        d = json.load(open(DISC))
        self.s = socket.create_connection((d["host"], d["port"]), timeout=timeout)
        self.f = self.s.makefile("rb")
        self.n = 0
        self.call("control.hello", {"protocol": 1, "token": d["token"]})

    def call(self, method, params=None):
        self.n += 1
        self.s.sendall((json.dumps({"jsonrpc": "2.0", "id": "g%d" % self.n, "method": method,
                                    "params": params or {}}) + "\n").encode())
        return json.loads(self.f.readline())

    def expect(self):
        d = self.call("doc.active")["result"]["doc"]
        return {"id": d["id"], "epoch": d["epoch"]}

    def mut(self, method, params):
        p = dict(params)
        p["expectDoc"] = self.expect()
        return self.call(method, p)
