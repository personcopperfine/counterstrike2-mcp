//! `pet_open_gate`'s host derivation and the path bytes, with the C's own buffer sizes.
//!
//! The C's `pet_open_gate` (`mcp_bridge_pet.inc:378-414`) declares `char app[1024]`,
//! `char addon[64]` and `char host[2048]` and passes each `sizeof` to the `kv3_check.h` helper.
//! `kv3_check`'s own wrappers use the host test's 512-byte buffer, so the port must call the
//! `*_into` variants with the C's sizes; these cases pin the difference:
//!
//! * a request path that makes the host path longer than 512 bytes - the C accepts it, a 512-byte
//!   `host` would refuse it as `read_failed`;
//! * a 64-character addon name - the C's `addon[64]` holds 63 characters plus the NUL, so 64 is
//!   `no_addon` (the 512-byte wrapper would have accepted it).
//!
//! The reader is a plain closure here (the C's `pet_read_vpcf` needs Windows), and the reply
//! builders take the game's raw path bytes, so the last case feeds a non-UTF-8 path through them.

use cs2_bridge::kv3_check;
use cs2_bridge::ops::pet::game;
use cs2_bridge::ops::pet::reply;

/// A `.vpcf` that passes `vpcf_check` (`kv3_check.h`: header, grammar, root `_class`).
const OK_DOC: &[u8] =
    b"<!-- kv3 encoding:text:format:vpcf -->\n{ _class = \"CParticleSystemDefinition\" }";

/// `<app>\game\bin\win64\cs2.exe`, the layout `kv3_app_root` requires (`mcp_bridge_pet.inc:368-373`).
fn exe_for(app: &str) -> String {
    format!("{app}\\game\\bin\\win64\\cs2.exe")
}

/// The addon-relative path the caller sends (`particles/...`, at most 512 characters).
fn request_path(total: usize) -> String {
    // `particles/` + name + `.vpcf`
    let name = "p".repeat(total - "particles/".len() - ".vpcf".len());
    format!("particles/{name}.vpcf")
}

/// `:394-400` with the C's `host[2048]`: app 450 + addon + a 450-character path resolves to a
/// host path over 512 bytes and the gate reads that file, instead of refusing at the smaller
/// host-test cap.
#[test]
fn a_host_path_over_512_bytes_is_accepted_like_the_c() {
    let app = format!("C:\\{}", "a".repeat(447)); // 450 bytes
    assert_eq!(app.len(), 450);
    let path = request_path(450);
    assert_eq!(path.len(), 450);
    let exe = exe_for(&app);

    // The derivation itself is the C's `kv3_host_path` into `host[2048]`.
    let mut host = [0u8; game::HOST_CAP];
    assert!(
        kv3_check::host_path_into(&app, "mcp_test", &path, &mut host),
        "the C's 2048-byte host buffer holds it"
    );
    let host = reply::cstr(&host);
    assert!(
        host.len() > 512,
        "the case is only interesting when the host-test 512-byte cap would have refused it: {}",
        host.len()
    );

    // With the 512-byte wrapper's cap the same derivation refuses: that is the regression this
    // test holds (`kv3_check::host_path` is the C host test's 512-byte buffer).
    let mut small = [0u8; 512];
    assert!(
        !kv3_check::host_path_into(&app, "mcp_test", &path, &mut small),
        "512 bytes cannot hold this host path"
    );

    // The gate reads the host file and accepts the valid document.
    let mut seen: Vec<u8> = Vec::new();
    let refused = game::open_gate_with(
        &path,
        Some(&exe),
        Some("cs2.exe -addon mcp_test"),
        |host| {
            seen = host.to_vec();
            Ok(OK_DOC.to_vec())
        },
    );
    assert!(refused.is_none(), "the C accepts this path");
    assert_eq!(
        seen,
        host,
        "the reader is handed the exact host path bytes (`:401`)"
    );
}

/// `:389-393` with the C's `addon[64]`: 63 characters fit, 64 do not (the buffer holds 63 plus
/// the NUL), so a 64-character addon is `no_addon` before any file is touched.
#[test]
fn a_sixty_four_character_addon_is_no_addon() {
    let app = "C:\\game";
    let exe = exe_for(app);
    let path = "particles/x.vpcf";

    let addon63 = "a".repeat(63);
    let refused = game::open_gate_with(
        path,
        Some(&exe),
        Some(&format!("cs2.exe -addon {addon63}")),
        |_| Ok(OK_DOC.to_vec()),
    );
    assert!(refused.is_none(), "63 characters fit the C's addon[64]");

    let addon64 = "a".repeat(64);
    let refused = game::open_gate_with(
        path,
        Some(&exe),
        Some(&format!("cs2.exe -addon {addon64}")),
        |_| panic!("no_addon is answered before the host path is read (`:389-393`)"),
    )
    .expect("a 64-character addon does not fit addon[64]");
    assert_eq!(
        refused.error.data,
        r#"{"reason":"no_addon","path":"particles/x.vpcf"}"#
    );
    assert_eq!(refused.log, "pet: open refused no_addon: particles/x.vpcf");

    // The 512-byte wrapper accepted it, which is the behaviour this case rules out.
    let mut wide = [0u8; 512];
    assert!(kv3_check::addon_name_into(
        &format!("cs2.exe -addon {addon64}"),
        &mut wide
    ));
}

/// The C's `app[1024]` (`:379`): an app root that does not fit the host test's 512-byte buffer is
/// still accepted, and one that does not fit the C's own 1024 refuses as `no_app_root`.
#[test]
fn the_app_root_uses_the_c_1024_byte_cap() {
    let app = format!("C:\\{}", "a".repeat(597)); // 600 bytes, the C's app[1024] holds it
    assert_eq!(app.len(), 600);
    let exe = exe_for(&app);
    assert!(
        kv3_check::app_root(&exe).is_none(),
        "the 512-byte wrapper refuses a 600-byte app root"
    );
    let mut wide = [0u8; game::APP_CAP];
    assert!(kv3_check::app_root_into(&exe, &mut wide));
    assert_eq!(reply::cstr(&wide), app.as_bytes());

    assert!(game::open_gate_with(
        "particles/x.vpcf",
        Some(&exe),
        Some("cs2.exe -addon mcp_test"),
        |_| Ok(OK_DOC.to_vec()),
    )
    .is_none());

    // `GetModuleFileNameA` failing is `no_app_root` before the addon.
    let refused = game::open_gate_with("particles/x.vpcf", None, Some("-addon a"), |_| {
        panic!("no_app_root is answered before any file is read (`:384-388`)")
    })
    .expect("no app root");
    assert_eq!(
        refused.error.data,
        r#"{"reason":"no_app_root","path":"particles/x.vpcf"}"#
    );
}

/// The reply builders take the game's path bytes, not a decoded string
/// (`GetPath` -> `Save`/`pet_read_vpcf`): a code-page byte reaches the C's escaper unchanged and
/// becomes the same U+FFFD `sb_addjson` writes (`mcp_bridge.c:289-319`).
#[test]
fn path_bytes_reach_the_builders_unconverted() {
    // 0xE9 is 'e-acute' in CP1252 and not a valid UTF-8 lead on its own: the C's escaper writes
    // one U+FFFD for it (EF BF BD), not the original byte.
    let path = b"C:\\content\\\xE9.vpcf";
    let mut b = reply::Buf::new(1024);
    reply::save_json(&mut b, true, false, path);
    assert_eq!(
        core::str::from_utf8(b.bytes()).expect("the escaper only writes UTF-8"),
        "{\"saved\":true,\"skippedClean\":false,\"path\":\"C:\\\\content\\\\\u{FFFD}.vpcf\"}"
    );

    // A whole UTF-8 sequence is passed through byte for byte.
    let mut b = reply::Buf::new(1024);
    reply::save_json(&mut b, true, false, "C:\\content\\é.vpcf".as_bytes());
    assert_eq!(
        core::str::from_utf8(b.bytes()).expect("valid UTF-8"),
        "{\"saved\":true,\"skippedClean\":false,\"path\":\"C:\\\\content\\\\é.vpcf\"}"
    );

    // The same bytes in the 512-byte error `data`.
    let error = reply::unloadable("read_failed", path);
    assert!(
        error.data.contains("\u{FFFD}.vpcf"),
        "raw path bytes go through `sb_addjson`: {}",
        error.data
    );
}
