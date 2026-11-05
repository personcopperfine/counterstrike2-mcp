//! Guard for the server `instructions` (streamline WP2.4): the only prose an agent reads besides
//! tools/list. Short, and every tool-like word in it must be a registered tool or alias.

/// Cap on the instructions text, in characters.
const MAX_INSTRUCTIONS_CHARS: usize = 3_000;
/// snake_case words in the text that are not tools (entity classes, game events).
const NOT_TOOLS: &[&str] = &[
    "logic_auto", "logic_eventlistener", "round_freeze_end",
    // playbook names (MCP prompts, read with get_playbook)
    "blockout_map", "iterate_loop", "cs_script", "particle_set",
];

#[test]
fn instructions_are_short_and_name_only_real_tools() {
    let text = hammer_mcp::INSTRUCTIONS;
    let n = text.chars().count();
    assert!(n <= MAX_INSTRUCTIONS_CHARS, "instructions are {n} chars (cap {MAX_INSTRUCTIONS_CHARS})");
    let registry = hammer_mcp::build_registry();
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
    let t = hammer_mcp::INSTRUCTIONS;
    for must in ["session_status", "new_map", "open_map", "readOnlyHint", "save_map", "waitMs", "run_map", "play_test", "Never step outside"] {
        assert!(t.contains(must), "instructions lack {must}");
    }
}
