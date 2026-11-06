//! The few bridge calls the runtime makes itself: `control.hello` (status's liveness line), the
//! tool's active-document method (`open` waits for the document) and `control.capabilities`
//! (`open` waits for the module identity checks). NDJSON over the TCP port the discovery file
//! names, one reply line per request, as the script's inline Python did.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};

/// The discovery file's fields the client needs (`crates/cs2-bridge/src/ipc/discovery.rs`).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Discovery {
    pub host: String,
    pub port: u16,
    pub token: String,
}

pub fn read_discovery(path: &Path) -> Result<Discovery> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// An authenticated connection; `hello` is the `control.hello` reply.
pub struct Session {
    stream: TcpStream,
    reader: BufReader<TcpStream>,
    pub hello: Value,
}

impl Session {
    pub fn connect(disc: &Discovery, timeout: Duration) -> Result<Self> {
        let addr = (disc.host.as_str(), disc.port)
            .to_socket_addrs()?
            .next()
            .with_context(|| format!("{}:{} resolves to no address", disc.host, disc.port))?;
        let stream = TcpStream::connect_timeout(&addr, timeout)
            .with_context(|| format!("connecting to {addr}"))?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let reader = BufReader::new(stream.try_clone()?);
        let mut session = Session {
            stream,
            reader,
            hello: Value::Null,
        };
        session.hello = session.call(
            "h",
            "control.hello",
            json!({ "protocol": 1, "token": disc.token }),
        )?;
        Ok(session)
    }

    /// One request, one reply line.
    pub fn call(&mut self, id: &str, method: &str, params: Value) -> Result<Value> {
        let mut line =
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string();
        line.push('\n');
        self.stream.write_all(line.as_bytes())?;
        let mut reply = String::new();
        if self.reader.read_line(&mut reply)? == 0 {
            anyhow::bail!("the bridge closed the connection");
        }
        serde_json::from_str(&reply)
            .with_context(|| format!("{method}: not a JSON reply: {}", reply.trim()))
    }
}

/// The open document's id from an active-document reply, or `None` (no document, or an error
/// reply such as `-32017`, which is not an open document either).
pub fn open_doc(reply: &Value) -> Option<String> {
    let doc = reply.get("result")?.get("doc")?.as_object()?;
    Some(match doc.get("id") {
        Some(Value::String(id)) => id.clone(),
        Some(other) => other.to_string(),
        None => "?".into(),
    })
}

/// Where the module identity checks are.
#[derive(Debug, PartialEq, Eq)]
pub enum Modules {
    /// Still verifying, or a module every session loads is not loaded yet.
    Pending(Vec<String>),
    /// Terminal: `name=state` for every module.
    Done(String),
}

/// The modules every tool session loads, plus its own DLL. The rest (pet, the other tool, ...)
/// load on demand, so `not_loaded` there is a final state, not a pending one.
const CORE: &[&str] = &[
    "tier0.dll",
    "engine2.dll",
    "toolframework2.dll",
    "scenesystem.dll",
    "schemasystem.dll",
    "rendersystemvulkan.dll",
];

/// A `control.capabilities` reply's module states, judged for a session whose tool DLL is
/// `tool_dll`.
pub fn modules_progress(reply: &Value, tool_dll: &str) -> Result<Modules> {
    let modules = reply
        .pointer("/result/modules")
        .and_then(Value::as_object)
        .context("no result.modules in the capabilities reply")?;
    let mut states = Vec::new();
    let mut pending = Vec::new();
    for (name, module) in modules {
        let state = module.get("state").and_then(Value::as_str).unwrap_or("?");
        let core = CORE.contains(&name.as_str()) || name == tool_dll;
        if state == "verifying" || (state == "not_loaded" && core) {
            pending.push(name.clone());
        }
        states.push(format!("{name}={state}"));
    }
    Ok(if pending.is_empty() {
        Modules::Done(states.join(","))
    } else {
        Modules::Pending(pending)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_reply() {
        assert_eq!(
            open_doc(&json!({"result": {"doc": {"id": "d1"}}})).as_deref(),
            Some("d1")
        );
        assert_eq!(
            open_doc(&json!({"result": {"doc": {"id": 7}}})).as_deref(),
            Some("7")
        );
        assert_eq!(
            open_doc(&json!({"result": {"doc": {}}})).as_deref(),
            Some("?")
        );
        assert_eq!(open_doc(&json!({"result": {"doc": null}})), None);
        assert_eq!(open_doc(&json!({"error": {"code": -32017}})), None);
    }

    #[test]
    fn module_states() {
        let reply = |m: Value| json!({"result": {"modules": m}});
        let done =
            reply(json!({"tier0.dll": {"state": "verified"}, "pet.dll": {"state": "not_loaded"}}));
        assert_eq!(
            modules_progress(&done, "hammer.dll").unwrap(),
            Modules::Done("pet.dll=not_loaded,tier0.dll=verified".into())
        );
        let tool_missing = reply(json!({"hammer.dll": {"state": "not_loaded"}}));
        assert_eq!(
            modules_progress(&tool_missing, "hammer.dll").unwrap(),
            Modules::Pending(vec!["hammer.dll".into()])
        );
        // the other tool's DLL is on demand
        assert!(matches!(
            modules_progress(&tool_missing, "modeldoc_editor.dll").unwrap(),
            Modules::Done(_)
        ));
        let verifying = reply(json!({"pet.dll": {"state": "verifying"}}));
        assert!(matches!(
            modules_progress(&verifying, "hammer.dll").unwrap(),
            Modules::Pending(_)
        ));
        assert!(modules_progress(&json!({"error": {}}), "hammer.dll").is_err());
    }
}
