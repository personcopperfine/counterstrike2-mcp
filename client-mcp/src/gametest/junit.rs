//! JUnit XML for stored `game_test` results: one testsuite per spec, one testcase per record
//! (`phase.label`, suffixed `#n` when the name repeats in the suite), a `failure` for a failed step
//! (its error and failing assertions), a `skipped` for a step that did not run. Every text and
//! attribute is XML-escaped and no timestamp is written, so the output for the same results is stable.
use serde_json::Value;
use std::collections::HashMap;

pub fn render(results: &[Value]) -> String {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites>\n");
    for r in results {
        suite(&mut out, r);
    }
    out.push_str("</testsuites>\n");
    out
}

fn suite(out: &mut String, r: &Value) {
    let name = r["name"].as_str().unwrap_or("game_test");
    let steps = r["steps"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let failures = steps.iter().filter(|s| !ok(s) && !skipped(s)).count();
    let skip_count = steps.iter().filter(|s| skipped(s)).count();
    out.push_str(&format!(
        "  <testsuite name=\"{}\" tests=\"{}\" failures=\"{}\" errors=\"0\" skipped=\"{}\" time=\"{}\">\n",
        escape(name),
        steps.len(),
        failures,
        skip_count,
        secs(r["durationMs"].as_u64().unwrap_or(0))
    ));
    out.push_str("    <properties>\n");
    property(out, "passed", &text(&r["passed"]));
    property(out, "pending", &text(&r["pending"]));
    property(out, "ticks.start", &text(&r["ticks"]["start"]));
    property(out, "ticks.end", &text(&r["ticks"]["end"]));
    property(out, "timeoutMs", &text(&r["timeoutMs"]));
    if let Some(path) = r["demo"]["path"].as_str() {
        property(out, "demo.path", path);
    }
    if let Some(next) = r["next"].as_str() {
        property(out, "next", next);
    }
    out.push_str("    </properties>\n");
    let mut seen: HashMap<String, usize> = HashMap::new();
    for s in steps {
        let base = format!(
            "{}.{}",
            s["phase"].as_str().unwrap_or("step"),
            s["label"].as_str().unwrap_or("step")
        );
        let n = seen
            .entry(base.clone())
            .and_modify(|n| *n += 1)
            .or_insert(1);
        let case = if *n == 1 { base } else { format!("{base}#{n}") };
        testcase(out, name, &case, s);
    }
    out.push_str("  </testsuite>\n");
}

fn testcase(out: &mut String, suite: &str, case: &str, s: &Value) {
    let time = secs(s["ms"].as_u64().unwrap_or(0));
    out.push_str(&format!(
        "    <testcase classname=\"{}\" name=\"{}\" time=\"{}\"",
        escape(suite),
        escape(case),
        time
    ));
    if ok(s) {
        out.push_str("/>\n");
        return;
    }
    out.push_str(">\n");
    if skipped(s) {
        let why = s["skipReason"].as_str().unwrap_or("not run");
        out.push_str(&format!("      <skipped message=\"{}\"/>\n", escape(why)));
    } else {
        let lines = failure_lines(s);
        let message = lines.join("; ");
        out.push_str(&format!(
            "      <failure message=\"{}\" type=\"{}\">{}</failure>\n",
            escape(&message),
            escape(s["tool"].as_str().unwrap_or("step")),
            escape(&lines.join("\n"))
        ));
    }
    out.push_str("    </testcase>\n");
}

/// The error (if any: a tool error object, or a plain string) and one line per failing assertion.
fn failure_lines(s: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    match &s["error"] {
        Value::Null => {}
        Value::String(e) => lines.push(e.clone()),
        obj => lines.push(format!(
            "{}: {}",
            obj["type"].as_str().unwrap_or("Error"),
            obj["message"].as_str().unwrap_or("")
        )),
    }
    for a in s["assertions"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
        if ok(a) {
            continue;
        }
        let mut line = format!(
            "{} {} wanted {} got {}",
            a["path"].as_str().unwrap_or("?"),
            a["op"].as_str().unwrap_or("?"),
            text(&a["wanted"]),
            text(&a["got"])
        );
        if let Some(e) = a["error"].as_str() {
            line.push_str(&format!(" ({e})"));
        }
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push("failed".to_string());
    }
    lines
}

fn property(out: &mut String, name: &str, value: &str) {
    out.push_str(&format!(
        "      <property name=\"{}\" value=\"{}\"/>\n",
        escape(name),
        escape(value)
    ));
}

fn ok(v: &Value) -> bool {
    v["ok"].as_bool() == Some(true)
}

fn skipped(v: &Value) -> bool {
    v["skipped"].as_bool() == Some(true)
}

/// Compact JSON for a value (strings as they are, null as `null`).
fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn secs(ms: u64) -> String {
    format!("{}.{:03}", ms / 1000, ms % 1000)
}

/// XML-escapes text and attribute values; characters XML 1.0 forbids become U+FFFD.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if is_xml_char(c) => out.push(c),
            _ => out.push('\u{FFFD}'),
        }
    }
    out
}

fn is_xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Value {
        json!([{
            "name": "walk_forward",
            "passed": false,
            "pending": false,
            "durationMs": 1534,
            "ticks": {"start": 100, "end": 180},
            "timeoutMs": 120000,
            "demo": {"name": "walk_forward", "path": "/x/a&b.dem", "ticks": {"start": 100, "end": 180}},
            "steps": [
                {"phase": "setup", "label": "round", "tool": "round_start", "ok": true, "ms": 12, "assertions": []},
                {"phase": "steps", "label": "walk", "tool": "input_sequence", "ok": false, "ms": 900, "assertions": [
                    {"path": "totals.distance", "op": "ge", "wanted": 100, "got": 42.5, "ok": false, "from": "result"},
                    {"path": "totals.ticks", "op": "eq", "wanted": 64, "got": 64, "ok": true, "from": "result"}
                ]},
                {"phase": "steps", "label": "shoot", "tool": "fire_weapon", "ok": false, "ms": 0, "skipped": true, "skipReason": "not run: an earlier step stopped the test", "assertions": []},
                {"phase": "steps", "label": "say", "tool": "console_exec", "ok": false, "ms": 3, "error": {"type": "WaitTimeout", "message": "\"<bad> & 'quote'\""}, "assertions": []},
                {"phase": "teardown", "label": "input_clear", "tool": "input_clear", "ok": true, "ms": 4, "assertions": []}
            ]
        }])
    }

    #[test]
    fn escape_covers_markup_and_forbidden_control_characters() {
        assert_eq!(
            escape("<a & 'b' \"c\">"),
            "&lt;a &amp; &apos;b&apos; &quot;c&quot;&gt;"
        );
        assert_eq!(escape("x\u{1}y"), "x\u{FFFD}y");
    }

    #[test]
    fn junit_golden() {
        let xml = render(sample().as_array().unwrap());
        let expected = concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
            "<testsuites>\n",
            "  <testsuite name=\"walk_forward\" tests=\"5\" failures=\"2\" errors=\"0\" skipped=\"1\" time=\"1.534\">\n",
            "    <properties>\n",
            "      <property name=\"passed\" value=\"false\"/>\n",
            "      <property name=\"pending\" value=\"false\"/>\n",
            "      <property name=\"ticks.start\" value=\"100\"/>\n",
            "      <property name=\"ticks.end\" value=\"180\"/>\n",
            "      <property name=\"timeoutMs\" value=\"120000\"/>\n",
            "      <property name=\"demo.path\" value=\"/x/a&amp;b.dem\"/>\n",
            "    </properties>\n",
            "    <testcase classname=\"walk_forward\" name=\"setup.round\" time=\"0.012\"/>\n",
            "    <testcase classname=\"walk_forward\" name=\"steps.walk\" time=\"0.900\">\n",
            "      <failure message=\"totals.distance ge wanted 100 got 42.5\" type=\"input_sequence\">totals.distance ge wanted 100 got 42.5</failure>\n",
            "    </testcase>\n",
            "    <testcase classname=\"walk_forward\" name=\"steps.shoot\" time=\"0.000\">\n",
            "      <skipped message=\"not run: an earlier step stopped the test\"/>\n",
            "    </testcase>\n",
            "    <testcase classname=\"walk_forward\" name=\"steps.say\" time=\"0.003\">\n",
            "      <failure message=\"WaitTimeout: &quot;&lt;bad&gt; &amp; &apos;quote&apos;&quot;\" type=\"console_exec\">WaitTimeout: &quot;&lt;bad&gt; &amp; &apos;quote&apos;&quot;</failure>\n",
            "    </testcase>\n",
            "    <testcase classname=\"walk_forward\" name=\"teardown.input_clear\" time=\"0.004\"/>\n",
            "  </testsuite>\n",
            "</testsuites>\n",
        );
        assert_eq!(xml, expected);
    }

    #[test]
    fn repeated_names_are_suffixed() {
        let xml = render(
            json!([{"name": "d", "steps": [
                {"phase": "steps", "label": "x", "tool": "game_state", "ok": true, "ms": 1},
                {"phase": "steps", "label": "x", "tool": "game_state", "ok": true, "ms": 1},
                {"phase": "steps", "label": "x", "tool": "game_state", "ok": true, "ms": 1}
            ]}])
            .as_array()
            .unwrap(),
        );
        assert!(xml.contains("name=\"steps.x\""));
        assert!(xml.contains("name=\"steps.x#2\""));
        assert!(xml.contains("name=\"steps.x#3\""));
    }

    #[test]
    fn output_is_well_formed_for_empty_and_pending_results() {
        let xml = render(
            json!([{"name": "p", "pending": true, "next": "again & more", "steps": []}])
                .as_array()
                .unwrap(),
        );
        assert!(xml.contains(
            "<testsuite name=\"p\" tests=\"0\" failures=\"0\" errors=\"0\" skipped=\"0\""
        ));
        assert!(xml.contains("name=\"next\" value=\"again &amp; more\""));
        assert_eq!(
            render(&[]),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites>\n</testsuites>\n"
        );
    }
}
