# 05 — Multiplayer: dedicated server + clients in containers

## Goal
Bring up a dedicated server and connect one or more clients (real or bots) so multiplayer logic,
networking and round flow can be tested; expose it as `server_up`, `client_connect`, `bots_add`.

## Prerequisites
- 01, 03, 04. Facts: `libengine2.so` carries dedicated-server strings; `../cs2-gym/docs/DESIGN.md`
  plans "a real CS2 dedicated server in podman (`harness/run-cs2.sh`)" as its oracle but has not run
  one yet; gbe `steam_settings` ship `disable_networking=1`.
- **RAM**: one CS2 client ~13 GB RSS (map runs 14–15 GB), host ~30 GB, no swap -> **at most one real
  client per host**; a dedicated server's footprint is unknown (measure first).

## Steps
1. **Dedicated server flavour** (`SessionKind::LinuxDedicated`): `cs2 -dedicated -insecure
   -console -usercon +sv_lan 1 +map de_dust2 +game_type 0 +game_mode 0` in the Linux image, no X;
   verify it starts without a GSLT in LAN mode; record RSS; log to `console.log`/stdout.
   Expose `server_console_exec` (rcon via `-usercon` + `rcon_password`, or stdin).
2. **One real client + bots**: WineTools or LinuxClient session with `connect 127.0.0.1:27015`
   (same `--network=host`); `bot_add_ct/bot_add_t`, `bot_stop`, `bot_mimic` for scripted opponents;
   SourceTV on the server (`tv_enable 1`, `tv_record`) for demos (07).
3. **Multiple real clients** = multiple hosts (or a bigger box): design `Runtime` so a client
   session can target a remote podman host (`CONTAINER_HOST=ssh://...`) — later; on this host the
   tool refuses a second client with `AnotherCs2Running`.
4. **Match control tools**: `match_setup {map, mode, bots}` (server cvars preamble), `round_restart`
   (`mp_restartgame 1`), `match_state` (parse server console: round, scores, players), `kick_all`.
5. **Determinism knobs** for tests: `sv_cheats 1`, `mp_freezetime 0`, `mp_warmup_end`,
   `mp_roundtime`, `bot_freeze`, `host_timescale` (server-side, affects clients).
6. Tests: argv builders; a live test that starts the server, waits for `Connection to Steam servers`
   / map loaded lines, connects the client and sees `client_connected`.

## Definition of done
- Server + one client + 2 bots play a round headlessly; `match_state` reports scores; a SourceTV
  demo of the round exists.

## Open questions
- Does `cs2 -dedicated` run under gbe without Steam (GSLT, `sv_lan 1`, `sv_setsteamaccount`)? Unverified.
- gbe `disable_networking=1` only affects gbe's interfaces; the game's own networking library
  (SteamNetworkingSockets) retries SDR certs — does direct `connect ip:port` work with
  `net_public_adr`/`sv_lan 1`? Unverified.
- Client-side bridge hooks for *multiple* clients (one bridge per process, one TCP port each:
  discovery file per prefix) — only matters off-host.

## Evidence
- None for the dedicated server yet; LAN strings and cvars exist in the binaries [D/S level].

## Status: live-tested to ServerLive and RCON; log path fixed, final re-test pending

Scripts: `docker/linux-dedicated.sh` runs `cs2.sh -dedicated` with no `-condebug`, no `+con_logfile`
and no mirror of the shared csgo/console.log. CS2 has no `con_logfile` command ("Unknown command
'con_logfile'!" in the live log). Without `-condebug` the full server log goes to stdout, and the
script tees stdout and stderr to `<artifacts>/server.log`. The ServerLive check reads that file.
`docker/lib-preamble.sh` is the shared preamble (flock on `/home/steam/.cs2mcp-preamble.lock`; a repeat
run changes nothing; a running peer's freetype stays aside and its links are not rewritten).

Observed live (host, LAN, anonymous, gbe): the server boots in about 3 s to about 510 MB RSS, map
de_dust2. Its stdout lines carry no timestamp or channel prefix, so markers are plain substrings.
`ServerLive` accepts any of these (server.log line numbers from the live run):
- `Network socket 'server' opened on port 27015` (line 179)
- `CNetworkGameServerBase::SetServerState (ss_loading -> ss_active)` (line 259)
- `Server is hibernating` (seen; the server prints it with no clients)

RCON (observed): the TCP listener is on the host's address (`127.0.1.1:27015` here), not 127.0.0.1.
UDP is on `*:27015`. Auth gets one `type 2` reply, with no empty `type 0` before it. CS2 does not
answer the empty sentinel command, so a command's reply ends after a 300 ms quiet period. `status`
replies in one packet, and `bot_add_ct` / `bot_add_t` work. RCON candidates, in order: 127.0.0.1,
the hostname's IPv4 addresses, and the `udp/ip :` line of server.log. A fresh MCP process found
127.0.1.1 through these candidates. `+ip 0.0.0.0` is untested; pass it in `args` to try it.

Session state: `<artifacts>/server-session.json` (mode 0600) holds the RCON password and port.
`server_console_exec` reads it, so it works from any MCP process. An explicit `rconPassword` overrides it.

Known open items:
- `launch_session {connect}` does NOT work: the client joins its own loopback listen server. Open.
- `client_connect` on a linux-client is NotImplemented (no command route).
- A client still writes the shared csgo/console.log in the game dir.

### Live verification (orchestrator; needs the image and the game in `cs2-home`)
1. `session_status`: `toolGroups.multiplayer.available: true`.
2. `server_up {map: "de_dust2", rconPassword: "verify-pw", timeoutMs: 180000}`. Expect a
   `cs2mcp-linux-dedicated-<hex>` container, `readiness.targetReached: true`, and `ServerLive` in
   `reached`. Note `artifactsDir`.
3. In `<artifactsDir>/server.log` expect the live markers above, and no `Unknown command 'con_logfile'`.
4. `server_console_exec {command: "status"}` returns a non-empty `response`.
   `server_console_exec {command: "quit"}` is refused with `ForbiddenCommand`.
5. `server_up` again with the same args while the server runs: `reused: true`, no second container.
6. `launch_session {kind: "linux-client", waitFor: "ContainerUp"}` while the server runs: no
   `KindBusy`. Both preambles log `PREAMBLE-LOCK-HELD`.
7. `stop_session` on both containers; `podman ps -a --filter name=cs2mcp-` shows nothing left.

Each failing step is a finding for this file.
