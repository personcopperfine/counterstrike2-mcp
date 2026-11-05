/* registry_gen.c - the differential generator for the Rust port of the bridge's method
 * registry (WP2.C): the method table (k_methods[]), the verification gate (verified_mask,
 * mod_state_name, exec_request's first-unmet-need order) and the pure reply builders
 * (build_caps, build_error/_ext, and the reply-building halves of the ipc_thread control
 * arms).
 *
 * crates/cs2-bridge/tests/{registry.rs,control.rs} do NOT compile this file as-is: they lift
 * the code under test verbatim out of the frozen bridge/mcp_bridge.c (by stable text markers,
 * failing loudly when a marker is absent) and inject it at the @...@ markers below, then
 * compile the result with cc. Everything around the injected regions is HARNESS - inputs and
 * no-op stubs for the socket/queue environment (out_push, send_all, logline, the critical
 * sections, GetTickCount64, q_depth); none of it re-implements anything under test.
 *
 * Output: one TAB-separated record per line, byte strings as lowercase hex ('-' = empty):
 *   METHOD  <name> <needs> <impl> <hidden> <blockedOn|->   - one k_methods[] row
 *   METHODCOUNT <n>                                        - METHOD_COUNT
 *   SCENARIO <name>                                        - module states + revision set
 *   CAPS       <hex>      - control.capabilities reply   (id "caps-id-1")
 *   HELLO      <hex>      - control.hello ok reply       (id "hello-id-1" / none: "noid")
 *   PING  <v>  <hex>      - control.ping reply, variants A/B/noid
 *   STATUS <v> <hex>      - control.status reply, variants A/B
 *   VERSION    <hex>      - control.version reply
 *   IPCGATE <m> <hex>     - the ipc_thread gate arms for method <m> (unknown/-32011/needs==0)
 *   EXECGATE <m> <hex>    - exec_request's head through the -32017 block ('-' = fell through:
 *                           the gate passed, dispatch begins - the Rust side must see Run)
 *
 * Build & run (from the repo root, after the test injected the regions):
 *   cc -std=c11 -O1 -Wall -Wextra -I bridge -o registry_gen <assembled>.c
 *   ./registry_gen
 */
#include <stdio.h>
#include <string.h>
#include <stddef.h>

typedef unsigned char u8;
typedef unsigned short u16;
typedef unsigned int u32;
typedef unsigned long long u64;

typedef int SOCKET;
#define INVALID_SOCKET (-1)

#include "bridge_pins.h"   /* -I bridge: the PIN_* indices, PIN_MODULE_COUNT, k_pinModules */

/*@BRIDGE_DEFINES@*/

/* ---- harness environment: the runtime state the extracted arms read (inputs) ---- */
static volatile long g_lastTick;      /* mcp_bridge.c:1015 (slot 13) */
static volatile u64 g_inflightStart;  /* mcp_bridge.c:1016 */
static volatile int g_shuttingDown;   /* mcp_bridge.c:1018 */
static u64 g_startTick;               /* mcp_bridge.c:1024 */
static u64 now;                       /* the reader loop's GetTickCount64() */
static u64 g_qdepth;                  /* q_depth()'s answer (a harness input) */
static SOCKET g_listener = INVALID_SOCKET;
static SOCKET client, g_authedClient;   /* the C's ipc_thread locals, named as there */
static u32 g_clientGen, gen;
static int hasId;
static int authed;
static char idbuf[128];
static char method[64];
static char resp[MAX_LINE];

typedef struct { char id[128]; char method[64]; u64 receivedAt; } Req;
static Req g_q[QCAP];
static int g_qCount, g_qHead;
static SOCKET g_qcs;

#define EnterCriticalSection(x) ((void)0)
#define LeaveCriticalSection(x) ((void)0)
#define GetTickCount64() (now)
#define out_push(r, g) ((void)0)
#define send_all(...) ((void)0)
#define logline(...) ((void)0)
static int q_depth(void) { return (int)g_qdepth; }

/*@SEQ@*/

/*@ENTITY_REGION@*/

/*@REGISTRY_REGION@*/

/*@BUILD_CAPS@*/

/*@ERROR_BUILDERS@*/

/* ---- the extracted arms, each wrapped in a harness function ---- */

static void arm_ping(void) {
    SB b; sb_init(&b, resp, MAX_LINE);
/*@ARM_PING@*/
}

static void arm_status(void) {
    SB b; sb_init(&b, resp, MAX_LINE);
/*@ARM_STATUS@*/
}

static void arm_caps(void) {
    SB b; sb_init(&b, resp, MAX_LINE);
/*@ARM_CAPS@*/
}

static void arm_version(void) {
    SB b; sb_init(&b, resp, MAX_LINE);
/*@ARM_VERSION@*/
}

static void arm_hello(void) {
/*@ARM_HELLO@*/
}

static void arm_ipcgate(void) {
    SB b; sb_init(&b, resp, MAX_LINE);
/*@ARM_IPCGATE@*/
}

/* the extracted exec_request head is a complete function definition (the dispatch after the
   gate is excluded and the closing brace is appended by the test) */
/*@ARM_EXEC@*/

/* ---- the driver: scenarios over the extracted code ---- */

static void phexresp(void) {
    if (!resp[0]) { fputs("-", stdout); return; }
    for (const u8 *p = (const u8 *)resp; *p; p++) printf("%02x", *p);
}

static void dump(const char *tag, const char *name) {
    printf("%s\t%s\t", tag, name ? name : "-");
    phexresp();
    printf("\n");
    resp[0] = 0;
}

static void set_module(int i, int state) {
    g_modState[i] = state;
    g_modBase[i] = (state == 2) ? (void *)1 : (void *)0;
}

static void run_scenario(const char *name, int rev, const int *states) {
    for (int i = 0; i < PIN_MODULE_COUNT; i++) set_module(i, states[i]);
    g_capsRevision = rev;
    printf("SCENARIO\t%s\n", name);

    strcpy(idbuf, "caps-id-1");
    arm_caps(); dump("CAPS", 0);

    strcpy(idbuf, "hello-id-1"); hasId = 1;
    arm_hello(); dump("HELLO", 0);
    hasId = 0; idbuf[0] = 0;
    arm_hello(); dump("HELLO", "noid");

    strcpy(idbuf, "ping-id-1");
    now = 10000; g_startTick = 9000; g_lastTick = 9950;
    g_inflightStart = 0; g_qdepth = 0; g_shuttingDown = 0;
    arm_ping(); dump("PING", "A");
    now = 20000; g_startTick = 10000; g_lastTick = 20500;
    g_inflightStart = 19000; g_qdepth = 3; g_shuttingDown = 0;
    arm_ping(); dump("PING", "B");
    idbuf[0] = 0;   /* a notification-shaped request: idbuf is "", the reply writes "" */
    now = 500; g_startTick = 500; g_lastTick = 0;
    g_inflightStart = 240; g_qdepth = 1; g_shuttingDown = 0;
    arm_ping(); dump("PING", "noid");

    strcpy(idbuf, "status-id-1");
    now = 20000; g_listener = 7; g_inflightStart = 0; g_shuttingDown = 0;
    g_qCount = 2; g_qHead = 0;
    strcpy(g_q[0].id, "id-1"); strcpy(g_q[0].method, "doc.active"); g_q[0].receivedAt = 19000;
    strcpy(g_q[1].id, "id-2"); strcpy(g_q[1].method, "pet.apply"); g_q[1].receivedAt = 19999;
    arm_status(); dump("STATUS", "A");
    g_qCount = 0; g_listener = INVALID_SOCKET; g_inflightStart = 42; g_shuttingDown = 1;
    arm_status(); dump("STATUS", "B");

    strcpy(idbuf, "version-id-1");
    arm_version(); dump("VERSION", 0);

    /* the ipc_thread gate arms: only the outcomes that arm itself answers - an unknown name
       (-32601, control.* or not) and a documented-but-blocked row (-32011). A needs==0
       implemented row (control.*) never reaches the arm in the C (the fast path replies
       first), and a needs!=0 verified-satisfied row is queued, not answered here. */
    static const char *const ipc[] = {
        "control.bogus", "no.such.method", "control.ping",
        "doc.list", "map.run", "job.status", "job.cancel", "entity.delete",
        "selection.set", "selection.get", "file.open_asset", "mesh.get", "material.assign",
        "mesh.setFaceRect", "mesh.moveVertices",
    };
    strcpy(idbuf, "gate-id-1");
    for (u32 i = 0; i < sizeof ipc / sizeof ipc[0]; i++) {
        strcpy(method, ipc[i]);
        arm_ipcgate(); dump("IPCGATE", ipc[i]);
    }

    /* exec_request's head: the -32017 PrerequisiteFailed path (the mask check runs on the
       main thread). '-' = the gate passed and the head fell through to dispatch (Run). */
    static const char *const exec[] = {
        "doc.active", "view.render", "world.render", "input.set", "input.status",
        "game.local", "game.weapons", "game.selectWeapon", "test.checkpoint", "console.exec",
        "game.tick", "game.saveRestore", "sound.status", "modeldoc.render",
        "modeldoc.compile", "pet.load", "pet.active", "pet.apply", "pet.schema",
        "node.list", "entity.create", "map.open", "control.bogus", "doc.list",
    };
    for (u32 i = 0; i < sizeof exec / sizeof exec[0]; i++) {
        Req rq;
        strcpy(rq.id, "exec-id-1");
        strcpy(rq.method, exec[i]);
        rq.receivedAt = 0;
        exec_request(&rq, resp, MAX_LINE);
        dump("EXECGATE", exec[i]);
    }
}

int main(void) {
    /* the method table, row for row (scenario-independent) */
    for (u32 i = 0; i < METHOD_COUNT; i++) {
        const Method *m = &k_methods[i];
        printf("METHOD\t%s\t%u\t%d\t%d\t%s\n", m->name, m->needs, m->impl, m->hidden,
               m->blockedOn ? m->blockedOn : "-");
    }
    printf("METHODCOUNT\t%u\n", (u32)METHOD_COUNT);

    int not_loaded[PIN_MODULE_COUNT]; for (int i = 0; i < PIN_MODULE_COUNT; i++) not_loaded[i] = 0;
    int verified[PIN_MODULE_COUNT];   for (int i = 0; i < PIN_MODULE_COUNT; i++) verified[i] = 2;
    /* pin order: hammer, tf2, scenesystem, engine2, rsv, client, server, tier0, schema,
       sound, modeldoc, pet - hammer mismatched, toolframework2 verifying, pet/sound absent */
    int mixed[PIN_MODULE_COUNT] = { 3, 1, 2, 2, 2, 2, 2, 2, 2, 0, 2, 0 };

    run_scenario("not_loaded", 1, not_loaded);
    run_scenario("verified", 5, verified);
    run_scenario("mixed", 7, mixed);
    run_scenario("rev2", 2, not_loaded);
    return 0;
}
