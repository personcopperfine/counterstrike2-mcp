//! The game command line, built in one place. `-insecure` is part of the fixed head and cannot
//! be removed by the caller: injected code (Metamod, the bridge, mmload) must never run in a
//! VAC-secured session (windows-native.md §4.1; README "VAC").

use crate::install::Os;
use anyhow::bail;

/// What the caller chose.
#[derive(Debug, Clone, Default)]
pub struct GameArgs {
    pub tools: bool,
    pub map: Option<String>,
    pub addon: Option<String>,
    /// after `--`
    pub extra: Vec<String>,
}

/// Engine flags that keep a session rendering at full speed without focus
/// (`cs2_mcp_common::runtime::docker::FOCUS_ARGS`, measured there).
pub const FOCUS_ARGS: [&str; 4] = ["+engine_no_focus_sleep", "0", "+r_always_render_all_windows", "1"];

/// The one flag every launch carries.
pub const INSECURE: &str = "-insecure";

/// Is `-insecure` among `args` (exact token, case-insensitive like the engine's parser)?
pub fn has_insecure<S: AsRef<str>>(args: &[S]) -> bool {
    args.iter().any(|a| a.as_ref().eq_ignore_ascii_case(INSECURE))
}

/// The full argument list after the program name, in the containers' order:
/// `[-tools] [-vulkan] -console -condebug -insecure [-addon a] [+map m] <extra> <focus flags>`.
/// Refuses extra arguments that would start a VAC-secured session on purpose (`-secure`).
pub fn build(os: Os, a: &GameArgs) -> anyhow::Result<Vec<String>> {
    for x in &a.extra {
        if x.eq_ignore_ascii_case("-secure") {
            bail!("refusing `-secure`: sessions with injected code always run with -insecure (VAC off)");
        }
    }
    let mut v: Vec<String> = Vec::new();
    if a.tools {
        v.push("-tools".into());
    }
    if os == Os::Windows {
        v.push("-vulkan".into());
    }
    v.extend(["-console", "-condebug", INSECURE].map(String::from));
    if let Some(addon) = &a.addon {
        v.push("-addon".into());
        v.push(addon.clone());
    }
    if let Some(map) = &a.map {
        v.push("+map".into());
        v.push(map.clone());
    }
    v.extend(a.extra.iter().cloned());
    v.extend(FOCUS_ARGS.map(String::from));
    debug_assert!(has_insecure(&v));
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insecure_is_always_there_and_secure_is_refused() {
        let a = GameArgs { tools: true, map: Some("de_dust2".into()), addon: None, extra: vec!["+sv_cheats".into(), "1".into()] };
        assert_eq!(
            build(Os::Windows, &a).unwrap(),
            [
                "-tools", "-vulkan", "-console", "-condebug", "-insecure", "+map", "de_dust2", "+sv_cheats", "1",
                "+engine_no_focus_sleep", "0", "+r_always_render_all_windows", "1"
            ]
        );
        let linux = build(Os::Linux, &GameArgs::default()).unwrap();
        assert!(has_insecure(&linux));
        assert!(!linux.contains(&"-vulkan".to_string()));
        let bad = GameArgs { extra: vec!["-SECURE".into()], ..Default::default() };
        assert!(build(Os::Windows, &bad).is_err());
    }
}
