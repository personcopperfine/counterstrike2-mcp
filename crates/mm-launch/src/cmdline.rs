//! The raw command line, split so the launcher can hand the child exactly the arguments it got.
//!
//! `GetCommandLineW` returns one string; there is no `argv` array to copy. The launcher stands in
//! for `cs2.exe` in the container's launch line, so the tail after `argv[0]` must reach cs2 byte
//! for byte (`-tools`, `+map de_dust2`, a quoted path with spaces, a tab separator). Rust's
//! `std::env::args` is the wrong tool: it re-splits *and* strips the quoting, which would change
//! what cs2 sees. So the string is only cut once, and the rest is passed through verbatim -
//! leading whitespace included, because that separator is part of what the child receives.

/// Split a raw Windows command line into `(argv0, tail)`.
///
/// `tail` is everything after `argv0` untouched, including the `' '`/`'\t'` that separated them
/// (empty when the line has no arguments). `argv0` is either a double-quoted run (`"C:\a b\x.exe"`,
/// the form `CreateProcessW` writes for a path with spaces) or a run up to the first space/tab.
/// An unterminated quote swallows the rest, as the platform parser does for a malformed line.
pub fn split_program(raw: &str) -> (&str, &str) {
    let start = raw.len() - raw.trim_start_matches([' ', '\t']).len();
    let rest_all = &raw[start..];
    if let Some(after_quote) = rest_all.strip_prefix('"') {
        match after_quote.find('"') {
            // `+ 2`: the opening quote and the closing one are both part of `argv0`.
            Some(close) => (&rest_all[..close + 2], &rest_all[close + 2..]),
            None => (rest_all, ""),
        }
    } else {
        match rest_all.find([' ', '\t']) {
            Some(sep) => (&rest_all[..sep], &rest_all[sep..]),
            None => (rest_all, ""),
        }
    }
}

/// Does the tail carry `-insecure` as a whole argument (case-insensitive, quotes stripped)?
///
/// The launcher injects Metamod's loader, so it refuses to start a game that could join a
/// VAC-secured session (windows-native.md §4.1): without `-insecure` there is no injection and no
/// game. The split is the platform's whitespace rule outside double quotes; a quoted
/// `"-insecure"` still counts (the engine sees the same token), `+-insecure` or `-insecurex` do
/// not.
pub fn tail_has_insecure(tail: &str) -> bool {
    let mut token = String::new();
    let mut quoted = false;
    let mut tokens = Vec::new();
    for c in tail.chars() {
        match c {
            '"' => quoted = !quoted,
            ' ' | '\t' if !quoted => {
                if !token.is_empty() {
                    tokens.push(std::mem::take(&mut token));
                }
            }
            _ => token.push(c),
        }
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    tokens.iter().any(|t| t.eq_ignore_ascii_case("-insecure"))
}

/// The command line the launcher gives `cs2.exe`: the child's own path, quoted (it may contain
/// spaces), followed by the caller's tail - or just the quoted path.
pub fn child_command_line(exe: &str, tail: &str) -> String {
    format!("\"{exe}\"{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_are_kept_in_argv0_and_the_tail_starts_after_the_closing_quote() {
        assert_eq!(
            split_program(r#""C:\Program Files\cs2\cs2.exe" -tools +map de_dust2"#),
            (r#""C:\Program Files\cs2\cs2.exe""#, " -tools +map de_dust2")
        );
        // No space at all after the closing quote: the tail is whatever follows it.
        assert_eq!(
            split_program(r#""C:\cs2.exe"-tools"#),
            (r#""C:\cs2.exe""#, "-tools")
        );
        // Nothing after it.
        assert_eq!(split_program(r#""C:\a b\cs2.exe""#), (r#""C:\a b\cs2.exe""#, ""));
    }

    #[test]
    fn an_unquoted_argv0_ends_at_the_first_space_or_tab() {
        assert_eq!(
            split_program(r"C:\game\cs2.exe -tools"),
            (r"C:\game\cs2.exe", " -tools")
        );
        assert_eq!(
            split_program("C:\\game\\cs2.exe\t-tools\t+map\tde_dust2"),
            ("C:\\game\\cs2.exe", "\t-tools\t+map\tde_dust2")
        );
        assert_eq!(split_program(r"C:\game\cs2.exe"), (r"C:\game\cs2.exe", ""));
    }

    #[test]
    fn an_empty_tail_is_kept_empty() {
        assert_eq!(split_program("cs2.exe"), ("cs2.exe", ""));
        assert_eq!(split_program(r#""cs2.exe""#), (r#""cs2.exe""#, ""));
        assert_eq!(split_program(""), ("", ""));
        assert_eq!(split_program("   \t "), ("", ""));
    }

    #[test]
    fn leading_whitespace_is_not_part_of_argv0() {
        assert_eq!(
            split_program(r#"  "C:\a b\cs2.exe" -tools"#),
            (r#""C:\a b\cs2.exe""#, " -tools")
        );
        assert_eq!(split_program(" \tcs2.exe -tools"), ("cs2.exe", " -tools"));
    }

    #[test]
    fn an_unterminated_quote_swallows_the_rest() {
        assert_eq!(
            split_program(r#""C:\a b\cs2.exe -tools"#),
            (r#""C:\a b\cs2.exe -tools"#, "")
        );
    }

    #[test]
    fn multi_byte_text_does_not_shift_the_cut() {
        // The cut is by bytes; a quote after a non-ASCII character still lands correctly.
        assert_eq!(
            split_program(r#""C:\játék\cs2.exe" -tools"#),
            (r#""C:\játék\cs2.exe""#, " -tools")
        );
    }

    #[test]
    fn insecure_must_be_a_whole_argument() {
        assert!(tail_has_insecure(" -tools -vulkan -insecure +map x"));
        assert!(tail_has_insecure("\t-INSECURE"));
        assert!(tail_has_insecure(r#" "-insecure" -tools"#));
        assert!(!tail_has_insecure(" -tools +map de_dust2"));
        assert!(!tail_has_insecure(" -insecurex +-insecure"));
        assert!(!tail_has_insecure(r#" +echo "a -insecure b""#));
        assert!(!tail_has_insecure(""));
    }

    #[test]
    fn the_child_line_is_the_quoted_exe_plus_the_tail_verbatim() {
        assert_eq!(
            child_command_line(r"C:\game\bin\win64\cs2.exe", " -tools +map de_dust2"),
            r#""C:\game\bin\win64\cs2.exe" -tools +map de_dust2"#
        );
        assert_eq!(
            child_command_line(r"C:\a b\cs2.exe", ""),
            r#""C:\a b\cs2.exe""#
        );
        // The tail keeps its own tabs and inner quotes: nothing is normalised.
        assert_eq!(
            child_command_line("cs2.exe", "\t+map\t\"de dust2\""),
            "\"cs2.exe\"\t+map\t\"de dust2\""
        );
    }
}
