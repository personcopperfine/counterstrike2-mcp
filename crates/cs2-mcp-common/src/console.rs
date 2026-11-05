//! The console command guard both servers apply before they send a line to the game: commands
//! that end the session or the process (or crash it) are never forwarded, whatever the route
//! (bridge `console.exec`, the VToolMessage command channel, RCON).

/// First tokens that no console route forwards (`console_exec` and `server_console_exec`):
/// client-side session enders, plus server-side ones (`rcon_password` would change the RCON
/// secret under the tools; `sv_shutdown` stops the server). Case-insensitive.
pub const REFUSED_COMMANDS: &[&str] = &[
    "quit",
    "exit",
    "disconnect",
    "test_exitprocess",
    "crash",
    "_restart",
    "killserver",
    "quit_prompt",
    "rcon_password",
    "sv_shutdown",
    "maps", // crashes the engine (2026-10-07)
];

/// Commands that load, unload or switch the map. hammer-mcp refuses them from `console_exec`
/// (`run_map` owns the load: it settles the previous load, validates the name and watches the
/// verdict); they are NOT in [`REFUSED_COMMANDS`] because client-mcp's `server_console_exec`
/// legitimately uses `changelevel` and friends.
pub const MAP_CHANGING_COMMANDS: &[&str] = &[
    "map",
    "changelevel",
    "map_workshop",
    "host_workshop_map",
    "ds_workshop_changelevel",
    "connect",
    "retry",
    "restart",
    "playdemo",
    "startupmenu",
];

/// Longest command line the RCON protocol carries (the server rejects longer requests).
pub const MAX_COMMAND_BYTES: usize = crate::rcon::MAX_COMMAND_BYTES;

/// `Some(reason)` when the command line must not be sent. Checks the length, every `;`/newline
/// segment's first token, and the body of an `alias` or `bind` (one that runs a refused command is
/// refused when it is defined). Not checked: what an `exec <cfg>` file contains.
pub fn refusal(command: &str) -> Option<String> {
    refusal_with(command, &[])
}

/// [`refusal`] plus the caller's own refused first tokens (`extra`, lower case), for example
/// [`MAP_CHANGING_COMMANDS`]. The reason names the offending token.
pub fn refusal_with(command: &str, extra: &[&str]) -> Option<String> {
    refusal_hinted(command, extra, STOP_SESSION_HINT)
}

/// What client-mcp's refusals tell the caller to do instead (it has the `stop_session` tool).
pub const STOP_SESSION_HINT: &str = "; use stop_session instead";

/// [`refusal_with`] with the server's own `hint` appended to an end-the-session refusal (a server
/// without a tool that ends the session passes `""`: hammer-mcp has no `stop_session`).
pub fn refusal_hinted(command: &str, extra: &[&str], hint: &str) -> Option<String> {
    if command.trim().is_empty() {
        return Some("empty command".into());
    }
    if command.len() > MAX_COMMAND_BYTES {
        return Some(format!(
            "{} bytes is longer than the {MAX_COMMAND_BYTES}-byte limit",
            command.len()
        ));
    }
    scan(command, extra, hint)
}

/// The engine's command-buffer limits (measured live, 2026-10-26): it drops any single command
/// longer than this many bytes with `WARNING: Command too long... ignoring!` (the limit is per
/// `;`-separated command, not per line: a 702-byte chain of short commands ran in full) ...
pub const ENGINE_COMMAND_MAX: usize = 510;
/// ... and a command whose name (first token) is longer than this is dropped with
/// `Console command too long.`.
pub const ENGINE_COMMAND_NAME_MAX: usize = 128;

/// The segments the guard looks at: the line cut on every `;`, `\n` and `\r`, quoted or not (see
/// [`refusal`]). Shared with [`engine_limit_violation`] so both read a line the same way.
pub fn command_segments(command: &str) -> impl Iterator<Item = &str> {
    command.split([';', '\n', '\r'])
}

/// Does a segment of `command` exceed what the engine's console accepts? `Some(message)` names the
/// limit, the numbers and the offending segment. A segment is measured trimmed of surrounding
/// whitespace. Segments follow [`command_segments`], so a `;` inside quotes also cuts (the length check
/// then under-counts one engine command; the readback's `Command too long` warning still catches
/// that, and the name check skips a segment that starts inside an open quote).
pub fn engine_limit_violation(command: &str) -> Option<String> {
    // a segment that starts inside an open quote (an odd number of `"` before it) is the tail of
    // one quoted argument, not a command: its first token is not a command name
    let mut quotes = 0usize;
    for raw in command_segments(command) {
        let in_quote = quotes % 2 == 1;
        quotes += raw.matches('"').count();
        let segment = raw.trim();
        if segment.len() > ENGINE_COMMAND_MAX {
            return Some(format!(
                "a `;`-separated command is {} bytes; the engine drops any single command over \
                 {ENGINE_COMMAND_MAX} bytes (\"Command too long... ignoring!\"); the limit is per \
                 command, not per line, so several short commands joined with `;` are fine. \
                 Starts: {:?}",
                segment.len(),
                segment.chars().take(40).collect::<String>()
            ));
        }
        let first = segment.split_whitespace().next().unwrap_or("");
        if !in_quote && first.len() > ENGINE_COMMAND_NAME_MAX {
            return Some(format!(
                "a command name is {} bytes; the engine drops a command whose name is over \
                 {ENGINE_COMMAND_NAME_MAX} bytes (\"Console command too long.\"). Starts: {:?}",
                first.len(),
                first.chars().take(40).collect::<String>()
            ));
        }
    }
    None
}

fn scan(command: &str, extra: &[&str], hint: &str) -> Option<String> {
    for segment in command_segments(command) {
        let Some(first) = segment.split_whitespace().next() else {
            continue;
        };
        let token = normalise(first);
        if REFUSED_COMMANDS.contains(&token.as_str()) {
            return Some(format!(
                "{first:?} ends the session/process, crashes the engine or changes the server's secrets{hint}"
            ));
        }
        if extra.contains(&token.as_str()) {
            return Some(format!("{first:?} changes the map or the connection"));
        }
        if token == "alias" || token == "bind" {
            let body = segment
                .split_whitespace()
                .skip(2)
                .collect::<Vec<_>>()
                .join(" ");
            let body = body.trim().trim_matches(['"', '\'']);
            if !body.is_empty() {
                if let Some(reason) = scan(body, extra, hint) {
                    return Some(format!("{token} body refused: {reason}"));
                }
            }
        }
    }
    None
}

/// The command a first word names, the way the engine's tokenizer reads it: leading `+`/`-` and
/// quotes dropped, cut at the first of the tokenizer's break characters (`{ } ( ) ' :` and `"`;
/// so `quit()` and `crash{}` are `quit` and `crash`), lower case.
fn normalise(token: &str) -> String {
    let token = token.trim_start_matches(['+', '-', '"', '\'']);
    let end = token
        .find(['{', '}', '(', ')', '\'', ':', '"'])
        .unwrap_or(token.len());
    token[..end].to_ascii_lowercase()
}

/// `cvar.get` takes 1..=64 names (the bridge's own bound).
pub const CVAR_NAMES_MAX: usize = 64;
/// One cvar name's longest accepted form.
pub const CVAR_NAME_MAX_BYTES: usize = 128;

/// The `names` argument of `cvar_get` (both servers): 1..=64 non-empty strings of at most 128
/// bytes without control characters, and nothing else in the object. `Err` is the message of an
/// invalid-arguments refusal.
pub fn cvar_names(args: &serde_json::Value) -> Result<Vec<String>, String> {
    let Some(object) = args.as_object() else {
        return Err(format!("the arguments must be an object, got {args}"));
    };
    if let Some(unknown) = object.keys().find(|key| key.as_str() != "names") {
        return Err(format!("unknown argument {unknown:?}: this tool takes \"names\""));
    }
    let list = object
        .get("names")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "names (array of strings) is required".to_string())?;
    if list.is_empty() || list.len() > CVAR_NAMES_MAX {
        return Err(format!(
            "names must hold 1..={CVAR_NAMES_MAX} entries, got {}",
            list.len()
        ));
    }
    list.iter()
        .map(|v| {
            v.as_str()
                .filter(|s| {
                    !s.is_empty()
                        && s.len() <= CVAR_NAME_MAX_BYTES
                        && !s.chars().any(char::is_control)
                })
                .map(str::to_string)
                .ok_or_else(|| {
                    format!(
                        "every name must be a non-empty string of at most {CVAR_NAME_MAX_BYTES} bytes, got {v}"
                    )
                })
        })
        .collect()
}

/// Cvars whose value is a secret by name (the bridge only redacts the `protected` flag, which
/// `rcon_password` does not carry in this build): the three known ones and any name containing
/// `password` or `token`. Cvar names are case-insensitive.
pub fn is_secret_cvar(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    matches!(n.as_str(), "rcon_password" | "sv_password" | "sv_setsteamaccount")
        || n.contains("password")
        || n.contains("token")
}

/// Redact a `cvar.get` reply by name: every entry whose `name` (or, failing that, the requested
/// name at the same index) is secret loses `value`/`default` and gets `redacted:true` plus
/// `redactedBy:"name"` and a `redactedNote` (the policy matches the name, the cvar may be harmless,
/// and in a `-tools` session the bridge route cannot print cvars, so the value is not readable
/// another way). Entries with `found:false` are left alone: a missing cvar is not hidden.
/// The bridge's own `protected` redaction stays as it is (no `redactedBy`).
pub fn redact_cvar_reply(reply: &mut serde_json::Value, requested: &[String]) {
    let Some(list) = reply.get_mut("cvars").and_then(serde_json::Value::as_array_mut) else {
        return;
    };
    for (i, entry) in list.iter_mut().enumerate() {
        let name = entry
            .get("name")
            .and_then(serde_json::Value::as_str)
            .or_else(|| requested.get(i).map(String::as_str))
            .unwrap_or("");
        if !is_secret_cvar(name) || entry.get("found").and_then(serde_json::Value::as_bool) == Some(false) {
            continue;
        }
        let already = entry.get("redacted").and_then(serde_json::Value::as_bool) == Some(true);
        if let Some(object) = entry.as_object_mut() {
            for key in ["value", "default"] {
                if object.contains_key(key) {
                    object.insert(key.into(), serde_json::Value::Null);
                }
            }
            object.remove("valueTruncated");
            object.insert("redacted".into(), serde_json::Value::Bool(true));
            if !already {
                object.insert("redactedBy".into(), "name".into());
                object.insert(
                    "redactedNote".into(),
                    "hidden by policy: the name looks like a secret (password/token); in a -tools \
                     session the value cannot be read another way"
                        .into(),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{engine_limit_violation, refusal};

    #[test]
    fn engine_limits_are_per_command_not_per_line() {
        let short = ["sv_cheats 1"; 60].join("; ");
        assert!(short.len() > 510, "{}", short.len());
        assert!(engine_limit_violation(&short).is_none());
        let at_limit = format!("hostname \"{}\"", "z".repeat(510 - 11));
        assert_eq!(at_limit.len(), 510);
        assert!(engine_limit_violation(&at_limit).is_none());
        let over = format!("hostname \"{}\"", "z".repeat(500));
        assert_eq!(over.len(), 511);
        let message = engine_limit_violation(&over).unwrap();
        assert!(message.contains("511 bytes") && message.contains("510"), "{message}");
        // the offender can be any segment, and a newline cuts like `;`
        assert!(engine_limit_violation(&format!("echo a; {over}")).is_some());
        assert!(engine_limit_violation(&format!("echo a\n{over}")).is_some());
        // a quoted `;` cuts too, exactly as in the guard
        let quoted = format!("say \"{};{}\"", "a".repeat(300), "b".repeat(300));
        assert!(engine_limit_violation(&quoted).is_none());
    }

    #[test]
    fn a_command_name_over_128_bytes_is_refused() {
        let message = engine_limit_violation(&format!("{} 1", "q".repeat(129))).unwrap();
        assert!(message.contains("129 bytes") && message.contains("128"), "{message}");
        assert!(engine_limit_violation(&format!("{} 1", "q".repeat(128))).is_none());
    }

    #[test]
    fn refuses_session_ending_commands() {
        for cmd in [
            "quit",
            "QUIT",
            "exit",
            "disconnect",
            "Test_ExitProcess 3",
            "echo hi; quit",
            "getpos\nquit",
            "crash",
            "+quit",
            "rcon_password hunter2",
            "sv_shutdown",
            "maps *",
            "echo a; maps de_dust2",
            "alias a \"quit\"",
            "alias a \"echo x; quit\"",
            "alias a quit",
            "\"quit\"",
            "quit()",
            "crash{}",
            "quit:x",
            "bind f1 quit",
            "bind f1 \"echo a; quit\"",
        ] {
            assert!(refusal(cmd).is_some(), "{cmd:?} should be refused");
        }
        assert!(refusal(&"x".repeat(4087)).is_some());
        assert!(refusal(&"x".repeat(4086)).is_none());
    }

    #[test]
    fn the_extra_list_refuses_map_changes_only_when_given() {
        use super::{refusal_with, MAP_CHANGING_COMMANDS};
        for cmd in [
            "map de_dust2",
            "changelevel de_dust2",
            "map_workshop 1 de_x",
            "host_workshop_map 1",
            "ds_workshop_changelevel x",
            "connect 1.2.3.4",
            "retry",
            "restart",
            "playdemo x",
            "startupmenu",
            "echo a; MAP x",
            "alias go \"map x\"",
            "map(x)",
        ] {
            assert!(refusal(cmd).is_none(), "{cmd:?} is allowed by the shared guard");
            assert!(refusal_with(cmd, MAP_CHANGING_COMMANDS).is_some(), "{cmd:?}");
        }
        for cmd in ["echo map", "maps_ok_cvar 1", "mp_restartgame 1", "sv_cheats 1"] {
            assert!(refusal_with(cmd, MAP_CHANGING_COMMANDS).is_none(), "{cmd:?}");
        }
    }

    #[test]
    fn allows_an_alias_of_an_ordinary_command() {
        assert!(refusal("alias bots \"bot_add_ct; bot_add_t\"").is_none());
    }

    #[test]
    fn a_quoted_semicolon_still_splits_and_over_refuses_on_purpose() {
        // the splitter cuts on every `;`, quoted or not: conservative, never a bypass. A quoted
        // `; quit` is refused so the guard needs no quote parsing (the engine may or may not
        // split inside quotes, and the guard must not depend on that).
        for cmd in [r#"echo "a; quit""#, r#"say "brb; exit""#] {
            assert!(refusal(cmd).is_some(), "{cmd:?} is refused by design");
        }
    }

    #[test]
    fn allows_ordinary_commands() {
        for cmd in [
            "getpos",
            "sv_cheats 1",
            "sv_cheats 1; echo ok",
            "echo maps",
            "echo quit",
            "mp_restartgame 1; echo done",
            "say exit now",
            "+forward",
        ] {
            assert!(refusal(cmd).is_none(), "{cmd:?} should pass");
        }
        assert!(refusal("   ").is_some());
    }

    #[test]
    fn cvar_names_are_validated() {
        use super::cvar_names;
        use serde_json::json;
        assert_eq!(cvar_names(&json!({"names": ["mp_c4timer"]})).unwrap(), vec!["mp_c4timer"]);
        let many: Vec<String> = (0..65).map(|i| format!("c{i}")).collect();
        for bad in [
            json!({}),
            json!({"names": []}),
            json!({"names": many}),
            json!({"names": [1]}),
            json!({"names": [""]}),
            json!({"names": ["a\nb"]}),
            json!({"names": ["a"], "x": 1}),
            json!([1]),
        ] {
            assert!(cvar_names(&bad).is_err(), "{bad}");
        }
        let sixty_four: Vec<String> = (0..64).map(|i| format!("c{i}")).collect();
        assert_eq!(cvar_names(&json!({"names": sixty_four})).unwrap().len(), 64);
    }

    #[test]
    fn secret_cvars_are_redacted_by_name() {
        use super::{is_secret_cvar, redact_cvar_reply};
        use serde_json::json;
        for n in ["rcon_password", "RCON_Password", "sv_password", "sv_setsteamaccount", "my_api_token", "x_PassWord_y"] {
            assert!(is_secret_cvar(n), "{n}");
        }
        for n in ["mp_c4timer", "sv_cheats", "sv_passwd_len"] {
            assert!(!is_secret_cvar(n), "{n}");
        }
        let mut reply = json!({"cvars": [
            {"name": "rcon_password", "found": true, "value": "hunter2", "default": "", "flags": []},
            {"name": "mp_c4timer", "found": true, "value": "40", "default": "40"},
            {"name": "sv_password", "found": true, "value": null, "default": null, "redacted": true},
            {"name": "gone_token", "found": false}
        ]});
        redact_cvar_reply(&mut reply, &[]);
        let c = &reply["cvars"];
        assert_eq!((c[0]["value"].clone(), c[0]["redacted"].clone()), (json!(null), json!(true)));
        assert_eq!(c[0]["default"], json!(null));
        assert_eq!((c[1]["value"].clone(), c[1].get("redacted")), (json!("40"), None));
        assert_eq!(c[2]["redacted"], json!(true));
        assert_eq!(c[0]["redactedBy"], json!("name"));
        assert!(c[2].get("redactedBy").is_none(), "the bridge's own redaction keeps no redactedBy");
        // a missing cvar is not reported as hidden
        assert!(c[3].get("redacted").is_none() && c[3].get("redactedBy").is_none(), "{}", c[3]);
    }
}
