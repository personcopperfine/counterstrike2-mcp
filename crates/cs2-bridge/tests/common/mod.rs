//! Shared machinery for the WP2.C differential tests (`registry.rs`, `control.rs`): lift the
//! regions under test verbatim out of the frozen `bridge/mcp_bridge.c` (by stable text
//! markers, failing loudly when a marker is absent), inject them into the committed generator
//! template `tests/data/registry_gen.c`, compile the result with `cc`, run it, and hand the
//! records to the caller.
//!
//! Everything the extraction finds must be there: a missing marker, a marker that moved, or a
//! missing `cc` is a loud FAILURE naming what is missing - never a skip (the brief forbids
//! gates that pass when the thing under test is absent).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// The repository root (`.../crates/cs2-bridge/../..`).
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root must exist")
}

/// `bridge/mcp_bridge.c`, the frozen source of truth.
pub fn bridge_source() -> PathBuf {
    repo_root().join("bridge/mcp_bridge.c")
}

/// `crates/cs2-bridge/tests/data`.
pub fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

/// The index of the one line containing `needle`, or a panic naming `what` and the needle.
fn line_index(lines: &[&str], needle: &str, what: &str) -> usize {
    let hits: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains(needle))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "{what}: the marker `{needle}` must appear exactly once in the frozen bridge source, found {}",
        hits.len()
    );
    hits[0]
}

/// The lines from the `begin` marker line up to (but excluding) the `end` marker line, or up
/// to and including it when `include_end` (`exact function boundaries`; the C's bytes, not a
/// re-derivation).
fn extract(src: &str, begin: &str, end: &str, include_end: bool, what: &str) -> String {
    let lines: Vec<&str> = src.lines().collect();
    let b = line_index(&lines, begin, what);
    let e = line_index(&lines, end, what);
    assert!(
        b < e,
        "{what}: the begin marker `{begin}` must precede the end marker `{end}`"
    );
    let stop = if include_end { e + 1 } else { e };
    let mut text = lines[b..stop].join("\n");
    text.push('\n');
    text
}

/// One whole line - the C's one-line helpers (`seq`), extracted verbatim.
fn extract_line(src: &str, needle: &str, what: &str) -> String {
    let lines: Vec<&str> = src.lines().collect();
    let i = line_index(&lines, needle, what);
    format!("{}\n", lines[i])
}

/// Like [`extract`], but the begin marker line is an arm opener (`if (...) {` /
/// `} else if (...) {`) whose innards are what is under test: skip it, and the end marker
/// line (which carries the arm's closing `}`) stays excluded. The extracted bytes are the
/// arm's statements, verbatim.
fn extract_after_opener(src: &str, begin: &str, end: &str, what: &str) -> String {
    let lines: Vec<&str> = src.lines().collect();
    let b = line_index(&lines, begin, what);
    let e = line_index(&lines, end, what);
    assert!(
        b + 1 < e,
        "{what}: the arm innards between `{begin}` and `{end}` must not be empty"
    );
    let mut text = lines[b + 1..e].join("\n");
    text.push('\n');
    text
}

/// The bridge's own protocol defines (`mcp_bridge.c:124-129`): MAX_LINE, QCAP, OUT_CAP,
/// BRIDGE_BUILD, GAME_BUILD, DEFAULT_DEADLINE_MS - verbatim, so no number is retyped.
fn bridge_defines(src: &str) -> String {
    let text = extract(
        src,
        "#define MAX_LINE (1024u * 1024u)",
        "#define DEFAULT_DEADLINE_MS 30000u",
        true,
        "bridge defines",
    );
    for needed in ["MAX_LINE", "QCAP", "BRIDGE_BUILD", "GAME_BUILD", "DEFAULT_DEADLINE_MS"] {
        assert!(text.contains(needed), "the bridge defines block must carry {needed}");
    }
    text
}

/// [`bridge_defines`] with the one value the Rust build deliberately changes: the version
/// string. The frozen C source says `m2.20`; the Rust bridge says [`cs2_bridge::BRIDGE_BUILD`]
/// (WP2.D). The substitution is asserted to hit exactly one literal in the defines block, so
/// every other byte of the generator still has to match the C's.
fn bridge_defines_with_rust_build(src: &str) -> String {
    let text = bridge_defines(src);
    assert_eq!(
        text.matches("\"m2.20\"").count(),
        1,
        "the C's BRIDGE_BUILD literal must appear exactly once in the defines block"
    );
    text.replace(
        "\"m2.20\"",
        &format!("\"{}\"", cs2_bridge::BRIDGE_BUILD),
    )
}

/// The entity JSON region (`mcp_bridge.c:226-1010`, the build.sh markers): the `sb_*` string
/// builder the reply builders write through - the escaping spec, not a re-implementation.
fn entity_region(src: &str) -> String {
    const MARK_BEGIN: &str = "/* ---- entity JSON region begin ---- */";
    const MARK_END: &str = "/* ---- entity JSON region end ---- */";
    let text = extract(src, MARK_BEGIN, MARK_END, false, "entity JSON region");
    for needed in ["sb_init", "sb_add", "sb_addnum", "sb_addi", "sb_addjson", "sb_putc"] {
        assert!(text.contains(needed), "the entity JSON region must carry {needed}");
    }
    text
}

/// The method-registry region: from the first NEED define through `mod_state_name`
/// (`mcp_bridge.c:1030-1176`) - NEED_* bits, the `Method` record, `k_methods[]`, METHOD_COUNT,
/// `find_method`, the `MS_*` states, `mod_ok`/`verified_mask`, `mod_state_name`.
fn registry_region(src: &str) -> String {
    let text = extract(
        src,
        "#define NEED_TF2 1u",
        "static void caps_bump(void)",
        false,
        "method registry region",
    );
    for needed in ["k_methods", "find_method", "verified_mask", "mod_state_name", "NEED_PET"] {
        assert!(text.contains(needed), "the method registry region must carry {needed}");
    }
    text
}

/// `build_caps` (`mcp_bridge.c:1189-1220`).
fn build_caps_region(src: &str) -> String {
    let text = extract(
        src,
        "/* capability object: one builder for control.hello and control.capabilities */",
        "/* mapped-image PE facts (never relocated fields only) */",
        false,
        "build_caps region",
    );
    assert!(
        text.contains("build_caps") && text.contains("methods"),
        "the build_caps region must carry the capabilities builder"
    );
    text
}

/// `build_error` / `build_error_ext` (`mcp_bridge.c:1562-1583`).
fn error_builders_region(src: &str) -> String {
    let text = extract(
        src,
        "/* ---------------- error responses ---------------- */",
        "/* M2.2 (d): expire queued requests whose deadline passed",
        false,
        "error builder region",
    );
    assert!(
        text.contains("build_error") && text.contains("build_error_ext"),
        "the error builder region must carry both builders"
    );
    text
}

/// The `control.ping` arm innards (`mcp_bridge.c:6499-6528`).
fn arm_ping(src: &str) -> String {
    extract_after_opener(
        src,
        "if (seq(method, \"control.ping\")) {",
        "} else if (seq(method, \"control.status\")) {",
        "control.ping arm",
    )
}

/// The `control.status` arm innards (`mcp_bridge.c:6531-6552`).
fn arm_status(src: &str) -> String {
    extract_after_opener(
        src,
        "} else if (seq(method, \"control.status\")) {",
        "} else if (seq(method, \"control.capabilities\")) {",
        "control.status arm",
    )
}

/// The `control.capabilities` arm innards (`mcp_bridge.c:6555-6557`).
fn arm_caps(src: &str) -> String {
    extract_after_opener(
        src,
        "} else if (seq(method, \"control.capabilities\")) {",
        "} else if (seq(method, \"control.version\")) {",
        "control.capabilities arm",
    )
}

/// The `control.version` arm innards (`mcp_bridge.c:6561-6577`).
fn arm_version(src: &str) -> String {
    extract_after_opener(
        src,
        "} else if (seq(method, \"control.version\")) {",
        "} else if (seq(method, \"control.cancel\")) {",
        "control.version arm",
    )
}

/// The `control.hello` ok reply (`mcp_bridge.c:6475-6486`), from `authed = 1;` through the
/// synchronous send.
fn arm_hello(src: &str) -> String {
    let text = extract(
        src,
        "authed = 1;",
        "logline(\"ipc: hello ok\");",
        true,
        "control.hello ok reply",
    );
    assert!(
        text.contains("build_caps"),
        "the hello arm must build the capabilities object"
    );
    text
}

/// The `ipc_thread` method-gate arms (`mcp_bridge.c:6601-6621`): the `-32601` arm for
/// `control.*` names, and the `find_method` chain's unknown / `-32011` / `needs == 0` arms.
/// Two deterministic, asserted transforms shape the extracted chain into a standalone
/// function body: the leading `} else ` becomes `if (`, and the trailing `} else {` (the
/// enqueue arm's opener, queue behaviour that is NOT under test) becomes the `}` closing the
/// `needs == 0` arm - plus one more `}` closing the `find_method` else block, whose own
/// closer sat behind the cut enqueue block.
fn arm_ipcgate(src: &str) -> String {
    let lines: Vec<&str> = src.lines().collect();
    let b = line_index(
        &lines,
        "} else if (method[0] == 'c' && method[1] == 'o'",
        "ipc gate arms",
    );
    let e = line_index(&lines, "Req rq;", "ipc gate arms");
    assert!(b < e, "ipc gate arms: the chain must precede the enqueue block");
    let mut body: Vec<String> = lines[b..e].iter().map(|l| l.to_string()).collect();
    let first = body.first().expect("the chain is non-empty").clone();
    let trimmed_first = first.trim_start();
    assert!(
        trimmed_first.starts_with("} else if ("),
        "ipc gate arms: the extracted chain must open with `}} else if (`, got `{first}`"
    );
    body[0] = trimmed_first.strip_prefix("} else ").expect("checked above").to_string();
    let last = body.last().expect("the chain is non-empty").clone();
    assert!(
        last.trim() == "} else {",
        "ipc gate arms: the extracted chain must end at the enqueue opener `}} else {{`, got `{last}`"
    );
    body.pop();
    body.push("}".to_string()); // closes the needs == 0 arm (the removed `} else {` opener)
    body.push("}".to_string()); // closes the find_method else block
    body.join("\n") + "\n"
}

/// `exec_request`'s head (`mcp_bridge.c:5657-5698`): find, `-32601`, `-32011`, and the
/// verified-mask check with the `-32017 PrerequisiteFailed` object. The dispatch after the
/// gate is not under test; the head is closed before it.
fn arm_exec(src: &str) -> String {
    let lines: Vec<&str> = src.lines().collect();
    let b = line_index(
        &lines,
        "static void exec_request(const Req *r, char *out, u32 outcap) {",
        "exec_request head",
    );
    let e = line_index(
        &lines,
        "if (seq(m->name, \"entity.create\")) {",
        "exec_request head",
    );
    assert!(b < e, "exec_request head: the function must precede the dispatch");
    let mut body: Vec<String> = lines[b..e].iter().map(|l| l.to_string()).collect();
    body.push("}".to_string());
    body.join("\n") + "\n"
}

/// Replaces the one `/*@NAME@*/` placeholder in `template` with `text`.
fn inject(template: &str, name: &str, text: &str) -> String {
    let marker = format!("/*@{name}@*/");
    assert_eq!(
        template.matches(&marker).count(),
        1,
        "the generator template must carry exactly one placeholder {marker}"
    );
    template.replace(&marker, text)
}

/// Builds the generator once per test binary: extraction + injection + `cc`. Returns the
/// binary path. Every failure names what is missing; there is no skip.
pub fn build_generator() -> &'static PathBuf {
    static GEN: OnceLock<PathBuf> = OnceLock::new();
    GEN.get_or_init(|| {
        let src = std::fs::read_to_string(bridge_source()).unwrap_or_else(|error| {
            panic!("the frozen bridge source {} must be readable: {error}", bridge_source().display())
        });
        let template = std::fs::read_to_string(data_dir().join("registry_gen.c"))
            .expect("the committed generator template tests/data/registry_gen.c must be readable");

        let assembled = inject(&template, "BRIDGE_DEFINES", &bridge_defines_with_rust_build(&src));
        let assembled = inject(&assembled, "SEQ", &extract_line(&src, "static int seq(const char *a, const char *b)", "seq"));
        let assembled = inject(&assembled, "ENTITY_REGION", &entity_region(&src));
        let assembled = inject(&assembled, "REGISTRY_REGION", &registry_region(&src));
        let assembled = inject(&assembled, "BUILD_CAPS", &build_caps_region(&src));
        let assembled = inject(&assembled, "ERROR_BUILDERS", &error_builders_region(&src));
        let assembled = inject(&assembled, "ARM_PING", &arm_ping(&src));
        let assembled = inject(&assembled, "ARM_STATUS", &arm_status(&src));
        let assembled = inject(&assembled, "ARM_CAPS", &arm_caps(&src));
        let assembled = inject(&assembled, "ARM_VERSION", &arm_version(&src));
        let assembled = inject(&assembled, "ARM_HELLO", &arm_hello(&src));
        let assembled = inject(&assembled, "ARM_IPCGATE", &arm_ipcgate(&src));
        let assembled = inject(&assembled, "ARM_EXEC", &arm_exec(&src));

        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        std::fs::create_dir_all(&dir).expect("the cargo target tmpdir is writable");
        let source = dir.join("registry_gen_assembled.c");
        std::fs::write(&source, assembled).expect("the assembled generator is writable");

        let out = dir.join("registry_gen");
        let result = Command::new("cc")
            .arg("-std=c11")
            .arg("-O1")
            .arg("-Wall")
            .arg("-Wextra")
            .arg("-I")
            .arg(repo_root().join("bridge"))
            .arg("-o")
            .arg(&out)
            .arg(&source)
            .output();
        let output = match result {
            Ok(output) => output,
            Err(error) => panic!("cc must be available to build the differential generator: {error}"),
        };
        assert!(
            output.status.success(),
            "cc failed for the assembled generator:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        out
    })
}

/// Runs the generator and returns its stdout as record lines (TAB-separated fields).
pub fn run_generator() -> Vec<String> {
    let output = Command::new(build_generator())
        .output()
        .expect("the differential generator must run");
    assert!(
        output.status.success(),
        "the differential generator failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Decodes the generator's hex field (`-` = empty).
#[allow(dead_code)] // each test binary uses the subset it needs
pub fn unhex(hex: &str) -> Vec<u8> {
    if hex == "-" {
        return Vec::new(); // the generator's empty byte string
    }
    assert!(
        hex.len().is_multiple_of(2),
        "a hex field must have an even length, got `{hex}`"
    );
    let bytes = hex.as_bytes();
    (0..bytes.len())
        .step_by(2)
        .map(|i| {
            let hi = (bytes[i] as char).to_digit(16).expect("a hex digit");
            let lo = (bytes[i + 1] as char).to_digit(16).expect("a hex digit");
            ((hi << 4) | lo) as u8
        })
        .collect()
}
