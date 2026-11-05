//! Size guard for `tools/list` (streamline WP2.2, workflows/streamline/PLAN.md). The list is paid
//! for in every agent's context on every session; it was 63 KB before the description pass and
//! 67 KB at its start. A later change that needs more words must trim others or raise these
//! limits on purpose, in review, not by accident.
use serde_json::Value;

mod common;

/// Compact JSON bytes of the whole `tools/list` tools array as served (annotations included):
/// 31637 B measured with toolsdump.py at WP2.2 review fixes, rounded up to 32000.
const MAX_TOOLS_LIST_BYTES: usize = 32_000;
/// One tool description (what it does, "Not for", units, traps; history goes to client-mcp/AGENT.md).
const MAX_TOOL_DESCRIPTION_CHARS: usize = 700;
/// One schema property description (use enums/min/max instead of prose).
const MAX_PROPERTY_DESCRIPTION_CHARS: usize = 120;

fn server() -> cs2_mcp_common::mcp::McpServer<client_mcp::state::ClientState> {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: client_mcp::state::ClientConfig,
    }
    let mut argv = vec!["t"];
    argv.extend(common::hermetic(&[]));
    client_mcp::build_server(&W::parse_from(argv).c).unwrap()
}

fn property_descriptions(schema: &Value, path: &str, out: &mut Vec<(String, usize)>) {
    match schema {
        Value::Object(m) => {
            for (k, v) in m {
                if k == "description" {
                    if let Value::String(s) = v {
                        out.push((path.to_string(), s.chars().count()));
                    }
                } else if k == "properties" {
                    if let Value::Object(props) = v {
                        for (name, p) in props {
                            property_descriptions(p, &format!("{path}.{name}"), out);
                        }
                    }
                } else {
                    property_descriptions(v, path, out);
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|v| property_descriptions(v, path, out)),
        _ => {}
    }
}

#[test]
fn tools_list_stays_small() {
    let server = server();
    // The real wire form, annotations included (`McpServer::tools_list_wire`).
    let wire = server.tools_list_wire();
    let total = serde_json::to_string(&wire).unwrap().len();
    let mut tool_over = Vec::new();
    let mut prop_over = Vec::new();
    for spec in server.registry().describe() {
        let n = spec.description.chars().count();
        if n > MAX_TOOL_DESCRIPTION_CHARS {
            tool_over.push(format!("{} ({n} chars)", spec.name));
        }
        let mut props = Vec::new();
        property_descriptions(&spec.input_schema, "", &mut props);
        for (p, n) in props {
            if n > MAX_PROPERTY_DESCRIPTION_CHARS {
                prop_over.push(format!("{}{p} ({n} chars)", spec.name));
            }
        }
    }
    assert!(
        tool_over.is_empty(),
        "tool descriptions over {MAX_TOOL_DESCRIPTION_CHARS} chars: {tool_over:?}"
    );
    assert!(
        prop_over.is_empty(),
        "property descriptions over {MAX_PROPERTY_DESCRIPTION_CHARS} chars: {prop_over:?}"
    );
    assert!(
        total <= MAX_TOOLS_LIST_BYTES,
        "tools/list is {total} bytes, limit {MAX_TOOLS_LIST_BYTES}"
    );
}
