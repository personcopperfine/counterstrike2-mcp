//! Syntax errors: the message, and the line/column the caller (and the user) sees.

use cs2_kv3::{parse, write, ParseError, MAX_DEPTH};

const HDR: &str = "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} \
                   format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->";

/// `body` on its own lines after the header, CRLF.
fn file(body: &[&str]) -> String {
    let mut text = String::from(HDR);
    text.push_str("\r\n");
    for line in body {
        text.push_str(line);
        text.push_str("\r\n");
    }
    text
}

fn failure(text: &str) -> ParseError {
    parse(text).expect_err("should not parse")
}

fn at(error: &ParseError, line: usize, col: usize) {
    assert_eq!(
        (error.line, error.col),
        (line, col),
        "position of {error:?} ({error})"
    );
}

#[test]
fn missing_header() {
    let error = failure("{\r\n}");
    at(&error, 1, 1);
    assert!(error.msg.contains("kv3 header"), "{}", error.msg);

    let error = failure("<!-- not kv3 -->\r\n{\r\n}");
    assert!(error.msg.contains("kv3"), "{}", error.msg);
}

#[test]
fn body_must_be_one_object() {
    let error = failure(&format!("{HDR}\r\n[ ]"));
    at(&error, 2, 1);
    assert!(error.msg.contains("`{`"), "{}", error.msg);

    let error = failure(&format!("{HDR}\r\n{{ }}\r\n1"));
    at(&error, 3, 1);
    assert!(error.msg.contains("trailing"), "{}", error.msg);
}

#[test]
fn object_entry_errors() {
    let error = failure(&file(&["{", "\ta 1", "}"]));
    at(&error, 3, 4);
    assert!(error.msg.contains("`=`"), "{}", error.msg);

    let error = failure(&file(&["{", "\ta = ", "}"]));
    at(&error, 4, 1);
    assert!(error.msg.contains("expected a value"), "{}", error.msg);

    let error = failure(&file(&["{", "\t1 = 2", "}"]));
    at(&error, 3, 2);
    // Same refusal as 686975d ("expected a key", for its `Number` token); a digit-leading token
    // is not a key either way, so only the wording differs (kept: upstream's own message shape).
    assert!(error.msg.contains("invalid key"), "{}", error.msg);

    let error = failure(&file(&["{", "\ta = foo", "}"]));
    at(&error, 3, 6);
    assert!(error.msg.contains("is not a value"), "{}", error.msg);
}

#[test]
fn string_errors() {
    // A `"…"` string keeps running over real newlines (upstream allows them), so an
    // unterminated one swallows the rest of the file and is only reported at the end.
    let error = failure(&file(&["{", "\ta = \"unterminated", "}"]));
    at(&error, 3, 6);
    assert!(error.msg.contains("unterminated string"), "{}", error.msg);

    // A `"…"` string that runs to the end of the file.
    let error = failure(&format!("{HDR}\r\n{{\r\n\ta = \"never closed at all"));
    at(&error, 3, 6);
    assert!(error.msg.contains("unterminated string"), "{}", error.msg);

    // An unterminated `"""` string (686975d's rule: it closes at the first `"""`, so the only
    // error left is running out of file).
    let error = failure(&file(&["{", "\ta = \"\"\"never closed", "}"]));
    at(&error, 3, 6);
    assert!(error.msg.contains("unterminated"), "{}", error.msg);
    assert!(error.msg.contains("\"\"\""), "{}", error.msg);
}

#[test]
fn number_and_character_errors() {
    // An unquoted value that is neither a literal nor a number (686975d said `` `…` is not a
    // kv3 number `` for the number-shaped tokens; the union keeps upstream's value-level
    // refusal, which fires for exactly the same inputs).
    let error = failure(&file(&["{", "\ta = 1.2.3", "}"]));
    at(&error, 3, 6);
    assert!(error.msg.contains("is not a value"), "{}", error.msg);

    let error = failure(&file(&["{", "\ta = 1e", "}"]));
    at(&error, 3, 6);
    assert!(error.msg.contains("is not a value"), "{}", error.msg);

    // Junk that cannot start a token is refused by the lexer, as 686975d did (the unquoted
    // run is 686975d's identifier/number shape, not upstream's "any printable ASCII").
    let error = failure(&file(&["{", "\ta = @", "}"]));
    at(&error, 3, 6);
    assert!(error.msg.contains("unexpected character"), "{}", error.msg);

    let error = failure(&file(&["{", "\ta = é", "}"]));
    at(&error, 3, 6);
    assert!(error.msg.contains("unexpected character"), "{}", error.msg);
}

#[test]
fn array_and_comment_errors() {
    let error = failure(&file(&["{", "\ta = [ 1, 2", "}"]));
    at(&error, 4, 1);
    assert!(error.msg.contains("`]`"), "{}", error.msg);

    let error = failure(&file(&["{", "\ta = /* never closed", "}"]));
    at(&error, 3, 6);
    assert!(error.msg.contains("unterminated `/*`"), "{}", error.msg);

    let error = failure(&file(&["{", "\ta = 1 // comment to eof"]));
    assert!(error.msg.contains("`}`"), "{}", error.msg);
}

#[test]
fn blob_errors() {
    // An odd nibble count is refused (as 686975d said; upstream refuses it too at the token).
    let error = failure(&file(&["{", "\ta = #[ 41 4", "\t]"]));
    at(&error, 3, 6);
    assert!(error.msg.contains("odd number of hex digits"), "{}", error.msg);

    let error = failure(&file(&["{", "\ta = #[ 4Z ]", "}"]));
    at(&error, 3, 10);
    assert!(error.msg.contains("hex digit"), "{}", error.msg);

    let error = failure(&file(&["{", "\ta = #[ 41 42"]));
    at(&error, 3, 6);
    assert!(error.msg.contains("unterminated `#[` blob"), "{}", error.msg);

    let error = failure(&file(&["{", "\ta = #x", "}"]));
    at(&error, 3, 6);
    assert!(error.msg.contains("expected `#[`"), "{}", error.msg);
}

#[test]
fn error_display_and_equality() {
    let error = failure(&file(&["{", "\ta 1", "}"]));
    assert_eq!(error.to_string(), format!("line {}, column {}: {}", error.line, error.col, error.msg));
    let clone = error.clone();
    assert_eq!(error, clone);
    let boxed: Box<dyn std::error::Error> = Box::new(error);
    assert!(boxed.to_string().contains("line 3"));
}

#[test]
fn a_utf8_bom_is_tolerated() {
    let text = format!("\u{feff}{HDR}\r\n{{\r\n}}");
    parse(&text).expect("a BOM before the header is fine");
}

// ---------------------------------------------------------------- nesting depth (MAX_DEPTH)

/// One body line `{ a = <value> }` after the header, so every token of the value sits on line 2
/// at column 7 + (its offset inside `<value>`): `{ a = ` is 6 characters.
fn one_line(value: &str) -> String {
    file(&[&format!("{{ a = {value} }}")])
}

/// `n` arrays nested in each other: `[[[…]]]`.
fn nested_arrays(n: usize) -> String {
    format!("{}{}", "[".repeat(n), "]".repeat(n))
}

/// `n` objects nested in each other around a number: `{ a = { a = … 1 … } }`.
fn nested_objects(n: usize) -> String {
    format!("{}1{}", "{ a = ".repeat(n), " }".repeat(n))
}

/// `n` flags on one value: `f:f:…:1` (any identifier is a flag name again, 686975d behaviour).
fn flag_chain(n: usize) -> String {
    format!("{}1", "f:".repeat(n))
}

/// The cap counts **open nesting levels**: the root object is level 1 (this is exactly how
/// `KV3_CHECK_DEPTH_CAP` counts in `bridge/kv3_check.h`, over the raw token stream), so a value
/// directly under a root key starts at level 2.
#[test]
fn nesting_at_the_cap_parses() {
    // `{` + MAX_DEPTH - 1 arrays: the innermost `[` is the MAX_DEPTH-th open level.
    let text = one_line(&nested_arrays(MAX_DEPTH - 1));
    let document = parse(&text).expect("the cap level itself is accepted");

    let mut value = document.root.get("a").expect("the `a` key");
    let mut level = 1; // the root object
    while let Some(items) = value.as_array() {
        level += 1;
        if items.is_empty() {
            break;
        }
        assert_eq!(items.len(), 1, "one array per level");
        value = &items[0];
    }
    assert_eq!(level, MAX_DEPTH, "the innermost array is the MAX_DEPTH-th level");

    let written = write(&document);
    assert_eq!(parse(&written).expect("the written form parses"), document, "round trip");
}

#[test]
fn nesting_past_the_cap_is_refused() {
    // One array more: the MAX_DEPTH-th `[` would open level MAX_DEPTH + 1, and that `[` — the
    // k-th bracket sits at column 6 + k, `{ a = ` being 6 characters — is the token it points at.
    let text = one_line(&nested_arrays(MAX_DEPTH));
    let error = failure(&text);
    at(&error, 2, 6 + MAX_DEPTH);
    assert!(error.msg.contains("nesting"), "{}", error.msg);
    assert!(error.msg.contains("256"), "{}", error.msg);
}

#[test]
fn object_nesting_past_the_cap_is_refused() {
    // `{ a = ` is 6 characters per level, so the k-th nested `{` sits at column 7 + 6 * (k - 1).
    let text = one_line(&nested_objects(MAX_DEPTH));
    let error = failure(&text);
    at(&error, 2, 7 + 6 * (MAX_DEPTH - 1));
    assert!(error.msg.contains("256"), "{}", error.msg);

    // One object less is the last accepted file: MAX_DEPTH levels, root included.
    parse(&one_line(&nested_objects(MAX_DEPTH - 1))).expect("one object less is at the cap");
}

#[test]
fn flag_chain_past_the_cap_is_refused() {
    // A flag counts as a level too (the recursion in `value()` is the same shape), so the chain
    // and the containers share the MAX_DEPTH budget; `f:` is 2 characters per level.
    parse(&one_line(&flag_chain(MAX_DEPTH - 1))).expect("a chain at the cap is accepted");

    let text = one_line(&flag_chain(MAX_DEPTH));
    let error = failure(&text);
    at(&error, 2, 5 + 2 * MAX_DEPTH);
    assert!(error.msg.contains("256"), "{}", error.msg);
}

#[test]
fn a_hundred_thousand_brackets_do_not_overflow() {
    // A depth bomb has to fail as a parse error, not as a stack overflow. Run it on a thread with
    // the stack a tokio worker actually gets (2 MB) rather than the test harness' 8 MB main
    // thread, so a regression (the cap removed or raised) shows up as a crash. Measured cost of a
    // capped parse: ~4.1 KB per nesting level in this debug build (255 levels = ~1.1 MB) and
    // ~0.45 KB in release, where the server runs. A 512 KB thread cannot hold 256 levels of *any*
    // recursive parser in debug — that number comes from the release profile, not from the cap.
    let text = one_line(&nested_arrays(100_000));
    let parsed = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .name("depth-bomb".to_string())
        .spawn(move || parse(&text))
        .expect("spawn the depth-bomb thread")
        .join()
        .expect("the parser must not overflow a tokio-sized stack");
    let error = parsed.expect_err("100_000 brackets must not parse");
    assert!(error.msg.contains("nesting deeper than 256 levels"), "{}", error.msg);
    at(&error, 2, 6 + MAX_DEPTH);
}
