//! Guard for the server `instructions` (streamline WP2.4): the only prose an agent reads besides
//! tools/list. Short, and every tool-like word in it must be a registered tool or alias.

/// Cap on the instructions text, in characters.
const MAX_INSTRUCTIONS_CHARS: usize = 1_800;
/// snake_case words in the text that are not tools (none today).
const NOT_TOOLS: &[&str] = &[];

#[test]
fn instructions_are_short_and_name_only_real_tools() {
    let text = client_mcp::INSTRUCTIONS;
    let n = text.chars().count();
    assert!(n <= MAX_INSTRUCTIONS_CHARS, "instructions are {n} chars (cap {MAX_INSTRUCTIONS_CHARS})");
    let registry = client_mcp::build_registry();
    let names = registry.names();
    let unknown: Vec<&str> = text
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| w.contains('_') && w.chars().all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit()))
        .filter(|w| !NOT_TOOLS.contains(w) && !names.contains(w))
        .collect();
    assert!(unknown.is_empty(), "instructions name tools that are not registered: {unknown:?}");
}

#[test]
fn instructions_cover_the_required_parts() {
    let t = client_mcp::INSTRUCTIONS;
    for must in ["session_status", "launch_session", "readOnlyHint", "stop_session", "Never step outside"] {
        assert!(t.contains(must), "instructions lack {must}");
    }
}
