/* ---------------- game state: schema-resolved entity reads, weapon select, sound events ----------------
   (client-mcp workflows/11-game-state.md, research/game-state.md). Included by mcp_bridge.c before
   exec_request(); everything here runs on the tools main thread (drain) except the sound wrappers,
   which record into a spin-locked ring from whichever thread the game starts sounds on.

   No netvar offsets are compiled in: every entity field is resolved at runtime through the game's own
   SchemaSystem (class + field name -> offset, docs/interfaces/ISchemaSystem.md "How to resolve a field
   offset at runtime"), so a game update that moves fields needs no bridge change. What IS pinned (sig DB,
   fail closed): the entity identity chunk table, the local-controller table, the view-angle accumulator,
   the engine client / schema / sound-op vtables and the slots called on them. */

/* ---- schema resolver (SchemaSystem_001 -> client.dll scope -> class binding -> field offset) ---- */
typedef void *(*SsFindScopeFn)(void *self, const char *module, const char **binding);
typedef void *(*TsFindRawClassFn)(void *self, const char *name);
typedef unsigned char (*TsIsGlobalFn)(void *self);   /* bool in AL: never read it as int */

static void *g_gsScope;                        /* CSchemaSystemTypeScope of client.dll */
#define GS_CACHE 192
typedef struct { const char *cls; const char *field; int off; } GsCacheEnt;
static GsCacheEnt g_gsCache[GS_CACHE];
static int g_gsCacheN;

/* forward: the F2 layout cross-check runs right after the scope is first obtained (defined below) */
static int gs_layout_check(OpErr *e);

static int gs_scope(OpErr *e) {
    if (g_gsScope) return 1;
    void **ss = (void **)mod_iface(PIN_SCHEMA, S2_IFACE_SCHEMASYSTEM);
    if (!obj_is(ss, PIN_SCHEMA, SSV_SCHEMASYSTEM_VT)) {
        op_err(e, -32017, "PrerequisiteFailed", "SchemaSystem_001 is not the pinned CSchemaSystem");
        return 0;
    }
    void **scope = (void **)((SsFindScopeFn)(*(void ***)ss)[S2_SLOT_ISchemaSystem_FindTypeScopeForModule])(ss, "client.dll", 0);
    if (!obj_is(scope, PIN_SCHEMA, SSV_TYPESCOPE_VT)) {
        op_err(e, -32017, "PrerequisiteFailed", "client.dll type scope is not the pinned CSchemaSystemTypeScope");
        return 0;
    }
    if (((TsIsGlobalFn)(*(void ***)scope)[S2_SLOT_ISchemaSystemTypeScope_IsGlobalScope])(scope)) {
        op_err(e, -32023, "SchemaUnavailable", "client.dll has not registered its schema scope yet");
        return 0;
    }
    g_gsScope = scope;
    /* F2: first time the scope is up, prove the compiled CEntityIdentity layout against the
       schema's own size/field offsets before anything dereferences the generated offsets. */
    if (!gs_layout_check(e)) { g_gsScope = 0; return 0; }
    return 1;
}

/* CSchemaClassInfo* by name (hash-only lookup: cls->m_pszName is verified);
   the layout is the generated S2_SchemaClassInfoData_t (bridge/s2sdk_gen.h) */
static S2_SchemaClassInfoData_t *gs_class(const char *name) {
    if (!g_gsScope) return 0;
    S2_SchemaClassInfoData_t *cls =
        (S2_SchemaClassInfoData_t *)((TsFindRawClassFn)(*(void ***)g_gsScope)[S2_SLOT_ISchemaSystemTypeScope_FindRawClassBinding])(g_gsScope, name);
    if (!cls || !mem_ok(cls)) return 0;
    const char *n = cls->m_pszName;
    if (!n || !mem_ok(n) || !seq(n, name)) return 0;
    return cls;
}

/* field offset inside cls, following single inheritance (m_pBaseClasses[0..]) with base offsets added */
static int gs_field_in(S2_SchemaClassInfoData_t *cls, const char *field, int depth, int *out) {
    if (!cls || depth > 24 || !mem_ok(cls)) return 0;
    u32 nf = cls->m_nFieldCount;
    u8 *fields = (u8 *)cls->m_pFields;
    if (nf && fields && mem_ok(fields)) {
        for (u32 i = 0; i < nf; i++) {
            u8 *f = fields + S2_SIZEOF_SchemaClassFieldData_t * i;
            const char *fn = *(const char **)f;
            if (fn && mem_ok(fn) && seq(fn, field)) { *out = *(int *)(f + S2_OFF_SchemaClassFieldData_t_m_nSingleInheritanceOffset); return 1; }
        }
    }
    u32 nb = cls->m_nBaseClassCount;
    u8 *bases = (u8 *)cls->m_pBaseClasses;
    if (nb && bases && mem_ok(bases)) {
        for (u32 j = 0; j < nb; j++) {
            u8 *b = bases + S2_SIZEOF_SchemaBaseClassInfoData_t * j;
            int sub = 0;
            if (gs_field_in(*(void **)(b + S2_OFF_SchemaBaseClassInfoData_t_m_pClass), field, depth + 1, &sub)) {
                *out = (int)*(u32 *)(b + S2_OFF_SchemaBaseClassInfoData_t_m_nOffset) + sub; return 1;
            }
        }
    }
    return 0;
}

/* offset of `cls::field` (cached per process; the schema data is static for the module's lifetime) */
/* a miss is cached too (off = -1) once the client scope is up: the schema is static, so a field that does not
   resolve is not walked again on every entity of every call */
static int gs_off(const char *cls, const char *field, int *out) {
    for (int i = 0; i < g_gsCacheN; i++)
        if (seq(g_gsCache[i].cls, cls) && seq(g_gsCache[i].field, field)) {
            if (g_gsCache[i].off < 0) return 0;
            *out = g_gsCache[i].off;
            return 1;
        }
    int off = 0;
    int found = gs_field_in(gs_class(cls), field, 0, &off);
    if (!found && !g_gsScope) return 0;    /* scope not up yet: nothing is known, so nothing is cached */
    if (g_gsCacheN < GS_CACHE) { g_gsCache[g_gsCacheN].cls = cls; g_gsCache[g_gsCacheN].field = field; g_gsCache[g_gsCacheN].off = found ? off : -1; g_gsCacheN++; }
    if (!found) return 0;
    *out = off;
    return 1;
}
static int gs_class_size(const char *cls, int *out) {
    S2_SchemaClassInfoData_t *c = gs_class(cls);
    if (!c) return 0;
    *out = c->m_nSize;
    return *out > 0;
}

/* ---- F2 runtime layout cross-check: compiled CEntityIdentity vs the live schema ----
   The bridge walks identities through the hand-pinned chunk table (CG_ENTITY_CHUNKS) and the
   GENERATED leading-block offsets (S2_OFF_CEntityIdentity_m_pInstance / _m_pClass / _m_EHandle,
   derived from the pinned upstream); field reads (m_name, m_designerName, ...) resolve through the
   schema at runtime. A game update that shifts the class between an upstream bump and this DLL
   would corrupt the generated-offset reads silently. After the client scope first comes up, compare
   what the schema reports with the header:
     - schema class size (m_nSize) vs S2_SIZEOF_CEntityIdentity;
     - every dereferenced field the schema registers vs its generated offset. In this game's schema
       CEntityIdentity does NOT register the leading trio: client.dll (whose scope gs_scope queries)
       has no field-name string for m_pInstance, m_pClass or m_EHandle, while it has them for
       m_nameStringTableIndex, m_name, m_designerName and m_flags. Those are the anchors (gen 20,
       24, 32, 48): a change in the leading block's total size moves them and is caught. A
       same-size reordering inside the trio (e.g. m_pClass and m_EHandle swapping) is invisible to
       any schema-based check; only an upstream header update catches that. Anchors the schema does not register are skipped
       (not failures); the check needs at least one anchor or the size to have a say.
   Computed once (unknown / ok / mismatch). A mismatch makes every game.* op that resolves entities
   fail closed with op_err(e, -32023, "SchemaUnavailable", <this message>); no crash, no wire change
   (m2.10 protocol frozen), and the intent is in the message: update s2sdk upstream. On the current
   game this is a no-op: schema size 112 == S2_SIZEOF_CEntityIdentity. */
static int g_gsLayout;                 /* 0 unchecked, 1 ok, 2 mismatch, 3 checked but nothing comparable */
static char g_gsLayoutMsg[160];

/* once the check has run: 2 is a sticky mismatch (fail closed with its message, forever);
   1/3 are as today. Called after the scope first came up. */
static int gs_layout_check(OpErr *e) {
    if (g_gsLayout == 2) { op_err(e, -32023, "SchemaUnavailable", g_gsLayoutMsg); return 0; }
    if (g_gsLayout) return 1;

    int szs = 0;
    int haveSize = gs_class_size("CEntityIdentity", &szs);
    struct { const char *field; int gen; int off; } anchors[] = {
        {"m_nameStringTableIndex", S2_OFF_CEntityIdentity_m_nameStringTableIndex, 0},
        {"m_name",                 S2_OFF_CEntityIdentity_m_name,                 0},
        {"m_designerName",         S2_OFF_CEntityIdentity_m_designerName,         0},
        {"m_flags",                S2_OFF_CEntityIdentity_m_flags,                0},
    };
    int resolved = 0, badSize = 0, badAnchor = -1;
    for (u32 i = 0; i < sizeof anchors / sizeof anchors[0]; i++) {
        if (gs_off("CEntityIdentity", anchors[i].field, &anchors[i].off)) resolved++;
        else anchors[i].off = anchors[i].gen;
        /* the schema not registering a field is FINE (see the banner): skip, don't fail */
        if (anchors[i].off != anchors[i].gen) badAnchor = (int)i;
    }
    if (haveSize && szs != S2_SIZEOF_CEntityIdentity) badSize = 1;
    if (!badSize && badAnchor < 0 && (haveSize || resolved)) {
        g_gsLayout = 1;                  /* whichever the schema gave us, upstream matches it */
        return 1;
    }
    if (!badSize && badAnchor < 0) {
        g_gsLayout = 3;                  /* schema had neither this class nor the anchors: */
        return 1;                        /* no evidence either way - behave as before */
    }
    SB m;
    sb_init(&m, g_gsLayoutMsg, sizeof g_gsLayoutMsg);
    sb_add(&m, "CEntityIdentity layout mismatch: schema ");
    if (badSize) {
        sb_addi(&m, (long long)szs);
        sb_add(&m, " vs header ");
        sb_addi(&m, (long long)S2_SIZEOF_CEntityIdentity);
        sb_add(&m, " size");
    } else {
        sb_add(&m, anchors[badAnchor].field);
        sb_add(&m, " ");
        sb_addi(&m, (long long)anchors[badAnchor].off);
        sb_add(&m, " vs header ");
        sb_addi(&m, (long long)anchors[badAnchor].gen);
        sb_add(&m, " offset");
    }
    sb_add(&m, " (update s2sdk upstream)");
    g_gsLayout = 2;
    op_err(e, -32023, "SchemaUnavailable", g_gsLayoutMsg);
    return 0;
}

/* typed reads; every failure (unknown field, unreadable memory) is reported, never guessed */
static int gs_rd(void *ent, const char *cls, const char *field, void *dst, u32 n) {
    int off;
    if (!ent || !gs_off(cls, field, &off)) return 0;
    u8 *p = (u8 *)ent + off;
    if (!mem_ok(p) || !mem_ok(p + n - 1)) return 0;
    for (u32 i = 0; i < n; i++) ((u8 *)dst)[i] = p[i];
    return 1;
}
static int gs_i32(void *ent, const char *c, const char *f, int *v) { return gs_rd(ent, c, f, v, 4); }
static int gs_u32(void *ent, const char *c, const char *f, u32 *v) { return gs_rd(ent, c, f, v, 4); }
static int gs_u8(void *ent, const char *c, const char *f, u8 *v) { return gs_rd(ent, c, f, v, 1); }
static int gs_f32(void *ent, const char *c, const char *f, float *v) { return gs_rd(ent, c, f, v, 4); }
static int gs_vec(void *ent, const char *c, const char *f, float *v) { return gs_rd(ent, c, f, v, 12); }
static int gs_ptr(void *ent, const char *c, const char *f, void **v) { return gs_rd(ent, c, f, v, 8); }
static const char *gs_cstr(void *ent, const char *c, const char *f) {   /* inline char[] field */
    int off;
    if (!ent || !gs_off(c, f, &off)) return 0;
    const char *s = (const char *)ent + off;
    return mem_ok(s) ? s : 0;
}

/* ---- entity identities (CG_ENTITY_CHUNKS; chunk geometry S2_CONST_MAX_ENTITIES_IN_LIST (512, upstream
   entityidentity.h); sizeof from the schema). sdkcheck.py holds the sigs measurement of the lookup's
   shift (client.json CCONST_IDENTITY_CHUNK_BITS, read out of the instruction) to `1 << bits == 512`. ---- */
_Static_assert((S2_CONST_MAX_ENTITIES_IN_LIST & (S2_CONST_MAX_ENTITIES_IN_LIST - 1)) == 0 && S2_CONST_MAX_ENTITIES_IN_LIST != 0,
               "the identity chunk size must be a power of two: gs_ident divides and masks by it");
#define GS_IDENT_CHUNK_MASK (S2_CONST_MAX_ENTITIES_IN_LIST - 1)
static int g_gsIdentSize;
static u8 *gs_ident(u32 index) {
    if (index >= (1u << S2_CONST_ENTITY_INDEX_BITS)) return 0;   /* identity index bound: 1 << ENTITY_INDEX_BITS (15) */
    if (!g_gsIdentSize && !gs_class_size("CEntityIdentity", &g_gsIdentSize)) return 0;
    u8 **chunks = *(u8 ***)((u8 *)g_modBase[PIN_CLIENT] + CG_ENTITY_CHUNKS);
    if (!chunks || !mem_ok(chunks)) return 0;
    u8 *chunk = chunks[index / S2_CONST_MAX_ENTITIES_IN_LIST];
    if (!chunk || !mem_ok(chunk)) return 0;
    return chunk + (u32)g_gsIdentSize * (index & GS_IDENT_CHUNK_MASK);
}
static void *gs_ent_by_index(u32 index) {
    u8 *id = gs_ident(index);
    if (!id || !mem_ok(id)) return 0;
    void *ent = *(void **)(id + S2_OFF_CEntityIdentity_m_pInstance);   /* CEntityIdentity::m_pInstance [S] */
    return (ent && mem_ok(ent)) ? ent : 0;
}
/* CHandle: index = h & ENTITY_INDEX_MASK, serial = h >> ENTITY_INDEX_BITS;
   CEntityIdentity::m_EHandle holds the full handle [S][V] */
static void *gs_ent_by_handle(u32 h) {
    if (h == S2_CONST_INVALID_EHANDLE_INDEX) return 0;
    u8 *id = gs_ident(h & S2_CONST_ENTITY_INDEX_MASK);
    if (!id || !mem_ok(id) || *(u32 *)(id + S2_OFF_CEntityIdentity_m_EHandle) != h) return 0;
    void *ent = *(void **)(id + S2_OFF_CEntityIdentity_m_pInstance);
    return (ent && mem_ok(ent)) ? ent : 0;
}
/* Game-owned name strings are never handed out in place: each read is copied into a bounded slot
   (GS_NAME_CAP bytes incl. NUL). The copy checks readability once per 4 KiB page (mem_ok) and stops at the
   NUL or the cap. The returned slot stays valid for the next GS_NAME_SLOTS name reads, which covers the
   callers that hold two names at once (class and targetname). Main thread only. */
#define GS_NAME_CAP 256
#define GS_NAME_SLOTS 4
#define GS_PAGE_MASK 0xFFFu             /* 4 KiB page granularity (mem_ok probes once per page) */
static char g_gsNames[GS_NAME_SLOTS][GS_NAME_CAP];
static u32 g_gsNameNext;
static int gs_copy_cstr(const char *src, char *dst) {
    if (!src) return 0;
    for (u32 i = 0; i < GS_NAME_CAP - 1; i++) {
        if (i == 0 || (((u64)(const void *)(src + i)) & GS_PAGE_MASK) == 0) {
            if (!mem_ok(src + i)) { dst[0] = 0; return 0; }
        }
        dst[i] = src[i];
        if (!src[i]) return 1;
    }
    dst[GS_NAME_CAP - 1] = 0;                 /* over-long: cut, still a readable name */
    return 1;
}
static char *gs_name_slot(void) {
    char *d = g_gsNames[g_gsNameNext];
    g_gsNameNext = (g_gsNameNext + 1) % GS_NAME_SLOTS;
    return d;
}
static const char *gs_designer_name(u32 index) {
    u8 *id = gs_ident(index);
    int off;
    if (!id || !mem_ok(id) || !gs_off("CEntityIdentity", "m_designerName", &off)) return 0;
    char *d = gs_name_slot();
    /* the field is a CUtlSymbolLarge: its char* sits at the symbol's own m_pString offset (0) */
    return gs_copy_cstr(*(const char **)(id + off + S2_OFF_CUtlSymbolLarge_m_pString), d) ? d : 0;
}
static int gs_ent_index(void *ent) {       /* CEntityInstance::m_pEntity (identity*) [S] */
    u8 *id = *(u8 **)((u8 *)ent + S2_OFF_CEntityInstance_m_pEntity);
    if (!id || !mem_ok(id)) return -1;
    return (int)(*(u32 *)(id + S2_OFF_CEntityIdentity_m_EHandle) & S2_CONST_ENTITY_INDEX_MASK);
}
static int gs_origin(void *ent, float *out) {
    void *node = 0;
    if (!gs_ptr(ent, "C_BaseEntity", "m_pGameSceneNode", &node) || !node || !mem_ok(node)) return 0;
    return gs_vec(node, "CGameSceneNode", "m_vecAbsOrigin", out);
}
static int gs_angles(void *ent, float *out) {
    void *node = 0;
    if (!gs_ptr(ent, "C_BaseEntity", "m_pGameSceneNode", &node) || !node || !mem_ok(node)) return 0;
    return gs_vec(node, "CGameSceneNode", "m_angAbsRotation", out);
}
static const char *gs_entity_name(u32 index) {   /* CEntityIdentity::m_name (CUtlSymbolLarge); empty = none */
    u8 *id = gs_ident(index);
    int off;
    if (!id || !mem_ok(id) || !gs_off("CEntityIdentity", "m_name", &off)) return 0;
    char *d = gs_name_slot();
    if (!gs_copy_cstr(*(const char **)(id + off + S2_OFF_CUtlSymbolLarge_m_pString), d) || !d[0]) return 0;
    return d;
}

/* ---- engine client (IVEngineClient2 slots 34/39/65) ---- */
/* Windows/MSVC: CPlayerSlot is a class, so the return goes through the hidden pointer `out` (this, out, slot). A Linux port must call the slot as int(this, slot). */
typedef int *(*EcGetLocalPlayerFn)(void *self, int *out, int splitSlot);
typedef unsigned char (*EcIsInGameFn)(void *self);   /* bool in AL */
typedef const char *(*EcGetLevelNameFn)(void *self);
static void **gs_engine_client(OpErr *e) {
    void **ec = (void **)mod_iface(PIN_ENGINE2, S2_IFACE_SOURCE2ENGINETOCLIENT);
    if (!obj_is(ec, PIN_ENGINE2, EV_ENGINECLIENT_VT)) {
        op_err(e, -32017, "PrerequisiteFailed", "Source2EngineToClient001 is not the pinned CEngineClient");
        return 0;
    }
    return ec;
}

/* local controller: CG_LOCAL_CONTROLLERS[0], cross-checked against GetLocalPlayer(0)+1 */
static void *gs_local_controller(void **ec, int *playerSlot, const char **why) {
    int ps = -1;
    ((EcGetLocalPlayerFn)(*(void ***)ec)[S2_SLOT_IVEngineClient2_GetLocalPlayer])(ec, &ps, 0);
    if (playerSlot) *playerSlot = ps;
    void *ctl = *(void **)((u8 *)g_modBase[PIN_CLIENT] + CG_LOCAL_CONTROLLERS);
    if (!ctl || !mem_ok(ctl)) { *why = "no local controller (not in a game?)"; return 0; }
    if (ps >= 0 && gs_ent_index(ctl) != ps + 1) { *why = "local controller table disagrees with GetLocalPlayer"; return 0; }
    return ctl;
}

/* ---- JSON writers ---- */
static void gs_add_kv_str(SB *out, const char *key, const char *v) {
    sb_add(out, "\""); sb_add(out, key); sb_add(out, "\":");
    if (v) sb_addjson(out, v); else sb_add(out, "null");
}
static void gs_add_kv_i(SB *out, const char *key, long long v) { sb_add(out, "\""); sb_add(out, key); sb_add(out, "\":"); sb_addi(out, v); }
static void gs_add_kv_f(SB *out, const char *key, float v) { sb_add(out, "\""); sb_add(out, key); sb_add(out, "\":"); sb_addf(out, v); }
static void gs_add_kv_b(SB *out, const char *key, int v) { sb_add(out, "\""); sb_add(out, key); sb_add(out, "\":"); sb_add(out, v ? "true" : "false"); }
static void gs_add_kv_v3(SB *out, const char *key, const float *v, int ok) {
    sb_add(out, "\""); sb_add(out, key); sb_add(out, "\":");
    if (ok) sb_addf3(out, v); else sb_add(out, "null");
}
/* schema-resolved typed fields: `"key":value`, or null when the field or its read fails (a null ent fails too).
   The unresolved field's name is appended to `miss` (may be 0) for the reply's schemaMissing list. */
static void gs_miss_add(SB *miss, const char *field) {
    if (!miss) return;
    if (miss->len) sb_add(miss, ",");
    sb_addjson(miss, field);
}
static void gs_fld_b(SB *out, SB *miss, const char *key, void *ent, const char *cls, const char *field) {
    u8 v = 0;
    sb_add(out, "\""); sb_add(out, key); sb_add(out, "\":");
    if (gs_u8(ent, cls, field, &v)) sb_add(out, v ? "true" : "false");
    else { sb_add(out, "null"); gs_miss_add(miss, field); }
}
static void gs_fld_u8(SB *out, SB *miss, const char *key, void *ent, const char *cls, const char *field) {
    u8 v = 0;
    sb_add(out, "\""); sb_add(out, key); sb_add(out, "\":");
    if (gs_u8(ent, cls, field, &v)) sb_addnum(out, v);
    else { sb_add(out, "null"); gs_miss_add(miss, field); }
}
static void gs_fld_i(SB *out, SB *miss, const char *key, void *ent, const char *cls, const char *field) {
    int v = 0;
    sb_add(out, "\""); sb_add(out, key); sb_add(out, "\":");
    if (gs_i32(ent, cls, field, &v)) sb_addi(out, v);
    else { sb_add(out, "null"); gs_miss_add(miss, field); }
}
static void gs_fld_f(SB *out, SB *miss, const char *key, void *ent, const char *cls, const char *field) {
    float v = 0;
    sb_add(out, "\""); sb_add(out, key); sb_add(out, "\":");
    if (gs_f32(ent, cls, field, &v)) sb_addf(out, v);
    else { sb_add(out, "null"); gs_miss_add(miss, field); }
}

/* weapon object -> JSON (C_BasePlayerWeapon / C_EconEntity) */
static void gs_weapon_json(void *w, SB *out) {
    int clip1 = -1, clip2 = -1, reserve = -1, def = -1;
    u32 h = S2_CONST_INVALID_EHANDLE_INDEX;
    gs_i32(w, "C_BasePlayerWeapon", "m_iClip1", &clip1);
    gs_i32(w, "C_BasePlayerWeapon", "m_iClip2", &clip2);
    { int rv[2] = { -1, -1 }; if (gs_rd(w, "C_BasePlayerWeapon", "m_pReserveAmmo", rv, 8)) reserve = rv[0]; }
    { int a = 0, b = 0; u16 d16 = 0;   /* C_EconEntity::m_AttributeManager -> C_AttributeContainer::m_Item -> C_EconItemView::m_iItemDefinitionIndex */
      if (gs_off("C_EconEntity", "m_AttributeManager", &a) && gs_off("C_AttributeContainer", "m_Item", &b)) {
          void *item = (u8 *)w + a + b;
          if (gs_rd(item, "C_EconItemView", "m_iItemDefinitionIndex", &d16, 2)) def = d16;
      } }
    int idx = gs_ent_index(w);
    u8 *id = idx >= 0 ? gs_ident((u32)idx) : 0;
    if (id && mem_ok(id)) h = *(u32 *)(id + S2_OFF_CEntityIdentity_m_EHandle);
    u8 reloading = 0; gs_u8(w, "C_CSWeaponBase", "m_bInReload", &reloading);
    sb_add(out, "{");
    gs_add_kv_i(out, "entindex", idx); sb_add(out, ",");
    gs_add_kv_i(out, "handle", (long long)h); sb_add(out, ",");
    gs_add_kv_str(out, "name", idx >= 0 ? gs_designer_name((u32)idx) : 0); sb_add(out, ",");
    gs_add_kv_i(out, "defIndex", def); sb_add(out, ",");
    gs_add_kv_i(out, "clip1", clip1); sb_add(out, ",");
    gs_add_kv_i(out, "clip2", clip2); sb_add(out, ",");
    gs_add_kv_i(out, "reserve", reserve); sb_add(out, ",");
    gs_add_kv_b(out, "reloading", reloading);
    sb_add(out, "}");
}

/* pawn -> weapon services (CPlayer_WeaponServices via C_BasePlayerPawn::m_pWeaponServices) */
static void *gs_weapon_services(void *pawn) {
    void *ws = 0;
    if (!gs_ptr(pawn, "C_BasePlayerPawn", "m_pWeaponServices", &ws) || !ws || !mem_ok(ws)) return 0;
    return ws;
}
static void *gs_active_weapon(void *pawn) {
    void *ws = gs_weapon_services(pawn);
    u32 h = S2_CONST_INVALID_EHANDLE_INDEX;
    if (!ws || !gs_u32(ws, "CPlayer_WeaponServices", "m_hActiveWeapon", &h)) return 0;
    return gs_ent_by_handle(h);
}

/* player (controller + pawn) -> JSON used by game.local and game.radar */
static void gs_player_json(void *ctl, void *localPawn, int withDetails, SB *out) {
    u32 hp = S2_CONST_INVALID_EHANDLE_INDEX;
    void *pawn = 0;
    if (gs_u32(ctl, "CCSPlayerController", "m_hPlayerPawn", &hp)) pawn = gs_ent_by_handle(hp);
    u8 team = 0, alive = 0; int health = 0;
    gs_u8(ctl, "C_BaseEntity", "m_iTeamNum", &team);
    gs_u8(ctl, "CCSPlayerController", "m_bPawnIsAlive", &alive);
    gs_i32(ctl, "CCSPlayerController", "m_iPawnHealth", &health);
    float origin[3] = { 0, 0, 0 }, eye[3] = { 0, 0, 0 }, ang[3] = { 0, 0, 0 }, vel[3] = { 0, 0, 0 }, vo[3] = { 0, 0, 0 };
    int haveOrigin = 0, haveAng = 0, haveVel = 0, haveEye = 0;
    u8 spotted = 0; u32 spottedMask[2] = { 0, 0 };
    if (pawn) {
        haveOrigin = gs_origin(pawn, origin);
        haveAng = gs_vec(pawn, "C_CSPlayerPawn", "m_angEyeAngles", ang);
        haveVel = gs_vec(pawn, "C_BaseEntity", "m_vecAbsVelocity", vel);
        if (haveOrigin && gs_vec(pawn, "C_BaseModelEntity", "m_vecViewOffset", vo)) {
            eye[0] = origin[0] + vo[0]; eye[1] = origin[1] + vo[1]; eye[2] = origin[2] + vo[2]; haveEye = 1;
        }
        int so = 0;
        if (gs_off("C_CSPlayerPawn", "m_entitySpottedState", &so)) {
            void *st = (u8 *)pawn + so;
            gs_u8(st, "EntitySpottedState_t", "m_bSpotted", &spotted);
            gs_rd(st, "EntitySpottedState_t", "m_bSpottedByMask", spottedMask, 8);
        }
    }
    sb_add(out, "{");
    gs_add_kv_i(out, "controllerIndex", gs_ent_index(ctl)); sb_add(out, ",");
    gs_add_kv_i(out, "pawnIndex", pawn ? gs_ent_index(pawn) : -1); sb_add(out, ",");
    gs_add_kv_str(out, "name", gs_cstr(ctl, "CBasePlayerController", "m_iszPlayerName")); sb_add(out, ",");
    gs_add_kv_i(out, "team", team); sb_add(out, ",");
    gs_add_kv_b(out, "alive", alive); sb_add(out, ",");
    gs_add_kv_i(out, "health", health); sb_add(out, ",");
    gs_add_kv_b(out, "isLocal", pawn && pawn == localPawn); sb_add(out, ",");
    gs_add_kv_v3(out, "origin", origin, haveOrigin); sb_add(out, ",");
    gs_add_kv_v3(out, "eyeAngles", ang, haveAng); sb_add(out, ",");
    gs_add_kv_b(out, "spotted", spotted); sb_add(out, ",");
    gs_add_kv_i(out, "spottedByMask", (long long)spottedMask[0]);
    if (withDetails) {
        sb_add(out, ",");
        gs_add_kv_v3(out, "eye", eye, haveEye); sb_add(out, ",");
        gs_add_kv_v3(out, "velocity", vel, haveVel); sb_add(out, ",");
        u8 life = 0, scoped = 0; u32 flags = 0;
        gs_u8(pawn, "C_BaseEntity", "m_lifeState", &life);
        gs_u32(pawn, "C_BaseEntity", "m_fFlags", &flags);
        gs_u8(pawn, "C_CSPlayerPawn", "m_bIsScoped", &scoped);
        gs_add_kv_i(out, "lifeState", life); sb_add(out, ",");
        gs_add_kv_i(out, "flags", (long long)flags); sb_add(out, ",");
        gs_add_kv_b(out, "scoped", scoped); sb_add(out, ",");
        /* armor/helmet/money/ducked/moveType: C_CSPlayerPawn::m_ArmorValue, CCSPlayerController::m_bPawnHasHelmet,
           CCSPlayerController::m_pInGameMoneyServices -> m_iAccount, C_BasePlayerPawn::m_pMovementServices ->
           CCSPlayer_MovementServices::m_bDucked, C_BaseEntity::m_MoveType (null when unresolved) */
        void *money = 0, *move = 0;
        gs_ptr(ctl, "CCSPlayerController", "m_pInGameMoneyServices", &money);
        if (pawn) gs_ptr(pawn, "C_BasePlayerPawn", "m_pMovementServices", &move);
        gs_fld_i(out, 0, "armor", pawn, "C_CSPlayerPawn", "m_ArmorValue"); sb_add(out, ",");
        gs_fld_b(out, 0, "helmet", ctl, "CCSPlayerController", "m_bPawnHasHelmet"); sb_add(out, ",");
        gs_fld_i(out, 0, "money", money, "CCSPlayerController_InGameMoneyServices", "m_iAccount"); sb_add(out, ",");
        gs_fld_b(out, 0, "ducked", move, "CCSPlayer_MovementServices", "m_bDucked"); sb_add(out, ",");
        gs_fld_u8(out, 0, "moveType", pawn, "C_BaseEntity", "m_MoveType"); sb_add(out, ",");
        void *aw = pawn ? gs_active_weapon(pawn) : 0;
        sb_add(out, "\"activeWeapon\":");
        if (aw) gs_weapon_json(aw, out); else sb_add(out, "null");
    }
    sb_add(out, "}");
}

static int gs_session_header(void **ec, SB *out) {   /* {"tick":..,"inGame":..,"map":..  (no closing brace) */
    int tick = ((EcGetTickFn)(*(void ***)ec)[S2_SLOT_IVEngineClient2_GetLastServerTick])(ec);
    int inGame = ((EcIsInGameFn)(*(void ***)ec)[S2_SLOT_IVEngineClient2_IsInGame])(ec);
    const char *map = ((EcGetLevelNameFn)(*(void ***)ec)[S2_SLOT_IVEngineClient2_GetLevelNameShort])(ec);
    sb_add(out, "{");
    gs_add_kv_i(out, "tick", tick); sb_add(out, ",");
    gs_add_kv_b(out, "inGame", inGame); sb_add(out, ",");
    gs_add_kv_str(out, "map", (map && mem_ok(map) && map[0]) ? map : 0);
    return inGame;
}

/* ---- game.local ---- */
static int op_game_local(const char *params, SB *out, OpErr *e) {
    (void)params;
    void **ec = gs_engine_client(e);
    if (!ec || !gs_scope(e)) return 0;
    const char *why = "?";
    int ps = -1;
    void *ctl = gs_local_controller(ec, &ps, &why);
    if (!ctl) { op_err(e, -32024, "NoLocalPlayer", why); return 0; }
    gs_session_header(ec, out);
    sb_add(out, ",");
    gs_add_kv_i(out, "playerSlot", ps); sb_add(out, ",");
    /* view angles: the pawn's eye angles (schema C_CSPlayerPawn::m_angEyeAngles; the local client writes
       them from its own usercmd every tick, so they follow the input hook's override) plus the hook's
       override state. The static xmmword next to g_CCSGOInput is only written by the static initializer
       (live run 2026-10-11) and is not used. */
    u32 hp = S2_CONST_INVALID_EHANDLE_INDEX; void *pawn = 0;
    if (gs_u32(ctl, "CCSPlayerController", "m_hPlayerPawn", &hp)) pawn = gs_ent_by_handle(hp);
    {
        float va[3] = { 0, 0, 0 };
        int haveVa = pawn ? gs_vec(pawn, "C_CSPlayerPawn", "m_angEyeAngles", va) : 0;
        gs_add_kv_v3(out, "viewAngles", va, haveVa); sb_add(out, ",");
        sb_add(out, "\"inputOverride\":");
        if (g_inAnglesOn) sb_addf3(out, g_inAngles); else sb_add(out, "null");
        sb_add(out, ",");
    }
    sb_add(out, "\"player\":");
    gs_player_json(ctl, pawn, 1, out);
    sb_add(out, "}");
    return 1;
}

/* ---- game.weapons ---- */
static int op_game_weapons(const char *params, SB *out, OpErr *e) {
    (void)params;
    void **ec = gs_engine_client(e);
    if (!ec || !gs_scope(e)) return 0;
    const char *why = "?";
    void *ctl = gs_local_controller(ec, 0, &why);
    if (!ctl) { op_err(e, -32024, "NoLocalPlayer", why); return 0; }
    u32 hp = S2_CONST_INVALID_EHANDLE_INDEX; void *pawn = 0;
    if (gs_u32(ctl, "CCSPlayerController", "m_hPlayerPawn", &hp)) pawn = gs_ent_by_handle(hp);
    if (!pawn) { op_err(e, -32024, "NoLocalPlayer", "local controller has no pawn (dead/spectating?)"); return 0; }
    void *ws = gs_weapon_services(pawn);
    if (!ws) { op_err(e, -32024, "NoLocalPlayer", "pawn has no weapon services"); return 0; }
    int vo = 0;
    if (!gs_off("CPlayer_WeaponServices", "m_hMyWeapons", &vo)) { op_err(e, -32023, "SchemaUnavailable", "CPlayer_WeaponServices::m_hMyWeapons"); return 0; }
    /* CPlayer_WeaponServices::m_hMyWeapons: CUtlVector<CHandle> (generated spec
       S2_CUtlVector_CEntityHandle; CEntityHandle is a 4-byte handle) [S] */
    S2_CUtlVector_CEntityHandle *vec = (S2_CUtlVector_CEntityHandle *)((u8 *)ws + vo);
    int n = vec->m_Size;
    u32 *hs = (u32 *)vec->m_pMemory;
    void *aw = gs_active_weapon(pawn);
    gs_session_header(ec, out);
    sb_add(out, ",\"active\":");
    if (aw) gs_weapon_json(aw, out); else sb_add(out, "null");
    sb_add(out, ",\"count\":"); sb_addi(out, (n > 0 && n < 256 && hs && mem_ok(hs)) ? n : 0);
    sb_add(out, ",\"weapons\":[");
    int wrote = 0;
    if (n > 0 && n < 256 && hs && mem_ok(hs)) {
        for (int i = 0; i < n; i++) {
            void *w = gs_ent_by_handle(hs[i]);
            if (!w) continue;
            if (wrote++) sb_add(out, ",");
            gs_weapon_json(w, out);
        }
    }
    sb_add(out, "]}");
    return 1;
}

/* ---- game.radar ---- */
static int op_game_radar(const char *params, SB *out, OpErr *e) {
    (void)params;
    void **ec = gs_engine_client(e);
    if (!ec || !gs_scope(e)) return 0;
    const char *why = "?";
    void *ctl = gs_local_controller(ec, 0, &why);
    void *localPawn = 0;
    if (ctl) { u32 hp = S2_CONST_INVALID_EHANDLE_INDEX; if (gs_u32(ctl, "CCSPlayerController", "m_hPlayerPawn", &hp)) localPawn = gs_ent_by_handle(hp); }
    gs_session_header(ec, out);
    sb_add(out, ",\"players\":[");
    int wrote = 0;
    for (u32 i = 1; i <= 64; i++) {           /* controllers occupy entindex 1..64 (player slot + 1) */
        void *c = gs_ent_by_index(i);
        if (!c) continue;
        const char *dn = gs_designer_name(i);
        if (!dn || !seq(dn, "cs_player_controller")) continue;
        if (wrote++) sb_add(out, ",");
        gs_player_json(c, localPawn, 0, out);
    }
    sb_add(out, "],\"bomb\":");
    int bombWritten = 0;
    sb_add(out, "[");
    int hostages = 0;
    static char hostBuf[2048];
    SB hb; sb_init(&hb, hostBuf, sizeof hostBuf);
    for (u32 i = 65; i < S2_CONST_MAX_EDICTS; i++) {      /* world entities; chunk-null skips 512 at a time */
        u8 *id = gs_ident(i);
        if (!id) { if ((i & GS_IDENT_CHUNK_MASK) == 0 && !gs_ident(i | GS_IDENT_CHUNK_MASK)) { i |= GS_IDENT_CHUNK_MASK; } continue; }
        void *ent = *(void **)id;
        if (!ent) continue;
        const char *dn = gs_designer_name(i);
        if (!dn) continue;
        if (seq(dn, "planted_c4")) {
            if (!mem_ok(ent)) continue;
            float o[3]; int ho = gs_origin(ent, o);
            u8 ticking = 0, defused = 0; int site = -1; float blow = 0;
            gs_u8(ent, "C_PlantedC4", "m_bBombTicking", &ticking);
            gs_u8(ent, "C_PlantedC4", "m_bBombDefused", &defused);
            gs_i32(ent, "C_PlantedC4", "m_nBombSite", &site);
            gs_f32(ent, "C_PlantedC4", "m_flC4Blow", &blow);
            if (bombWritten++) sb_add(out, ",");
            sb_add(out, "{\"state\":\"planted\",");
            gs_add_kv_i(out, "entindex", (int)i); sb_add(out, ",");
            gs_add_kv_v3(out, "origin", o, ho); sb_add(out, ",");
            gs_add_kv_b(out, "ticking", ticking); sb_add(out, ",");
            gs_add_kv_b(out, "defused", defused); sb_add(out, ",");
            gs_add_kv_i(out, "site", site); sb_add(out, ",");
            gs_add_kv_f(out, "blowTime", blow);
            sb_add(out, "}");
        } else if (seq(dn, "weapon_c4")) {
            if (!mem_ok(ent)) continue;
            float o[3]; int ho = gs_origin(ent, o);
            u32 owner = S2_CONST_INVALID_EHANDLE_INDEX; gs_u32(ent, "C_BaseEntity", "m_hOwnerEntity", &owner);
            void *own = gs_ent_by_handle(owner);
            if (bombWritten++) sb_add(out, ",");
            sb_add(out, own ? "{\"state\":\"carried\"," : "{\"state\":\"dropped\",");
            gs_add_kv_i(out, "entindex", (int)i); sb_add(out, ",");
            gs_add_kv_i(out, "ownerPawnIndex", own ? gs_ent_index(own) : -1); sb_add(out, ",");
            gs_add_kv_v3(out, "origin", o, ho);
            sb_add(out, "}");
        } else if (seq(dn, "hostage_entity")) {
            if (!mem_ok(ent)) continue;
            float o[3]; int ho = gs_origin(ent, o);
            u8 rescued = 0; int state = -1; u32 leader = S2_CONST_INVALID_EHANDLE_INDEX; int health = 0;
            gs_u8(ent, "C_Hostage", "m_isRescued", &rescued);
            gs_i32(ent, "C_Hostage", "m_nHostageState", &state);
            gs_u32(ent, "C_Hostage", "m_leader", &leader);
            gs_i32(ent, "C_BaseEntity", "m_iHealth", &health);
            void *ld = gs_ent_by_handle(leader);
            if (hostages++) sb_add(&hb, ",");
            sb_add(&hb, "{");
            gs_add_kv_i(&hb, "entindex", (int)i); sb_add(&hb, ",");
            gs_add_kv_v3(&hb, "origin", o, ho); sb_add(&hb, ",");
            gs_add_kv_i(&hb, "state", state); sb_add(&hb, ",");
            gs_add_kv_b(&hb, "rescued", rescued); sb_add(&hb, ",");
            gs_add_kv_i(&hb, "health", health); sb_add(&hb, ",");
            gs_add_kv_i(&hb, "leaderPawnIndex", ld ? gs_ent_index(ld) : -1);
            sb_add(&hb, "}");
        }
    }
    sb_add(out, "],\"hostages\":[");
    if (!hb.ovf) sb_add(out, hostBuf);       /* on overflow hostBuf is cut mid-object: emit none, flag it */
    sb_add(out, "]");
    if (hb.ovf) sb_add(out, ",\"hostagesTruncated\":true");
    sb_add(out, "}");
    return 1;
}

/* ---- game.rules: C_CSGameRules through the cs_gamerules proxy (C_CSGameRulesProxy::m_pGameRules) ----
   The proxy's designer name is cs_gamerules (FGD; confirmed by live probe). Its entindex is cached and
   re-validated by designer name on each call, like the radar's world walk. Fields that do not resolve are
   null and listed in schemaMissing; a missing proxy or rules object is NoGameRules (not in a game). */
static int g_gsRulesIdx = -1;
static void *gs_gamerules_ent(int *idxOut) {
    if (g_gsRulesIdx >= 0) {                 /* trust the cache only while the identity agrees with it */
        const char *dn = gs_designer_name((u32)g_gsRulesIdx);
        void *ent = gs_ent_by_index((u32)g_gsRulesIdx);
        if (dn && seq(dn, "cs_gamerules") && ent && gs_ent_index(ent) == g_gsRulesIdx) { *idxOut = g_gsRulesIdx; return ent; }
        g_gsRulesIdx = -1;
    }
    for (u32 i = 1; i < S2_CONST_MAX_EDICTS; i++) {       /* chunk-null skips 512 at a time */
        u8 *id = gs_ident(i);
        if (!id) { if ((i & GS_IDENT_CHUNK_MASK) == 0 && !gs_ident(i | GS_IDENT_CHUNK_MASK)) { i |= GS_IDENT_CHUNK_MASK; } continue; }
        const char *dn = gs_designer_name(i);
        if (!dn || !seq(dn, "cs_gamerules")) continue;
        void *ent = gs_ent_by_index(i);
        if (!ent) continue;
        g_gsRulesIdx = (int)i;
        *idxOut = (int)i;
        return ent;
    }
    return 0;
}
static int op_game_rules(const char *params, SB *out, OpErr *e) {
    (void)params;
    void **ec = gs_engine_client(e);
    if (!ec || !gs_scope(e)) return 0;
    int off = 0;
    if (!gs_off("C_CSGameRulesProxy", "m_pGameRules", &off)) { op_err(e, -32023, "SchemaUnavailable", "C_CSGameRulesProxy::m_pGameRules"); return 0; }
    int idx = -1;
    void *proxy = gs_gamerules_ent(&idx);
    void *rules = 0;
    if (!proxy || !gs_ptr(proxy, "C_CSGameRulesProxy", "m_pGameRules", &rules) || !rules || !mem_ok(rules)) {
        op_err(e, -32025, "NoGameRules", "no cs_gamerules proxy with a C_CSGameRules object (not in a game?)");
        return 0;
    }
    char miss[1024] = "";
    SB mb; sb_init(&mb, miss, sizeof miss);
    gs_session_header(ec, out); sb_add(out, ",");
    gs_add_kv_i(out, "proxyIndex", idx); sb_add(out, ",");
    gs_fld_b(out, &mb, "warmup", rules, "C_CSGameRules", "m_bWarmupPeriod"); sb_add(out, ",");
    gs_fld_b(out, &mb, "freeze", rules, "C_CSGameRules", "m_bFreezePeriod"); sb_add(out, ",");
    gs_fld_b(out, &mb, "teamIntro", rules, "C_CSGameRules", "m_bTeamIntroPeriod"); sb_add(out, ",");
    gs_fld_b(out, &mb, "gameRestart", rules, "C_CSGameRules", "m_bGameRestart"); sb_add(out, ",");
    gs_fld_b(out, &mb, "hasMatchStarted", rules, "C_CSGameRules", "m_bHasMatchStarted"); sb_add(out, ",");
    gs_fld_i(out, &mb, "gamePhase", rules, "C_CSGameRules", "m_gamePhase"); sb_add(out, ",");
    gs_fld_i(out, &mb, "roundsPlayed", rules, "C_CSGameRules", "m_totalRoundsPlayed"); sb_add(out, ",");
    gs_fld_f(out, &mb, "roundStartTime", rules, "C_CSGameRules", "m_fRoundStartTime"); sb_add(out, ",");
    gs_fld_f(out, &mb, "restartRoundTime", rules, "C_CSGameRules", "m_flRestartRoundTime"); sb_add(out, ",");
    gs_fld_i(out, &mb, "roundTimeSec", rules, "C_CSGameRules", "m_iRoundTime"); sb_add(out, ",");
    gs_fld_i(out, &mb, "roundWinStatus", rules, "C_CSGameRules", "m_iRoundWinStatus"); sb_add(out, ",");
    gs_fld_i(out, &mb, "roundEndReason", rules, "C_CSGameRules", "m_eRoundEndReason"); sb_add(out, ",");
    gs_fld_b(out, &mb, "bombPlanted", rules, "C_CSGameRules", "m_bBombPlanted"); sb_add(out, ",");
    sb_add(out, "\"schemaMissing\":["); sb_add(out, miss); sb_add(out, "]}");
    return 1;
}

/* ---- game.entities {class?, name?, max?, entindex?}: identities whose designer name (class) and/or targetname
   (name) match, in entindex order (1..MAX_EDICTS-1; chunk-null skips like game.radar). max counts matches (1..256), so a
   class filter reaches any index. entindex (1..MAX_EDICTS-1) reads only that slot: no scan, count 0 or 1, and the class/
   name filters still apply to it. Per entity: entindex, class, name (null when empty), origin, angles
   (CGameSceneNode::m_angAbsRotation), health, toggleState (C_BaseToggle::m_toggle_state: the client schema has no
   such field, so it is null until a resolver can read it). ---- */
#define GS_ENT_MAX 256
static int op_game_entities(const char *params, SB *out, OpErr *e) {
    static char cls[128], nm[128];
    cls[0] = 0; nm[0] = 0;
    long long max = 64, only = 0;
    int single = 0;
    if (params) {
        json_string(params, "class", cls, sizeof cls);
        json_string(params, "name", nm, sizeof nm);
        json_int(params, "max", &max);
        single = json_int(params, "entindex", &only);
    }
    if (max < 1 || max > GS_ENT_MAX) { op_err(e, -32602, "InvalidParams", "max must be 1..256"); return 0; }
    if (single && (only < 1 || only > S2_CONST_MAX_EDICTS - 1)) {
        char bnd[40]; SB eb;                 /* "1..S2_CONST_MAX_EDICTS-1", spelled from the const */
        sb_init(&eb, bnd, sizeof bnd);
        sb_add(&eb, "entindex must be 1.."); sb_addnum(&eb, S2_CONST_MAX_EDICTS - 1u);
        op_err(e, -32602, "InvalidParams", bnd); return 0;
    }
    void **ec = gs_engine_client(e);
    if (!ec || !gs_scope(e)) return 0;
    gs_session_header(ec, out);
    sb_add(out, ",\"entities\":[");
    int wrote = 0, truncated = 0;
    u32 first = single ? (u32)only : 1, last = single ? (u32)only + 1 : S2_CONST_MAX_EDICTS;
    for (u32 i = first; i < last && !truncated; i++) {
        u8 *id = gs_ident(i);
        if (!id) { if ((i & GS_IDENT_CHUNK_MASK) == 0 && !gs_ident(i | GS_IDENT_CHUNK_MASK)) { i |= GS_IDENT_CHUNK_MASK; } continue; }
        void *ent = gs_ent_by_index(i);
        if (!ent) continue;
        const char *dn = gs_designer_name(i);
        const char *tn = gs_entity_name(i);
        if (cls[0] && (!dn || !seq(dn, cls))) continue;
        if (nm[0] && (!tn || !seq(tn, nm))) continue;
        if (wrote == max) { truncated = 1; break; }
        u32 mark = out->len;                 /* one entity is written whole or rolled back */
        if (wrote) sb_add(out, ",");
        float o[3] = { 0, 0, 0 }, a[3] = { 0, 0, 0 };
        int ho = gs_origin(ent, o), ha = gs_angles(ent, a);
        sb_add(out, "{");
        gs_add_kv_i(out, "entindex", (int)i); sb_add(out, ",");
        gs_add_kv_str(out, "class", dn); sb_add(out, ",");
        gs_add_kv_str(out, "name", tn); sb_add(out, ",");
        gs_add_kv_v3(out, "origin", o, ho); sb_add(out, ",");
        gs_add_kv_v3(out, "angles", a, ha); sb_add(out, ",");
        gs_fld_i(out, 0, "health", ent, "C_BaseEntity", "m_iHealth"); sb_add(out, ",");
        gs_fld_i(out, 0, "toggleState", ent, "C_BaseToggle", "m_toggle_state");
        sb_add(out, "}");
        if (out->ovf || out->len + 64 > out->cap) {   /* keep ~64 B for the trailer; the entity goes back out */
            out->len = mark; out->buf[mark] = 0; out->ovf = 0;
            truncated = 1;
            break;
        }
        wrote++;
    }
    sb_add(out, "],\"count\":"); sb_addi(out, wrote);
    sb_add(out, ",\"truncated\":"); sb_add(out, truncated ? "true" : "false");
    sb_add(out, "}");
    return 1;
}

/* ---- game.selectWeapon: the game's own selection paths, no synthetic keys ----
   {slot:n} / {command:...}: `slot0`..`slot13`, `invnext`, `invprev`, `invnextselect`, `invprevselect`, `lastinv`
   are client ConCommands (client!0x1800DEDE0 registers "slot1" -> 0x180C1DFE0 -> split-screen slot object vfn 79);
   they are queued on the command buffer exactly as a key bind would. Allow-listed, so usable outside -testscript.
   {weapon:"weapon_x"} / {defIndex:n} / {handle:h}: CS2 has no `use <weapon>` command (no "use" string is
   referenced in client.dll or server.dll on 2000930). The native path is CCSGOInput slot 9
   MakeWeaponSelection(this, splitSlot, C_BasePlayerWeapon*) (client!0x180B77040), which is what slotN ends in:
   it stores the weapon's handle at this+0x694 and the next CreateMove writes CBaseUserCmdPB.weaponselect. The
   weapon must be one the local pawn holds (m_hMyWeapons). Runs on the main thread like every game.* method. */
typedef void (*InMakeWeaponSelectionFn)(void *self, int splitSlot, void *weapon);
static const char *const k_gsInvCommands[] = { "invnext", "invprev", "invnextselect", "invprevselect", "lastinv", 0 };
static int gs_is_weapon_name(const char *s) {
    static const char pfx[] = "weapon_";
    for (int i = 0; pfx[i]; i++) if (s[i] != pfx[i]) return 0;
    for (const char *p = s + 7; *p; p++)
        if (!((*p >= 'a' && *p <= 'z') || (*p >= '0' && *p <= '9') || *p == '_')) return 0;
    return s[7] != 0;
}
static int gs_starts(const char *s, const char *pfx) {
    while (*pfx) if (*s++ != *pfx++) return 0;
    return 1;
}
static int gs_weapon_def(void *w) {
    int a = 0, b = 0; u16 d16 = 0;
    if (!gs_off("C_EconEntity", "m_AttributeManager", &a) || !gs_off("C_AttributeContainer", "m_Item", &b)) return -1;
    return gs_rd((u8 *)w + a + b, "C_EconItemView", "m_iItemDefinitionIndex", &d16, 2) ? d16 : -1;
}
/* weapon held by the local pawn matching exactly one of name / defIndex / handle; "weapon_knife" also matches
   any knife variant (weapon_knife_t, weapon_knife_karambit, weapon_bayonet, ...) when there is no exact match */
static void *gs_find_held_weapon(const char *name, long long def, long long handle, OpErr *e) {
    void **ec = gs_engine_client(e);
    if (!ec || !gs_scope(e)) return 0;
    const char *why = "?";
    void *ctl = gs_local_controller(ec, 0, &why);
    if (!ctl) { op_err(e, -32024, "NoLocalPlayer", why); return 0; }
    u32 hp = S2_CONST_INVALID_EHANDLE_INDEX; void *pawn = 0;
    if (gs_u32(ctl, "CCSPlayerController", "m_hPlayerPawn", &hp)) pawn = gs_ent_by_handle(hp);
    if (!pawn) { op_err(e, -32024, "NoLocalPlayer", "local controller has no pawn (dead/spectating?)"); return 0; }
    void *ws = gs_weapon_services(pawn);
    int vo = 0;
    if (!ws) { op_err(e, -32024, "NoLocalPlayer", "pawn has no weapon services"); return 0; }
    if (!gs_off("CPlayer_WeaponServices", "m_hMyWeapons", &vo)) { op_err(e, -32023, "SchemaUnavailable", "CPlayer_WeaponServices::m_hMyWeapons"); return 0; }
    S2_CUtlVector_CEntityHandle *vec = (S2_CUtlVector_CEntityHandle *)((u8 *)ws + vo);
    int n = vec->m_Size;
    u32 *hs = (u32 *)vec->m_pMemory;
    if (!(n > 0 && n < 256 && hs && mem_ok(hs))) n = 0;
    void *knife = 0;
    for (int i = 0; i < n; i++) {
        void *w = gs_ent_by_handle(hs[i]);
        if (!w) continue;
        if (handle >= 0) { if ((long long)hs[i] == handle) return w; continue; }
        if (def >= 0) { if (gs_weapon_def(w) == def) return w; continue; }
        int idx = gs_ent_index(w);
        const char *dn = idx >= 0 ? gs_designer_name((u32)idx) : 0;
        if (!dn) continue;
        if (seq(dn, name)) return w;
        if (!knife && seq(name, "weapon_knife") && (gs_starts(dn, "weapon_knife") || seq(dn, "weapon_bayonet"))) knife = w;
    }
    if (knife) return knife;
    op_err(e, -32602, "InvalidParams", "the local player does not hold that weapon (see game.weapons)");
    return 0;
}
static int op_game_select_weapon(const char *params, SB *out, OpErr *e) {
    static char cmd[160];
    static char word[96];
    cmd[0] = 0;
    long long slot = 0, def = -1, handle = -1;
    int byWeapon = 0;
    if (params && json_int(params, "slot", &slot)) {
        if (slot < 0 || slot > 13) { op_err(e, -32602, "InvalidParams", "slot must be 0..13"); return 0; }
        SB c; sb_init(&c, cmd, sizeof cmd); sb_add(&c, "slot"); sb_addnum(&c, (u64)slot);
    } else if (params && json_string(params, "command", word, sizeof word) && word[0]) {
        int ok = 0;
        for (int i = 0; k_gsInvCommands[i]; i++) if (seq(word, k_gsInvCommands[i])) ok = 1;
        if (!ok) { op_err(e, -32602, "InvalidParams", "command must be invnext|invprev|invnextselect|invprevselect|lastinv"); return 0; }
        SB c; sb_init(&c, cmd, sizeof cmd); sb_add(&c, word);
    } else if (params && json_string(params, "weapon", word, sizeof word) && word[0]) {
        if (!gs_is_weapon_name(word)) { op_err(e, -32602, "InvalidParams", "weapon must look like weapon_ak47"); return 0; }
        byWeapon = 1;
    } else if (params && json_int(params, "defIndex", &def)) {
        if (def < 0 || def > 65535) { op_err(e, -32602, "InvalidParams", "defIndex must be 0..65535"); return 0; }
        byWeapon = 1; word[0] = 0;
    } else if (params && json_int(params, "handle", &handle)) {
        if (handle < 0 || handle > 0xFFFFFFFFll) { op_err(e, -32602, "InvalidParams", "handle must be a u32 entity handle"); return 0; }
        byWeapon = 1; word[0] = 0;
    } else {
        op_err(e, -32602, "InvalidParams", "one of slot, command, weapon, defIndex or handle required");
        return 0;
    }
    if (byWeapon) {
        if (!mod_ok(PIN_CLIENT) || !mod_ok(PIN_SCHEMA)) {
            op_err(e, -32017, "PrerequisiteFailed", "weapon/defIndex/handle selection needs client.dll and schemasystem.dll pinned");
            return 0;
        }
        u8 *cb = (u8 *)g_modBase[PIN_CLIENT];
        void **in = *(void ***)(cb + CG_INPUT_PTR);
        /* the input hook swaps the vptr to a private copy; either way slot 9 is the pinned original */
        if (!(obj_is(in, PIN_CLIENT, CV_CSGOINPUT_VT) || (in && in == g_inObj && in_installed()))) {
            op_err(e, -32017, "PrerequisiteFailed", "g_pInput is not the pinned CCSGOInput (vtable)");
            return 0;
        }
        void *w = gs_find_held_weapon(word, def, handle, e);
        if (!w) return 0;
        ((InMakeWeaponSelectionFn)((void **)(cb + CV_CSGOINPUT_VT))[S2_SLOT_CClientInput_MakeWeaponSelection])(in, 0, w);
        sb_add(out, "{\"queued\":true,\"via\":\"MakeWeaponSelection\",\"weapon\":");
        gs_weapon_json(w, out);
        sb_add(out, "}");
        return 1;
    }
    void **is = (void **)mod_iface(PIN_ENGINE2, S2_IFACE_INPUTSERVICE);
    if (!obj_is(is, PIN_ENGINE2, EV_INPUTSERVICE_VT)) {
        op_err(e, -32017, "PrerequisiteFailed", "InputService_001 is not the pinned CInputService");
        return 0;
    }
    ((IsAddCommandTextFn)(*(void ***)is)[S2_SLOT_IInputService_AddCommandText])(is, 5, cmd, 0);
    sb_add(out, "{\"queued\":true,\"via\":\"command\",\"command\":"); sb_addjson(out, cmd); sb_add(out, "}");
    return 1;
}

/* ---- sound events: wrap ISoundOpSystem start slots 11/12/13 (record, then call the original) ----
   The wrapper slots are not empty (unlike the input hook), so this is the first vtable *wrap* in the bridge:
   no game code is patched, the original target is called unchanged, and uninstall restores the vptr.
   SNC_SOS_VT_SLOTS (sigs (b), bridge_pins.h) is the CSoundOpSystem vftable length measured from its RTTI COL. */
#define SND_RING 256
#define SND_PARAMS_MAX 128        /* packed start params copied per event (drain hex <= 256 chars) */
typedef struct { u32 seq; u32 hash; int entindex; u32 guid; u8 slot; u8 layer; int tick; u64 ms; u32 aux[2]; int inst150; char name[64]; int plen; u16 pcopied; u8 praw[SND_PARAMS_MAX]; } SndEvt;
typedef const char *(*SosNameByGuidFn)(void *self, u32 guid);
typedef int *(*SosInstIntFn)(void *self, int *out, u32 guid);
typedef void (*SosListenerFn)(void *self, void *l);
typedef void *(*SosStartParamsFn)(void *self, void *outGuid, const void *params);
typedef void *(*SosStartByNameFn)(void *self, void *outGuid, const char *name, void *params);
/* SosStartFn / SndEvt.layer: params byte +0x22 (S2_OFF_SosStartSoundEventParams_t_m_nLayer) and the 4th argument of slot 13 are the voice layer, not flags */
typedef void *(*SosStartFn)(void *self, void *outGuid, const char *name, int entindex, u8 layer);
typedef const char *(*SemGetNameFn)(void *self, u32 hash);
static void **g_sosObj;
static void *g_sosOrigVt;
static void *g_sosVtCopy[1 + SNC_SOS_VT_SLOTS];
static SosStartParamsFn g_sosOrigParams;
static SosStartByNameFn g_sosOrigByName;
static SosStartFn g_sosOrigStart;
static volatile long g_sndLock;
static SndEvt g_snd[SND_RING];
static u32 g_sndHead, g_sndCount, g_sndSeq, g_sndDropped, g_sndTotal;

static void snd_lock(void) { while (il_cmpxchg(&g_sndLock, 1, 0) != 0) {} }
static void snd_unlock(void) { il_xchg(&g_sndLock, 0); }

static const char *snd_name_of(u32 hash) {
    if (!g_sosObj) return 0;
    void **sem = (void **)((u8 *)g_sosObj + S2_BASEOFF_ISoundOpSystem_ISoundEventManager);   /* ISoundEventManager base */
    if (*(void **)sem != (void *)((u8 *)g_modBase[PIN_SOUND] + SNV_SOUNDEVENTMGR_VT)) return 0;
    const char *n = ((SemGetNameFn)(*(void ***)sem)[S2_SLOT_ISoundEventManager_GetSoundEventName])(sem, hash);
    return (n && mem_ok(n)) ? n : 0;
}
static void snd_ev_init(SndEvt *ev, u8 slot, u32 hash, const char *name, int entindex, u32 guid, u8 layer) {
    ev->seq = 0; ev->hash = hash; ev->entindex = entindex; ev->guid = guid; ev->slot = slot; ev->layer = layer;
    ev->tick = -1; ev->ms = GetTickCount64(); ev->aux[0] = ev->aux[1] = 0; ev->inst150 = -1;
    ev->plen = -1; ev->pcopied = 0;
    for (u32 q = 0; q < sizeof ev->praw; q++) ev->praw[q] = 0;
    const char *n = name ? name : snd_name_of(hash);
    u32 i = 0;
    if (n) for (; i < sizeof ev->name - 1 && n[i]; i++) ev->name[i] = n[i];
    ev->name[i] = 0;
}
static void snd_push(SndEvt *ev) {
    snd_lock();
    ev->seq = ++g_sndSeq;
    g_sndTotal++;
    if (g_sndCount >= SND_RING) { g_sndHead = (g_sndHead + 1) % SND_RING; g_sndCount--; g_sndDropped++; }
    g_snd[(g_sndHead + g_sndCount) % SND_RING] = *ev;
    g_sndCount++;
    snd_unlock();
}
static void snd_record(u8 slot, u32 hash, const char *name, int entindex, u32 guid, u8 layer) {
    SndEvt ev;
    snd_ev_init(&ev, slot, hash, name, entindex, guid, layer);
    snd_push(&ev);
}
/* m2.20: the packed start params. SndStartEventParams carries a pointer (+S2_OFF_SosStartSoundEventParams_t_m_pFieldData)
   and a byte length (+S2_OFF_SosStartSoundEventParams_t_m_nFieldDataSize) to the caller's packed field buffer (sigs (b),
   verified live: a snd_event_* start carries 4dce7c5a 0a 0c00 <f32 x><f32 y><f32 z> - field hash
   'position', type 0x0a, u16 size 12, the entity's Hammer origin). Captured ONLY here, in the
   slot-11 params hook; a len < 0 or > 65535 counts as unreadable (plen -1). The copy probes one
   page at a time (mem_ok reads through the loader; a SND_PARAMS_MAX-sized run crosses <= 2 pages)
   and stops at the first unreadable page - pcopied reports what actually landed. */
static void snd_capture_packed(const u8 *p, SndEvt *ev) {
    ev->plen = -1; ev->pcopied = 0;
    if (!p
        || !mem_ok(p + S2_OFF_SosStartSoundEventParams_t_m_pFieldData + sizeof(u64) - 1)
        || !mem_ok(p + S2_OFF_SosStartSoundEventParams_t_m_nFieldDataSize + sizeof(int) - 1))
        return;
    const u8 *pp = *(const u8 *const *)(p + S2_OFF_SosStartSoundEventParams_t_m_pFieldData);
    int len = *(const int *)(p + S2_OFF_SosStartSoundEventParams_t_m_nFieldDataSize);
    if (len < 0 || len > 65535) return;                 /* plen stays -1 = unreadable */
    ev->plen = len;
    u32 n = (u32)len < SND_PARAMS_MAX ? (u32)len : SND_PARAMS_MAX;
    for (u32 q = 0; q < n; q++) {
        if ((q == 0 || (((unsigned long long)(pp + q)) & 0xFFF) == 0) && !mem_ok(pp + q)) break;   /* first byte + every page start */
        ev->praw[ev->pcopied++] = pp[q];
    }
}
/* SosStartSoundEventParams_t: the S2_OFF_* member offsets and the S2_SIZEOF_ guard below are generated
   (s2sdk_gen.h); sdkcheck.py holds each offset to the sigs measurement. The guard is the whole struct
   (40 bytes), 4 more than the last field the hook reads needs (+0x22 layer, 36 bytes) - callers pass a
   full struct. The packed start params are read ONLY here - see snd_capture_packed. */
static void *snd_hook_params(void *self, void *outGuid, const void *params) {
    void *r = g_sosOrigParams(self, outGuid, params);
    u32 guid = outGuid && mem_ok(outGuid) ? *(u32 *)outGuid : 0;
    const u8 *p = (const u8 *)params;
    if (p && mem_ok(p + S2_SIZEOF_SosStartSoundEventParams_t)) {
        SndEvt ev;
        snd_ev_init(&ev, S2_SLOT_ISoundOpSystem_StartSoundEvent_2, *(const u32 *)(p + S2_OFF_SosStartSoundEventParams_t_m_nSoundEventHash), 0,
                    *(const int *)(p + S2_OFF_SosStartSoundEventParams_t_m_SourceEntity), guid, p[S2_OFF_SosStartSoundEventParams_t_m_nLayer]);
        snd_capture_packed(p, &ev);
        snd_push(&ev);
    }
    return r;
}
static void *snd_hook_by_name(void *self, void *outGuid, const char *name, void *params) {
    void *r = g_sosOrigByName(self, outGuid, name, params);
    u32 guid = outGuid && mem_ok(outGuid) ? *(u32 *)outGuid : 0;
    u8 *p = (u8 *)params;
    u32 hash = (p && mem_ok(p + S2_OFF_SosStartSoundEventParams_t_m_nSoundEventHash + sizeof(u32))) ? *(u32 *)(p + S2_OFF_SosStartSoundEventParams_t_m_nSoundEventHash) : 0;  /* slot 12 writes the hash to the params' hash field */
    int ent = (p && mem_ok(p + S2_OFF_SosStartSoundEventParams_t_m_SourceEntity + sizeof(int))) ? *(int *)(p + S2_OFF_SosStartSoundEventParams_t_m_SourceEntity) : -1;
    snd_record(12, hash, (name && mem_ok(name)) ? name : 0, ent, guid, 0);
    return r;
}
static void *snd_hook_start(void *self, void *outGuid, const char *name, int entindex, u8 layer) {
    void *r = g_sosOrigStart(self, outGuid, name, entindex, layer);
    u32 guid = outGuid && mem_ok(outGuid) ? *(u32 *)outGuid : 0;
    snd_record(13, 0, (name && mem_ok(name)) ? name : 0, entindex, guid, layer);
    return r;
}
static int snd_installed(void) { return g_sosObj && *(void **)g_sosObj == (void *)&g_sosVtCopy[1]; }
static int snd_install(OpErr *e) {
    if (snd_installed()) return 1;
    void **obj = (void **)mod_iface(PIN_SOUND, S2_IFACE_SOUNDOPSYSTEM);
    if (!obj_is(obj, PIN_SOUND, SNV_SOUNDOPSYSTEM_VT)) {
        op_err(e, -32017, "PrerequisiteFailed", "SoundOpSystem001 is not the pinned CSoundOpSystem");
        return 0;
    }
    void **vt = (void **)((u8 *)g_modBase[PIN_SOUND] + SNV_SOUNDOPSYSTEM_VT);
    g_sosVtCopy[0] = vt[-1];
    for (int i = 0; i < (int)SNC_SOS_VT_SLOTS; i++) g_sosVtCopy[1 + i] = vt[i];
    g_sosOrigParams = (SosStartParamsFn)vt[S2_SLOT_ISoundOpSystem_StartSoundEvent_2];
    g_sosOrigByName = (SosStartByNameFn)vt[S2_SLOT_ISoundOpSystem_StartSoundEvent_1];
    g_sosOrigStart = (SosStartFn)vt[S2_SLOT_ISoundOpSystem_StartSoundEvent];
    g_sosVtCopy[1 + S2_SLOT_ISoundOpSystem_StartSoundEvent_2] = (void *)snd_hook_params;
    g_sosVtCopy[1 + S2_SLOT_ISoundOpSystem_StartSoundEvent_1] = (void *)snd_hook_by_name;
    g_sosVtCopy[1 + S2_SLOT_ISoundOpSystem_StartSoundEvent] = (void *)snd_hook_start;
    g_sosObj = obj;
    g_sosOrigVt = vt;
    *(void *volatile *)obj = (void *)&g_sosVtCopy[1];
    logline("sound: hook installed (CSoundOpSystem vptr -> private vtable copy; slots 11/12/13 wrapped)");
    return 1;
}
static void snd_uninstall(void) {
    if (!snd_installed()) return;
    *(void *volatile *)g_sosObj = g_sosOrigVt;
    logline("sound: hook removed");
}
/* ---- supported sink: ISoundOpSystem slot 81 add-listener / 82 remove-listener ----
   The operator system keeps a vector of listener objects (sos+0x2518) and calls, under the sos lock:
   slot 0 (self, int* processed) at the end of ProcessEventQueue, slot 1 (self, int*) / slot 2 (self, i64*) /
   slot 4 (self, int*) from the per-frame update, slot 3 (self, msgQueue*) for lib-stack-field messages and
   slot 5 (self, guid, aux0, aux1) after every successful StartSoundEvent_Internal (soundsystem!0x1801C5D90,
   0x1801E1F80). All call sites discard the return value, so a no-op vtable is safe. The callback carries no
   hash/entity: the name is read while the instance is alive (slot 68 GetSoundEventNameByGuid); slot 86's
   instance int +0x150 is recorded as a candidate source entity (unverified). Registered object, no vptr
   swap, no code patched. Runs on whichever thread drives CSoundSystem::Update (inline or thread-pool job). */
#define SND_LISTENER_SLOTS 16      /* listener-object vtable shape (b): not in the sigs DB yet (bridge-owned object) */
static void *g_sndLvt[SND_LISTENER_SLOTS];
static void **g_sndListener = g_sndLvt;      /* the object: its first (only) member is the vptr */
static int g_sndListenerOn;
static void snd_l_noop(void) {}
static void snd_l_on_start(void *self, u32 guid, u32 aux0, u32 aux1) {
    (void)self;
    const char *name = 0;
    int inst = -1;
    if (g_sosObj) {
        void **vt = g_sosOrigVt ? (void **)g_sosOrigVt : *(void ***)g_sosObj;
        name = ((SosNameByGuidFn)vt[S2_SLOT_ISoundOpSystem_GetActiveSoundEventName])(g_sosObj, guid);
        if (name && !mem_ok(name)) name = 0;
        int tmp = -1;
        ((SosInstIntFn)vt[S2_SLOT_ISoundOpSystem_GetSoundEventSourceEntity])(g_sosObj, &tmp, guid);
        inst = tmp;
    }
    SndEvt ev;
    snd_ev_init(&ev, S2_SLOT_ISoundOpSystem_AddSoundEventListener, 0, name, -1, guid, 0);
    ev.aux[0] = aux0; ev.aux[1] = aux1; ev.inst150 = inst;
    snd_push(&ev);
}
static int snd_sos_resolve(OpErr *e) {        /* g_sosObj/g_sosOrigVt without installing anything */
    if (g_sosObj) return 1;
    void **obj = (void **)mod_iface(PIN_SOUND, S2_IFACE_SOUNDOPSYSTEM);
    if (!obj_is(obj, PIN_SOUND, SNV_SOUNDOPSYSTEM_VT)) {
        op_err(e, -32017, "PrerequisiteFailed", "SoundOpSystem001 is not the pinned CSoundOpSystem");
        return 0;
    }
    g_sosObj = obj;
    g_sosOrigVt = (void *)((u8 *)g_modBase[PIN_SOUND] + SNV_SOUNDOPSYSTEM_VT);
    return 1;
}
static int snd_listener_install(OpErr *e) {
    if (g_sndListenerOn) return 1;
    if (!snd_sos_resolve(e)) return 0;
    for (int i = 0; i < SND_LISTENER_SLOTS; i++) g_sndLvt[i] = (void *)snd_l_noop;
    g_sndLvt[S2_SLOT_ISoundEventListener_OnSoundEventStarted] = (void *)snd_l_on_start;
    static void *vtp = (void *)g_sndLvt;      /* object = { vptr } */
    g_sndListener = &vtp;
    ((SosListenerFn)((void **)g_sosOrigVt)[S2_SLOT_ISoundOpSystem_AddSoundEventListener])(g_sosObj, g_sndListener);
    g_sndListenerOn = 1;
    logline("sound: listener registered (ISoundOpSystem slot 81)");
    return 1;
}
static void snd_listener_uninstall(void) {
    if (!g_sndListenerOn) return;
    ((SosListenerFn)((void **)g_sosOrigVt)[S2_SLOT_ISoundOpSystem_RemoveSoundEventListener])(g_sosObj, g_sndListener);
    g_sndListenerOn = 0;
    logline("sound: listener removed (ISoundOpSystem slot 82)");
}

static void snd_status_json(SB *out) {
    sb_add(out, "{\"installed\":"); sb_add(out, (snd_installed() || g_sndListenerOn) ? "true" : "false");
    sb_add(out, ",\"mode\":"); sb_add(out, snd_installed() && g_sndListenerOn ? "\"both\"" : snd_installed() ? "\"wrap\"" : g_sndListenerOn ? "\"listener\"" : "null");
    sb_add(out, ",\"pending\":"); sb_addnum(out, g_sndCount);
    sb_add(out, ",\"total\":"); sb_addnum(out, g_sndTotal);
    sb_add(out, ",\"dropped\":"); sb_addnum(out, g_sndDropped);
    sb_add(out, ",\"ringSize\":"); sb_addnum(out, SND_RING);
    sb_add(out, "}");
}
static int op_sound(const char *method, const char *params, SB *out, OpErr *e) {
    if (seq(method, "sound.uninstall")) { snd_uninstall(); snd_listener_uninstall(); snd_status_json(out); return 1; }
    /* sink selection: {mode:"wrap"|"listener"|"both"}; default = wrap until the listener's entity field is verified */
    static char mode[16];
    mode[0] = 0;
    if (params) json_string(params, "mode", mode, sizeof mode);
    int wantWrap = 1, wantListener = 0;
    if (seq(mode, "listener")) { wantWrap = 0; wantListener = 1; }
    else if (seq(mode, "both")) { wantWrap = 1; wantListener = 1; }
    else if (mode[0] && !seq(mode, "wrap")) { op_err(e, -32602, "InvalidParams", "mode must be wrap|listener|both"); return 0; }
    if (!mode[0] && (snd_installed() || g_sndListenerOn)) { wantWrap = snd_installed(); wantListener = g_sndListenerOn; }
    if (wantWrap && !snd_install(e)) return 0;
    if (!wantWrap) snd_uninstall();
    if (wantListener && !snd_listener_install(e)) return 0;
    if (!wantListener) snd_listener_uninstall();
    if (seq(method, "sound.status") || seq(method, "sound.subscribe")) { snd_status_json(out); return 1; }
    /* sound.drain {max?:int, resolve?:bool} */
    long long max = 128;
    int resolve = 1;
    if (params) { json_int(params, "max", &max); json_bool(params, "resolve", &resolve); }
    if (max < 1) max = 1;
    if (max > SND_RING) max = SND_RING;
    static SndEvt batch[SND_RING];
    u32 n = 0, dropped, total;
    snd_lock();
    while (n < (u32)max && g_sndCount) { batch[n++] = g_snd[g_sndHead]; g_sndHead = (g_sndHead + 1) % SND_RING; g_sndCount--; }
    dropped = g_sndDropped; total = g_sndTotal; g_sndDropped = 0;
    snd_unlock();
    int haveScope = resolve && gs_scope(e);
    if (resolve && !haveScope) { e->code = 0; e->name = 0; e->data[0] = 0; }   /* positions are optional */
    u64 now = GetTickCount64();
    sb_add(out, "{\"events\":[");
    u32 k = 0;                               /* events actually written */
    for (; k < n; k++) {
        /* each event is written whole or rolled back: worst case ~1000 B (escaped name, entityName
           from game memory, <= 256 hex chars of packed params on a slot-11 event) */
        u32 mark = out->len;
        SndEvt *ev = &batch[k];
        if (k) sb_add(out, ",");
        sb_add(out, "{");
        gs_add_kv_i(out, "seq", ev->seq); sb_add(out, ",");
        gs_add_kv_i(out, "hash", (long long)ev->hash); sb_add(out, ",");
        gs_add_kv_str(out, "name", ev->name[0] ? ev->name : 0); sb_add(out, ",");
        gs_add_kv_i(out, "entindex", ev->entindex); sb_add(out, ",");
        gs_add_kv_i(out, "guid", (long long)ev->guid); sb_add(out, ",");
        gs_add_kv_i(out, "via", ev->slot); sb_add(out, ",");
        gs_add_kv_i(out, "flags", ev->layer);   /* wire key kept for clients; the byte is SosStartSoundEventParams_t::m_nLayer, the voice layer (0xFE none, 0xFF all) */ sb_add(out, ",");
        gs_add_kv_i(out, "ageMs", (long long)(now - ev->ms));
        /* m2.20: the packed start params, slot-11 starts only (the other slots carry no params
           pointer the wrap sees). len is the caller's declared byte count, hex the <= 128 bytes
           that were copied, truncated true when the copy stopped early. plen < 0 = the params
           pointer/len were unreadable -> null (the Rust side treats it as "no params seen"). */
        if (ev->slot == S2_SLOT_ISoundOpSystem_StartSoundEvent_2) {
            sb_add(out, ",\"params\":");
            if (ev->plen < 0) { sb_add(out, "null"); }
            else {
                static const char hx[] = "0123456789abcdef";
                sb_add(out, "{\"len\":"); sb_addnum(out, (u32)ev->plen);
                sb_add(out, ",\"hex\":\"");
                for (u32 q = 0; q < ev->pcopied; q++) {
                    char c[3] = { hx[ev->praw[q] >> 4], hx[ev->praw[q] & 15], 0 };
                    sb_add(out, c);
                }
                sb_add(out, "\",\"truncated\":");
                sb_add(out, ev->pcopied < (u16)ev->plen ? "true" : "false");
                sb_add(out, "}");
            }
        }
        if (ev->slot == 81) {
            sb_add(out, ",\"aux\":["); sb_addnum(out, ev->aux[0]); sb_add(out, ","); sb_addnum(out, ev->aux[1]); sb_add(out, "]");
            sb_add(out, ","); gs_add_kv_i(out, "inst150", ev->inst150);
        }
        if (haveScope && ev->entindex >= 0) {
            void *ent = gs_ent_by_index((u32)ev->entindex);
            float o[3]; int ho = ent ? gs_origin(ent, o) : 0;
            sb_add(out, ",");
            gs_add_kv_v3(out, "origin", o, ho); sb_add(out, ",");
            gs_add_kv_str(out, "entityName", ent ? gs_designer_name((u32)ev->entindex) : 0);
        }
        sb_add(out, "}");
        /* roll back an event that overflowed or left < 256 B for the tail (the tail is < 100 B) */
        if (out->ovf || out->len + 256 > out->cap) {
            out->len = mark; out->buf[mark] = 0; out->ovf = 0;
            break;
        }
    }
    /* requeue the unwritten tail at the FRONT (oldest first) so nothing is lost; if the ring is full, the oldest go.
       room/skip are computed under the lock: the game thread may push while we serialized. */
    u32 m = n - k, room, skip, pending;
    snd_lock();
    room = SND_RING - g_sndCount;
    skip = m > room ? m - room : 0;
    g_sndDropped += skip;
    for (u32 i = n; i-- > k + skip;) { g_sndHead = (g_sndHead + SND_RING - 1) % SND_RING; g_snd[g_sndHead] = batch[i]; g_sndCount++; }
    pending = g_sndCount;
    snd_unlock();
    sb_add(out, "],\"truncated\":"); sb_add(out, k < n ? "true" : "false");
    sb_add(out, ",\"dropped\":"); sb_addnum(out, dropped);
    sb_add(out, ",\"total\":"); sb_addnum(out, total);
    sb_add(out, ",\"pending\":"); sb_addnum(out, pending);
    sb_add(out, "}");
    return 1;
}

/* game.schemaProbe {names?:[...]} - diagnostics: which module names resolve to a non-global type scope.
   Default candidates cover the spellings a module might register under. */
typedef const char *(*TsGetScopeNameFn)(void *self);
static int op_game_schema_probe(const char *params, SB *out, OpErr *e) {
    void **ss = (void **)mod_iface(PIN_SCHEMA, S2_IFACE_SCHEMASYSTEM);
    if (!obj_is(ss, PIN_SCHEMA, SSV_SCHEMASYSTEM_VT)) { op_err(e, -32017, "PrerequisiteFailed", "SchemaSystem_001 is not the pinned CSchemaSystem"); return 0; }
    static const char *defaults[] = { "client.dll", "client", "engine2.dll", "server.dll", "schemasystem.dll", "tier0.dll", "host.dll", "csgo/bin/win64/client.dll", "Client.dll", 0 };
    static char name[128];
    sb_add(out, "{\"results\":[");
    int wrote = 0;
    const char *arr = params ? json_find(params, "names") : 0;
    const char *p = (arr && *arr == '[') ? arr + 1 : 0;
    for (int i = 0;; i++) {
        const char *n;
        if (p) {
            while (*p == ' ' || *p == ',') p++;
            if (*p != '"') break;
            p = jstr(p, name, sizeof name);
            if (!p) break;
            n = name;
        } else {
            n = defaults[i];
            if (!n) break;
        }
        void **scope = (void **)((SsFindScopeFn)(*(void ***)ss)[S2_SLOT_ISchemaSystem_FindTypeScopeForModule])(ss, n, 0);
        int isObj = obj_is(scope, PIN_SCHEMA, SSV_TYPESCOPE_VT);
        int isGlobal = isObj ? ((TsIsGlobalFn)(*(void ***)scope)[S2_SLOT_ISchemaSystemTypeScope_IsGlobalScope])(scope) : -1;
        const char *sn = isObj ? ((TsGetScopeNameFn)(*(void ***)scope)[S2_SLOT_ISchemaSystemTypeScope_GetScopeName])(scope) : 0;
        if (wrote++) sb_add(out, ",");
        sb_add(out, "{"); gs_add_kv_str(out, "name", n); sb_add(out, ",");
        gs_add_kv_b(out, "found", isObj && !isGlobal); sb_add(out, ",");
        gs_add_kv_str(out, "scopeName", (sn && mem_ok(sn)) ? sn : 0); sb_add(out, ",");
        gs_add_kv_i(out, "isGlobal", isGlobal); sb_add(out, "}");
    }
    sb_add(out, "]}");
    return 1;
}

static int op_game_state(const char *method, const char *params, SB *out, OpErr *e) {
    if (seq(method, "game.schemaProbe")) return op_game_schema_probe(params, out, e);
    if (seq(method, "game.local")) return op_game_local(params, out, e);
    if (seq(method, "game.weapons")) return op_game_weapons(params, out, e);
    if (seq(method, "game.radar")) return op_game_radar(params, out, e);
    if (seq(method, "game.rules")) return op_game_rules(params, out, e);
    if (seq(method, "game.entities")) return op_game_entities(params, out, e);
    if (seq(method, "game.selectWeapon")) return op_game_select_weapon(params, out, e);
    op_err(e, -32601, "MethodNotFound", method);
    return 0;
}
