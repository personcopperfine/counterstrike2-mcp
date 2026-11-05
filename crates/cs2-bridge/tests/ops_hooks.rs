//! The M4 HOOKS lane's host tests (no game): the input/sound/reply builders byte-for-byte
//! against the C's `sb_add` sequences, the tap/edge and ring/wrap/drop logic, the
//! param-validation paths, and the mandatory negative gate tests. The hooks themselves (the
//! KHook registrations and the callbacks) are unit-tested in their own modules over a mock
//! `KHookApi`; this binary tests the bodies around them and the registry gate.
//!
//! Tests that touch the lanes' global statics serialize on [`TEST_SER`]; the crate runs its
//! tests on several threads (`--test-threads=4`).

use core::ffi::c_void;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use cs2_bridge::control;
use cs2_bridge::dispatch;
use cs2_bridge::entity_json::Sb;
use cs2_bridge::modules::ModuleViews;
use cs2_bridge::ops;
use cs2_bridge::queue::Request;
use cs2_bridge::registry::{self, ModState};
use cs2_bridge::wiring::BridgeState;

fn test_ser() -> MutexGuard<'static, ()> {
    static TEST_SER: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_SER
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A module-state source a test constructs: every module in one state, revision fixed.
struct FakeModules {
    state: ModState,
    revision: u64,
}

impl ModuleViews for FakeModules {
    fn state(&self, _module: u8) -> ModState {
        self.state
    }

    fn is_verified(&self, _module: u8) -> bool {
        self.state == ModState::Verified
    }

    fn revision(&self) -> u64 {
        self.revision
    }
}

/// A request for the handler under test.
fn request(method: &str, params: Option<&str>) -> Request {
    Request {
        id: "7".to_string(),
        method: method.to_string(),
        params: params.map(str::to_string),
        received: std::time::Instant::now(),
        deadline: None,
        gen: 0,
    }
}

fn handler(table: &[(&str, ops::Handler)], method: &str) -> ops::Handler {
    let (_, h) = table
        .iter()
        .find(|(name, _)| *name == method)
        .unwrap_or_else(|| panic!("{method} is not in the lane's HANDLERS"));
    *h
}

// ---------------------------------------------------------------------------------------
// Input lane: a fake CCSGOInput - vptr(8) + pad(8), then the 64-slot button table at the
// generated m_pButtons offset (16) - with fake buttons (mask at 0, name pointer at 8).
// ---------------------------------------------------------------------------------------

/// A fake button: `[u8; 16]` = the mask (u64, offset 0) + the name pointer (offset 8).
struct FakeBtn(Box<[u8; 16]>);

/// A fake name: NUL-terminated, boxed (a stable address, like the engine's static strings).
/// Kept alive only so the pointers in the button blobs stay valid.
#[allow(dead_code)]
struct FakeName(Box<[u8]>);

/// The fake object: `[u8; 16 + 64*8]` = vptr + pad + the button table.
/// `2`/`3` keep the name boxes and the vptr target alive for the pointers.
#[allow(dead_code)]
struct FakeInputObj(
    Box<[u8; 16 + 64 * 8]>,
    Vec<FakeBtn>,
    Vec<FakeName>,
    Box<[*const c_void; 8]>,
);

/// The recording apply's log: `(iv address, delta as i32 bits)` per call.
static APPLY_LOG: Mutex<Vec<(isize, i32)>> = Mutex::new(Vec::new());

impl FakeInputObj {
    /// `buttons` maps bit -> Some(name): a button with mask `1 << bit` and that name.
    fn new(buttons: &[(&str, u32)]) -> Self {
        let mut obj = Box::new([0u8; 16 + 64 * 8]);
        let vt = Box::new([core::ptr::null::<c_void>(); 8]); // the vptr target (unread here)
        obj[0..8].copy_from_slice(&(vt.as_ptr() as u64).to_ne_bytes());
        let mut btns = Vec::new();
        let mut names = Vec::new();
        let table_at = 16;
        for (name, bit) in buttons {
            let name_box: Box<[u8]> = format!("{name}\0").into_bytes().into_boxed_slice();
            let mut btn = Box::new([0u8; 16]);
            btn[0..8].copy_from_slice(&(1u64 << bit).to_ne_bytes()); // mask exactly 1 << i
            btn[8..16].copy_from_slice(&(name_box.as_ptr() as u64).to_ne_bytes());
            let idx = table_at + *bit as usize * 8;
            obj[idx..idx + 8]
                .copy_from_slice(&core::ptr::from_ref(&*btn).addr().to_ne_bytes());
            btns.push(FakeBtn(btn));
            names.push(FakeName(name_box));
        }
        Self(obj, btns, names, vt)
    }

    fn obj_ptr(&self) -> *const c_void {
        self.0.as_ptr().cast()
    }

    /// The button's CInputValue address (`btn + m_InputValue`), what the C's hook passes.
    fn btn_input_value_addr(&self, bit: u32) -> *const u8 {
        let btn = self.1[bit as usize].0.as_ptr();
        btn.wrapping_add(8) // offset_of!(ClientInputButton_t, m_InputValue)
    }
}

/// `in_status_json` (`mcp_bridge.c:4898-4928`) uninstalled: the C's exact bytes with every
/// counter at 0 and no table (`g_inObj` null).
#[test]
fn input_status_json_uninstalled_matches_the_c_bytes() {
    let mut buf = vec![0u8; 2048];
    let mut sb = Sb::new(&mut buf);
    let read = ops::input::InStatusRead {
        installed: false,
        want: 0,
        applied: 0,
        angles_on: false,
        angles: [0.0; 3],
        frames: 0,
        edges: 0,
        angle_ticks: 0,
        conflicts: 0,
    };
    ops::input::in_status_json_in(&read, core::ptr::null(), &mut sb);
    // The C's sb_add sequence, `mcp_bridge.c:4898-4927` with t = 0 (both loops skipped).
    assert_eq!(
        core::str::from_utf8(sb.as_cstr()).unwrap(),
        r#"{"installed":false,"held":[],"angles":null,"frames":0,"edges":0,"angleTicks":0,"buttons":[],"conflicts":0}"#
    );
}

/// `in_status_json` over a fake table: held names, the angles object, and the button list
/// (`mcp_bridge.c:4900-4927`).
#[test]
fn input_status_json_matches_the_c_bytes_over_a_fake_table() {
    let fake = FakeInputObj::new(&[("attack", 0), ("attack2", 1), ("duck", 3)]);
    let read = ops::input::InStatusRead {
        installed: true,
        want: 1u64 | 1u64 << 3, // attack + duck held
        applied: 1u64,          // attack's +1 owned
        angles_on: true,
        angles: [10.0, 20.5, 30.25],
        frames: 12,
        edges: 3,
        angle_ticks: 5,
        conflicts: 2,
    };
    let mut buf = vec![0u8; 2048];
    let mut sb = Sb::new(&mut buf);
    ops::input::in_status_json_in(&read, fake.obj_ptr(), &mut sb);
    // The C's order: held first (in want|applied order, `:4903-4909`), then the angles
    // through sb_addf (`:4912-4914`; three decimals when the fraction is non-zero), then
    // the counters, then every named button (`:4920`).
    assert_eq!(
        core::str::from_utf8(sb.as_cstr()).unwrap(),
        concat!(
            r#"{"installed":true,"held":["attack","duck"],"angles":[10,20.500,30.250],"#,
            r#""frames":12,"edges":3,"angleTicks":5,"#,
            r#""buttons":["attack","attack2","duck"],"conflicts":2}"#
        ),
        "mcp_bridge.c:4898-4928's bytes"
    );
}

/// `input.set`'s param errors (`mcp_bridge.c:4950`, `:4960-4964`) over a fake table.
#[test]
fn input_set_param_errors_match_the_c() {
    let _s = test_ser();
    let fake = FakeInputObj::new(&[("attack", 0)]);
    let obj = fake.obj_ptr();
    // A name not in the table (`:4961-4963`).
    let err = ops::input::input_set_installed(Some(br#"{"press":["attack","shoot"]}"#), obj)
        .unwrap_err();
    assert_eq!(err.code, -32602);
    assert_eq!(err.name, "InvalidParams");
    assert_eq!(err.data, r#"{"reason":"unknown button or bad list: shoot"}"#);
    // A non-array value (`:4875`): the C's bad[0] == 0 -> "?".
    let err = ops::input::input_set_installed(Some(br#"{"press":"attack"}"#), obj).unwrap_err();
    assert_eq!(err.data, r#"{"reason":"unknown button or bad list: ?"}"#);
    // A truncated params whose key has no value: `json_find` answers `start == len` (the C
    // reads the NUL there, `:4875`) -> "?" - a validation error, not a panic
    // (`mcp_bridge.c:4870-4893`).
    let err = ops::input::input_set_installed(Some(br#"{"press":"#), obj).unwrap_err();
    assert_eq!(err.code, -32602);
    assert_eq!(err.name, "InvalidParams");
    assert_eq!(err.data, r#"{"reason":"unknown button or bad list: ?"}"#);
    // The same with trailing spaces after the colon (json_find skips them; no value follows).
    let err = ops::input::input_set_installed(Some(br#"{"press":   "#), obj).unwrap_err();
    assert_eq!(err.code, -32602);
    assert_eq!(err.data, r#"{"reason":"unknown button or bad list: ?"}"#);
    // A malformed `angles` (`:4950`).
    let err = ops::input::input_set_installed(Some(br#"{"angles":true}"#), obj).unwrap_err();
    assert_eq!(err.data, r#"{"reason":"angles must be [pitch,yaw,roll] or null"}"#);
    // A failed list rolls `g_inWant` back (`:4956`).
    {
        let mut st = ops::input::lock_state();
        st.want = 0;
        st.applied = 0;
        st.tap = [0; 64];
        st.regap = 0;
    }
    assert!(ops::input::input_set_installed(Some(br#"{"press":["attack"],"release":["nope"]}"#), obj).is_err());
    assert_eq!(ops::input::lock_state().want, 0, "the press was rolled back");
}

/// Press / release / tap through the frame-begin body with a recording apply
/// (`mcp_bridge.c:4804-4823`, `:4894-4896`): the apply target is the button's CInputValue
/// (`btn + m_InputValue`, the C's `:4816` arithmetic) and the deltas follow the held mask.
#[test]
fn input_press_release_tap_edge_logic() {
    let _s = test_ser();
    let fake = FakeInputObj::new(&[("attack", 0)]);
    let obj = fake.obj_ptr();
    {
        let mut st = ops::input::lock_state();
        st.want = 0;
        st.applied = 0;
        st.tap = [0; 64];
        st.regap = 0;
    }
    ops::input::IN_EDGES.store(0, core::sync::atomic::Ordering::Relaxed);
    APPLY_LOG.lock().unwrap().clear();
    // The expected iv: btn + m_InputValue, the C's argument (`:4816`).
    let expect_iv = fake.btn_input_value_addr(0) as isize;

    unsafe extern "C" fn record(iv: *mut c_void, slot: i32, delta: f32, frac: f32) {
        let _ = slot;
        let _ = frac;
        APPLY_LOG.lock().unwrap().push((iv as isize, delta as i32));
    }
    let apply: ops::input::InApplyFn = record;

    // press:["attack"] -> the first frame applies +1.0 to the button's CInputValue.
    ops::input::input_set_installed(Some(br#"{"press":["attack"]}"#), obj).unwrap();
    ops::input::frame_begin_locked(&mut ops::input::lock_state(), obj, apply);
    assert_eq!(
        ops::input::IN_EDGES.load(core::sync::atomic::Ordering::Relaxed),
        1
    );
    assert_eq!(*APPLY_LOG.lock().unwrap(), vec![(expect_iv, 1)]);
    // ... and the second frame does nothing (the bit is already applied).
    ops::input::frame_begin_locked(&mut ops::input::lock_state(), obj, apply);
    assert_eq!(
        ops::input::IN_EDGES.load(core::sync::atomic::Ordering::Relaxed),
        1
    );
    assert_eq!(APPLY_LOG.lock().unwrap().len(), 1);

    // release:["attack"] -> the next frame applies -1.0.
    ops::input::input_set_installed(Some(br#"{"release":["attack"]}"#), obj).unwrap();
    ops::input::frame_begin_locked(&mut ops::input::lock_state(), obj, apply);
    assert_eq!(
        ops::input::IN_EDGES.load(core::sync::atomic::Ordering::Relaxed),
        2
    );
    assert_eq!(*APPLY_LOG.lock().unwrap(), vec![(expect_iv, 1), (expect_iv, -1)]);

    // tap:["attack"], frames:2 -> +1 on the tap's frame, held while the tap counts down,
    // -1 on the frame after the last tap frame (the C's fold, `:4809`).
    ops::input::input_set_installed(Some(br#"{"tap":["attack"],"frames":2}"#), obj).unwrap();
    ops::input::frame_begin_locked(&mut ops::input::lock_state(), obj, apply); // +1
    ops::input::frame_begin_locked(&mut ops::input::lock_state(), obj, apply); // still held
    let edges = ops::input::IN_EDGES.load(core::sync::atomic::Ordering::Relaxed);
    ops::input::frame_begin_locked(&mut ops::input::lock_state(), obj, apply); // -1
    assert_eq!(
        ops::input::IN_EDGES.load(core::sync::atomic::Ordering::Relaxed),
        edges + 1,
        "the tap's release"
    );
    let deltas: Vec<i32> = APPLY_LOG.lock().unwrap().iter().map(|(_, d)| *d).collect();
    assert_eq!(&deltas[2..], &[1, -1], "the tap: press frame, release after the last held frame");
    // The state is clean again.
    {
        let st = ops::input::lock_state();
        assert_eq!(st.want, 0);
        assert_eq!(st.applied, 0);
        assert_eq!(st.tap, [0; 64]);
    }
}

/// WP1.6 (ti_convoy FIXME 26): `release` and `tap` of the SAME held button in one `input.set`
/// must release it for a frame and then tap it. The apply deltas are per changed bit, so a
/// held button that is released and re-tapped inside one call never changed: no edge, no shot.
#[test]
fn input_release_and_tap_of_a_held_button_in_one_call_re_presses_it() {
    let _s = test_ser();
    let fake = FakeInputObj::new(&[("attack", 0), ("jump", 1)]);
    let obj = fake.obj_ptr();
    {
        let mut st = ops::input::lock_state();
        st.want = 0;
        st.applied = 0;
        st.tap = [0; 64];
        st.regap = 0;
    }
    APPLY_LOG.lock().unwrap().clear();
    unsafe extern "C" fn record(iv: *mut c_void, _slot: i32, delta: f32, _frac: f32) {
        APPLY_LOG.lock().unwrap().push((iv as isize, delta as i32));
    }
    let apply: ops::input::InApplyFn = record;
    let frame = || ops::input::frame_begin_locked(&mut ops::input::lock_state(), obj, apply);
    let deltas = || -> Vec<i32> { APPLY_LOG.lock().unwrap().iter().map(|(_, d)| *d).collect() };

    ops::input::input_set_installed(Some(br#"{"press":["attack"]}"#), obj).unwrap();
    frame();
    assert_eq!(deltas(), vec![1]);
    // release + tap of attack (and an unrelated jump tap) in one call
    ops::input::input_set_installed(
        Some(br#"{"release":["attack"],"tap":["attack","jump"],"frames":2}"#),
        obj,
    )
    .unwrap();
    frame(); // attack goes up (the gap frame), jump goes down
    assert_eq!(deltas(), vec![1, -1, 1], "attack released, jump pressed");
    frame(); // attack goes down again: a real second press
    assert_eq!(deltas(), vec![1, -1, 1, 1]);
    frame(); // both held (2 frames each)
    frame(); // both released at the end of the tap
    assert_eq!(deltas().len(), 6);
    let st = ops::input::lock_state();
    assert_eq!((st.want, st.applied, st.tap), (0, 0, [0; 64]), "nothing left held");
}

/// `input.clear` resets want / taps / angles (`mcp_bridge.c:4966-4971`).
///
/// The reply carries the live `edges`/`angleTicks` counters, which `input.clear` does not reset
/// (the C keeps them cumulative); the other cases of this binary increment them, so they are
/// zeroed here for the exact bytes - the same reset [`input_uninstall_noop_answers_the_status_object`]
/// uses. (Before that, the expected zeros only held when this case ran before them, which the
/// test order does not guarantee.)
#[test]
fn input_clear_resets() {
    let _s = test_ser();
    // `input.clear` is the C's: it resets want/taps/angles and never the counters, so pin the
    // three the reply carries (another case in this binary increments them - that ordering is
    // what made this case flake at `--test-threads=4`).
    use core::sync::atomic::Ordering;
    ops::game::IN_FRAMES.store(0, Ordering::Relaxed);
    ops::input::IN_EDGES.store(0, Ordering::Relaxed);
    ops::input::IN_ANGLE_TICKS.store(0, Ordering::Relaxed);
    {
        let mut st = ops::input::lock_state();
        st.want = 0xFF;
        st.applied = 0xFF;
        st.tap[5] = 7;
    }
    ops::game::IN_ANGLES_ON.store(true, Ordering::Relaxed);
    let mut buf = vec![0u8; 2048];
    let mut sb = Sb::new(&mut buf);
    ops::input::op_input_installed(b"input.clear", Some(b"{}"), core::ptr::null(), &mut sb)
        .unwrap();
    let st = ops::input::lock_state();
    assert_eq!(st.want, 0);
    assert_eq!(st.applied, 0xFF, "clear does not drop the applied deltas (the C's)");
    assert_eq!(st.tap, [0; 64]);
    drop(st);
    assert!(!ops::game::IN_ANGLES_ON.load(Ordering::Relaxed));
    // The reply is the status object (`:4973`).
    assert_eq!(
        core::str::from_utf8(sb.as_cstr()).unwrap(),
        r#"{"installed":false,"held":[],"angles":null,"frames":0,"edges":0,"angleTicks":0,"buttons":[],"conflicts":0}"#
    );
}

/// `input.uninstall` on a not-installed hook: the arm answers the status object without
/// touching any module. The body is called through the handler table (an off-ramp row, so it
/// needs no KHook - `off_ramps_never_need_khook`). The counters are the live statics, reset
/// here for the exact bytes.
#[test]
fn input_uninstall_noop_answers_the_status_object() {
    let _s = test_ser();
    {
        let mut st = ops::input::lock_state();
        st.want = 0;
        st.applied = 0;
        st.tap = [0; 64];
        st.regap = 0;
    }
    ops::game::IN_ANGLES_ON.store(false, core::sync::atomic::Ordering::Relaxed);
    ops::game::IN_FRAMES.store(0, core::sync::atomic::Ordering::Relaxed);
    ops::input::IN_EDGES.store(0, core::sync::atomic::Ordering::Relaxed);
    ops::input::IN_ANGLE_TICKS.store(0, core::sync::atomic::Ordering::Relaxed);
    let got = handler(ops::input::HANDLERS, "input.uninstall")(&request(
        "input.uninstall",
        Some("{}"),
    ));
    let want = control::result_ok(
        Some("7"),
        r#"{"installed":false,"held":[],"angles":null,"frames":0,"edges":0,"angleTicks":0,"buttons":[],"conflicts":0}"#,
    );
    assert_eq!(got, want);
}

// ---------------------------------------------------------------------------------------
// Sound lane.
// ---------------------------------------------------------------------------------------

/// `snd_status_json` (`mcp_bridge_game.inc:1094-1102`) in all four shapes.
#[test]
fn sound_status_json_matches_the_c_bytes() {
    for (installed, listener_on, mode) in [
        (false, false, "null"),
        (true, false, "\"wrap\""),
        (false, true, "\"listener\""),
        (true, true, "\"both\""),
    ] {
        let mut buf = vec![0u8; 256];
        let mut sb = Sb::new(&mut buf);
        let read = ops::sound::SndStatusRead {
            installed,
            listener_on,
            pending: 2,
            total: 300,
            dropped: 44,
            blocked_on: false,
        };
        ops::sound::snd_status_json_in(&read, &mut sb);
        assert_eq!(
            core::str::from_utf8(sb.as_cstr()).unwrap(),
            format!(
                r#"{{"installed":{},"mode":{},"pending":2,"total":300,"dropped":44,"ringSize":256}}"#,
                if installed || listener_on { "true" } else { "false" },
                mode
            ),
            "mcp_bridge_game.inc:1094-1102's bytes"
        );
    }
}

/// The ring's wrap/drop counters (`mcp_bridge_game.inc:936-944`): the oldest event is
/// dropped once the 256-entry ring is full.
#[test]
fn sound_ring_wraps_and_counts_drops() {
    let _s = test_ser();
    {
        let mut ring = ops::sound::ring_lock();
        *ring = ops::sound::Ring {
            slots: Vec::new(),
            head: 0,
            count: 0,
            seq: 0,
            dropped: 0,
            total: 0,
        };
    }
    for i in 0..300u32 {
        ops::sound::snd_record_for_test(12, i, c"boom".as_ptr().cast(), -1, i, 0);
    }
    let ring = ops::sound::ring_lock();
    assert_eq!(ring.count, 256, "the ring is full");
    assert_eq!(ring.total, 300);
    assert_eq!(ring.dropped, 44, "the oldest 44 were dropped");
    assert_eq!(ring.seq, 300);
    assert_eq!(ring.slots[ring.head as usize].hash, 44, "the oldest kept event");
    drop(ring);
    // And a drain clears the dropped count (`:1129`).
    let mut buf = vec![0u8; 64];
    let mut sb = Sb::new(&mut buf);
    ops::sound::snd_drain_for_test(Some(br#"{"max":1}"#), &mut sb).unwrap();
    assert_eq!(ops::sound::ring_lock().dropped, 0, "the drain read-and-cleared it");
}

/// One event's JSON (`mcp_bridge_game.inc:1141-1182`) through [`ops::sound::
/// snd_event_json`], byte-for-byte against the C's sb_add sequence.
#[test]
fn sound_event_json_matches_the_c_bytes() {
    let mut buf = vec![0u8; 4096];
    // A slot-11 (with-params) event: name empty (unresolved) -> null name, params copied.
    let ev = ops::sound::SndEvt {
        seq: 7,
        hash: 0x1234_5678,
        entindex: 3,
        guid: 42,
        slot: ops::sound::START_WITH_PARAMS_SLOT, // the with-params start's slot (11 MSVC)
        layer: 0xFE,
        ms: 1000,
        plen: 12,
        pcopied: 3,
        praw: {
            let mut praw = [0u8; 128];
            praw[..3].copy_from_slice(&[0xab, 0xcd, 0xef]);
            praw
        },
        ..Default::default()
    };
    {
        let mut sb = Sb::new(&mut buf);
        ops::sound::snd_event_json(&ev, 500, None, &mut sb);
        let want = format!(
            concat!(
                r#"{{"seq":7,"hash":305419896,"name":null,"entindex":3,"guid":42,"via":{},"#,
                r#""flags":254,"ageMs":500,"params":{{"len":12,"hex":"abcdef","truncated":true}}}}"#
            ),
            ops::sound::START_WITH_PARAMS_SLOT
        );
        assert_eq!(
            core::str::from_utf8(sb.as_cstr()).unwrap(),
            want,
            "mcp_bridge_game.inc:1141-1182's bytes (truncated: 3 of 12 copied)"
        );
    }

    // A slot-81 (listener) event: the aux guids + inst150, with a resolved entity block.
    let ev = ops::sound::SndEvt {
        seq: 8,
        entindex: -1,
        guid: 99,
        slot: 81,
        inst150: 17,
        aux: [1000, 2000],
        ..Default::default()
    };
    {
        let mut sb = Sb::new(&mut buf);
        ops::sound::snd_event_json(
            &ev,
            250,
            Some(ops::sound::EventScope {
                origin: Some([1.5, 2.5, 3.5]),
                entity_name: Some(b"prop_physics".to_vec()),
            }),
            &mut sb,
        );
        assert_eq!(
            core::str::from_utf8(sb.as_cstr()).unwrap(),
            concat!(
                r#"{"seq":8,"hash":0,"name":null,"entindex":-1,"guid":99,"via":81,"flags":0,"#,
                r#""ageMs":250,"aux":[1000,2000],"inst150":17,"origin":[1.500,2.500,3.500],"#,
                r#""entityName":"prop_physics"}"#
            ),
            "the listener event with the optional scope block"
        );
    }

    // A by-name event with an empty-name gate and no scope.
    let mut ev = ops::sound::SndEvt {
        seq: 9,
        entindex: 5,
        slot: 12,
        ..Default::default()
    };
    ev.name[..4].copy_from_slice(b"boom");
    {
        let mut sb = Sb::new(&mut buf);
        ops::sound::snd_event_json(&ev, 10, None, &mut sb);
        assert_eq!(
            core::str::from_utf8(sb.as_cstr()).unwrap(),
            r#"{"seq":9,"hash":0,"name":"boom","entindex":5,"guid":0,"via":12,"flags":0,"ageMs":10}"#
        );
    }
}

/// `sound.drain`'s flow over the real body (the scope fails on the host and is treated as
/// optional, exactly as the C treats it, `mcp_bridge_game.inc:1131-1132`); the max clamp
/// (`:1123-1124`).
#[test]
fn sound_drain_flow_and_max_clamp() {
    let _s = test_ser();
    let mut buf = vec![0u8; 98304];
    let mut sb = Sb::new(&mut buf);
    {
        let mut ring = ops::sound::ring_lock();
        *ring = ops::sound::Ring {
            slots: Vec::new(),
            head: 0,
            count: 0,
            seq: 0,
            dropped: 0,
            total: 0,
        };
    }
    for i in 0..3u32 {
        ops::sound::snd_record_for_test(13, i, c"explode".as_ptr().cast(), -1, 10 + i, 3);
    }
    ops::sound::snd_drain_for_test(Some(br#"{"max":0}"#), &mut sb).unwrap(); // clamps to 1
    let text = core::str::from_utf8(sb.as_cstr()).unwrap();
    assert!(
        text.starts_with(concat!(
            r#"{"events":[{"seq":1,"hash":0,"name":"explode","entindex":-1,"guid":10,"#,
            r#""via":13,"flags":3,"ageMs":"#
        )),
        "{text}"
    );
    assert!(
        text.ends_with(r#","truncated":false,"dropped":0,"total":3,"pending":2}"#),
        "{text}"
    );
    // max:0 consumed ONE event (the clamp); two are pending.
    assert_eq!(ops::sound::ring_lock().count, 2, "{text}");
}

/// The near-full rollback (`mcp_bridge_game.inc:1183-1187`): an event that would leave
/// < 256 B for the tail is rolled back whole and requeued; `truncated` is true.
#[test]
fn sound_drain_rolls_back_near_full() {
    let _s = test_ser();
    let mut buf = vec![0u8; 400];
    let mut sb = Sb::new(&mut buf);
    {
        let mut ring = ops::sound::ring_lock();
        *ring = ops::sound::Ring {
            slots: Vec::new(),
            head: 0,
            count: 0,
            seq: 0,
            dropped: 0,
            total: 0,
        };
    }
    for i in 0..3u32 {
        ops::sound::snd_record_for_test(12, i, c"boom".as_ptr().cast(), -1, i, 0);
    }
    ops::sound::snd_drain_for_test(None, &mut sb).unwrap();
    let text = core::str::from_utf8(sb.as_cstr()).unwrap();
    // The first event fits in 400 B; the second is rolled back whole and requeued.
    assert!(
        text.starts_with(concat!(
            r#"{"events":[{"seq":1,"hash":0,"name":"boom","entindex":-1,"guid":0,"#,
            r#""via":12,"flags":0,"ageMs":"#
        )),
        "{text}"
    );
    assert!(text.ends_with(r#","truncated":true,"dropped":0,"total":3,"pending":2}"#), "{text}");
    assert_eq!(ops::sound::ring_lock().count, 2, "the unwritten tail was requeued");
}

/// ONE event that alone overflows the builder (`mcp_bridge_game.inc:1183-1187` with the
/// rollback firing on the FIRST event): the reply is still a normal drain - an empty
/// `events` array and the tail, because [`Sb::rewind`] cleared the overflow flag - with
/// `truncated:true`, and the event is requeued, not the overflow error.
#[test]
fn sound_drain_rolls_back_an_event_that_alone_overflows() {
    let _s = test_ser();
    let mut buf = vec![0u8; 100]; // fits the head + tail; no event of ~90 B plus the 256 B tail room
    let mut sb = Sb::new(&mut buf);
    {
        let mut ring = ops::sound::ring_lock();
        *ring = ops::sound::Ring {
            slots: Vec::new(),
            head: 0,
            count: 0,
            seq: 0,
            dropped: 0,
            total: 0,
        };
    }
    ops::sound::snd_record_for_test(12, 0, c"boom".as_ptr().cast(), -1, 0, 0);
    ops::sound::snd_drain_for_test(None, &mut sb).unwrap();
    let text = core::str::from_utf8(sb.as_cstr()).unwrap();
    assert_eq!(
        text,
        r#"{"events":[],"truncated":true,"dropped":0,"total":1,"pending":1}"#,
        "{text}"
    );
    assert_eq!(ops::sound::ring_lock().count, 1, "the unwritten event was requeued");
}

/// The listener vtable's padding (`mcp_bridge_game.inc:1042-1043`, the `:1078-1079` fill):
/// the bridge-owned table is `max(VTLEN, SND_LISTENER_SLOTS = 16)` entries - the real slot
/// is the on-start thunk, and every other entry, INCLUDING the padding slots
/// `VTLEN..SND_LISTENER_SLOTS`, is the no-op (a game update adding a virtual to
/// `ISoundEventListener` still reads inside the table).
#[test]
fn sound_listener_table_is_padded_to_the_cs_16_with_noop_stubs() {
    use s2sdk_sys::generated::ISoundEventListener_slots;
    let _s = test_ser();
    ops::sound::snd_lvt_fill_for_test();
    let (table, noop) = ops::sound::snd_lvt_for_test();
    let vtlen = ISoundEventListener_slots::VTLEN;
    assert_eq!(
        table.len(),
        vtlen.max(16),
        "the table is the generated shape padded to the C's 16"
    );
    for (i, slot) in table.iter().enumerate() {
        if i == ISoundEventListener_slots::OnSoundEventStarted {
            assert_ne!(*slot, noop, "slot {i} is the on-start thunk");
        } else {
            assert_eq!(*slot, noop, "slot {i} is the no-op");
        }
    }
    // The brief's assertion spelled out: the padding slots VTLEN..SND_LISTENER_SLOTS.
    for (i, slot) in table.iter().enumerate().skip(vtlen).take(16 - vtlen.min(16)) {
        assert_eq!(*slot, noop, "padding slot {i} (VTLEN = {vtlen}) is the no-op");
    }
}

/// The malformed `mode` (`mcp_bridge_game.inc:1112`) through the real handler - it precedes any
/// install. `sound.subscribe` is the row that runs the mode selection; its param check answers
/// before the install, so this needs no game (the identity refusal, which needs a pinned
/// `soundsystem.dll`, comes later).
#[test]
fn sound_mode_param_error_precedes_any_install() {
    let got = handler(ops::sound::HANDLERS, "sound.subscribe")(&request(
        "sound.subscribe",
        Some(r#"{"mode":"x"}"#),
    ));
    let want = control::error_ext(
        Some("7"),
        -32602,
        "InvalidParams",
        r#"{"reason":"mode must be wrap|listener|both"}"#,
    );
    assert_eq!(got, want);
}

/// `sound.uninstall` on a not-installed bridge: the C's status object, no game access
/// (driven through the real wiring, gate passed). The ring's counters are the live
/// statics, reset here for the exact bytes.
#[test]
fn sound_uninstall_noop_answers_the_status_object() {
    let _s = test_ser();
    {
        let mut ring = ops::sound::ring_lock();
        *ring = ops::sound::Ring {
            slots: Vec::new(),
            head: 0,
            count: 0,
            seq: 0,
            dropped: 0,
            total: 0,
        };
    }
    let state = BridgeState::new(
        Arc::new(FakeModules { state: ModState::Verified, revision: 0 }),
        Box::new(dispatch::is_ported),
    );
    let got = state.exec_request(&request("sound.uninstall", Some("{}")));
    assert_eq!(
        got,
        control::result_ok(
            Some("7"),
            r#"{"installed":false,"mode":null,"pending":0,"total":0,"dropped":0,"ringSize":256}"#
        )
    );
}

/// The body-level identity refusals on the host: `mod_iface` answers null, so the C's
/// `obj_is` refusals fire with the C's exact messages (`mcp_bridge.c:5022-5024`,
/// `game.inc:1009-1011`, `:1067-1069`).
///
/// The bodies are called directly: with every module verified `exec_request` reaches them, and
/// the identity check is what this case is about (it runs before any hook is registered). Only
/// the rows that install have it: `sound.subscribe` here, `game.saveRestore` below - and
/// `sound.status` / `sound.drain` reach it only while a KHook API is installed (without one
/// they answer the blockedOn note instead, which is what lets them answer after a Metamod
/// unload; `off_ramps_never_need_khook`). It drains the shared ring, so it takes the serial
/// guard like every other case that touches the lanes' statics.
#[test]
fn sound_and_saverestore_bodies_refuse_unpinned_interfaces() {
    let _s = test_ser();
    let got = handler(ops::sound::HANDLERS, "sound.subscribe")(&request(
        "sound.subscribe",
        Some("{}"),
    ));
    let want = control::error_ext(
        Some("7"),
        -32017,
        "PrerequisiteFailed",
        r#"{"reason":"SoundOpSystem001 is not the pinned CSoundOpSystem"}"#,
    );
    assert_eq!(got, want, "sound.subscribe: the install's identity refusal");
    // The report rows answer a status object instead of refusing.
    for method in ["sound.status", "sound.drain"] {
        let got = handler(ops::sound::HANDLERS, method)(&request(method, Some("{}")));
        assert!(!got.contains("PrerequisiteFailed"), "{method}: no install, no refusal: {got}");
    }
    let got = handler(ops::game::HANDLERS, "game.saveRestore")(&request(
        "game.saveRestore",
        Some("{}"),
    ));
    let want = control::error_ext(
        Some("7"),
        -32017,
        "PrerequisiteFailed",
        r#"{"reason":"Source2ServerConfig001 is not the pinned CCSGameConfiguration (+8 view)"}"#,
    );
    assert_eq!(got, want);
}

/// NEED_KHOOK's negative: every module verified, no `IKHook` installed - the gate answers
/// `-32017 PrerequisiteFailed` with the exact `blockedOn` for the one row that installs a
/// **KHook** hook (`game.saveRestore`), on the main thread (`exec_request`) and queued-not-
/// answered on the IPC lane, and no body runs (the identity refusal below would have a
/// different `data`).
///
/// The off-ramps are the other half of the rule (`off_ramps_never_need_khook`): with the same
/// module mask and no KHook they still answer, because a user whose input override is
/// installed must be able to clear or uninstall it after a Metamod unload.
#[test]
fn khook_gate_answers_prerequisite_failed_for_hook_rows() {
    let _s = test_ser();
    let state = BridgeState::new(
        Arc::new(FakeModules { state: ModState::Verified, revision: 0 }),
        Box::new(dispatch::is_ported),
    );
    assert!(
        s2sdk::hook::api().is_none(),
        "the negative needs the process-wide API slot empty (no mock in this binary)"
    );
    let got = state.exec_request(&request("game.saveRestore", Some("{}")));
    let want = control::error_ext(
        Some("7"),
        -32017,
        "PrerequisiteFailed",
        r#"{"blockedOn":"metamod: KHook not loaded"}"#,
    );
    assert_eq!(got, want, "game.saveRestore: the gate's answer, no body ran");
    // The IPC lane queues the row (the main thread decides -32017), it never replies here.
    assert!(
        matches!(state.route("game.saveRestore"), cs2_bridge::ipc::Route::Queue),
        "the lane queues it"
    );
    // The other rows that install a KHook hook answer the same, and their arming bodies (which
    // would read a pinned module global / resolve a game interface) never run.
    for method in ["input.set", "sound.subscribe"] {
        let got = state.exec_request(&request(method, Some("{}")));
        assert_eq!(got, want, "{method}: the gate's answer, no body ran");
    }
    // A row that installs no hook is unaffected.
    let got = state.exec_request(&request("sound.uninstall", Some("{}")));
    assert!(!got.contains("KHook not loaded"), "{got}");
}

/// NEGATIVE (review round 2): the off-ramps must never need KHook. `meta unload` leaves an
/// installed input override behind; refusing `input.clear` / `input.uninstall` there would leave
/// buttons forced with no way to release them. Every row that does not INSTALL a KHook hook
/// answers `Run` without KHook, and `exec_request` reaches its body (the identity refusal on the
/// host), not the gate.
#[test]
fn off_ramps_never_need_khook() {
    let _s = test_ser();
    let state = BridgeState::new(
        Arc::new(FakeModules { state: ModState::Verified, revision: 0 }),
        Box::new(dispatch::is_ported),
    );
    assert!(s2sdk::hook::api().is_none(), "no mock in this binary");
    for method in [
        "input.clear",
        "input.status",
        "input.uninstall",
        "sound.status",
        "sound.drain",
        "sound.uninstall",
    ] {
        assert_eq!(
            registry::gate_with(method, !registry::NEED_KHOOK, false),
            registry::Gate::Run,
            "{method}: an off-ramp row needs no KHook"
        );
        assert!(
            matches!(state.route(method), cs2_bridge::ipc::Route::Queue),
            "{method}: still queued for the main thread"
        );
    }
    // The rows themselves are not driven through `exec_request` here: their bodies read the
    // pinned `client.dll`/`soundsystem.dll` globals (e.g. `in_install`'s
    // `*(base + CG_INPUT_PTR)`), which this host has not verified. The gate answer is what this
    // case is about; the off-ramps' own bodies are covered by `input_clear_resets`,
    // `input_uninstall_noop_answers_the_status_object` and
    // `sound_uninstall_noop_answers_the_status_object`.
}

/// NEGATIVE (mandatory): with every module unverified, `exec_request` answers the gate's
/// `-32017` naming the first unmet need in the C's `bits[]` order, and no body runs - the
/// reply is byte-for-byte the gate builder's (`data.module` / `check` / `state`).
#[test]
fn unverified_modules_answer_prerequisite_failed_and_bodies_never_run() {
    let state = BridgeState::new(
        Arc::new(FakeModules { state: ModState::NotLoaded, revision: 0 }),
        Box::new(dispatch::is_ported),
    );
    // NEED_INPUT = client | engine: engine2 precedes client in the C's bits[] order.
    for method in ["input.set", "input.clear", "input.status", "input.uninstall"] {
        let got = state.exec_request(&request(method, Some("{}")));
        let want = control::prerequisite_failed(Some("7"), "engine2.dll", ModState::NotLoaded);
        assert_eq!(got, want, "{method}: the gate's answer, no body ran");
        assert!(got.contains(r#""check":"identity""#), "{method}: {got}");
    }
    // NEED_SERVER.
    let got = state.exec_request(&request("game.saveRestore", Some("{}")));
    let want = control::prerequisite_failed(Some("7"), "server.dll", ModState::NotLoaded);
    assert_eq!(got, want);
    // NEED_SOUND.
    for method in ["sound.status", "sound.subscribe", "sound.drain", "sound.uninstall"] {
        let got = state.exec_request(&request(method, Some("{}")));
        let want = control::prerequisite_failed(Some("7"), "soundsystem.dll", ModState::NotLoaded);
        assert_eq!(got, want, "{method}: the gate's answer, no body ran");
    }
}

/// The registered rows that were "not advertised" before this lane stay tracked: the
/// wave-3-style examples move to a row that stays blocked in the C table (`material.assign`).
#[test]
fn the_lane_serves_its_rows_and_the_blocked_example_stays_blocked() {
    for (name, _) in ops::input::HANDLERS {
        let row = registry::find(name).unwrap_or_else(|| panic!("`{name}` absent"));
        assert!(row.implemented && dispatch::is_ported(name), "`{name}` served");
    }
    for (name, _) in ops::sound::HANDLERS {
        let row = registry::find(name).unwrap_or_else(|| panic!("`{name}` absent"));
        assert!(row.implemented && dispatch::is_ported(name), "`{name}` served");
    }
    // The blocked example (registry.rs:181): still a blocked row, never advertised.
    let row = registry::find("material.assign").unwrap();
    assert!(!row.implemented && !dispatch::is_ported("material.assign"));
    // The `game.saveRestore` row is served by the game group's hooks module.
    let row = registry::find("game.saveRestore").unwrap();
    assert!(row.implemented && dispatch::is_ported("game.saveRestore"));
}
