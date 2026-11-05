/* ---------------- pet.dll lifecycle (m2.12): .vpcf documents + the operator registry ----------------

   Object chain (hammer-mcp/research/pet-re.md, all [V] static; [I] marked inline): the load path
   is toolframework2's (`GetToolIndexByName(fw, &u8 idx, "pet")` slot 22 + `LoadToolSystem(fw,
   u8 idx)` slot 24 (S2_SLOT_IToolFramework2_LoadTool), exactly what `tool_request pet` does minus tool slot
   16, a nop in pet). pet.dll's exported `CreateInterface("ToolSystem2_001")` (*ret 0 = found*)
   returns the static singleton whose identity is `base+PETG_APP_OBJ`:
     app->m_pActiveSession -> active CPetSession (may be 0)
       -> session->m_pDoc -> CPetDoc (vtable PETV_DOC_VT)
            -> doc->m_pRoot -> CPetRootNode (0 when the document is empty or untitled:
               the "no root" state - a load pet cannot parse never reaches it, it wedges instead)
            -> PETFN_ROOT_GETCHILDCOUNT/GETCHILD(root, type 0..8, i) + node vtable slot
               S2_SLOT_CBasePetNode_GetClassNameA -> the child's `_class`.

   Everything below runs on the main-thread drain. pet.dll is late-loaded: modules_poll pins and
   verifies it after LoadToolSystem maps it (the module poll picks the DLL up within ~32 frames,
   then the verifier hashes 9.3 MB). A failed LoadToolSystem is cached by the framework forever and
   pops a modal, so `pet.load` runs it at most once and reports loaded:false as a RESULT.

   Modality policy (same as modeldoc, protocol-diff-m2.12.md): only host-validated, addon-content-
   relative `particles/...` paths reach OpenFile, whose "Cannot open" / "Open anyway?" / "Failed to
   load" modals would block the main thread. ReloadDocument never prompts (pet-re.md [V static]),
   but it does NOT fail gracefully on a file pet cannot parse: live 2026-10-13 (the "pet load
   robustness" table in protocol-diff-m2.12.md) three inputs recursed until the Wine main thread's
   stack overflowed (`err:virtual:virtual_setup_exception stack overflow`) and wedged the process
   even against SIGTERM - `{{{ not kv3`, a valid header + truncated body, and a BALANCED file with a
   key that has no value - while a wrong root `_class` killed it outright (fail-fast c0000409). The
   old "a failed load leaves the document with NO root" claim is WRONG for malformed input. So BOTH
   `pet.reload` (the path captured from GetPath) and `pet.open` (the path resolved from the app root
   + the `-addon` name) read the file and run it through the kv3_check.h gate - vpcf_check: kv3
   header with format:vpcf, the KV3-text grammar, and the ROOT object's own `_class` - and refuse
   with -32602 UnloadableFile BEFORE the call. */

#include "kv3_check.h"      /* G2: the grammar + root-class gate pet.reload/pet.open run first */
#include "json_text.h"      /* m2.13 WP-B1: the JSON string-body unescaper pet.apply runs first */

/* (b) a virtual call through a vtable slot index of the receiver (the generated S2_SLOT_* number;
   the sigs DB measures it as a byte displacement and holds it to slot * 8). */
static void *pet_vfn(void *obj, u32 slot) {
    void **vt = *(void ***)obj;
    return vt[slot];
}

typedef int (*PetMgrFuncCountFn)(void *mgr, int type);
typedef void *(*PetMgrFuncGetFn)(void *mgr, int type, int index);
typedef const char *(*PetFactoryNameFn)(void *def);
typedef void **(*PetFactoryClassInfoFn)(void *def, void **out8);
typedef const char *(*PetNodeClassNameFn)(void *node);
typedef int (*PetRootChildCountFn)(void *root, int type);
typedef void *(*PetRootChildFn)(void *root, int type, int index);

/* m2.13 WP-B1: the live-edit entry points, each pinned to a byte-checked prologue in the sigs DB
   (never a cast through void *). */
typedef void (*PetDocBeginUndoStepFn)(void *doc, const char *name, u8 notify);
typedef void (*PetDocUndoFn)(void *doc);
typedef void (*PetDocRedoFn)(void *doc);
typedef u8 (*PetDocSaveToFileFn)(void *doc, const char *path, void *err, void *cb);
typedef void (*PetRootLoadFromKV3Fn)(void *root, void *kv);
typedef void (*PetRootRefreshCachesFn)(void *root, void *caches);
typedef void (*PetKv3CtxFn)(void *ctx, int mode);
typedef void *(*PetKv3CtxRootFn)(void *ctx);
typedef void (*PetKv3CtxDtorFn)(void *ctx);
typedef void (*PetSessionOnDocModifiedFn)(void *session);
typedef void (*PetNotifyFuncListFn)(void *obj);
typedef u8 (*PetLoadKV3Fn)(void *kv, void *err, const char *text, const void *id, const char *name, u32 flags);
typedef void *(*PetSchemaTypeScopeFn)(void);
typedef void (*PetKv3StripToSchemaFn)(void *kv, void *typeScope);
typedef u32 (*PetRootBehaviorVersionFn)(void *root);
typedef void (*PetRootFilterFunctionsFn)(void *root, u32 behaviorVersion);

/* the non-virtual, non-exported pet entry points: bound once from their pinned RVAs (byte-pinned
   prologues) after the module's identity check, exactly like MDFN_SESSION_COMPILE. `g_petChild*`
   are pass-1's root walkers; the rest are m2.13's live-edit calls. */
static PetRootChildCountFn g_petChildCount;
static PetRootChildFn g_petChild;
static PetDocBeginUndoStepFn g_petBeginUndoStep;
static PetDocUndoFn g_petUndo;
static PetDocRedoFn g_petRedo;
static PetDocSaveToFileFn g_petSaveToFile;
static PetRootLoadFromKV3Fn g_petRootLoadFromKV3;
static PetRootRefreshCachesFn g_petRefreshCaches;
static PetKv3CtxFn g_petKv3CtxCtor;
static PetKv3CtxRootFn g_petKv3CtxRoot;
static PetKv3CtxDtorFn g_petKv3CtxDtor;
static PetSessionOnDocModifiedFn g_petSessionOnDocModified;
static PetNotifyFuncListFn g_petNotifyFuncList;
static PetSchemaTypeScopeFn g_petTypeScope;
static PetKv3StripToSchemaFn g_petStripToSchema;
static PetRootBehaviorVersionFn g_petBehaviorVersion;
static PetRootFilterFunctionsFn g_petFilterFunctions;
static int g_petBound;
static CPetApp *g_pet;                  /* resolved once; pinned module -> stable identity */

/* pet.load's one-shot latch. The framework loads a tool DLL once per index and caches the
   outcome - a failed load is never retried and pops a modal - so the bridge attempts the load
   exactly once per session and memoizes the answer: a second pet.load returns the same result
   instead of asking the framework again (which is what "load once, restart the runtime on
   loaded:false" in the protocol note means). 0 = not attempted; once attempted, LoadToolSystem was
   called exactly once with the confirmed index (`g_petLoadIdx` < 0 = GetToolIndexByName said
   unknown, so it was never called at all). */
static int g_petLoadTried;
static int g_petLoadIdx = -1;
static int g_petLoadOk;

static void pet_bind(void) {
    if (g_petBound || !mod_ok(PIN_PET)) return;
    u8 *pb = (u8 *)g_modBase[PIN_PET];
    g_petChildCount = (PetRootChildCountFn)(pb + PETFN_ROOT_GETCHILDCOUNT);
    g_petChild = (PetRootChildFn)(pb + PETFN_ROOT_GETCHILD);
    g_petBeginUndoStep = (PetDocBeginUndoStepFn)(pb + PETFN_DOC_BEGINUNDOSTEP);
    g_petUndo = (PetDocUndoFn)(pb + PETFN_DOC_UNDO);
    g_petRedo = (PetDocRedoFn)(pb + PETFN_DOC_REDO);
    g_petSaveToFile = (PetDocSaveToFileFn)(pb + PETFN_DOC_SAVETOFILE);
    g_petRootLoadFromKV3 = (PetRootLoadFromKV3Fn)(pb + PETFN_ROOT_LOADFROMKV3);
    g_petRefreshCaches = (PetRootRefreshCachesFn)(pb + PETFN_ROOT_REFRESHCACHES);
    g_petKv3CtxCtor = (PetKv3CtxFn)(pb + PETFN_KV3CTX_CTOR);
    g_petKv3CtxRoot = (PetKv3CtxRootFn)(pb + PETFN_KV3CTX_ROOT);
    g_petKv3CtxDtor = (PetKv3CtxDtorFn)(pb + PETFN_KV3CTX_DTOR);
    g_petSessionOnDocModified = (PetSessionOnDocModifiedFn)(pb + PETFN_SESSION_ONDOCMODIFIED);
    g_petNotifyFuncList = (PetNotifyFuncListFn)(pb + PETFN_NOTIFY_FUNCLIST);
    g_petTypeScope = (PetSchemaTypeScopeFn)(pb + PETFN_SCHEMA_TYPESCOPE);
    g_petStripToSchema = (PetKv3StripToSchemaFn)(pb + PETFN_KV3_STRIP_TO_SCHEMA);
    g_petBehaviorVersion = (PetRootBehaviorVersionFn)(pb + PETFN_ROOT_BEHAVIORVERSION);
    g_petFilterFunctions = (PetRootFilterFunctionsFn)(pb + PETFN_ROOT_FILTER_FUNCTIONS);
    g_petBound = 1;
}

/* CPetApp via pet.dll's exported CreateInterface - the static singleton, identity-checked against
   its pinned static object (mirrors resolve_modeldoc). */
static CPetApp *resolve_pet(void) {
    if (g_pet) return g_pet;
    if (!mod_ok(PIN_PET)) { logline("pet: module not verified"); return 0; }
    u8 *pb = (u8 *)g_modBase[PIN_PET];
    void *hm = GetModuleHandleA(k_pinModules[PIN_PET].name);
    if (hm != (void *)pb) { logline("pet: module handle changed"); return 0; }
    S2CreateInterfaceFn ci = (S2CreateInterfaceFn)GetProcAddress(hm, S2_SYM_CREATEINTERFACE);
    if (!ci) { logline("pet: CreateInterface export missing"); return 0; }
    int ret = -1;
    CPetApp *app = (CPetApp *)ci(S2_TOOLSYSTEM2_VERSION, &ret);
    if (!app || ret != 0) {                     /* ret 0 = found */
        lognum("pet: ToolSystem2_001 unresolved, ret=", (u64)(long long)ret);
        return 0;
    }
    if ((void *)app != (void *)(pb + PETG_APP_OBJ)) { logline("pet: app identity mismatch"); return 0; }
    if (!obj_is(app, PIN_PET, PETV_APP_VT)) { logline("pet: app vtable mismatch"); return 0; }
    pet_bind();
    g_pet = app;
    logline("pet: app resolved (ToolSystem2_001 export, ret 0)");
    return app;
}

/* active session with per-hop validation. reason = short diagnostic string. */
static CPetSession *active_pet(const char **reason) {
    *reason = "ok";
    if (!mod_ok(PIN_PET)) {
        *reason = (g_modState[PIN_PET] == MS_MISMATCH) ? "pet_mismatch"
                : (g_modState[PIN_PET] == MS_VERIFYING) ? "pet_verifying"
                : "pet_not_loaded";
        return 0;
    }
    CPetApp *app = resolve_pet();
    if (!app) { *reason = "pet_unavailable"; return 0; }
    CPetSession *session = CPetApp_ActiveSession(app);
    if (!session) { *reason = "no_session"; return 0; }
    if (!obj_is(session, PIN_PET, PETV_SESSION_VT)) { *reason = "bad_session"; return 0; }
    if (!session->m_pDoc) { *reason = "no_document"; return 0; }
    if (!obj_is(session->m_pDoc, PIN_PET, PETV_DOC_VT)) { *reason = "bad_doc"; return 0; }
    return session;
}

/* the child's `_class` (node vtable slot S2_SLOT_CBasePetNode_GetClassNameA). Strict: a null or
   unreadable pointer, or a name that does not fit the bounded buffer, is a failure - a cut
   class name would be silently wrong. The node itself cannot be identity-checked (the class
   varies per config), so the vptr, the slot it is called through and the returned string are
   each probed before use. */
static int pet_node_class(void *node, char *out, u32 cap) {
    if (!node || !mem_ok(node)) return 0;
    void **vt = *(void ***)node;
    if (!vt || !mem_ok(vt) || !mem_ok(&((void **)vt)[S2_SLOT_CBasePetNode_GetClassNameA])) return 0;
    const char *s = ((PetNodeClassNameFn)pet_vfn(node, S2_SLOT_CBasePetNode_GetClassNameA))(node);
    return md_str(s, out, cap) == 1;
}

/* the nine function arrays of the active root node: [{type, count, classes:[...]}]. Fails closed:
   a count outside 0..4096 or an unreadable child/class aborts the whole object (`e` set, 0
   returned) and the caller rolls the reply buffer back - never a partial list. */
static int pet_arrays_json(SB *out, void *root, OpErr *e) {
    sb_add(out, "[");
    for (int t = 0; t <= 8; t++) {
        int count = g_petChildCount(root, t);        /* direct call, pinned prologue */
        if (count < 0 || count > 4096) {
            op_err(e, -32017, "PrerequisiteFailed", "root_child_count_unusable");
            return 0;
        }
        if (t) sb_add(out, ",");
        sb_add(out, "{\"type\":"); sb_addi(out, t);
        sb_add(out, ",\"count\":"); sb_addi(out, count);
        sb_add(out, ",\"classes\":[");
        for (int i = 0; i < count; i++) {            /* GetChild has NO bounds check: 0<=i<count */
            char cls[128];
            if (!pet_node_class(g_petChild(root, t, i), cls, sizeof cls)) {
                op_err(e, -32017, "PrerequisiteFailed", "root_child_unreadable");
                return 0;
            }
            if (i) sb_add(out, ",");
            sb_addjson(out, cls);
        }
        sb_add(out, "]}");
    }
    sb_add(out, "]");
    return 1;
}

/* the doc object shared by pet.active / pet.open. 1 = wrote it, 0 = no doc to write and *reason
   says why (no document / bad root - a result, not an error), -1 = `e` is set and
   nothing may be written. */
static int pet_doc_json(SB *out, CPetSession *session, const char **reason, OpErr *e) {
    u32 mark = out->len;
    if (!session->m_pDoc) { *reason = "no_document"; return 0; }
    CPetDoc *doc = session->m_pDoc;
    char path[1024];
    md_str(session->vt->GetPath(session), path, sizeof path);
    int dirty = session->vt->IsModified(session) ? 1 : 0;
    void *root = doc->m_pRoot;                 /* 0 when there is no root (empty/untitled doc): hasRoot:false, arrays:null */
    sb_add(out, "{\"path\":"); sb_addjson(out, path);
    sb_add(out, ",\"dirty\":"); sb_add(out, dirty ? "true" : "false");
    sb_add(out, ",\"isNewFile\":"); sb_add(out, session->m_bIsNew ? "true" : "false");
    sb_add(out, ",\"hasRoot\":"); sb_add(out, root ? "true" : "false");
    sb_add(out, ",\"arrays\":");
    if (!root) {
        sb_add(out, "null");
    } else {
        if (!obj_is(root, PIN_PET, PETV_ROOTNODE_VT)) {
            *reason = "bad_root";
            out->len = mark; out->buf[mark] = 0;
            return 0;
        }
        pet_bind();
        if (!pet_arrays_json(out, root, e)) {
            out->len = mark; out->buf[mark] = 0;
            return -1;
        }
    }
    sb_add(out, ",\"docType\":\"vpcf\"}");
    return 1;
}

/* ---- G1: the loadability guard pet.reload runs before ReloadDocument ---- */

/* 16 MB is far past any real .vpcf (the biggest in the game are a few hundred KB) and bounds the
   transient allocation the guard makes on the main thread. */
#define PET_VPCF_MAX ((u64)16 << 20)

/* GetFileType's FILE_TYPE_DISK: only a real file is read. A FIFO/device named *.vpcf inside the
   addon content dir would block ReadFile on the main thread forever, which is worse than a
   refusal. */
#define PET_FILE_TYPE_DISK 1u

/* read `path` (at most PET_VPCF_MAX bytes) into a fresh HeapAlloc buffer sized from the file. 1 =
   bufOut/lenOut are set (the caller frees with HeapFree(GetProcessHeap(), 0, ...)); 0 = reasonOut is
   set: "too_large" (over PET_VPCF_MAX), else "read_failed" (cannot open/size/read, not a disk file,
   allocation failed, or the file grew while it was read). */
static int pet_read_vpcf(const char *path, u8 **bufOut, u32 *lenOut, const char **reasonOut) {
    *bufOut = 0;
    *lenOut = 0;
    *reasonOut = "?";
    HANDLE f = CreateFileA(path, GENERIC_READ,
                           FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                           0, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, 0);
    if (!f || f == INVALID_HANDLE_VALUE) { *reasonOut = "read_failed"; return 0; }
    if (GetFileType(f) != PET_FILE_TYPE_DISK) {             /* a pipe/device would block forever */
        CloseHandle(f);
        *reasonOut = "read_failed";
        return 0;
    }
    /* size the buffer from the file (one extra byte detects a file that grew while we read) */
    u32 hi = 0, size = GetFileSize(f, &hi);
    if (size == 0xFFFFFFFFu) { CloseHandle(f); *reasonOut = "read_failed"; return 0; }   /* error (or 4 GB) */
    if (hi || size > (u32)PET_VPCF_MAX) { CloseHandle(f); *reasonOut = "too_large"; return 0; }
    u8 *buf = (u8 *)HeapAlloc(GetProcessHeap(), 0, (u64)size + 1);
    if (!buf) { CloseHandle(f); *reasonOut = "read_failed"; return 0; }
    u32 total = 0;
    int ok = 1;
    while (total <= size) {
        u32 want = (size + 1) - total, got = 0;
        if (!ReadFile(f, buf + total, want, &got, 0)) { ok = 0; break; }
        if (!got) break;                        /* EOF */
        total += got;
    }
    CloseHandle(f);
    if (!ok || total > size) {                  /* a read error, or the file changed under us */
        HeapFree(GetProcessHeap(), 0, buf);
        *reasonOut = "read_failed";
        return 0;
    }
    *bufOut = buf;
    *lenOut = total;
    return 1;
}

/* the -32602 reply for a refused reload: message UnloadableFile, data {reason, path}. OpErr.data
   is only 192 B, so a long GetPath is shortened from the left (a '~' marks the cut) until the JSON
   fits - a truncated raw path would emit a value with no closing quote (invalid JSON). */
static void pet_unloadable(OpErr *e, const char *reason, const char *path) {
    e->code = -32602;
    e->name = "UnloadableFile";
    char pbuf[1040];
    {                                           /* the whole path when it fits */
        SB db;
        sb_init(&db, e->data, sizeof e->data);
        sb_add(&db, "{\"reason\":"); sb_addjson(&db, reason);
        sb_add(&db, ",\"path\":"); sb_addjson(&db, path);
        sb_add(&db, "}");
        if (!db.ovf) return;
    }
    pbuf[0] = '~';
    cpy(pbuf + 1, path, sizeof pbuf - 1);
    for (;;) {
        SB db;
        sb_init(&db, e->data, sizeof e->data);
        sb_add(&db, "{\"reason\":"); sb_addjson(&db, reason);
        sb_add(&db, ",\"path\":"); sb_addjson(&db, pbuf);
        sb_add(&db, "}");
        if (!db.ovf) return;
        u32 n = slen(pbuf);
        if (n <= 1) {                           /* nothing left to cut: keep the JSON valid */
            sb_init(&db, e->data, sizeof e->data);
            sb_add(&db, "{\"reason\":"); sb_addjson(&db, reason);
            sb_add(&db, ",\"path\":null}");
            return;
        }
        memmove(pbuf + 1, pbuf + 2, n - 1);     /* drop one byte after the marker */
        pbuf[0] = '~';
    }
}

/* one log line per refusal (the bridge log has no printf): "pet: <what> refused <reason>: <path>" */
static void pet_log_refusal(const char *what, const char *reason, const char *path) {
    char msg[1200];
    u32 n;
    cpy(msg, "pet: ", sizeof msg);
    n = slen(msg);
    cpy(msg + n, what, sizeof msg - n);
    n = slen(msg);
    cpy(msg + n, " refused ", sizeof msg - n);
    n = slen(msg);
    cpy(msg + n, reason, sizeof msg - n);
    n = slen(msg);
    cpy(msg + n, ": ", sizeof msg - n);
    n = slen(msg);
    cpy(msg + n, path, sizeof msg - n);
    logline(msg);
}

/* ---- G2: the pet.open gate. pet.open's caller names the file addon-content-relative
   (`particles/x.vpcf`) because that is what pet's own OpenFile resolves; the bridge turns it into
   the same file on the host - <app>\content\csgo_addons\<addon>\<path with '/' -> '\'> - and runs
   the same vpcf_check as pet.reload before OpenFile (a malformed file wedges the main thread there
   too, and a missing one pops a modal). The derivation itself is pure string code in kv3_check.h
   (kv3_app_root / kv3_addon_name / kv3_host_path) and is covered by the host unit test; this
   wrapper only supplies the two process facts: <app> from GetModuleFileNameA(NULL) (the exe is
   <app>\game\bin\win64\cs2.exe) and <addon> from `-addon <name>` in GetCommandLineA (runtime.sh
   passes it). */

/* <app> for the content-host gates (pet.open, map.open). It must NOT be inlined into a gate: its
   1 KB exe buffer plus a gate's own buffers would pass 4 KB, and the freestanding Windows build has
   no __chkstk for clang's stack probe to call. */
static int __attribute__((noinline)) editor_app_root(char *out, u32 cap) {
    char exe[1024];
    u32 n = GetModuleFileNameA(0, exe, sizeof exe);
    if (!n || n >= sizeof exe) return 0;
    return kv3_app_root(exe, (u64)n, out, cap);
}

/* resolve `path` and gate it. 1 = loadable (nothing written); 0 = `e` set, OpenFile must NOT be
   called. data.path is the host file the gate read, the caller's addon-relative path for the two
   path-shape refusals (which never reach a file). */
static int pet_open_gate(const char *path, OpErr *e) {
    char app[1024], addon[64], host[2048];
    u8 *fbuf = 0;
    u32 flen = 0;
    const char *why = "?";
    int ok;
    if (!editor_app_root(app, sizeof app)) {
        pet_log_refusal("open", "no_app_root", path);
        pet_unloadable(e, "no_app_root", path);
        return 0;
    }
    if (!kv3_addon_name(GetCommandLineA(), addon, sizeof addon)) {
        pet_log_refusal("open", "no_addon", path);
        pet_unloadable(e, "no_addon", path);
        return 0;
    }
    if (!kv3_host_path(app, addon, path, host, sizeof host)) {
        /* unreachable with the caps above: app <= 1023, addon <= 63, path <= 512, all bounded by
           the caller (a path over 512 chars is refused as InvalidParams before this runs) */
        pet_log_refusal("open", "read_failed", path);
        pet_unloadable(e, "read_failed", path);
        return 0;
    }
    if (!pet_read_vpcf(host, &fbuf, &flen, &why)) {
        pet_log_refusal("open", why, host);
        pet_unloadable(e, why, host);
        return 0;
    }
    ok = vpcf_check((const char *)fbuf, (u64)flen, &why);
    HeapFree(GetProcessHeap(), 0, fbuf);
    if (!ok) {
        pet_log_refusal("open", why, host);
        pet_unloadable(e, why, host);
        return 0;
    }
    return 1;
}

/* pet.load: NEED_TF2 (pet.dll is not loaded yet, so NEED_PET could never be satisfied). The
   framework is asked for the tool index, LoadToolSystem is called exactly ONCE with it (see
   g_petLoadTried), and loaded:false is a result: a failed load is cached by the framework forever
   and pops a modal, so the session has to be restarted rather than retried. Never calls
   LoadToolSystem for an index the framework did not confirm (0xFF = unknown). */
static int op_pet_load(SB *out, OpErr *e) {
    IToolFramework2 *fw = (IToolFramework2 *)mod_iface(PIN_TF2, S2_IFACE_TOOLFRAMEWORK2);
    if (!obj_is(fw, PIN_TF2, TFV_FRAMEWORK_VT)) {
        op_err(e, -32017, "PrerequisiteFailed", "framework_unavailable");
        return 0;
    }
    if (GetModuleHandleA(k_pinModules[PIN_PET].name)) {     /* already mapped: nothing is called */
        sb_add(out, "{\"loaded\":true,\"alreadyLoaded\":true,\"toolIndex\":null,\"module\":");
        sb_addjson(out, mod_state_name(PIN_PET));
        sb_add(out, "}");
        return 1;
    }
    if (!g_petLoadTried) {              /* the one attempt: index lookup + LoadToolSystem */
        g_petLoadTried = 1;
        u64 idx = 0xFF;                 /* the framework stores a 1-byte index */
        fw->vt->GetToolIndexByName(fw, &idx, "pet");
        u8 tool = (u8)idx;
        if (tool == 0xFF) {             /* no such tool: LoadToolSystem would be a wild index */
            g_petLoadIdx = -1;
            op_err(e, -32017, "PrerequisiteFailed", "tool_index_unknown");
            return 0;
        }
        void *rc = fw->vt->LoadToolSystem(fw, tool);
        g_petLoadIdx = (int)tool;
        g_petLoadOk = (rc != 0 && GetModuleHandleA(k_pinModules[PIN_PET].name) != 0);
        lognum("pet: LoadToolSystem rc ", (u64)(long long)(size_t)rc);
    }
    if (g_petLoadIdx < 0) {             /* memoized: the tool index was never known */
        op_err(e, -32017, "PrerequisiteFailed", "tool_index_unknown");
        return 0;
    }
    sb_add(out, "{\"loaded\":"); sb_add(out, g_petLoadOk ? "true" : "false");
    sb_add(out, ",\"alreadyLoaded\":false");
    sb_add(out, ",\"toolIndex\":"); sb_addi(out, (long long)g_petLoadIdx);
    sb_add(out, ",\"module\":"); sb_addjson(out, mod_state_name(PIN_PET));
    sb_add(out, "}");
    return 1;
}

/* pet.open {path}: pet's OpenFile pops modals for a missing file, a path outside the content dir
   and load errors, so every garbled path is refused here first (same policy as modeldoc.open,
   with the .vpcf extension). The path is relative to the addon content root.
   G2: the path is then resolved on the host (pet_open_gate) and the file must pass the same
   vpcf_check as pet.reload - a malformed .vpcf that reaches OpenFile wedges the main thread (live
   2026-10-13), and a missing one pops a modal. */
static int op_pet_open(const char *params, SB *out, OpErr *e) {
    char path[520];
    path[0] = 0;
    if (!params || !json_string(params, "path", path, sizeof path) || !path[0]) {
        op_err(e, -32602, "InvalidParams",
               "path: relative to the addon content root (particles/...), 1..512 chars, no '..'");
        return 0;
    }
    u32 pl = slen(path);
    if (pl > 512) { op_err(e, -32602, "InvalidParams", "path: 1..512 chars"); return 0; }
    {
        const char *raw = json_find(params, "path");    /* json_string collapses escapes */
        if (raw) {
            for (const char *q = raw + 1; *q && *q != '"'; q++) {
                if (*q == '\\') {
                    op_err(e, -32602, "InvalidParams", "path: no JSON escapes (use '/' separators)");
                    return 0;
                }
            }
        }
    }
    for (u32 i = 0; i < pl; i++) {
        if ((u8)path[i] < 0x20 || (u8)path[i] == 0x7f) {
            op_err(e, -32602, "InvalidParams", "path: no control characters");
            return 0;
        }
    }
    if (path[0] == '/' || path[0] == '\\' || (pl > 1 && path[1] == ':')) {
        op_err(e, -32602, "InvalidParams",
               "path: relative to the addon content root (particles/...), not absolute");
        return 0;
    }
    if (pl < 6 || path[pl - 5] != '.' || (path[pl - 4] | 0x20) != 'v' || (path[pl - 3] | 0x20) != 'p'
        || (path[pl - 2] | 0x20) != 'c' || (path[pl - 1] | 0x20) != 'f') {
        op_err(e, -32602, "InvalidParams", "path: a .vpcf file");
        return 0;
    }
    for (u32 i = 0; i + 1 < pl; i++) {      /* only '..' needs a refusal: OpenFile resolves the
                                               rest, and a path outside the content dir is modal */
        if (path[i] == '.' && path[i + 1] == '.') {
            op_err(e, -32602, "InvalidParams", "path: no '..'");
            return 0;
        }
    }
    /* G2: the file the addon-relative path names must be loadable, on the host, before OpenFile */
    if (!pet_open_gate(path, e)) return 0;
    CPetApp *app = resolve_pet();
    if (!app) { op_err(e, -32003, "NoActiveDocument", "pet_unavailable"); return 0; }
    void *opened = app->vt->OpenFile(app, path, 1);     /* flags 1 = what the UI passes */
    logline("pet: open ");
    logline(path);
    lognum("pet: open rc ", (u64)(long long)(size_t)opened);
    const char *areason = "?";
    CPetSession *as = active_pet(&areason);             /* the opened session when it succeeded,
                                                           else the prior one */
    sb_add(out, "{\"opened\":"); sb_add(out, opened ? "true" : "false");
    sb_add(out, ",\"path\":"); sb_addjson(out, path);
    sb_add(out, ",\"active\":");
    u32 mark = out->len;
    if (as) {
        const char *dreason = "?";
        int r = pet_doc_json(out, as, &dreason, e);
        if (r < 0) { out->len = mark; out->buf[mark] = 0; return 0; }
        if (r == 0) { out->len = mark; out->buf[mark] = 0; sb_add(out, "null"); }
    } else {
        sb_add(out, "null");
    }
    sb_add(out, "}");
    return 1;
}

/* one row of pet.functionTypes. 1 = written, 0 = `e` set and the partial row rolled back (the
   caller rolls the whole reply back too: a partial registry list is worse than no list). */
static int pet_type_row(SB *out, void *def, int t, int i, int first, OpErr *e) {
    u32 mark = out->len;
    char name[128], clsname[128];
    if (!def || !mem_ok(def) || !mem_ok(*(void **)def)) goto bad;
    if (md_str(((PetFactoryNameFn)pet_vfn(def, S2_SLOT_IParticleFunctionDefinition_GetName))(def), name, sizeof name) != 1)
        goto bad;
    /* GetSchemaBinding(def, out8): the definition writes the SchemaClassInfoData_t* into the
       8-byte local and returns the local's address (IDB: `*out = *staticBinding; return out`) -
       read the returned pointer, then dereference it. */
    void *out8 = 0;
    void **retp = ((PetFactoryClassInfoFn)pet_vfn(def, S2_SLOT_IParticleFunctionDefinition_GetSchemaBinding))(def, &out8);
    if (!retp || !mem_ok(retp) || !*retp || !mem_ok(*retp)) goto bad;
    S2_SchemaClassInfoData_t *cls = (S2_SchemaClassInfoData_t *)*retp;
    if (md_str(cls->m_pszName, clsname, sizeof clsname) != 1) goto bad;
    u32 nmd = cls->m_nStaticMetadataCount;
    if (nmd > 4096) goto bad;
    u8 *md = (u8 *)cls->m_pStaticMetadata;
    if (nmd && (!md || !mem_ok(md) || !mem_ok(md + S2_SIZEOF_SchemaMetadataEntryData_t * nmd - 1)))
        goto bad;
    if (!first) sb_add(out, ",");
    sb_add(out, "{\"type\":"); sb_addi(out, t);
    sb_add(out, ",\"index\":"); sb_addi(out, i);
    sb_add(out, ",\"name\":"); sb_addjson(out, name);
    sb_add(out, ",\"className\":"); sb_addjson(out, clsname);
    sb_add(out, ",\"metadata\":[");
    int obsolete = 0;
    for (u32 k = 0; k < nmd; k++) {
        char mname[128];
        if (md_str(*(const char **)(md + S2_SIZEOF_SchemaMetadataEntryData_t * k +
                                    S2_OFF_SchemaMetadataEntryData_t_m_pszName), mname, sizeof mname) != 1)
            goto bad;
        if (k) sb_add(out, ",");
        sb_addjson(out, mname);
        if (seq(mname, "MObsoleteParticleFunction")) obsolete = 1;
    }
    sb_add(out, "],\"obsolete\":"); sb_add(out, obsolete ? "true" : "false");
    sb_add(out, ",\"singleton\":null}");
    return 1;
bad:
    out->len = mark; out->buf[mark] = 0;
    op_err(e, -32017, "PrerequisiteFailed", "function_definition_unreadable");
    return 0;
}

/* pet.functionTypes: the registry (particles.dll IParticleSystemMgr) the operator picker is built
   from. Per type 0..6: count, then every definition's friendly name (vtable slot
   S2_SLOT_IParticleFunctionDefinition_GetName), schema class name + static metadata (S2_SLOT_IParticleFunctionDefinition_GetSchemaBinding ->
   SchemaClassInfoData_t). `obsolete` is the picker's hide rule; `singleton` is always null this
   pass (definition slot 1 would tell, but nothing consumes it yet). Fails closed: any unusable
   count, unreadable definition/pointer or cut string aborts the reply - never a partial list. */
static int op_pet_function_types(const char *params, SB *out, OpErr *e) {
    (void)params;
    u8 *pb = (u8 *)g_modBase[PIN_PET];
    void *mgr = *(void **)(pb + PETG_PARTICLE_MGR);
    if (!mgr || !mem_ok(mgr)) {                 /* 0 before pet's Connect [I: unchecked live] */
        op_err(e, -32017, "PrerequisiteFailed", "particle_mgr_null");
        return 0;
    }
    {   /* the manager's vtable, which pet_vfn dereferences at both slots below (and every
           definition's vtable, checked per row): probe the slots before the first call */
        void *vt = *(void **)mgr;
        if (!vt || !mem_ok(vt) || !mem_ok(&((void **)vt)[S2_SLOT_IParticleSystemMgr_GetFunctionDefinitionCount])
            || !mem_ok(&((void **)vt)[S2_SLOT_IParticleSystemMgr_GetFunctionDefinition])) {
            op_err(e, -32017, "PrerequisiteFailed", "particle_mgr_unreadable");
            return 0;
        }
    }
    int counts[7];
    long long total = 0;
    for (int t = 0; t <= 6; t++) {
        int c = ((PetMgrFuncCountFn)pet_vfn(mgr, S2_SLOT_IParticleSystemMgr_GetFunctionDefinitionCount))(mgr, t);
        if (c < 0 || c > 4096) {
            op_err(e, -32017, "PrerequisiteFailed", "function_count_unusable");
            return 0;
        }
        counts[t] = c;
        total += c;
    }
    u32 mark = out->len;
    sb_add(out, "{\"count\":"); sb_addi(out, total);
    sb_add(out, ",\"types\":[");
    int first = 1;
    for (int t = 0; t <= 6; t++) {
        for (int i = 0; i < counts[t]; i++) {
            void *def = ((PetMgrFuncGetFn)pet_vfn(mgr, S2_SLOT_IParticleSystemMgr_GetFunctionDefinition))(mgr, t, i);
            if (!pet_type_row(out, def, t, i, first, e)) {
                out->len = mark; out->buf[mark] = 0;
                return 0;
            }
            first = 0;
        }
    }
    sb_add(out, "]}");
    return 1;
}

/* ---- schema read (particles.dll's own type scope) ---- */

/* own fields of cls, then the bases' fields depth-first (pet_schema_bases walks the same order).
   names/types are bounded copies; an unreadable hop fails closed. */
static int pet_schema_fields(SB *out, S2_SchemaClassInfoData_t *cls, const char *declaredIn, int *n) {
    u32 nf = cls->m_nFieldCount;
    if (!nf) return 1;
    S2_SchemaClassFieldData_t *fields = cls->m_pFields;
    if (!fields || !mem_ok(fields) || !mem_ok(fields + nf - 1)) return 0;
    for (u32 i = 0; i < nf; i++) {
        S2_SchemaClassFieldData_t *f = &fields[i];
        char nm[128], tn[128];
        int haveType = 0;
        if (md_str(f->m_pszName, nm, sizeof nm) != 1) return 0;
        if (f->m_pType && mem_ok(f->m_pType)) {
            /* CSchemaType::m_sTypeName (a CUtlString): both hops are probed */
            const char **pp = (const char **)((u8 *)f->m_pType + S2_OFF_CSchemaType_m_sTypeName +
                                              S2_OFF_CUtlString_m_pString);
            if (mem_ok(pp) && *pp && md_str(*pp, tn, sizeof tn) == 1) haveType = 1;
        }
        if ((*n)++) sb_add(out, ",");
        sb_add(out, "{\"name\":"); sb_addjson(out, nm);
        sb_add(out, ",\"type\":"); if (haveType) sb_addjson(out, tn); else sb_add(out, "null");
        sb_add(out, ",\"declaredIn\":"); sb_addjson(out, declaredIn);
        sb_add(out, "}");
    }
    return 1;
}

static int pet_schema_walk(SB *out, S2_SchemaClassInfoData_t *cls, int depth, int *n) {
    if (!cls || !mem_ok(cls)) return 0;
    if (depth > 8) return 1;                    /* the chain here is 2-3 deep: stop, don't fail */
    char cname[128];
    if (md_str(cls->m_pszName, cname, sizeof cname) != 1) return 0;
    if (!pet_schema_fields(out, cls, cname, n)) return 0;
    u32 nb = cls->m_nBaseClassCount;
    if (!nb) return 1;
    S2_SchemaBaseClassInfoData_t *bases = cls->m_pBaseClasses;
    if (!bases || !mem_ok(bases) || !mem_ok(bases + nb - 1)) return 0;
    for (u32 j = 0; j < nb; j++)
        if (!pet_schema_walk(out, (S2_SchemaClassInfoData_t *)bases[j].m_pClass, depth + 1, n)) return 0;
    return 1;
}

/* the flattened base chain, in the same walk order as pet_schema_fields (base, base's bases, ...).
   Stops at the same depth the field walk stops at, so `bases` never names a class whose fields are
   absent (both passes cover depths 0..8). */
static int pet_schema_bases(SB *out, S2_SchemaClassInfoData_t *cls, int depth, int *n) {
    if (!cls || !mem_ok(cls) || depth >= 8) return 1;
    u32 nb = cls->m_nBaseClassCount;
    if (!nb) return 1;
    S2_SchemaBaseClassInfoData_t *bases = cls->m_pBaseClasses;
    if (!bases || !mem_ok(bases) || !mem_ok(bases + nb - 1)) return 0;
    for (u32 j = 0; j < nb; j++) {
        S2_SchemaClassInfoData_t *b = (S2_SchemaClassInfoData_t *)bases[j].m_pClass;
        char bn[128];
        if (!b || !mem_ok(b) || md_str(b->m_pszName, bn, sizeof bn) != 1) return 0;
        if ((*n)++) sb_add(out, ",");
        sb_addjson(out, bn);
        if (!pet_schema_bases(out, b, depth + 1, n)) return 0;
    }
    return 1;
}

static int pet_schema_metadata(SB *out, S2_SchemaClassInfoData_t *cls) {
    u32 nm = cls->m_nStaticMetadataCount;
    if (nm > 4096) return 0;
    sb_add(out, "[");
    if (nm) {
        u8 *md = (u8 *)cls->m_pStaticMetadata;
        if (!md || !mem_ok(md) || !mem_ok(md + S2_SIZEOF_SchemaMetadataEntryData_t * nm - 1)) return 0;
        for (u32 i = 0; i < nm; i++) {
            char s[128];
            if (md_str(*(const char **)(md + S2_SIZEOF_SchemaMetadataEntryData_t * i +
                                        S2_OFF_SchemaMetadataEntryData_t_m_pszName), s, sizeof s) != 1)
                return 0;
            if (i) sb_add(out, ",");
            sb_addjson(out, s);
        }
    }
    sb_add(out, "]");
    return 1;
}

/* pet.schema {class}: particles.dll's OWN type scope (never the cached client.dll one - a class
   name is resolved in the module that registered it), class lookup by name with an m_pszName
   equality check like gs_class, then own fields + the flattened base chain (declaredIn) and the
   class's static metadata. found:false is a result, not an error. */
static int op_pet_schema(const char *params, SB *out, OpErr *e) {
    char clsname[512];              /* long enough that a >127-char name is refused, not cut */
    clsname[0] = 0;
    if (!params || !json_string(params, "class", clsname, sizeof clsname) || !clsname[0]) {
        op_err(e, -32602, "InvalidParams", "class (string) required, 1..127 chars, [A-Za-z0-9_]");
        return 0;
    }
    if (slen(clsname) > 127) { op_err(e, -32602, "InvalidParams", "class: 1..127 chars"); return 0; }
    for (u32 i = 0; clsname[i]; i++) {
        char c = clsname[i];
        if (!((c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '_')) {
            op_err(e, -32602, "InvalidParams", "class: [A-Za-z0-9_] only");
            return 0;
        }
    }
    void **ss = (void **)mod_iface(PIN_SCHEMA, S2_IFACE_SCHEMASYSTEM);
    if (!obj_is(ss, PIN_SCHEMA, SSV_SCHEMASYSTEM_VT)) {
        op_err(e, -32017, "PrerequisiteFailed", "SchemaSystem_001 is not the pinned CSchemaSystem");
        return 0;
    }
    void **scope = (void **)((SsFindScopeFn)(*(void ***)ss)[S2_SLOT_ISchemaSystem_FindTypeScopeForModule])(ss, "particles.dll", 0);
    if (!obj_is(scope, PIN_SCHEMA, SSV_TYPESCOPE_VT)) {
        op_err(e, -32023, "SchemaUnavailable", "particles.dll has not registered its schema scope yet");
        return 0;
    }
    if (((TsIsGlobalFn)(*(void ***)scope)[S2_SLOT_ISchemaSystemTypeScope_IsGlobalScope])(scope)) {
        op_err(e, -32023, "SchemaUnavailable", "particles.dll resolved to the global type scope");
        return 0;
    }
    S2_SchemaClassInfoData_t *cls =
        (S2_SchemaClassInfoData_t *)((TsFindRawClassFn)(*(void ***)scope)[S2_SLOT_ISchemaSystemTypeScope_FindRawClassBinding])(scope, clsname);
    int found = 0;
    if (cls && mem_ok(cls)) {
        const char *n = cls->m_pszName;
        if (n && mem_ok(n) && seq(n, clsname)) found = 1;
    }
    sb_add(out, "{\"class\":"); sb_addjson(out, clsname);
    sb_add(out, ",\"found\":"); sb_add(out, found ? "true" : "false");
    if (!found) {
        sb_add(out, ",\"size\":null,\"project\":null,\"bases\":[],\"fields\":[],\"metadata\":[]}");
        return 1;
    }
    char proj[128];
    int haveProj = md_str(cls->m_pszProjectName, proj, sizeof proj) == 1;
    u32 mark = out->len;
    int nb = 0, nf = 0;
    sb_add(out, ",\"size\":"); sb_addi(out, cls->m_nSize);
    sb_add(out, ",\"project\":"); if (haveProj) sb_addjson(out, proj); else sb_add(out, "null");
    sb_add(out, ",\"bases\":[");
    if (!pet_schema_bases(out, cls, 0, &nb)) goto bad_schema;
    sb_add(out, "],\"fields\":[");
    if (!pet_schema_walk(out, cls, 0, &nf)) goto bad_schema;
    sb_add(out, "],\"metadata\":");
    if (!pet_schema_metadata(out, cls)) goto bad_schema;
    sb_add(out, "}");
    return 1;
bad_schema:
    out->len = mark; out->buf[mark] = 0;
    op_err(e, -32023, "SchemaUnavailable", "particles.dll schema walk hit an unreadable hop");
    return 0;
}

/* ---- m2.13 WP-B1: live edit (pet.text / pet.apply / pet.undo / pet.redo) ----

   All four run on the main-thread drain. pet's undo is whole-document snapshots: BeginUndoStep
   opens a named step (the record is a full clone of the live root), Undo/Redo apply-and-pop through
   the history and its partner (`hist->m_pPartner`), and both restore with
   PETFN_ROOT_LOADFROMKV3 into the LIVE root (the pointer is kept, the children are rebuilt). Undo
   and Redo do NOT emit DocumentModified, so the bridge emits it itself (NotifyFuncList + the
   session's OnDocumentModified) to make the preview recompile and the title say modified.
   pet.apply parses the new text into a KV3 context BEFORE it opens the undo step: a LoadKV3 failure
   must not leave an empty step (RE "Second pass" in hammer-mcp/research/pet-re.md).

   Shared contracts (plan section 0.1): text is capped at PET_TEXT_MAX both ways; the private text
   file lives in the discovery directory (outside every content search path, so the asset system
   never sees it); untitled documents are allowed (nothing modal is involved - pet.save still
   refuses them). */

#define PET_TEXT_MAX 393216u        /* 384 KB of raw .vpcf text, request and reply */
#define PET_KV3CTX_BYTES 16384u     /* static KV3 parse context: the RE says 4608 (0x12A0 frame), 16 KB over-allocates */
#define PET_LABEL_MAX 64u           /* pet.apply label cap */
#define PET_LOADKV3_FLAGS 0u        /* pet's own text-overload callers pass 0 [IDA-checked, wave 0] */
#define PET_LOADKV3_SYM "?LoadKV3@@YA_NPEAVKeyValues3@@PEAVCUtlString@@PEBDAEBUKV3ID_t@@2I@Z"

/* Static because the freestanding build has no __chkstk for a >3 KB stack local, 16-aligned because
   the KV3 context holds aligned members, and zeroed before every ctor (the ctor does not clear). */
static __attribute__((aligned(16))) u8 g_petKv3Ctx[PET_KV3CTX_BYTES];

/* the 64-byte zero callback block PETFN_DOC_SAVETOFILE reads at +0x38 and skips when 0 */
static u8 g_petSaveCb[64];

typedef struct {
    CPetSession *session;
    CPetDoc *doc;
    void *root;
    CUndoHistory *hist;
    CUndoHistory *partner;
} PetLiveDoc;

/* <LOCALAPPDATA>\cs2mcpbridge\pet_text.vpcf - the same base the discovery file uses (the bridge
   creates the directory at listener start), outside every content search path so the asset system
   never sees it. 1 = written, 0 = the path does not fit `cap`. */
static int pet_text_path(char *out, u32 cap) {
    char base[260];
    u32 n = GetEnvironmentVariableA("LOCALAPPDATA", base, sizeof base);
    if (n == 0 || n >= sizeof base) cpy(base, "C:\\users\\steamuser\\AppData\\Local", sizeof base);
    SB p; sb_init(&p, out, cap);
    sb_add(&p, base); sb_add(&p, "\\cs2mcpbridge\\pet_text.vpcf");
    return !p.ovf;
}

/* After the live tree is rebuilt the UI's selection ({type, index} pairs) may point past a shrunken
   array (GetChild has no bounds check): set every entry to pet's own "none" {9, -1}, as its undo
   apply and RemoveNode do. Only existing entries are written; nothing is allocated. */
static void pet_clear_selection(CPetDoc *doc) {
    int n = doc->m_Selection.m_Size;
    int *sel = (int *)doc->m_Selection.m_pMemory;
    if (n <= 0 || n > 4096 || !sel || !mem_ok(sel) || !mem_ok(sel + 2 * n - 1)) return;
    for (int i = 0; i < n; i++) { sel[2 * i] = 9; sel[2 * i + 1] = -1; }
}

/* the common prologue (plan section 0.1): active_pet -> root -> undo history -> its partner, each
   hop mem_ok + obj_is. `-32003 {reason}` for a missing/unusable document (no_root, bad_root),
   `-32017 {reason}` for an unusable history. 1 = `c` filled; 0 = `e` set. */
static int pet_live_doc(PetLiveDoc *c, OpErr *e) {
    c->session = 0; c->doc = 0; c->root = 0; c->hist = 0; c->partner = 0;
    const char *reason = "?";
    CPetSession *session = active_pet(&reason);
    if (!session) { op_err(e, -32003, "NoActiveDocument", reason); return 0; }
    CPetDoc *doc = session->m_pDoc;
    void *root = doc->m_pRoot;
    if (!root) { op_err(e, -32003, "NoActiveDocument", "no_root"); return 0; }
    if (!obj_is(root, PIN_PET, PETV_ROOTNODE_VT)) {
        op_err(e, -32003, "NoActiveDocument", "bad_root");
        return 0;
    }
    CUndoHistory *hist = doc->m_pUndoHistory;
    if (!hist || !obj_is(hist, PIN_PET, PETV_UNDOHIST_VT)) {
        op_err(e, -32017, "PrerequisiteFailed", "bad_undo_history");
        return 0;
    }
    CUndoHistory *partner = hist->m_pPartner;
    if (!partner || !obj_is(partner, PIN_PET, PETV_UNDOHIST_VT)) {
        op_err(e, -32017, "PrerequisiteFailed", "bad_redo_history");
        return 0;
    }
    c->session = session; c->doc = doc; c->root = root; c->hist = hist; c->partner = partner;
    return 1;
}

/* canUndo/canRedo re-read through the two histories' own HasEntries slot, then the nine arrays.
   Runs AFTER the tree changed, so it never fails the call (a retry would add a second undo step):
   an unreadable child rolls only the array list back and answers `arrays:null` plus
   `arraysError {reason}`. `mark` is unused (kept for the callers' shape). */
static int pet_live_tail(SB *out, PetLiveDoc *c, u32 mark, OpErr *e) {
    (void)mark;
    int dirty = c->session->vt->IsModified(c->session) ? 1 : 0;
    int canUndo = c->hist->vt->HasEntries(c->hist) ? 1 : 0;
    int canRedo = c->partner->vt->HasEntries(c->partner) ? 1 : 0;
    sb_add(out, ",\"dirty\":"); sb_add(out, dirty ? "true" : "false");
    sb_add(out, ",\"canUndo\":"); sb_add(out, canUndo ? "true" : "false");
    sb_add(out, ",\"canRedo\":"); sb_add(out, canRedo ? "true" : "false");
    sb_add(out, ",\"arrays\":");
    u32 amark = out->len;
    if (!pet_arrays_json(out, c->root, e)) {
        out->len = amark; out->buf[amark] = 0;
        sb_add(out, "null,\"arraysError\":"); sb_add(out, e->data);
        e->code = 0;
    }
    return 1;
}

/* pet.text {}: serialize the live tree to the private path (SaveToFile does not touch the session's
   path or dirty flag), read it back with the same helper pet.reload uses, and report it. The
   reported `path` is the document's own GetPath, not the private file. */
static int op_pet_text(const char *params, SB *out, OpErr *e) {
    (void)params;
    PetLiveDoc c;
    if (!pet_live_doc(&c, e)) return 0;
    char tpath[600], docpath[1024];
    if (!pet_text_path(tpath, sizeof tpath)) {
        op_err(e, -32017, "PrerequisiteFailed", "text_path_too_long");
        return 0;
    }
    const char *errp = 0;                       /* CUtlString {char *} = {0} */
    if (!g_petSaveToFile(c.doc, tpath, &errp, g_petSaveCb)) {
        char msg[192];
        cpy(msg, "save_to_file_failed", sizeof msg);
        if (errp && mem_ok(errp)) {             /* fold the callee's error text in, bounded */
            u32 n = slen(msg);
            cpy(msg + n, ": ", sizeof msg - n);
            n = slen(msg);
            md_str(errp, msg + n, sizeof msg - n);
        }
        op_err(e, -32017, "PrerequisiteFailed", msg);
        return 0;
    }
    u8 *buf = 0;
    u32 len = 0;
    const char *why = "?";
    if (!pet_read_vpcf(tpath, &buf, &len, &why)) {
        op_err(e, -32017, "PrerequisiteFailed",
               seq(why, "too_large") ? "text_too_large" : "text_read_failed");
        return 0;
    }
    if (len > PET_TEXT_MAX) {
        HeapFree(GetProcessHeap(), 0, buf);
        op_err(e, -32017, "PrerequisiteFailed", "text_too_large");
        return 0;
    }
    buf[len] = 0;                               /* within pet_read_vpcf's size+1 allocation */
    if (slen((const char *)buf) != len) {       /* `text` would be cut at the NUL while `bytes` is not */
        HeapFree(GetProcessHeap(), 0, buf);
        op_err(e, -32017, "PrerequisiteFailed", "text_has_nul");
        return 0;
    }
    sb_add(out, "{\"text\":"); sb_addjson(out, (const char *)buf);
    HeapFree(GetProcessHeap(), 0, buf);
    if (out->ovf) {                             /* never answer with a cut string */
        op_err(e, -32017, "PrerequisiteFailed", "text_too_large");
        return 0;
    }
    if (md_str(c.session->vt->GetPath(c.session), docpath, sizeof docpath) == 0) docpath[0] = 0;
    sb_add(out, ",\"bytes\":"); sb_addi(out, (long long)len);
    sb_add(out, ",\"path\":"); sb_addjson(out, docpath);
    sb_add(out, ",\"dirty\":"); sb_add(out, c.session->vt->IsModified(c.session) ? "true" : "false");
    sb_add(out, ",\"isNewFile\":"); sb_add(out, c.session->m_bIsNew ? "true" : "false");
    sb_add(out, ",\"canUndo\":"); sb_add(out, c.hist->vt->HasEntries(c.hist) ? "true" : "false");
    sb_add(out, ",\"canRedo\":"); sb_add(out, c.partner->vt->HasEntries(c.partner) ? "true" : "false");
    sb_add(out, "}");
    return 1;
}

/* pet.apply {text, label?}: unescape -> vpcf_check -> prologue -> LoadKV3 -> ONE undo step that
   replaces the live tree. Failure never reaches BeginUndoStep. See the header comment. */
static int op_pet_apply(const char *params, SB *out, OpErr *e) {
    u64 plen = params ? (u64)slen(params) : 0;
    const char *tv = params ? json_find(params, "text") : 0;
    if (!tv || *tv != '"') {
        op_err(e, -32602, "InvalidParams", "text (string) required");
        return 0;
    }
    const char *src = tv + 1;
    if ((u64)(src - params) > plen) {           /* json_find cannot report past the NUL */
        op_err(e, -32602, "InvalidParams", "text (string) required");
        return 0;
    }
    u64 srcLen = plen - (u64)(src - params);
    u64 cap = srcLen + 1;
    char *text = (char *)HeapAlloc(GetProcessHeap(), 0, cap ? cap : 1);
    if (!text) { op_err(e, -32017, "PrerequisiteFailed", "text_alloc_failed"); return 0; }
    u64 tlen = 0;
    const char *why = "?";
    if (!json_text_unescape(src, srcLen, text, cap, &tlen, &why)) {
        HeapFree(GetProcessHeap(), 0, text);
        op_err(e, -32602, "InvalidParams", why);
        return 0;
    }
    if (tlen == 0 || tlen > PET_TEXT_MAX) {
        HeapFree(GetProcessHeap(), 0, text);
        op_err(e, -32602, "InvalidParams", "text: 1..393216 bytes");
        return 0;
    }
    char label[PET_LABEL_MAX + 1];
    const char *lraw = params ? json_find(params, "label") : 0;
    if (lraw) {
        char lb[512];
        u32 ll;
        if (*lraw != '"' || !json_string(params, "label", lb, sizeof lb)) {
            HeapFree(GetProcessHeap(), 0, text);
            op_err(e, -32602, "InvalidParams", "label: 1..64 printable ASCII (0x20..0x7E), no quote or backslash");
            return 0;
        }
        ll = slen(lb);
        if (ll < 1 || ll > PET_LABEL_MAX) {
            HeapFree(GetProcessHeap(), 0, text);
            op_err(e, -32602, "InvalidParams", "label: 1..64 printable ASCII (0x20..0x7E), no quote or backslash");
            return 0;
        }
        for (u32 i = 0; i < ll; i++) {
            u8 ch = (u8)lb[i];
            if (ch < 0x20 || ch > 0x7e || ch == '"' || ch == '\\') {   /* json_find is substring-based */
                HeapFree(GetProcessHeap(), 0, text);
                op_err(e, -32602, "InvalidParams", "label: 1..64 printable ASCII (0x20..0x7E), no quote or backslash");
                return 0;
            }
        }
        cpy(label, lb, sizeof label);
    } else {
        cpy(label, "MCP apply", sizeof label);
    }
    /* the same grammar + root-class gate pet.reload/pet.open run: both loader steps below wedge on
       malformed input */
    if (!vpcf_check(text, tlen, &why)) {
        pet_log_refusal("apply", why, "pet.apply:text");
        pet_unloadable(e, why, "pet.apply:text");
        HeapFree(GetProcessHeap(), 0, text);
        return 0;
    }
    PetLiveDoc c;
    if (!pet_live_doc(&c, e)) { HeapFree(GetProcessHeap(), 0, text); return 0; }
    /* parse BEFORE the undo step (a LoadKV3 failure must not leave an empty step) */
    memset(g_petKv3Ctx, 0, sizeof g_petKv3Ctx);
    g_petKv3CtxCtor(g_petKv3Ctx, 0);
    void *kv = g_petKv3CtxRoot(g_petKv3Ctx);
    if (!kv) {
        g_petKv3CtxDtor(g_petKv3Ctx);
        HeapFree(GetProcessHeap(), 0, text);
        op_err(e, -32017, "PrerequisiteFailed", "kv3_root_null");
        return 0;
    }
    void *tier0 = GetModuleHandleA("tier0.dll");    /* must be the verified PIN_TIER0 base */
    if (tier0 != (void *)g_modBase[PIN_TIER0]) {
        g_petKv3CtxDtor(g_petKv3Ctx);
        HeapFree(GetProcessHeap(), 0, text);
        op_err(e, -32017, "PrerequisiteFailed", "tier0_handle_mismatch");
        return 0;
    }
    PetLoadKV3Fn loadkv3 = (PetLoadKV3Fn)GetProcAddress(tier0, PET_LOADKV3_SYM);
    if (!loadkv3) {                             /* resolved per call; never cached across module state */
        g_petKv3CtxDtor(g_petKv3Ctx);
        HeapFree(GetProcessHeap(), 0, text);
        op_err(e, -32017, "PrerequisiteFailed", "loadkv3_export_missing");
        return 0;
    }
    const char *errp = 0;                       /* CUtlString err = {0} */
    u8 ok = loadkv3(kv, &errp, text, (const void *)((u8 *)g_modBase[PIN_PET] + PETG_KV3ID_VPCF66),
                    "", PET_LOADKV3_FLAGS);
    if (!ok) {
        char eb[100];
        g_petKv3CtxDtor(g_petKv3Ctx);
        HeapFree(GetProcessHeap(), 0, text);
        if (md_str(errp, eb, sizeof eb) == 0) cpy(eb, "?", sizeof eb);
        for (char *q = eb; *q; q++)             /* tier0's message quotes raw input bytes: keep the reply UTF-8 */
            if ((u8)*q >= 0x80 || (u8)*q < 0x20) *q = '?';
        pet_log_refusal("apply", "kv3_load_failed", eb);
        pet_unloadable(e, "kv3_load_failed", eb);
        /* errp is a tier0 heap string: leaked on purpose (the bridge has no tier0 free), rare path */
        return 0;
    }
    /* LoadFromFile's own filters, in its order: strip every _class object to its schema class
       (unknown keys out, defaults in) BEFORE the load, then drop unregistered/out-of-version
       function nodes AFTER it. pet's undo skips both because its snapshots already passed them;
       this text has not. */
    void *scope = g_petTypeScope();
    if (!scope) {
        g_petKv3CtxDtor(g_petKv3Ctx);
        HeapFree(GetProcessHeap(), 0, text);
        op_err(e, -32017, "PrerequisiteFailed", "no_type_scope");
        return 0;
    }
    g_petStripToSchema(kv, scope);
    g_petBeginUndoStep(c.doc, label, 1);
    g_petRootLoadFromKV3(c.root, kv);
    g_petFilterFunctions(c.root, g_petBehaviorVersion(c.root));
    pet_clear_selection(c.doc);
    g_petRefreshCaches(c.root, c.doc->m_unk102);
    {
        static u64 flist[4];                    /* the callee reads [3] as the session */
        flist[0] = 0; flist[1] = 0; flist[2] = 0; flist[3] = (u64)(size_t)c.session;
        g_petNotifyFuncList(flist);
    }
    g_petSessionOnDocModified(c.session);
    g_petKv3CtxDtor(g_petKv3Ctx);
    HeapFree(GetProcessHeap(), 0, text);
    u32 mark = out->len;
    sb_add(out, "{\"applied\":true,\"label\":"); sb_addjson(out, label);
    sb_add(out, ",\"bytes\":"); sb_addi(out, (long long)tlen);
    if (!pet_live_tail(out, &c, mark, e)) return 0;
    sb_add(out, "}");
    return 1;
}

/* pet.undo / pet.redo {}: apply-and-pop on the history / its partner, then the two notifications
   pet's own Undo omits (the preview must recompile, the title must say modified). An empty history
   is a RESULT (done:false + reason), never an error. */
static int op_pet_undo_redo(const char *params, SB *out, OpErr *e, int redo) {
    (void)params;
    PetLiveDoc c;
    if (!pet_live_doc(&c, e)) return 0;
    CUndoHistory *target = redo ? c.partner : c.hist;
    int has = target->vt->HasEntries(target) ? 1 : 0;
    u32 mark = out->len;
    sb_add(out, "{\"done\":"); sb_add(out, has ? "true" : "false");
    if (!has) {
        sb_add(out, ",\"reason\":"); sb_addjson(out, redo ? "nothing_to_redo" : "nothing_to_undo");
    } else {
        if (redo) g_petRedo(c.doc); else g_petUndo(c.doc);
        static u64 flist[4];                    /* the callee reads [3] as the session */
        flist[0] = 0; flist[1] = 0; flist[2] = 0; flist[3] = (u64)(size_t)c.session;
        g_petNotifyFuncList(flist);
        g_petSessionOnDocModified(c.session);
    }
    if (!pet_live_tail(out, &c, mark, e)) return 0;
    sb_add(out, "}");
    return 1;
}

/* the seven pet.* methods (k_methods rows: pet.load needs toolframework2 only, the rest pin
   pet.dll; pet.schema additionally needs the schema system). */
static int op_pet_render(const char *params, SB *out, OpErr *e);   /* mcp_bridge_pet_render.inc */

static int op_pet(const char *method, const char *params, SB *out, OpErr *e) {
    if (seq(method, "pet.render")) return op_pet_render(params, out, e);
    if (seq(method, "pet.load")) return op_pet_load(out, e);
    /* m2.13 live edit (WP-B1) */
    if (seq(method, "pet.text")) return op_pet_text(params, out, e);
    if (seq(method, "pet.apply")) return op_pet_apply(params, out, e);
    if (seq(method, "pet.undo")) return op_pet_undo_redo(params, out, e, 0);
    if (seq(method, "pet.redo")) return op_pet_undo_redo(params, out, e, 1);
    if (seq(method, "pet.active")) {
        /* never errors on no doc: {doc:null, reason} */
        const char *reason = "?";
        CPetSession *session = active_pet(&reason);
        if (!session) {
            sb_add(out, "{\"doc\":null,\"reason\":"); sb_addjson(out, reason); sb_add(out, "}");
            return 1;
        }
        u32 mark = out->len;
        sb_add(out, "{\"doc\":");
        const char *dreason = "?";
        int r = pet_doc_json(out, session, &dreason, e);
        if (r < 0) { out->len = mark; out->buf[mark] = 0; return 0; }
        if (r == 0) {                       /* root unusable: the same shape as no session */
            out->len = mark; out->buf[mark] = 0;
            sb_add(out, "{\"doc\":null,\"reason\":"); sb_addjson(out, dreason); sb_add(out, "}");
            return 1;
        }
        sb_add(out, "}");
        return 1;
    }
    if (seq(method, "pet.open")) return op_pet_open(params, out, e);
    if (seq(method, "pet.reload")) {
        const char *reason = "?";
        CPetSession *session = active_pet(&reason);
        if (!session) { op_err(e, -32003, "NoActiveDocument", reason); return 0; }
        int dirty = session->vt->IsModified(session) ? 1 : 0;
        int force = 0;
        if (params) {
            const char *f = json_find(params, "force");
            if (f && f[0] == 't') force = 1;
        }
        if (dirty && !force) {
            op_err(e, -32016, "UnsavedChanges", "document is dirty; pass force:true to discard");
            return 0;
        }
        char path[1024];
        int havePath = md_str(session->vt->GetPath(session), path, sizeof path) == 1;
                                                        /* capture before the reload */
        if (session->m_bIsNew || !path[0]) {       /* ReloadDocument would load "" into an empty
                                                       document: useless and destructive */
            op_err(e, -32015, "UntitledDocument", "no path to reload from");
            return 0;
        }
        /* G2: a file pet cannot parse must never reach ReloadDocument (see the header comment: it
           recurses until the Wine main thread's stack overflows and the process wedges). Read the
           captured path and check it with vpcf_check (kv3 header + grammar + root `_class`) first;
           refuse as -32602 UnloadableFile otherwise. */
        {
            u8 *fbuf = 0;
            u32 flen = 0;
            int lexOk;
            const char *why = havePath ? "?" : "read_failed";
            if (!havePath || !pet_read_vpcf(path, &fbuf, &flen, &why)) {
                /* a cut GetPath (md_str != 1) cannot identify the file the session will load */
                pet_log_refusal("reload", why, path);
                pet_unloadable(e, why, path);
                return 0;
            }
            lexOk = vpcf_check((const char *)fbuf, (u64)flen, &why);
            HeapFree(GetProcessHeap(), 0, fbuf);
            if (!lexOk) {
                pet_log_refusal("reload", why, path);
                pet_unloadable(e, why, path);
                return 0;
            }
        }
        session->vt->ReloadDocument(session);       /* replaces the CPetDoc: re-resolve below */
        const char *areason = "?";
        CPetSession *as = active_pet(&areason);
        int hasRoot = (as && as->m_pDoc && as->m_pDoc->m_pRoot) ? 1 : 0;
        if (!as) logline("pet: reload left no valid document");
        lognum("pet: reload, dirty was ", (u64)dirty);
        sb_add(out, "{\"reloaded\":"); sb_add(out, as ? "true" : "false");
        sb_add(out, ",\"hasRoot\":"); sb_add(out, hasRoot ? "true" : "false");
        sb_add(out, ",\"path\":"); sb_addjson(out, path);
        sb_add(out, ",\"discardedChanges\":"); sb_add(out, dirty ? "true" : "false");
        sb_add(out, "}");
        return 1;
    }
    if (seq(method, "pet.save")) {
        const char *reason = "?";
        CPetSession *session = active_pet(&reason);
        if (!session) { op_err(e, -32003, "NoActiveDocument", reason); return 0; }
        if (session->m_bIsNew) {
            op_err(e, -32015, "UntitledDocument", "no path (Save As would open a modal)");
            return 0;
        }
        char path[1024];                            /* copied BEFORE the save: the path string may
                                                       be reallocated by Save */
        if (md_str(session->vt->GetPath(session), path, sizeof path) != 1 || !path[0]) {
            op_err(e, -32015, "UntitledDocument", "no usable path (Save As would open a modal)");
            return 0;
        }
        int dirty = session->vt->IsModified(session) ? 1 : 0;
        int force = 0;                              /* force:true saves a clean document too: pet
                                                       re-serializes it (its own file form) */
        if (params) {
            const char *f = json_find(params, "force");
            if (f && f[0] == 't') force = 1;
        }
        int saved = 1, skipped = 1;
        if (dirty || force) {
            skipped = 0;
            saved = session->vt->Save(session, path, path, 0, 0) ? 1 : 0;   /* the FileSave call */
            if (saved) session->vt->SetModified(session, 0);                /* mirrors FileSave */
        }
        sb_add(out, "{\"saved\":"); sb_add(out, saved ? "true" : "false");
        sb_add(out, ",\"skippedClean\":"); sb_add(out, skipped ? "true" : "false");
        sb_add(out, ",\"path\":"); sb_addjson(out, path);
        sb_add(out, "}");
        return 1;
    }
    if (seq(method, "pet.functionTypes")) return op_pet_function_types(params, out, e);
    if (seq(method, "pet.schema")) return op_pet_schema(params, out, e);
    op_err(e, -32601, "MethodNotFound", method);    /* only the twelve arrive here */
    return 0;
}
