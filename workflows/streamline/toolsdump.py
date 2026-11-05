import json, subprocess, sys
binary, out = sys.argv[1], sys.argv[2]
extra = sys.argv[3:]
p = subprocess.Popen([binary, *extra], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
def send(m): p.stdin.write(json.dumps(m) + "\n"); p.stdin.flush()
def recv(i):
    while True:
        line = p.stdout.readline()
        if not line: raise SystemExit("server closed")
        m = json.loads(line)
        if m.get("id") == i: return m
send({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"dump","version":"0"}}})
init = recv(1)
send({"jsonrpc":"2.0","method":"notifications/initialized"})
tools, cursor, i = [], None, 2
while True:
    send({"jsonrpc":"2.0","id":i,"method":"tools/list","params":({"cursor":cursor} if cursor else {})})
    r = recv(i)["result"]; tools += r["tools"]; cursor = r.get("nextCursor"); i += 1
    if not cursor: break
p.stdin.close(); p.wait(timeout=10)
json.dump({"initialize": init["result"], "tools": tools}, open(out, "w"), indent=1)
d = sum(len(t.get("description","")) for t in tools); s = sum(len(json.dumps(t.get("inputSchema",{}))) for t in tools)
print(f"{binary}: {len(tools)} tools, descriptions {d} chars, schemas {s} chars, instructions {len(init['result'].get('instructions',''))} chars")
