/* mcp_bridge_map.inc - the `map.open` arm (m2.16), included by mcp_bridge.c after
 * mcp_bridge_pet.inc (which brings in kv3_check.h's host-path helpers and the shared <app> helper
 * `editor_app_root`).
 *
 * `map.open {path}` opens a `.vmap` in Hammer through the same call the UI uses
 * (CBaseEditorApp::OpenFile, app vtable slot 18, flags 1) - the first map-opening primitive the
 * bridge has: `file.open_asset` (the VToolMessage asset route) was never implemented because it is
 * not an open/reload primitive at all (idabins docs/flows/file-edit-reload-compile.md §B).
 *
 * Four gates run BEFORE the call, all of them because OpenFile fails loudly-but-interactively:
 *
 *  1. path shape (map_path.h's map_open_path_ok): OpenFile's content-dir checks pop a modal box
 *     ("Cannot open asset ...", "outside the content dir") and block the tools main thread until a
 *     person clicks it. The gate accepts the CANONICAL addon-relative spelling only (review M1):
 *     no `\`, no `//`, no `.`/`..` segment and no segment ending in `.` or a space.
 *  2. the host file must exist (under <app>\content\csgo_addons\<addon>\maps\): a missing file pops
 *     the same kind of box. -32014 AssetNotFound, so the client sees a known code.
 *  3. the file must carry the binary-v9 DMX header (review H1): a file that fails to LOAD reaches
 *     OpenFileInternal's code 0/3 path, which pops the modal "Error opening file" QMessageBox on
 *     the tools main thread and wedges the drain (nobody clicks in the headless setup). The gate
 *     is map_path.h's map_vmap_header_ok on the file's first bytes - the header the crate's own
 *     reader (hammer-mcp/src/vmap/dmx.rs) accepts, NOT a 16-byte prefix (a `keyvalues2` and a
 *     `binary 8` header share `<!-- dmx encodin` with the accepted one). The Rust layer parses the
 *     whole file with its own reader before anything is sent; this is the defence in depth.
 *     -32014 AssetNotFound {reason:"InvalidMapFile"}.
 *  4. the file must not already be open in a session. OpenFileInternal's session-match slot (35,
 *     +0x118) is the `xor al,al; ret` stub `hammer!0x18009DBC0` in this build, so it never matches
 *     any session: a second OpenFile for an already-open file creates a SECOND session for the same
 *     file and leaks the first (its undo history, views and CMapDocSession stay alive). The bridge
 *     walks the app's session vector (S2_OFF_CBaseEditorApp_m_Sessions: count, data pointer at +8; the same vector
 *     OpenFileInternal loops) and answers `existing:true` without calling OpenFile. Each session's
 *     path TEXT comes from its docSession's +0x6B0 char* (map_session_path), NOT from slot 26,
 *     which hands back a CUtlString ADDRESS, never text (review C1).
 *
 * `force` from the first cut was dropped (review L1): it was not reachable from the Rust tool, it
 * could not bypass the unreadable-vector refusal, and its only effect was to create the duplicated
 * session the gate exists to prevent.
 *
 * All of it runs on the tools main thread (the queue drain), like every other hammer method.
 */

#include "map_path.h"       /* the path-shape gate, the DMX header gate + the same-file compare */

/* REVIEW C1: the session's map path TEXT (`<wine path>\...\maps\x.vmap`, exactly what
 * doc_fingerprint reads): the `char *` member of the docSession's CUtlString at +0x6B0 - the
 * same `char *` that session->m_Doc.m_Path and CMapDocSession_Path deref. Slot 26
 * (`CHammerEditorSessionVtbl::GetMapFilePath`) returns the ADDRESS of that CUtlString
 * (`lea rax,[rcx+0x6B0]` @hammer!0x13ED70; a set `cmove` swaps in a static default CUtlString when
 * there is no doc, whose m_pString == 0 makes it the EMPTY string). It was read as a
 * `const char *` before: the copied "bytes" were the raw pointer, and the leak gate could never
 * see an already-open file.
 *
 * 1 = `out` holds a NUL-terminated, non-empty, probed path; 0 = no doc / the empty string /
 * unreadable or cut storage (md_str probes readability again at every page boundary and answers 0
 * with `out[0] == 0` when it meets an unreadable byte). An untitled session answers 0 and is
 * skipped, never dereferenced. `mem_ok(p)` runs before the first byte of `p` is read (review-5 L1:
 * a stale or garbage `char *` here must fault nothing on `p[0]`; `mem_ok` is the guard).
 */
static int __attribute__((unused)) map_session_path(CHammerEditorSession *s, char *out, u32 cap) {
    CMapDocSession *d = s ? s->m_pDocSession : 0;
    const char *p = (d && mem_ok(d)) ? CMapDocSession_Path(d) : 0;
    return p && mem_ok(p) && p[0] && md_str(p, out, cap) == 1;
}

/* <app>\content\csgo_addons\<addon>\<rel>, the host file Hammer's OpenFile resolves for `rel`.
 * 1 = `host` holds it; 0 = `e` is set and OpenFile must NOT be called.
 * REVIEW L4: never inline (the review's stack note: 1.1 KB of locals must not share frame space
 * with op_map_open's 3.6 KB - staying under the 4 KB reserve depends on it). */
static __attribute__((noinline)) int map_host_gate(const char *rel, char *host, u32 hostcap, OpErr *e) {
    char app[1024], addon[64];
    HANDLE f;
    if (!editor_app_root(app, sizeof app)) {
        logline("map: open refused no_app_root");
        op_err(e, -32017, "PrerequisiteFailed", "no_app_root");
        return 0;
    }
    if (!kv3_addon_name(GetCommandLineA(), addon, sizeof addon)) {
        logline("map: open refused no_addon");
        op_err(e, -32017, "PrerequisiteFailed", "no_addon");
        return 0;
    }
    if (!kv3_host_path(app, addon, rel, host, hostcap)) {
        /* unreachable with these caps: app <= 1023, addon <= 63, rel <= 512 (MAP_PATH_MAX) */
        op_err(e, -32603, "InternalError", "host_path_too_long");
        return 0;
    }
    f = CreateFileA(host, GENERIC_READ,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    0, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, 0);
    if (!f || f == INVALID_HANDLE_VALUE) {
        logline("map: open refused no_such_file ");
        logline(host);
        op_err(e, -32014, "AssetNotFound", "no_such_file");
        return 0;
    }
    if (GetFileType(f) != PET_FILE_TYPE_DISK) {      /* a pipe/device would block a later read */
        CloseHandle(f);
        logline("map: open refused not_a_file");
        op_err(e, -32014, "AssetNotFound", "not_a_file");
        return 0;
    }
    CloseHandle(f);
    return 1;
}

/* REVIEW H1(b): the content gate. `host`'s first bytes must carry the DMX header the crate's
 * reader accepts (map_path.h's map_vmap_header_ok: `<!-- dmx encoding binary 9 format <fmt> <ver>
 * -->` + its NUL). 1 = OpenFile may be called; 0 = `e` is set and OpenFile is NOT called.
 * The window is MAP_VMAP_HEADER_MAX bytes - the whole point of the pure helper is that the header
 * (43 bytes live) sits well inside it, and a file whose header does not end inside it cannot be
 * one the reader accepts either.
 * REVIEW L4: noinline for the same stack reason as map_host_gate. */
static __attribute__((noinline)) int map_header_gate(const char *host, OpErr *e) {
    char head[MAP_VMAP_HEADER_MAX];
    const char *why = "?";
    HANDLE f;
    u32 got = 0;
    f = CreateFileA(host, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    0, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, 0);
    if (!f || f == INVALID_HANDLE_VALUE) {              /* vanished between the gates */
        logline("map: open refused header gate no file ");
        logline(host);
        op_err(e, -32014, "AssetNotFound", "no_such_file");
        return 0;
    }
    if (!ReadFile(f, head, sizeof head, &got, 0)) got = 0;   /* `got` is undefined on failure */
    CloseHandle(f);
    if (!got || !map_vmap_header_ok(head, (u32)got, &why)) {
        logline("map: open refused, not a binary-v9 vmap: ");
        logline(why);
        logline(" ");
        logline(host);
        op_err(e, -32014, "AssetNotFound", "InvalidMapFile");
        return 0;
    }
    return 1;
}

/* The session that already holds `host`, or 0. *activeOut = 1 when it is the app's active session
 * (map.open never switches documents). *badOut = 1 when the vector could not be walked at all - the
 * caller refuses rather than call OpenFile with the gate switched off (a stale layout must fail
 * loudly, not leak a session).
 *
 * REVIEW C1: the per-session path TEXT is the doc's `char *` at +0x6B0
 * (CMapDocSession_Path), NOT the value slot 26 (`GetMapFilePath`) returns - that is the
 * ADDRESS of the docSession's CUtlString member (`lea rax,[rcx+0x6B0]` @hammer!0x13ED70), so the old
 * `md_str(s->vt->GetMapFilePath(s), ...)` copied the 6-ish bytes of a heap pointer and never
 * matched anything (the leak gate could never fire, and every repeat open leaked a session).
 * REVIEW L6: an unusable session path is NOT an error and never was: the loop `continue`s past it
 * (an untitled session has no path at all). REVIEW L4: noinline (the 3.6 KB of local buffers stay
 * under the 4 KB reserve only if the helpers do not share frame space). */
static __attribute__((noinline)) CHammerEditorSession *map_session_of(CHammerApp *app, const char *host,
                                                                     int *activeOut, int *badOut) {
    u32 n = CHammerApp_SessionCount(app), i;
    CHammerEditorSession **data = CHammerApp_Sessions(app);
    *activeOut = 0;
    *badOut = 0;
    if (!n) return 0;
    if (n > 4096u || !data || !mem_ok(data) || !mem_ok(data + n - 1)) {
        lognum("map: session vector unreadable, count ", n);
        *badOut = 1;
        return 0;
    }
    for (i = 0; i < n; i++) {
        CHammerEditorSession *s = data[i];
        char spath[1024];
        if (!s) continue;
        if (!obj_is(s, PIN_HAMMER, HV_SESSION_VT)) {      /* not a session: the layout moved */
            lognum("map: session vector entry is not a session at index ", i);
            *badOut = 1;
            return 0;
        }
        if (!map_session_path(s, spath, sizeof spath)) {
            lognum("map: session has no readable path (no doc / untitled / cut) at index ", i);
            continue;                                     /* an untitled/empty session, not a match */
        }
        if (map_path_eq(spath, host)) {
            *activeOut = (s == CHammerApp_ActiveSession(app));
            return s;
        }
    }
    return 0;
}

/* map.open {path}: the path + host + content + session-leak gates, then OpenFile; the result
 * carries the new session's Wine path (`opened:true`), the session that already holds the file
 * (`existing:true`, OpenFile NOT called) or `opened:false` with a reason (the re-entrancy guard, or
 * a load failure - the client turns that into OpenFailed, NOT retry-safe). */
static int op_map_open(const char *params, SB *out, OpErr *e) {
    char path[MAP_PATH_MAX + 8], host[2048], openPath[1024];
    const char *reason = "?";
    CHammerApp *app;
    CHammerEditorSession *existing;
    int active = 0, bad = 0;
    void *opened;
    path[0] = 0;
    /* review-5 L7: map_session_path leaves `out` untouched when the path is null/empty, so the
       `existing` branch below could read openPath uninitialised; it cannot happen today (the same
       call just succeeded inside map_session_of on the same thread), but the empty string is what
       that branch's fallback (`?:`) expects, so it is set here, at the declaration. */
    openPath[0] = 0;
    if (!params || !json_string(params, "path", path, sizeof path) || !path[0]) {
        op_err(e, -32602, "InvalidParams",
               "path: relative to the addon content root (maps/... .vmap), 1..512 chars, no '..'");
        return 0;
    }
    {
        const char *raw = json_find(params, "path");        /* json_string collapses escapes */
        if (raw) {
            for (const char *q = raw + 1; *q && *q != '"'; q++) {
                if (*q == '\\') {
                    op_err(e, -32602, "InvalidParams", "path: no JSON escapes (use '/' separators)");
                    return 0;
                }
            }
        }
    }
    if (!map_open_path_ok(path, &reason)) { op_err(e, -32602, "InvalidParams", reason); return 0; }
    if (!map_host_gate(path, host, sizeof host, e)) return 0;
    if (!map_header_gate(host, e)) return 0;
    app = resolve_hammer();
    if (!app) { op_err(e, -32003, "NoActiveDocument", "hammer_unavailable"); return 0; }
    existing = map_session_of(app, host, &active, &bad);
    if (bad) { op_err(e, -32603, "InternalError", "session_vector_unreadable"); return 0; }
    if (existing) {
        /* REVIEW C1: the session's own path TEXT via map_session_path; when that is unusable
           (untitled/cut) the requested addon-relative path names the same file and is what the
           caller asked about. */
        map_session_path(existing, openPath, sizeof openPath);
        doc_fingerprint();
        logline("map: already open ");
        logline(openPath[0] ? openPath : path);
        sb_add(out, "{\"opened\":false,\"existing\":true,\"path\":");
        sb_addjson(out, openPath[0] ? openPath : path);
        sb_add(out, ",\"active\":"); sb_add(out, active ? "true" : "false");
        sb_add(out, ",\"epoch\":"); sb_addnum(out, g_fp.epoch);
        sb_add(out, "}");
        return 1;
    }
    logline("map: open ");
    logline(path);
    logline(" -> host ");
    logline(host);                                  /* review L3: both spellings in the log */
    opened = app->vt->OpenFile(app, path, 1);       /* flags 1 = what the UI passes */
    lognum("map: open rc ", (u64)(long long)(size_t)opened);
    doc_fingerprint();                              /* a new session changes the fingerprint */
    openPath[0] = 0;
    /* REVIEW L6/C1: the session OpenFile RETURNED is the precise answer (slot 18 hands it back), and
       its path text is read the same way as the leak gate's. The active session is the fallback: on
       a success they are the same object, and a failure leaves both empty. */
    if (opened && !map_session_path((CHammerEditorSession *)opened, openPath, sizeof openPath)) {
        CHammerEditorSession *as = CHammerApp_ActiveSession(app);
        if (as && obj_is(as, PIN_HAMMER, HV_SESSION_VT))
            map_session_path(as, openPath, sizeof openPath);
    }
    if (openPath[0]) {                              /* review L3: Hammer's own spelling of it */
        logline("map: session path ");
        logline(openPath);
    }
    sb_add(out, "{\"opened\":"); sb_add(out, opened ? "true" : "false");
    sb_add(out, ",\"existing\":false,\"path\":");
    /* on failure there is no new document to report: the path the caller named is what it asked
       about. `opened:false` is the re-entrancy guard (its own log line) or a load failure,
       indistinguishable after the fact - the client maps this to OpenFailed and treats it as NOT
       retry-safe (its own local vmap parse is what made a malformed file unreachable; the box, not
       the file, is what a retry would pop again). */
    sb_addjson(out, opened ? openPath : path);
    if (!opened) sb_add(out, ",\"reason\":\"open_failed\"");
    sb_add(out, ",\"epoch\":"); sb_addnum(out, g_fp.epoch);
    sb_add(out, "}");
    return 1;
}
