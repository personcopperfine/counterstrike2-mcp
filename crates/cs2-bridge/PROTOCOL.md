# cs2-bridge JSON-RPC protocol

This is the wire contract of the bridge's JSON-RPC endpoint: what a client (`client-mcp`,
`hammer-mcp`, or anything else holding the session token) sends, and what the bridge answers.
It describes the protocol as the code implements it today. The code is the authority:
`src/registry.rs` (the method table), `src/ipc/` (transport and handshake), `src/control.rs`,
`src/queue.rs`, `src/dispatch.rs`, `src/wiring.rs` and `src/ops/**` (the method bodies). If this
document and the code disagree, the code is right and this document has a bug.

Cite a section as `crates/cs2-bridge/PROTOCOL.md §9.2`. Section numbers are stable: a new method
gets a new subsection at the end of its group, and a removed one keeps its number with a
"removed" line.

## 1. Conventions

- **Protocol version 1.** `protocol` is 1 in `control.hello`, `control.version`, the discovery file
  and every reply's `bridge` member. Changes so far have been additive (new methods, new result
  members appended), so a client should ignore members it does not know.
- **Build string.** `bridge.build` and `bridgeVersion` carry the bridge build (`BRIDGE_BUILD` in
  `src/lib.rs`, currently `m3.18`). It names a build, not a protocol level: gate features on
  `control.capabilities.methods`, not on the build string.
- **Game build.** `gameBuild` is the Steam build id the pins were generated for
  (`PIN_GAME_BUILD` in `src/generated/pins.rs`).
- **Notation.** `{a, b?}` means member `b` is optional. `x|null` means the member is present and
  may be `null`. An integer range `1..64` includes both ends. "Refusal" means an error reply sent
  before anything in the game changed, unless the row says otherwise.
- **Evidence markers.** `[V live]` means the behaviour was observed against a running game or tools
  session. `[V bin]` means it was read in the game binary. Statements without a marker come from
  the code.

## 2. Transport and discovery

### 2.1 Discovery file

On start the bridge writes `session.json` and removes it on a clean stop.

| Host | Directory |
|---|---|
| Windows (and Wine) | `%LOCALAPPDATA%\cs2mcpbridge\`; if `LOCALAPPDATA` is unset, the default user profile's local app-data directory |
| Linux | `$XDG_RUNTIME_DIR/cs2mcpbridge/`, else `$HOME/.local/share/cs2mcpbridge/` |

```json
{"pid":1234,"protocol":1,"bridgeVersion":"m3.18","gameBuild":"2000940","transport":"tcp",
 "host":"127.0.0.1","port":49731,"pipeName":null,"token":"<64 hex>","startedAt":"..."}
```

- The file is written atomically (temporary file, then rename).
- On Linux the directory must be owned by the effective user and is mode 0700; the file is
  created with `O_EXCL|O_NOFOLLOW` and mode 0600.
- `token` is 32 bytes from the OS random generator, written as 64 lowercase hex digits. A new token
  is made on every start. If the random generator fails, the bridge fails closed: no file, no
  listener.

### 2.2 Connection rules

- TCP on `127.0.0.1` only, on an ephemeral port (the `port` in the discovery file).
- One authenticated connection at a time. A second connection is answered `-32007 Busy` and
  closed.
- A new connection has 5000 ms to send `control.hello`. Otherwise it is closed.
- A write that cannot complete within 2 s closes the connection.

### 2.3 Framing

- NDJSON: one JSON object per line, terminated by `\n`, UTF-8.
- A line may be at most 1048576 bytes (advertised as `limits.maxLineBytes`). A longer line is
  answered `-32600 "line too long"` and the connection is closed.
- Empty lines and lines that start with a NUL byte are skipped.
- Replies are lines too, and a reply never exceeds `maxLineBytes`. Each method caps its own result
  (see the per-method limits).

### 2.4 Handshake (`control.hello`)

The first frame must be:

```json
{"jsonrpc":"2.0","id":"1","method":"control.hello","params":{"protocol":1,"token":"<64 hex>"}}
```

The bridge reads the first `protocol` member and the first `token` member in the line.

| Condition | Reply, then |
|---|---|
| no `method` or no `jsonrpc` member | `-32700 "parse error"`, close |
| any method other than `control.hello` | `-32001 NotAuthenticated`, close |
| `protocol` is not 1 | `-32002 VersionMismatch`, close |
| missing or wrong token (constant-time compare) | `-32001 NotAuthenticated`, close |
| success | `{protocol:1, bridgeVersion, gameBuild, capabilities}`; `capabilities` is the `control.capabilities` object (§7.4) |

After authentication a second `control.hello` is answered `-32601`.

## 3. Envelope

### 3.1 Requests

`{"jsonrpc":"2.0","id":"<string>","method":"<name>","params":{...}}`

- `jsonrpc` only has to be present. Its value is not checked.
- `id` must be a JSON **string** (at most 127 bytes). A numeric id counts as absent. An
  authenticated request without a string id is answered `-32600 "invalid request: id required"`.
- `method` is at most 63 bytes.
- `params` is used only when it is an object; anything else counts as no params.
- `params.deadlineMs` (optional, §4.3) bounds the time a request may wait in the queue.

### 3.2 Results

`{"jsonrpc":"2.0","id":"<id>","result":{...},"bridge":{"protocol":1,"build":"m3.18"}}`

Every reply, results and errors alike, carries the `bridge` member.

### 3.3 Errors

`{"jsonrpc":"2.0","id":"<id>","error":{"code":-32602,"message":"InvalidParams","data":{...}},"bridge":{...}}`

- `message` is the error **name** from §5 (for example `InvalidParams`). The two framing errors
  are the exceptions: their message is the literal text (`"parse error"`, `"line too long"`,
  `"invalid request: id required"`, `"invalid params: ..."`).
- `data` is optional. Its common shapes:

| `data` | Meaning |
|---|---|
| `{"reason":"<text>"}` | the usual refusal: `reason` says what was wrong |
| `{"reason":"<text>","maybeExecuted":true}` | the failure came after the game call started: the change may have landed. Read the state back before retrying |
| `{"blockedOn":"<text>"}` | the method is not implemented, or depends on something absent (KHook, Metamod, Windows) |
| `{"module":"<x.dll>","check":"identity","state":"<state>"}` | the generic prerequisite gate (§4.2) |
| method-specific members | listed in the method's section |

- A refusal's `data` object is at most 512 bytes.
- If a method's result does not fit its buffer, the reply is `-32603` with the method's overflow
  text as the message and no `data`. The text is listed in the method's section where it differs
  from `"InternalError"`.

## 4. Dispatch

### 4.1 Gate order

For an authenticated request with an id:

1. `control.*` is answered on the IPC thread (§7). An unknown `control.*` name is `-32601`.
2. A name not in the method table: `-32601 MethodNotFound`.
3. A row that is in the table but not implemented: `-32011 NotImplemented {"blockedOn":"<text>"}`
   (§24), answered at once, without queueing.
4. A row answered on the IPC lane (§4.5) runs at once.
5. Every other row is queued for the game's main thread (§4.3). The remaining checks run when it
   is drained:
   - a needed module is not verified: `-32017 PrerequisiteFailed`
     `{"module":"<x.dll>","check":"identity","state":"<state>"}` (§4.2);
   - every module is verified, but the row registers a KHook hook and no `IKHook` is present:
     `-32017 PrerequisiteFailed {"blockedOn":"metamod: KHook not loaded"}`;
   - otherwise the method body runs. A panic inside a body is caught and answered
     `-32603 InternalError {"reason":"exec panicked"}`.

### 4.2 Prerequisites

Each row names the modules it needs (`needs` in `src/registry.rs`). A module is usable once the
bridge has verified its identity against the pins.

| Bit | Module |
|---|---|
| `NEED_TF2` 1 | `toolframework2.dll` |
| `NEED_HAMMER` 2 | `hammer.dll` |
| `NEED_SCENE` 4 | `scenesystem.dll` |
| `NEED_ENGINE` 8 | `engine2.dll` |
| `NEED_RSV` 16 | `rendersystemvulkan.dll` |
| `NEED_CLIENT` 32 | `client.dll` |
| `NEED_SERVER` 64 | `server.dll` |
| `NEED_TIER0` 128 | `tier0.dll` |
| `NEED_SCHEMA` 256 | `schemasystem.dll` |
| `NEED_SOUND` 512 | `soundsystem.dll` |
| `NEED_MODELDOC` 1024 | `modeldoc_editor.dll` |
| `NEED_PET` 2048 | `pet.dll` |
| `NEED_KHOOK` 4096 | not a module: an `IKHook` from Metamod must be present |

- `NEED_RENDER` = scene + engine + rsv. `NEED_INPUT` = client + engine.
- When several modules are missing, the error names the first one in this order: tf2, scenesystem,
  engine2, rsv, hammer, client, server, tier0, schema, sound, modeldoc, pet.
- Module states: `not_loaded`, `verifying`, `verified`, `mismatch`. `modeldoc_editor.dll` is
  `not_loaded` until a ModelDoc window opens. `pet.dll` is `not_loaded` until `pet.load` (§17.2)
  maps it.
- `control.capabilities.methods` lists the implemented, non-hidden rows whose module needs are met.
  `NEED_KHOOK` is always treated as met there: a hook row is advertised because the build
  implements it, and whether it can run right now is the gate's answer. The capabilities
  `revision` moves when a module's state changes.
- Rows that need KHook: `game.saveRestore`, `input.set`, `sound.subscribe`.

### 4.3 Queue, deadlines and cancellation

- The queue holds 64 requests. When it is full, a new request is answered `-32007 Busy`.
- `params.deadlineMs`: the default is 30000; 0 means no deadline. A negative value is answered
  `-32602 "invalid params: deadlineMs < 0"` (no `data`). A request still queued when its deadline
  passes is answered `-32008 QueueTimeout`. A modal dialog on the main thread therefore shows up
  as queue timeouts, while `control.*` keeps answering.
- The IPC thread sweeps expired requests from the head of the queue, so a client gets its
  `QueueTimeout` even while the main thread is stuck. A request behind a live one is checked when
  it reaches the drain.
- `control.cancel {id}` (§7.6) removes a request that is still queued. A request already running
  cannot be cancelled.

### 4.4 Main-thread drain

- The game's main thread drains the queue once per tick: at most 4 requests per tick, within an
  8 ms budget, never re-entered.
- Every queued method body runs on the main thread. That is why Hammer, ModelDoc, pet and game
  reads are safe, and also why a body that blocks (a modal dialog) blocks the drain.
- `control.ping` reports `mainThreadAgeMs` (time since the last drain tick) and `queueDepth`, so a
  client can tell a busy main thread from a dead one.

### 4.5 Rows answered on the IPC lane

These run on the IPC thread, need no module, and are never queued: `control.*`, `job.*` and
`metamod.plugins`. They answer while the main thread is blocked.

## 5. Error codes

| Code | Name | Meaning |
|---|---|---|
| -32700 | `parse error` | the frame has no `method` or no `jsonrpc` (before auth: then close) |
| -32600 | `line too long` / `invalid request: id required` | framing (§2.3, §3.1) |
| -32601 | `MethodNotFound` | unknown method, or a second `control.hello` |
| -32602 | `InvalidParams` | bad params; `data.reason` says which. Also `UnloadableFile` (§17.1) and the bare `invalid params: ...` texts |
| -32603 | `InternalError` | a game call failed, a result overflowed, or a body panicked. With `maybeExecuted:true` the change may have landed |
| -32001 | `NotAuthenticated` | no hello yet, or a bad token |
| -32002 | `VersionMismatch` | hello `protocol` is not 1 |
| -32003 | `NoActiveDocument` | no usable active document (Hammer, ModelDoc or pet); `data.reason` names the failing step |
| -32004 | `DocumentChanged` | `expectDoc` does not match the active document (§6.3) |
| -32005 | `StaleHandle` | an entity handle from an earlier epoch, or unknown (§6.6) |
| -32006 | `UnknownClass` | the class is not in Hammer's node-factory registry |
| -32007 | `Busy` | the queue is full, or another connection is authenticated |
| -32008 | `QueueTimeout` | the request's deadline passed while queued |
| -32010 | `NotCancellable` | `control.cancel` on a request that is not queued |
| -32011 | `NotImplemented` | the row is blocked (§24), a disallowed op inside `edit.batch`, or `console.exec` outside a test/tools session |
| -32012 | `PartialFailure` | an `edit.batch` op failed after earlier ops ran (§10) |
| -32014 | `AssetNotFound` | the file a path names does not exist or is not a usable file |
| -32015 | `UntitledDocument` | the document has no path (Save As would open a modal) |
| -32016 | `UnsavedChanges` | a reload would discard changes; pass `force:true` |
| -32017 | `PrerequisiteFailed` | a module, interface, object or state the method needs is missing; `data` has `module`, `reason` or `blockedOn` |
| -32018 | `DocumentNotEditable` | the document or node cannot take an edit right now (§6.2) |
| -32019 | `NoViewport` | no render target for a render (`data.reason` names the step) |
| -32020 | `RenderFailed` | the render ran but produced nothing usable |
| -32021 | `NoGameWorld` | `world.render` with no map loaded in the engine |
| -32022 | `NodeNotFound` | no node with that `nodeID` in the live document |
| -32023 | `SchemaUnavailable` | a schema scope, class or field the read needs is missing |
| -32024 | `NoLocalPlayer` | no local controller or pawn (not in a game, dead, spectating) |
| -32025 | `NoGameRules` | no `cs_gamerules` entity with a `C_CSGameRules` object |
| -32026 | `ConnectionNotFound` | `entity.removeConnections` filter matched nothing |

Codes -32009 and -32013 are unused.

## 6. Hammer document model

This section applies to every `doc.*`, `map.*`, `undo.*`, `entity.*`, `edit.batch`, `node.*`,
`mesh.*`, `selection.*` and `view.render` call. All of them need `hammer.dll` and run on the main
thread.

### 6.1 Document id and epoch

- The active document is identified by `id` = `"d<N>"`. N increases whenever Hammer's active
  document session changes (another tab, a new or opened map).
- `epoch` increases whenever the bridge sees the active document change underneath it: a new
  session, a new root node, a new path, or a replaced root handle. A reload bumps it.
- `doc.active` (§8.1) reports both. Clients pass them back as `expectDoc`.

### 6.2 Gates

**Read gate** (`entity.get`, `entity.find`, `node.list`, `selection.get`, `view.render`):
the active document must resolve, otherwise `-32003 NoActiveDocument {reason}`. `mesh.get`
also checks `expectDoc` when one is given.

**Write gate** (every method that changes the document, plus `selection.set` and `undo.*`),
in this order:

1. `-32003 NoActiveDocument {reason}`. `reason` is one of: the module state reason,
   `hammer_unavailable`, `no_session`, `bad_session`, `no_document`, `bad_doc_session`,
   `doc_session_backptr`, `bad_doc`, `doc_backptr`.
2. `-32018 DocumentNotEditable {reason}`. `reason` is one of `no_root`, `bad_root`, `inactive`,
   `no_selection`, `bad_selection`, `edit_scope_active` (Hammer has an edit scope of its own open), `no_history`, `bad_history`, `history_not_active`.
3. `-32004 DocumentChanged {reason}` (§6.3).

A node that Hammer has locked is refused per node with `-32018 "node is locked"` (or
`"nodeID N is locked"`).

### 6.3 `expectDoc`

`expectDoc` is a top-level member of `params`: `{"id":"d3","epoch":7}`. It is **required** on
every write-gated call. `DocumentChanged` reasons:

| reason | Meaning |
|---|---|
| `missing` | no `expectDoc` |
| `missing_id` / `missing_epoch` | the member lacks `id` or `epoch` |
| `doc_id` | another document is active |
| `epoch` | same document, but it changed (reload, new root) since the client looked |

On `DocumentChanged`, re-read with `doc.active` and re-plan. Do not retry blindly.

### 6.4 The `doc` tail

Every successful write-gated result ends with
`"doc":{"id":"dN","topSerial":N,"undoDepth":N}`. This describes the document **after** the call:
`undoDepth` is the undo stack depth and `topSerial` the serial of the top undo entry. A client can
check that an undo or redo did what it expected without another round trip. `mesh.get` also
carries it. Errors carry no `doc` tail, and neither does a `-32012` batch failure.

### 6.5 Undo groups

- Each document write opens **one** Hammer undo group, named with the label in the table below,
  around all of its changes. One `undo.undo` reverts the whole call.
- All parameter validation happens **before** the group opens, so a refusal leaves the undo stack
  untouched.
- If a game call fails after the group opened, the reply is `-32603` (with `maybeExecuted:true`
  where the change may have landed). The group stays on the stack with whatever landed; undo it
  with `undo.undo`.
- `selection.set` and `map.*` do not touch the undo stack. `map.reload` resets it (Hammer reloads
  the document).

| Undo label | Written by |
|---|---|
| `MCP: create entity` | `entity.create` |
| `MCP: set keyvalues` | `entity.setKeyValues`; `edit.batch` without a create |
| `MCP: batch` | `edit.batch` with at least one `entity.create` |
| `MCP: set transform` | `node.setTransform` |
| `MCP: delete nodes` | `node.delete` |
| `MCP: add connections` | `entity.addConnections` |
| `MCP: remove connections` | `entity.removeConnections` |
| `MCP: tie meshes to entity` | `entity.tieMeshes` |
| `Untie from entity` | `entity.untie` (Hammer's own group) |
| `MCP: create mesh` | `mesh.create` |
| `MCP: set mesh geometry` | `mesh.setGeometry` |
| `MCP: set mesh material` | `mesh.setMaterial` |
| `MCP: set face texture` | `mesh.setFaceTexture` |
| `MCP: create prefab` | `node.createPrefab` |
| `MCP: create prefab from selection` | `node.prefabFromSelection` |

### 6.6 Entity handles

- `entity.create` and `entity.tieMeshes` return a `handle` `"n<N>"` for the node they made. The
  table holds 32 handles and is scoped to the epoch.
- A handle from an earlier epoch is refused `-32005 StaleHandle {reason:"epoch_changed"}`. A handle
  never issued is refused `{reason:"unknown_handle"}`.
- `nodeID` (Hammer's own node id) is the stable reference. Prefer it once a result has given it.

### 6.7 Parameter rules shared by the Hammer methods

- `nodeID` must be an integer in `0..2147483647`. Otherwise:
  `"nodeID: a non-negative int up to 2147483647 is required"` (or `"nodeID required"`).
- Entity-method params are read from the **top level** of `params`: a member of the same name
  nested in a value is not used.
- Vectors are `[x,y,z]` of finite numbers. `origin` components must satisfy `|c| <= 1048576`.
  `scales` components must be finite and in `(0, 1000]`.

## 7. Control plane

All of these are answered on the IPC thread (§4.5), need no module, and answer while the main
thread is blocked.

### 7.1 `control.hello`

The handshake (§2.4). The row is hidden from `capabilities.methods`.

### 7.2 `control.ping`

`{}` → `{ok:true, uptimeMs, mainThreadAgeMs, drainBlocked, inflight, queueDepth, modules, revision, shuttingDown}`

- `modules` maps each pinned module name to its state string (`{"hammer.dll":"verified",...}`).
- `inflight` is 1 while the main thread runs a request, else 0.
- `drainBlocked` is true when that request has been running for more than 250 ms.
- `mainThreadAgeMs` is the time since the last drain tick (0 before the first one).

### 7.3 `control.status`

`{}` → `{queue:[{id, method, ageMs}], jobs:[{id, kind, state}], listener:bool, drainBlocked:false, inflight, revision, shuttingDown}`

### 7.4 `control.capabilities`

`{}` →

```json
{"methods":["doc.active", "..."],
 "features":{"tcp":true,"pipe":false,"consoleCommand":false},
 "limits":{"maxLineBytes":1048576},
 "modules":{"hammer.dll":{"state":"verified","sha256":"<16 hex>","relaxed":["..."]}, "...":{}},
 "revision":N}
```

- `methods`: see §4.2.
- `sha256`: the first 8 bytes of the pinned image hash.
- `relaxed`: present only when non-empty. It lists the pinned slots whose identity check was
  relaxed because another Metamod plugin's KHook hook occupies them.

### 7.5 `control.version`

`{}` → `{protocol:1, bridgeVersion, gameBuild, hostModules:[{name, state, timeDateStamp}]}`

### 7.6 `control.cancel`

`{id}` → `{cancelled:true}`, or `-32010 NotCancellable {"reason":"not queued or already running"}`.
A cancelled request gets no other reply.

### 7.7 `control.shutdown`

`{}` → `{closing:true}`. The bridge flushes the replies already pending, sends this one, closes
the queue and stops accepting connections.

## 8. Documents, maps and undo

### 8.1 `doc.active`

Read-only. It never fails on "no document".

- `{}` → `{doc:null, reason}`, with a §6.2 step-1 reason, or:

```json
{"doc":{"id":"d3","path":"...","dirty":true,"active":true,"editable":true,"notEditableReason":"ok",
        "epoch":7,"docType":"vmap","isNewFile":false,"undoName":"MCP: batch","undoDepth":4,"topSerial":12}}
```

| Member | Meaning |
|---|---|
| `path` | the document's path as Hammer holds it (absolute host path), at most 4095 bytes (cut on a UTF-8 boundary) |
| `dirty` | `bool`, or `null` when unreadable |
| `editable` / `notEditableReason` | whether the write gate's step 2 passes; `"ok"`, or the §6.2 step-2 reason |
| `undoName` | the label of the top undo entry, or `null` |

### 8.2 `map.open`

`{path}` → open a `.vmap` in Hammer. No `expectDoc`.

- `path` is relative to the addon content root (`maps/x.vmap`). Refusals (`-32602`, in order):
  - `path: relative to the addon content root (maps/... .vmap), 1..512 chars, no '..'` (no
    `path` string)
  - `path: no JSON escapes (use '/' separators)`
  - `path: 1..512 chars`
  - `path: relative to the addon content root (maps/...), not absolute`
  - `path: no control characters`
  - `path: no ':'`
  - `path: no '\' (use '/' separators)`
  - `path: a .vmap file`
  - `path: under maps/ (the addon's map directory)`
  - `path: a map file name under maps/`
  - `path: no '..'`
  - `path: no empty path segment ('//')`
  - `path: no '.' or '..' path segment`
  - `path: no path segment ending in '.' or a space`
- The bridge then resolves the host file: `<app>\content\csgo_addons\<addon>\<path>`. `<app>` is
  four levels above the game executable (`game\bin\win64\cs2.exe`). `<addon>` is the `-addon`
  command-line argument (`[A-Za-z0-9_]`).
  - `-32017 {reason:"no_app_root"}` or `{reason:"no_addon"}` when either cannot be found.
  - `-32014 AssetNotFound {reason}`: `no_such_file`, `not_a_file`, or `InvalidMapFile` (not a
    binary DMX v9 `.vmap`). A file Hammer cannot open pops a modal dialog, so the bridge refuses
    such a file before calling `OpenFile`.
  - `-32003 {reason:"hammer_unavailable"}`; `-32603 "session_vector_unreadable"`.
- Results:
  - The map is already open: `{opened:false, existing:true, path, active, epoch}`. Nothing is
    called.
  - Otherwise: `{opened:bool, existing:false, path, reason?:"open_failed", epoch}`.
    `opened:false` is a result: Hammer refused the file.
- The new document has a new `id` and `epoch`. Read them with `doc.active`.

### 8.3 `map.reload`

`{expectDoc, force?}` → `{reloaded:true, path, discardedChanges, epoch}`

- Gate order: write gate (§6.2), then `-32015 UntitledDocument {"reason":"no path (Save-As is modal)"}`,
  then `-32016 UnsavedChanges {"reason":"document is dirty; pass force:true to discard"}` unless
  `force:true`.
- Hammer re-reads the file from disk. The undo history is gone afterwards, and `epoch` changes.

### 8.4 `map.save`

`{}` → `{saved:bool, path, skippedClean:bool}`

- No `expectDoc`. Gates: `-32003`, `-32018`, `-32015` (untitled: Save As would open a modal).
- A clean document is not saved (`skippedClean:true`).
- The path is read with a 4096-byte cap.

### 8.5 `undo.undo`

`{expectDoc}` → `{undone:bool, name, undoDepth, doc}`

- Write gate. Undoes the top entry of Hammer's undo stack (whatever made it, the bridge or the
  user).
- `name` is the label undone. An empty stack answers `{undone:false, name:null, doc}`.

### 8.6 `undo.redo`

`{expectDoc}` → `{redone:bool, name, redoDepth, doc}`

- Write gate, plus `-32017 {"reason":"bad_redo_history"}` when the redo history does not resolve.
- Compare `doc.topSerial` / `doc.undoDepth` (§6.4) with the values from before the undo to confirm
  the round trip.

## 9. Entities

`class` names are Hammer entity classnames (`info_player_start`). Matching is case-insensitive and
exact.

### 9.1 `entity.get`

`{nodeID}` → read one live entity. Read gate.

```json
{"nodeID":12,"classname":"trigger_multiple","targetname":"t1","origin":[0,0,0],"angles":[0,0,0]|null,
 "scales":[1,1,1]|null,"keyValues":{"k":"v"},"keyValueCount":N,"unsupported":[...],"truncatedValues":[...],
 "truncated":true?,
 "connections":[{"outputName","targetType","targetName","inputName","overrideParam","delay","timesToFire","truncated"?}]|null,
 "connectionsTruncated"?:true,"connectionsTotal"?:N,
 "children":[{"nodeID":N|null,"class":"mesh"}]|null,"childrenTruncated"?:true,"childrenTotal"?:N,
 "source":"live"}
```

- `keyValues` values are a string, an integer, a float, a boolean, or `null`.
  - `null` with the name in `unsupported` (at most 64 names): an attribute of another type
    (vector, colour, ...).
  - `null` with the name in `truncatedValues`: a string longer than 4096 bytes.
- Caps: 512 keyvalues (`truncated:true` past that), 512 connections, 1024 children.
- Errors: `-32602 "not an entity"`; `-32022 NodeNotFound "no node with this nodeID in the live document"`;
  `-32603 "entity_properties missing"`; overflow `-32603 "entity has too many or too large keyvalues"`.

### 9.2 `entity.find`

`{class?, targetname?, within?:{origin:[x,y,z], radius}, limit?}` → query live entities. Read
gate.

- `limit` is `1..2000` (default 500). All given filters must match.
- Result: `{entities:[{nodeID, classname, targetname, origin}], count, visited, truncated, source:"live"}`.
  `visited` is the number of nodes walked. `truncated` means more matches exist past `limit`.
- Refusals: `class: a non-empty string is required`, `targetname: a non-empty string is required`,
  `limit: an int in 1..2000 is required`, `within: {origin:[x,y,z], radius > 0} required`.

### 9.3 `entity.create`

`{class, origin?, angles?, scales?, keyValues?, expectDoc}` → one new entity. Write gate. Undo:
`MCP: create entity`.

- Refusals, all before the undo group:
  - `class` (at most 63 bytes);
  - `origin: [x,y,z] required`, `angles: [pitch,yaw,roll] required`, `scales: [x,y,z] required`,
    `scales: each finite and in (0, 1000]`;
  - `-32006 UnknownClass "not in the CMapNodeFactory registry"`.
- Result: `{handle, nodeID?, class, origin, scales, undoLabel:"MCP: create entity", keyValues?:{applied, warnings}, doc}`.
  `scales` is always present: the applied scales, `[1,1,1]` when none were given. `keyValues`
  has the §9.5 report when keyvalues were given. `nodeID` is omitted when the new node's id
  cannot be read; the `handle` still works.
- Overflow: `-32603 "response too large (use fewer keyvalues)"` (16 KB result).

### 9.4 `entity.setKeyValues`

`{handle? | nodeID?, values:{key:value,...}, expectDoc}` → write keyvalues on one entity. Write
gate. Undo: `MCP: set keyvalues`.

- Exactly one of `handle` / `nodeID`: otherwise `handle and nodeID are mutually exclusive` /
  `handle or nodeID required`. `values` must be an object.
- Refusals before the group: `-32005`, `-32022`, `-32602 "not an entity"`, `-32018 "node is locked"`,
  `-32603 "entity_properties missing"`.
- Result:
  `{applied:{key:bool}, warnings:[...], previous:{key:value|null}, previousTruncated:[...], valueUndo:"recorded", doc}`.
  - `previous` is the exact value before the write (`null` if the key was absent).
  - `valueUndo:"recorded"` means the node's state was recorded in the undo group, so `undo.undo`
    restores the previous values.
- Overflow: `-32603 "response too large (use fewer keyvalues)"` (64 KB result).

### 9.5 How keyvalue writes behave

This applies to `entity.setKeyValues`, `entity.create` `keyValues`, `entity.tieMeshes` `keyValues`
and `edit.batch`.

- A string value is written through Hammer's own `SetKeyValue`. A key the entity does not have is
  created as a string attribute. An existing int, float or bool attribute keeps its type and
  is parsed from the value. Any other attribute type is refused with a warning.
- A key that is refused gets `applied:{key:false}` and a warning. The rest of the call goes on.
  Warnings:
  - `<k>: value longer than 1024 bytes`
  - `<k>: attribute not readable`
  - `<k>: number out of range for this attribute`
  - `<k>: number out of range for an int attribute`
  - `<k>: attribute type <n> not writable`
  - ``<k>: refused, the entity spells this key "<v>" (SetKeyValue matches case-insensitively, `previous` is exact)``.
    Write the key with the entity's own spelling.
- Each node is recorded in the undo group once per call, so undo restores every value.

### 9.6 `entity.addConnections`

`{nodeID, connections:[{output, target, input, param?, delay?, timesToFire?}], expectDoc}` → add
I/O connections. Write gate. Undo: `MCP: add connections`.

- 1..64 entries. `output`/`target`/`input` are 1..255 bytes with no control characters. `param`
  may be empty (same limits otherwise). `delay` is `0..3600`. `timesToFire` is `-1` (unlimited) or
  `>= 1`. An unknown member in an entry is refused.
- Refusals: `connections: 1..64 entries of {...}`;
  `connections: this entity has room for fewer than the requested entries (max 512 live)`.
  Both come before the group.
- Result:
  `{nodeID, added:[{index, outputName, targetName, inputName, overrideParam, delay, timesToFire} | {readback:false, index?}], count, landed:[{index|null, landed}], undoLabel, doc}`.
  `added` is the read-back of each new connection.

### 9.7 `entity.removeConnections`

`{nodeID, indices? | match?:{output?, target?, input?}, expectDoc}` → remove connections. Write
gate. Undo: `MCP: remove connections`.

- Exactly one of `indices` / `match`.
  - `indices`: 1..512 distinct non-negative integers, the positions `entity.get` reports.
  - `match`: every given member must match byte for byte (case-sensitive). A `match` that selects nothing is
    `-32026 ConnectionNotFound "no live connection matches the match filter"`; one that selects
    more than 512 is refused.
- Refusals: `exactly one of indices / match is required`;
  `indices: 1..512 distinct non-negative ints up to 2147483647`; the `match: ...` texts;
  `-32603 "connection vector unreadable"`.
- Connections are removed from the highest index down, so the given indices stay valid during the
  call. A removal that fails after the group opened is `-32603` with `maybeExecuted:true`.
- Result: `{nodeID, removed:[ascending indices], count, remaining, undoLabel, doc}`.

### 9.8 `entity.tieMeshes`

`{class? | entityID?, nodeIDs:[...], keyValues?, allowRetie?, expectDoc}` → tie meshes into a new
brush entity (`class`) or into an existing one (`entityID`). Write gate. Undo:
`MCP: tie meshes to entity`.

- `class` is 1..63 bytes and must be a solid (brush) class. `nodeIDs` are 1..64 distinct mesh
  nodes. `keyValues` is allowed only with `class`, and may not set `classname`, `origin` or
  `angles`. `allowRetie:true` lets a mesh already tied to another entity move.
- Refusals before the group:
  - `exactly one of class / entityID is required`, `class: at most 63 bytes`;
  - `keyValues: an object, and only with class`, `keyValues: malformed object`;
  - `allowRetie: true or false required`;
  - `nodeIDs: at least one nodeID is required`, `nodeIDs: at most 64`, `nodeIDs: duplicate nodeID`;
  - per node: `not a mesh`, `already tied` (with `parentNodeID`), `mesh is inside a prefab/instance`;
  - for `entityID`: `not a brush entity`, `entity is inside a prefab/instance`;
  - `-32006 UnknownClass`.
- After the group opened:
  - `not a solid (brush) class` is answered with `undoGroupOpened` / `undoLabel` in `data` (the
    group stays);
  - `-32603 "CreateNodeByName failed (see bridge log)"`;
  - a refused `AddChild` is `-32603` with `maybeExecuted`, `tied` and `removedEmptyEntities`
    (each list capped at 10);
  - `the first mesh's origin/angles are unreadable`.
- Result:
  `{nodeID?, handle, class:"entity", classname, created:bool, origin, meshes:[...], alreadyTied:[...], removedEmptyEntities:[...], keyValues?, noop:bool, undoLabel|null, doc}`.
  - A new entity takes the first mesh's origin and angles.
  - An entity left with no meshes by a re-tie is deleted and listed in `removedEmptyEntities`.
  - `noop:true` (every mesh already tied to the target) writes nothing, and `undoLabel` is then
    `null`.

### 9.9 `entity.untie`

`{nodeID, expectDoc}` → move a brush entity's meshes back to the world and delete the entity.
Write gate. Undo: Hammer's own `Untie from entity` group.

- Refusals: `not a brush entity`, `child list unreadable`, `entity has no parent`.
- Result: `{nodeID, meshes:[...], entityDeleted:true, meshesTruncated?, undoLabel:"Untie from entity", doc}`.

## 10. `edit.batch`

`{ops:[{method, params}], expectDoc}` → run several entity writes in **one** undo group. Write
gate (checked once, for the batch).

- `ops` missing or not an array: `-32602 "invalid params: ops"` (no `data`).
- Allowed `method`s: `entity.create` and `entity.setKeyValues`. Any other is answered per op as
  `-32011 NotImplemented {reason:"<method>"}`, which fails the batch.
- An op's `params` is that method's params without `expectDoc`. Each op object is copied into an
  8 KB buffer, so keep each op under 8 KB of JSON. The result of each op is that method's result
  without the `doc` tail.
- `"$N"` as a `handle` value is replaced by the handle of the N-th successful `entity.create` in
  this batch (0-based, at most 16). An out-of-range `$N` is passed on as written and fails as a
  handle.
- Each node is recorded once for undo, whatever the number of ops that touch it.
- Undo label: `MCP: batch` if any op is `entity.create`, else `MCP: set keyvalues`.
- Success: `{results:[...], failedIndex:null, doc}`.
- The first failing op stops the batch:
  `-32012 PartialFailure {results:[..., {"error":{code, message, data}}], failedIndex:N}`. The ops
  before it ran and stay in the open undo group: `undo.undo` reverts them all.
- If the result is too large:
  `-32012 {"failedIndex":null,"reason":"batch result too large; the ops ran - verify with entity.find, do not retry"}`.

## 11. Nodes

Node classes, as reported by `node.list` and `entity.get` children: `mesh`, `entity`, `group`,
`prefab`, `instance`, `world`, `other`.

### 11.1 `node.list`

`{class?, limit?}` → walk the live tree. Read gate.

- `class` is `"all"` (the default) or one of the node classes. `limit` is `1..10000` (default 2000).
- Result: `{nodes:[{nodeID, class, origin, vertexSlots?, faceSlots?}], count, visited, truncated, source:"live"}`.
  `vertexSlots` and `faceSlots` are reported for meshes.
- The walk stops at depth 64 or after 200000 nodes. `-32603 "no world node"` when the document has
  no world.

### 11.2 `node.delete`

`{nodeIDs:[...], expectDoc}` → delete up to 256 nodes. Write gate. Undo: `MCP: delete nodes`.

- Refusals (all before the group):
  - `nodeIDs: [int,...] required`, `nodeIDs: up to 256 non-negative ints`, `nodeIDs: malformed`,
    `nodeIDs: empty`;
  - `-32022 "nodeID N not found"`;
  - `-32602 "nodeID N cannot be deleted (world|other)"`;
  - `-32018 "nodeID N is locked"`.
- Result: `{deleted:[nodeIDs], undoLabel:"MCP: delete nodes", doc}`.

### 11.3 `node.setTransform`

`{nodeID, origin?, angles?, scales?, expectDoc}` → set an entity's or a mesh's transform. Write
gate. Undo: `MCP: set transform`.

- At least one of `origin`, `angles`, `scales` (`at least one of origin, angles, scales is required`).
  Range rules as in §6.7 (`origin: each |component| <= 1048576`).
- Refusals: `-32022`; `-32602 "not an entity or mesh"`; `-32018 "node is locked"`.
- The values are applied in the order scales, angles, origin.
- Result: `{nodeID, class, origin|null, angles|null, scales|null, undoLabel, doc}`, read back from the
  node after the write.

### 11.4 `node.createPrefab`

`{path, origin?, angles?}` (plus `expectDoc`) → add a prefab node that references an existing
`.vmap`. Write gate. Undo: `MCP: create prefab`.

- `path` follows the `map.open` rules (§8.2). It must not be a map open in Hammer:
  `path: that map is open in Hammer (close it first, or pick another file)`.
- The file must exist: `-32014 {no_such_file | not_a_file | InvalidMapFile}`. `-32017 {no_app_root | no_addon}`.
- After the group opened:
  - `-32603 "AddPrefab created no node (see bridge log); nothing was added"`;
  - `-32603 PrefabPathRejected` with `maybeExecuted`: Hammer stored no target path, so an empty
    prefab was added. Undo it with `undo.undo`.
- Result: `{nodeID, class:"prefab", targetMapPath, loaded, origin, angles, undoLabel, warning?, doc}`.
- `warning` (the parts joined by `; `):
  - `targetMapPath '<t>' differs from the requested path '<p>'`;
  - `the referenced map is not loaded (the prefab is empty in the live document); it may load after map.save + map.reload`.

### 11.5 `node.prefabFromSelection`

`{path, centered?}` (plus `expectDoc`) → write the current selection to a new `.vmap` and replace it
with a prefab of that file. Write gate. Undo: `MCP: create prefab from selection`.

- `path` follows the `map.open` rules and must not exist yet:
  `path: that file already exists (the worker would overwrite it; pick a new name)`.
- Refusals:
  - `-32014 "directory_missing: create the target directory first"`;
  - `-32017 "selection_empty: select the nodes first"`;
  - `-32017 "selection_mode: the selection mode must be groups, objects or meshes"`.
- After the group opened: `-32603 "the prefab worker returned no node (see bridge log); the group may hold partial changes"`.
- Result:
  `{nodeID, class, targetMapPath, written, loaded, selectedCount, centered, fileStaysOnUndo:true, undoLabel, warning?, doc}`.
  - `fileStaysOnUndo` means `undo.undo` restores the nodes but does not delete the written file.
  - `warning` has the §11.4 parts, plus `the worker returned a prefab but the file does not exist on disk`
    when `written` is false.

## 12. Meshes

### 12.1 Geometry parameters

`mesh.create` and `mesh.setGeometry` take a polygon soup. Hammer welds it into a mesh.

| Member | Rule |
|---|---|
| `vertices` | `[[x,y,z],...]`, 3..65536 entries, each `|c| < 1e6` |
| `faces` | `[[i,j,k,...],...]`, each at least 3 vertex indices, at most 65536 faces and 2^20 indices in total |
| `material` | required: a material path, for example `materials/dev/reflectivity_30.vmat` |
| `faceMaterials` | optional, one `string|null` per face (`null` = `material`), at most 64 distinct |

Parser refusals use the member name: `vertices: [[x,y,z],...] required`,
`material: a material path, e.g. materials/dev/reflectivity_30.vmat`, and so on. All mesh writes
use the write gate, return at most 512 KB, and overflow as `-32603 "response too large (use limit)"`.

The mesh summary in the results below is `{nodeID, class:"mesh", origin, vertexSlots, faceSlots}`.

### 12.2 `mesh.create`

`{vertices, faces, material, faceMaterials?, expectDoc}` → a new mesh in the world. Undo:
`MCP: create mesh`.

- `-32603 "no document root"`.
- After the group opened: `-32603 "CreateElement<CMapMesh> failed"`,
  `-32603 "Hammer created no faces (degenerate polygons?)"`.
- Result: summary + `{inputVertices, faces, materials, undoLabel, doc}`.

### 12.3 `mesh.setGeometry`

`{nodeID, vertices, faces, material, faceMaterials?, expectDoc}` → replace a mesh's polygons.
Undo: `MCP: set mesh geometry`.

- Refusals: `-32022`, `node is not a mesh`. After the group: `-32603 "could not clear the mesh"`.
- Result: summary + `{inputVertices, faces, undoLabel, doc}`.

### 12.4 `mesh.setMaterial`

`{nodeID, material, expectDoc}` → put one material on every face. Undo: `MCP: set mesh material`.

- Refusals: `material required`, `mesh has no faces`, `could not collect face handles`.
- Result: summary + `{faces, material, undoLabel, doc}`.

### 12.5 `mesh.get`

`{nodeID, expectDoc?}` → read a live mesh back. Read gate; `expectDoc` is checked when present.

```json
{"nodeID":5,"class":"mesh","origin":[...],"angles":[...],"scales":[...],"space":"local",
 "worldSpace":{...},"parentChain":[...],"vertices":[[x,y,z],...],"faces":[[i,j,k],...],
 "faceMaterials":[...],"materials":[...],"vertexCount":N,"faceCount":N,"doc":{...}}
```

- Floats are printed as the shortest text that round-trips, so a read-back can be compared
  exactly. Vertices are in the mesh's local space. `worldSpace` and `parentChain` give the transform to
  world space.
- A mesh that does not fit one reply is
  `-32602 {reason:"mesh too large for one reply", vertexCount, faceCount, maxLineBytes}`.
- Other errors: `could not read the mesh origin`, `mesh has no position stream`.

### 12.6 `mesh.setFaceTexture`

`{nodeID, mode, faces?, ..., expectDoc}` → set texture alignment per face. Undo:
`MCP: set face texture`.

| `mode` | Effect | Extra params |
|---|---|---|
| `world` | world-aligned projection | `keepScale?`; `shift?`, `scale?`, `rotate?` applied afterwards |
| `face` | face-aligned projection | the same as `world` |
| `justify` | Hammer's justify | `justify`: `left`, `right`, `top`, `bottom`, `center`, `fit`, `fitU`, `fitV`. No shift/scale/rotate (they would undo it) |
| `set` | set shift / scale / rotation only | at least one of `shift?:[u,v]`, `scale?:[u,v]` (each > 0, world units per texel), `rotate?` (degrees, `|r| <= 36000`) |
| `axes` | write the texture axes directly | `faces:[{index, axisU:[x,y,z,w], axisV:[x,y,z,w], scale}]` (up to 65536; axes may not be zero-length or parallel) |

- `texSet` is 0 or 1 (which texture set; default 0). `faces` is a list of distinct face indices,
  in the order `mesh.get` reports them; omit it for every face. In `axes` mode `faces` holds the
  objects above and no other alignment member is allowed.
- Refusal texts name the member: `mode: one of world, face, justify, set, axes is required`,
  `keepScale: only valid with mode "world" or "face"`, `faces[k]: index i is listed twice`, and
  so on.
- Errors: `-32603 TextureSetUnknown` (`maybeExecuted` once a write happened);
  `-32603 TextureWriteMismatch {"reason":"face <i>: <what>","maybeExecuted":true}`: the write
  landed but the read-back disagrees with what was asked.
- Result: summary + `{mode, texSet, touched, readBackTruncated, exact?, faces:[{index, axisU, axisV, scale}], undoLabel, doc}`.
  `faces` holds the read-back of the first 1000 touched faces. `exact` is reported in `axes` mode.

## 13. Selection

Selection changes are not undo entries and do not set the dirty bit.

### 13.1 `selection.get`

`{}` → `{mode, count, nodeIDs:[...], faces:[{nodeID, faces:[...]}], unmapped}`. Read gate.

`mode` is one of `groups`, `objects`, `meshes`, `vertices`, `edges`, `faces`, `mixed`, `tile`,
or `null`. `unmapped` counts selected items the bridge could not map to a node.

### 13.2 `selection.set`

`{op, nodeIDs? | faces?, expectDoc}` → change the selection. **Write gate**, so `expectDoc` is
required and `-32018` applies.

- `op` is `replace`, `add`, `remove` or `clear`.
- `nodeIDs`: at most 4096. `faces`: `[{nodeID, faces:[...]}]`, at most 4096 entries and 65536
  indices.
- Refusals:
  - `op: replace, add, remove or clear is required`;
  - `nodeIDs: at most 4096 ids per call`, `faces: at most 4096 entries per call`,
    `faces: at most 65536 indices per call`;
  - `unknown nodeIDs` (with `unknown`, `unknownTotal`), `not a mesh`;
  - `face index out of range` (with `faceCount`, `badIndices`);
  - `-32603 "selection mode unknown"`.
- Result: `{requested, applied|null, appliedError?, modeChanged, undoGroupOpened:false, doc}`.

## 14. Jobs

Jobs run external tools without blocking the game. Answered on the IPC lane (§4.5), with no module
needed. Only kind `resourcecompiler` exists. It is Windows-only and one job runs at a time.

### 14.1 `job.start`

`{kind:"resourcecompiler", input, game?, flags?, timeoutMs?, clientRef?}` →
`{jobId, state:"running", pid, log:{windows, relative}, command, startedAtMs}`

| Param | Rule |
|---|---|
| `input` | `csgo_addons/<addon>/...`, at most 260 bytes, segments of `[A-Za-z0-9_.-]`, an existing file with extension `vmap`, `vmdl`, `vpcf`, `vsndevts`, `vmat`, `vtex`, `vsnd`, `vsmart`, `vanim`, `vmks` or `js` |
| `flags` | each at most once. Plain: `-world`, `-phys`, `-vis`, `-bakelighting`, `-nav`, `-fshallow`, `-quiet`, `-unbufferedio`, `-noassert`, `-nop4`, `-f`. With a number: `-maxtextureres 1..16384`, `-threads 1..64` |
| `timeoutMs` | `1000..3600000`, default 1500000 |
| `clientRef` | 1..64 bytes of `[A-Za-z0-9_.-]`, echoed by `job.status` |

- `jobId` is `j<N>-<nonce>`.
- The log goes to `<app>\cs2mcp_jobs\<jobId>.log` (`log.relative` = `cs2mcp_jobs/<jobId>.log`).
  Outside the tools install it goes to `%LOCALAPPDATA%\cs2mcpbridge\jobs\`, with `relative` `""`.
- Errors:
  - `-32602 {reason}` for each param rule.
  - `-32017 {blockedOn}`: `job: resourcecompiler is Windows-only`, `job: <id> running`,
    `job: another job is starting`, `job: resourcecompiler.exe not found at ...`,
    `job: bridge is shutting down`, `job: cannot locate the game directory from ...`,
    `job: AssignProcessToJobObject failed: ...`.
  - `-32603 "spawn failed: ..."` and `"watcher thread failed: ..."`.

### 14.2 `job.status`

`{jobId}` →
`{jobId, kind, state, input, clientRef, pid, elapsedMs, failLines, fatal, logTail, log, startedAtMs, exitCode?, summary?:{compiled, failed, skipped, seconds}}`.
`{}` → `{jobs:[{jobId, kind, state, input, clientRef, elapsedMs, startedAtMs, log}]}`, oldest first.
The bridge keeps the 16 most recent finished jobs; an older job is forgotten and its log file is
deleted.

- `state`: `running`, `ok`, `failed`, `timeout`, `cancelled`.
  - The tool's `OK:` summary line ends the job: `failed` if it reports failures, else `ok`.
  - An exit without that line is `failed` with `exitCode`.
  - The deadline gives `timeout`.
  - After a `FATAL ERROR` line with no `OK:` line, the job is ended `failed` 10 s later
    (`fatal:true`).
- `logTail`: the last 40 log lines. `failLines`: up to 100 failure lines. Lines are cut at 400
  characters.
- `-32602 "unknown jobId"`.

### 14.3 `job.cancel`

`{jobId}` → `{cancelled:bool, state}`.

- A running job is ended (its process tree runs in a Windows job object) and becomes `cancelled`.
- A finished job answers `cancelled:false` with its final state.
- `-32602 "unknown jobId"`.

## 15. Renders

### 15.1 Common rules

- Every render writes a PNG and returns its path. Directory: `CS2_BRIDGE_RENDERS` when set, else
  `Z:\app\renders\` when it exists, else `%LOCALAPPDATA%\cs2mcpbridge\renders\`.
- `name` (optional): `[A-Za-z0-9._-]`, 1..64 characters, no leading dot; a trailing `.png` is
  dropped. Default `render_<tick>_<seq>`. The same name overwrites the file (atomic replace).
- Size refusal: `width/height: 16..<max> each, at most <pixels> pixels`. Name refusal:
  `name: [A-Za-z0-9._-], 1..64 chars, no leading dot`. Both come before any game object is
  touched.
- `stats`: `{untouchedPixels, nonBlackPixels, meanLuma, minLuma, maxLuma, crc32}`.
  `untouchedPixels > 0` means the renderer did not fill the image.
- `-32020 RenderFailed {reason}`: `out of memory`, `readback did not run (no pixels written)`,
  `png encode: out of memory`, `could not write <dir>`.

### 15.2 `view.render`

Renders Hammer's 3D viewport camera, or an explicit camera, offscreen. Read gate (`-32003 {reason}`).

| Param | Default |
|---|---|
| `width` × `height` | 640 × 360; 16..4096 each, at most 8388608 pixels |
| `format` | 0 (RGBA8888) or 2 (RGB888) |
| `origin`, `angles` | the viewport camera's |
| `fov` | the viewport's (or 90); `(1, 179)` degrees |
| `znear` / `zfar` | the viewport's (or 1 / 9000); `0 < znear < zfar <= 1000000` |
| `srgb` | true for format 0 |
| `hideToolMaterials` | false |

- `-32019 NoViewport {reason}`: `no_view_manager`, `bad_view_manager`, `no_3d_view`, `bad_view`.
- Result:
  `{path, file, width, height, bytes, camera:{origin, angles, fov, aspect, znear, zfar, source:"explicit"|"mixed"|"viewport", viewport:{...}}, view:{type, renderMode, views}, toolMaterials:"shown"|"hidden", format, srgb, stats, ms:{render, encode}}`.

### 15.3 `world.render`

Renders the engine's loaded map (not the Hammer document) from an explicit camera. No document
needed.

| Param | Default |
|---|---|
| `origin`, `angles` | required (`origin: [x,y,z] required`, `angles: [pitch,yaw,roll] required`) |
| `width` × `height` | 1920 × 1080; 16..8192 each, at most 33554432 pixels |
| `fov` | 90; `fov: (1, 179) degrees, horizontal` |
| `znear` / `zfar` | 4 / 30000 |
| `srgb` | false |

- `-32021 NoGameWorld "no map is loaded in the engine (map <name>)"`.
- `-32020` also for `scene world identity check failed` and `Tools rendering pipeline not found`.
- Result:
  `{path, file, width, height, bytes, camera:{origin, angles, fov, znear, zfar}, pipeline:"Tools->CSGO", srgb, stats, ms:{render, readback, encode}}`.

### 15.4 `pet.render` and `modeldoc.render`

Render the particle editor's preview viewport (`pet.render`, needs `pet.dll`) or ModelDoc's main
viewport (`modeldoc.render`, needs `modeldoc_editor.dll`) at the viewport's current camera. Neither
opens a modal or changes state. A document is optional (`hasDocument`).

- Params: `{width? 16..4096 (1024), height? 16..4096 (768), name?}`, at most 8388608 pixels.
- Result:
  `{path, file, width, height, bytes, format:"RGBA8888", hasDocument, stats, durationMs}`. The
  pixels are display-ready (no sRGB conversion).
- `-32019 NoViewport {reason}`:
  - `pet.render`: `pet_unavailable`, `no_main_window`, `bad_main_window`, `no_preview`,
    `bad_preview`, `no_scene_widget`, `bad_scene_widget`, `no_scene`, `bad_scene`, `no_camera`,
    `bad_camera`.
  - `modeldoc.render`: `modeldoc_mismatch`, `modeldoc_verifying`, `modeldoc_not_loaded`,
    `modeldoc_unavailable`, `no_session`, `bad_session`, `no_main_window`, `bad_main_window`,
    `no_scene_widget`, `bad_scene_widget`, `no_tool_widget`, `bad_tool_widget`, `no_scene`,
    `bad_scene`, `no_camera`, `bad_camera`.
  - A `bad_*` step logs the observed vtable so a wrong pin can be fixed.

## 16. ModelDoc

Needs `modeldoc_editor.dll` (loaded once a ModelDoc window is open). `OpenFile` pops a modal
dialog for a missing or broken file, so a tool should only send paths of files it has checked on
the host.

### 16.1 `modeldoc.active`

`{}` → `{doc:{path, dirty, isNewFile, hasAsset, compileStatus:null, docType:"vmdl"}}` or
`{doc:null, reason}`. It never fails on "no document".

- `reason`: `modeldoc_not_loaded`, `modeldoc_verifying`, `modeldoc_mismatch`,
  `modeldoc_unavailable`, `no_session`, `bad_session`, `no_document`, `bad_doc`.
- `path` is the absolute host path (`""` when unset).
- `compileStatus` is always `null`: the session's compile status is a sink object, not a string
  `[V live]`.

### 16.2 `modeldoc.open`

`{path}` → `{opened:bool, path, active:{...as modeldoc.active doc}|null}`

- `path` is relative to the addon content root (`models/x/y.vmdl`), 1..512 characters.
- Refusals (`-32602`):
  - `path: relative to the addon content root (models/...), 1..512 chars, no '..'`;
  - `path: no JSON escapes (use '/' separators)`, `path: no control characters`;
  - `path: relative to the addon content root (models/...), not absolute`;
  - `path: a .vmdl file`, `path: no '..'`.
- `-32003 {reason:"modeldoc_unavailable"}`.
- `opened:false` is a result, not an error: `OpenFile` returned no session (it refused the file,
  or it was busy). An already-open path is re-activated instead of opened twice. A second,
  different file opens a second session; sessions cannot be closed through the bridge.

### 16.3 `modeldoc.reload`

`{force?}` → `{reloaded:bool, path, discardedChanges}`

- `-32003 {reason}`.
- `-32016 "document is dirty; pass force:true to discard"`, checked first.
- `-32015 "no path to reload from"`.
- No modal. The undo history is reset.

### 16.4 `modeldoc.save`

`{}` → `{saved:bool, skippedClean:bool, path}`

- `-32003`.
- `-32015 "no path (Save As would open a modal)"` for a new file;
  `-32015 "no usable path (Save As would open a modal)"` for an empty or oversized path.
- A clean document is not saved. A failed save is `saved:false`.

### 16.5 `modeldoc.compile`

`{}` → `{status:null, failed:null, savedFirst:bool}`

- Compiles the active model synchronously, saving first when dirty (`savedFirst`).
- The compile result is not readable through this call: check the `.vmdl_c` modification time.
- `-32017 {reason:"no_asset"}` when the session has no asset object. A `.vmdl` created after the
  tools started has none `[V live]`: compile it through `job.start` instead.

### 16.6 `modeldoc.nodeTypes`

`{}` → `{count, types:[{typeId, className}]}`: the registered ModelDoc node classes, the valid
`_class` values. No document needed.

- `-32017`: `node-type table unusable (count N)` (0 or more than 4096),
  `node-type descriptor unreadable`, `node-type className unreadable`.
- Never a partial list. Result buffer 256 KB.

### 16.7 `modeldoc.render`

See §15.4.

## 17. Particle editor (pet)

### 17.1 Paths and the load gate

- Particle paths are relative to the addon content root (`particles/x/y.vpcf`).
- pet's loader does not survive malformed input, so the bridge checks a `.vpcf` itself before
  pet loads it (`pet.open`, `pet.reload`, `pet.apply`) `[V live]`:

| Input pet was asked to load | pet's behaviour |
|---|---|
| not KV3, or a KV3 header with a truncated body | main-thread stack overflow, the process wedges |
| balanced but invalid KV3 (a key without a value) | the same |
| a root `_class` other than `CParticleSystemDefinition` | the process exits (fail-fast) |
| unknown operator class, wrong field type | tolerated |

- The gate parses the file as KV3 text: the header with `format:vpcf`, one root object,
  `key = value` members, comma-separated arrays, strings, raw strings, flags, numbers, literals,
  comments. It also requires the root object's own `_class` (matched case-insensitively, every
  occurrence) to be exactly `CParticleSystemDefinition`. Class and field names inside are not
  checked.
- Refusal: `-32602` with message `UnloadableFile` and `data:{reason, path}`.
  - Grammar reasons: `binary`, `no_kv3_header`, `not_vpcf`, `unterminated_comment`,
    `unterminated_string`, `no_root_object`, `unbalanced`, `too_deep` (nesting over 256),
    `trailing_content`, `expected_equals`, `expected_value`, `bad_key`, `bad_token`,
    `comma_in_object`, `expected_comma`, `no_root_class`, `wrong_root_class`.
  - File reasons: `too_large` (over 16 MB), `read_failed` (missing, unreadable, or not a disk
    file).
  - `pet.open` only: `no_app_root`, `no_addon`.
  - `pet.apply` only: `kv3_load_failed`.
  - `path` is the host file read (`"pet.apply:text"` for `pet.apply`).
- The host file is `<app>\content\csgo_addons\<addon>\<path>`, resolved as in §8.2.

### 17.2 `pet.load`

`{}` → `{loaded:bool, alreadyLoaded:bool, toolIndex:int|null, module:<state>}`. Needs only
`toolframework2.dll`.

- Loads the particle editor tool. A failed load pops a modal and the framework caches the failure,
  so the bridge tries **at most once per session**. A second call answers from the first answer.
  `loaded:false` is a result: restart the session.
- When `pet.dll` is already mapped, nothing is called and the answer is
  `{loaded:true, alreadyLoaded:true, toolIndex:null, module}`.
- `-32017 {reason}`: `framework_unavailable`, `tool_index_unknown`.
- `module` is pet.dll's state after the call. The other `pet.*` methods answer the generic `-32017`
  until it is `verified`.

### 17.3 `pet.active`

`{}` → `{doc:{path, dirty, isNewFile, hasRoot, arrays:[{type, count, classes:[...]}]|null, docType:"vpcf"}}`
or `{doc:null, reason}`. It never fails on "no document".

- `reason`: `pet_not_loaded`, `pet_verifying`, `pet_mismatch`, `pet_unavailable`, `no_session`,
  `bad_session`, `no_document`, `bad_doc`, `bad_root`.
- `hasRoot:false` with `arrays:null` is an empty document.
- `arrays` covers the nine function-type indices 0..8.
- `-32017 {reason}`: `root_child_count_unusable` (a count outside 0..4096), `root_child_unreadable`.
  Never a partial list.

### 17.4 `pet.open`

`{path}` → `{opened:bool, path, active:{...as pet.active doc}|null}`

- Path refusals as in §16.2, with `particles/...` and `path: a .vpcf file`.
- Then the load gate (§17.1). Then `-32003 {reason:"pet_unavailable"}`.
- `opened:false` is a result, as for ModelDoc. An unusable root there gives `active:null`.

### 17.5 `pet.reload`

`{force?}` → `{reloaded:bool, hasRoot, path, discardedChanges}`

- `-32003 {reason}`.
- `-32016` (checked first).
- `-32015 "no path to reload from"`.
- The load gate on the document's own file. On refusal pet is not called and the document keeps
  its tree.
- `hasRoot` is the new root's presence.

### 17.6 `pet.save`

`{force?}` → `{saved:bool, skippedClean:bool, path}`

- `-32003`, `-32015` (as §16.4).
- A clean document is skipped unless `force:true`. A failed save is `saved:false`.

### 17.7 `pet.text`

`{}` → `{text, bytes, path, dirty, isNewFile, canUndo, canRedo}`

- `text` is the live document serialized as KV3 text, at most 393216 bytes.
- `path` is the document's own path. The text is written to a private file
  (`%LOCALAPPDATA%\cs2mcpbridge\pet_text.vpcf`, outside every search path) and read back. Neither
  the document's path nor its dirty flag changes.
- `-32017 {reason}`: `text_path_too_long`, `save_to_file_failed: <pet's error text>`,
  `text_read_failed`, `text_too_large`, `text_has_nul`.
- Untitled documents are allowed.

### 17.8 `pet.apply`

`{text, label?}` → replace the live tree with `text` as **one pet undo step**. Needs `pet.dll` and
`tier0.dll`.

- `text`: 1..393216 bytes after JSON unescaping.
- `label`: 1..64 printable ASCII characters, no `"` or `\`. Default `MCP apply`.
- Refusals: `text (string) required`, `text: 1..393216 bytes`,
  `label: 1..64 printable ASCII (0x20..0x7E), no quote or backslash`.
- Unescape refusals (`-32602` with the reason as the message): `unterminated`, `bad_escape`, `nul`,
  `non_ascii_escape`, `surrogate`, `control_byte`, `too_long`.
- Then the load gate (§17.1) and KV3 parsing, **before** the undo step opens: a failure leaves the
  tree and the history untouched.
- `-32017 {reason}`: `kv3_root_null`, `tier0_handle_mismatch`, `loadkv3_export_missing`,
  `no_type_scope`, `bad_undo_history`, `bad_redo_history`.
- Unknown keys and unregistered operator classes are dropped, as when pet opens a file.
- Result: `{applied:true, label, bytes, dirty, canUndo, canRedo, arrays|null, arraysError?:{reason}}`.
  Once the tree changed nothing is an error: an unlistable new tree reports `arrays:null` and
  `arraysError`. Do not retry, because a retry would add a second undo step.

### 17.9 `pet.undo` / `pet.redo`

`{}` → `{done:bool, reason?, dirty, canUndo, canRedo, arrays}`

- pet's own history (not Hammer's).
- An empty history is a result: `done:false` with `reason:"nothing_to_undo"` / `"nothing_to_redo"`.
- `-32017 {reason:"bad_undo_history"|"bad_redo_history"}`.

### 17.10 `pet.functionTypes`

`{}` → `{count, types:[{type, index, name, className, metadata:[...], obsolete, singleton:null}]}`:
the registered particle operators, per function type 0..6.

- `obsolete` follows the editor's own hide rule (`MObsoleteParticleFunction`).
- `singleton` is always `null`.
- `-32017 {reason}`: `particle_mgr_null`, `particle_mgr_unreadable`, `function_count_unusable`,
  `function_definition_unreadable`. Never a partial list. Result buffer 512 KB.

### 17.11 `pet.schema`

`{class}` (1..127 characters of `[A-Za-z0-9_]`) → `{class, found, size, project, bases:[...], fields:[{name, type, declaredIn}], metadata:[...]}`.
Needs `pet.dll` and `schemasystem.dll`.

- Reads from the `particles.dll` schema scope.
- `fields` are flattened, base classes included, each with `declaredIn`. `bases` is the
  flattened chain, at most 8 levels deep.
- `found:false` is a result: `size`/`project` are `null` and the lists are empty.
- Refusals: `class: 1..127 chars`, `class: [A-Za-z0-9_] only`.
- `-32023`: `particles.dll has not registered its schema scope yet`,
  `particles.dll resolved to the global type scope`,
  `particles.dll schema walk hit an unreadable hop` (the walk is not partial).

### 17.12 `pet.render`

See §15.4.

## 18. Sound events

Needs `soundsystem.dll`. The bridge records sound-event starts into a 256-entry ring buffer. There
are two sources:

| Mode | Source |
|---|---|
| `wrap` | KHook post-hooks on `CSoundOpSystem`'s three start-sound-event entries (`via` 11, 12, 13). Needs KHook |
| `listener` | a bridge-owned listener registered with the sound system's add-listener call (`via` 81). No KHook |
| `both` | both sources |

Hooks only observe: they never change a call or its result.

### 18.1 `sound.subscribe`

`{mode?}` → status (§18.2). The arming row. It needs KHook, because the default mode installs the
wrap.

- `mode` is `wrap`, `listener` or `both`. When absent, the current selection is kept; with nothing
  armed yet, `wrap` is used. A source the mode does not name is removed.
- `-32602 "mode must be wrap|listener|both"`.
- `-32017 "SoundOpSystem001 is not the pinned CSoundOpSystem"`.
- `-32017 {"blockedOn":"khook: retry next tick","reason":...}` when KHook is busy: retry.

### 18.2 `sound.status`

`{mode?}` → `{installed, mode:"wrap"|"listener"|"both"|null, pending, total, dropped, ringSize:256, blockedOn?}`

- `sound.status` and `sound.drain` run the same mode selection as `sound.subscribe`, so the first
  of them arms the sources. They do not need KHook: without it they skip the wrap, still install
  the listener when `mode` names it, and add `"blockedOn":"metamod: KHook not loaded"` when the
  wrap was wanted. That member is a note, not an error.
- `pending` is the number of events waiting. `total` counts all events recorded. `dropped` counts
  events lost to a full ring.

### 18.3 `sound.drain`

`{max?, resolve?, mode?}` → take events from the ring (after the §18.2 mode selection).

- `max` defaults to 128 and is clamped to 1..256. `resolve` defaults to true: it adds the event
  name and the source entity's name.
- Result:
  `{events:[{seq, hash, name|null, entindex, guid, via, flags, ageMs, params?, aux?, inst150?, origin?, entityName?}], truncated, dropped, total, pending, blockedOn?}`.
  - `params` (`via` 11 only): the packed start parameters, `{len, hex, truncated}` (at most 128
    bytes as hex), or `null`.
  - `inst150` is reported for `via` 81 events.
  - `entityName` is reported when `resolve` is set and `entindex >= 0`.
- Each event is written whole or not at all. When the 96 KB reply fills before `max` events,
  `truncated` is true and the unwritten events go back to the front of the ring: call again.
- Overflow: `-32603 "InternalError: sound drain result too large"`.

### 18.4 `sound.uninstall`

`{}` → status. Removes both sources.

## 19. Input

Needs `client.dll` and `engine2.dll`. The bridge drives the local player's buttons and view angles
through a KHook hook on the client input object. Every reply is the input status (§19.2).

### 19.1 `input.set`

`{press?:[names], release?:[names], tap?:[names], frames?, angles?:[pitch,yaw,roll]|null}` (needs
KHook)

- Button names come from the game's own input table: case-insensitive, and each must name a single
  button bit. `input.status` lists them in `buttons`.
- `press` holds a button until it is released. `release` lets it go. `tap` presses it for
  `frames` frames (1..1000, default 1).
- A `release` and a `tap` of a held button in the same call give a real release on the next frame,
  then the tap.
- `angles` overrides the view angles every tick until `angles:null` or `input.clear`. While the
  override is set, console `setang` has no effect.
- An unknown name rolls the whole call back:
  `-32602 {reason:"unknown button or bad list: <name>"}`.
  Also `-32602 "angles must be [pitch,yaw,roll] or null"`.
- Install errors: `-32017 "g_pInput is not the pinned CCSGOInput (vtable)"`;
  `-32017 {"blockedOn":"khook: retry next tick","reason":...}`.

### 19.2 `input.status`, `input.clear`, `input.uninstall`

- Status object:
  `{installed, held:[names], angles:[p,y,r]|null, frames, edges, angleTicks, buttons:[names], conflicts}`.
  - `conflicts` counts calls since the last arm on which another plugin's hook also changed the
    view angles. Each such call also writes one rate-limited log line.
- `input.status` reads the status without installing anything.
- `input.clear` releases everything and drops the angle override.
- `input.uninstall` removes the hook.

## 20. Game state

### 20.1 Common rules

- All `game.*` reads run on the main thread and change nothing. `game.local`, `game.weapons`,
  `game.radar`, `game.rules` and `game.entities` need `client.dll`, `engine2.dll` and
  `schemasystem.dll`.
- Fields are resolved through the client schema at run time. A field that does not resolve is
  `null`, never a guess. `game.rules` lists those fields in `schemaMissing`.
- Most results begin with the session header `{tick, inGame, map, ...}`.
- Schema errors:
  - `-32017 "SchemaSystem_001 is not the pinned CSchemaSystem"`;
  - `-32017 "client.dll type scope is not the pinned CSchemaSystemTypeScope"`;
  - `-32017 "Source2EngineToClient001 is not the pinned CEngineClient"`;
  - `-32023 "client.dll has not registered its schema scope yet"`;
  - `-32023 "CEntityIdentity layout mismatch: schema ... vs header ... (update s2sdk upstream)"`:
    the generated SDK layout disagrees with the live schema.
- Result buffer 96 KB.

### 20.2 `game.tick`

`{}` → `{tick, frames}`. `tick` is the last server tick. `frames` counts the frames the input
hook (§19) has seen (0 until it is installed). Needs `engine2.dll`.

### 20.3 `game.local`

`{}` → `{tick, inGame, map, playerSlot, viewAngles, inputOverride:[p,y,r]|null, player}`

- `player` is
  `{controllerIndex, pawnIndex, name, team, alive, health, isLocal, origin, eyeAngles, spotted, spottedByMask, eye, velocity, lifeState, flags, scoped, armor, helmet, money, ducked, moveType, activeWeapon}`.
  `activeWeapon` is a weapon object (§20.4) or `null`.
- `inputOverride` is the angle override `input.set` holds.
- `-32024 NoLocalPlayer`: `no local controller (not in a game?)`,
  `local controller table disagrees with GetLocalPlayer`.

### 20.4 `game.weapons`

`{}` → `{tick, inGame, map, active, count, weapons:[{entindex, handle, name, defIndex, clip1, clip2, reserve, reloading}]}`

- `-32024`: also `local controller has no pawn (dead/spectating?)`, `pawn has no weapon services`.
- `-32023 "CPlayer_WeaponServices::m_hMyWeapons"`.

### 20.5 `game.radar`

`{}` → `{tick, inGame, map, players:[player without the local-only members], bomb:[...], hostages:[...], hostagesTruncated?}`

- `bomb` entries:
  - `{state:"planted", entindex, origin, ticking, defused, site, blowTime}`;
  - `{state:"carried"|"dropped", entindex, ownerPawnIndex, origin}`.
- `hostages` entries: `{entindex, origin, state, rescued, health, leaderPawnIndex}`.
- `spotted` / `spottedByMask` are the game's own flags. Every player, bomb and hostage is listed,
  spotted or not.

### 20.6 `game.rules`

`{}` →
`{tick, inGame, map, proxyIndex, warmup, freeze, teamIntro, gameRestart, hasMatchStarted, gamePhase, roundsPlayed, roundStartTime, restartRoundTime, roundTimeSec, roundWinStatus, roundEndReason, bombPlanted, schemaMissing:[...]}`

- `-32025 NoGameRules "no cs_gamerules proxy with a C_CSGameRules object (not in a game?)"`.
- `-32023 "C_CSGameRulesProxy::m_pGameRules"`.

### 20.7 `game.entities`

`{class?, name?, max?, entindex?}` → `{tick, inGame, map, entities:[{entindex, class, name, origin, angles, health, toggleState}], count, truncated}`

- `class` filters by designer name and `name` by targetname. `max` is 1..256 (default 64) and
  counts matches. `entindex` (1..16383) reads only that slot.
- `name` and `toggleState` are `null` when empty or unresolvable.
- `truncated` means more matches exist past `max`, or the reply filled (entities are written whole).
- Refusals: `max must be 1..256`, `entindex must be 1..16383`.

### 20.8 `game.selectWeapon`

Exactly one of:

| Param | Effect | Result |
|---|---|---|
| `slot` 0..13 | queues `slot<N>` on the command buffer | `{queued:true, via:"command", command}` |
| `command` | one of `invnext`, `invprev`, `invnextselect`, `invprevselect`, `lastinv` | same |
| `weapon` (`weapon_<name>`), `defIndex` 0..65535, `handle` (u32) | selects a weapon the local player holds, through the game's own weapon-selection call | `{queued:true, via:"MakeWeaponSelection", weapon:{...}}` |

- The weapon forms need `client.dll` and `schemasystem.dll`.
- Refusals:
  - `one of slot, command, weapon, defIndex or handle required`;
  - the per-member range texts;
  - `the local player does not hold that weapon (see game.weapons)`.
- `-32017`: `weapon/defIndex/handle selection needs client.dll and schemasystem.dll pinned`,
  `g_pInput is not the pinned CCSGOInput (vtable)`, `InputService_001 is not the pinned CInputService`.

### 20.9 `game.schemaProbe`

`{names?:[module names]}` → `{results:[{name, found, scopeName, isGlobal}]}`. Diagnostics: which
module names have a schema scope. Without `names` it probes a built-in list (`client.dll`,
`server.dll`, `engine2.dll`, ...).

### 20.10 `game.saveRestore`

`{enable?}` (default true) → `{enabled, conflicts}`. Needs `server.dll` and KHook.

- Lets the game save and load, through a KHook hook on the server config's save/restore query.
- `enable:false` disarms it.
- `conflicts` counts calls since the last arm on which another plugin's hook also changed the
  answer. Each also writes one rate-limited log line.
- `-32017 "Source2ServerConfig001 is not the pinned CCSGameConfiguration (+8 view)"`;
  `-32017 {"blockedOn":"khook: retry next tick"}`.

## 21. `cvar.get`

`{names:[...]}` → read console variables. Needs `tier0.dll`.

- `names`: 1..64 entries, each 1..128 bytes with no control characters.
- Result:
  `{cvars:[{name, found:true, type, flags, flagsRaw, value, valueTruncated?, default} | {name, found:false} | {name, found:false, error}]}`.
  - String values are cut at 1024 bytes.
  - A protected cvar reports `value:null, default:null, redacted:true`.
- Refusals: `params must be a JSON object`, `names (array of strings) required`,
  `names must hold 1..=64 entries, got N`.
- `-32017 "VEngineCvar007 is not the pinned CCvar"`.

## 22. Console and test script

### 22.1 `console.exec`

`{command}` → `{queued:true}`. Needs `engine2.dll`.

- Appends `command` (at most 1023 bytes) to the engine command buffer.
- Allowed only when the game was started with `-testscript` or `-tools`. Otherwise:
  `-32011 "console.exec is only enabled in -testscript or -tools sessions"`.
- `-32602 "command (string) required"`; `-32017 "InputService_001 is not the pinned CInputService"`.
- `queued:true` means the text was added to the buffer. The command runs later, on the engine's
  own schedule, and its output is not returned.

### 22.2 `test.checkpoint`

`{name}` (at most 127 bytes) → `{active}`. Needs `tier0.dll`.

- Calls the test-script manager's checkpoint with `name`.
- `active` says whether a test script is running.
- `-32602 "name (string) required"`; `-32017 "TestScriptMgr001 is not the pinned CTestScriptMgr"`.

### 22.3 `test.active`

`{}` → `{active}`.

## 23. `metamod.plugins`

`{}` → `{plugins:[{id, file, status, source, self}], generation, rebuiltAtMs}`. Answered on the
IPC lane (§4.5).

- `status`: `running`, `paused`, `refused`, `error`, `not_found`, or `unknown(<n>)`.
- `self` marks the bridge's own entry.
- The answer is a snapshot that the main thread rebuilds when the plugin list changes (Metamod's
  plugin events). Before the first rebuild it is `{plugins:[], generation:0, rebuiltAtMs:0}`.
- `-32017 {blockedOn:"metamod: not loaded"}` or `{blockedOn:"metamod: IPluginManager unavailable"}`.
- `-32603 "plugin table too large"`.

## 24. Blocked rows

These names are in the method table but not implemented. Each is answered at once with
`-32011 NotImplemented {"blockedOn":"<text>"}`.

| Method | `blockedOn` | Use instead |
|---|---|---|
| `doc.list` | `bridge-protocol.md §7 doc.list (not implemented in M2.2)` | `doc.active` |
| `map.run` | `superseded by hammer-mcp run_map (VToolMessage command channel)` | hammer-mcp `run_map` |
| `file.open_asset` | `superseded by map.open (m2.16): the VToolMessage asset route is not an open/reload primitive (file-edit-reload-compile.md §B)` | `map.open` |
| `entity.delete` | `superseded by node.delete (m2.5, by nodeID)` | `node.delete` |
| `mesh.setFaceRect`, `mesh.createFaceFromPoints`, `mesh.moveVertices`, `mesh.extrudeFaces`, `mesh.deleteFaces` | `superseded by mesh.setGeometry (polygon route, m2.5; MCP tools do the op)` | `mesh.setGeometry` |
| `material.assign` | `superseded by mesh.setMaterial (m2.5)` | `mesh.setMaterial` |

The `blockedOn` texts are the registry's literal strings.
