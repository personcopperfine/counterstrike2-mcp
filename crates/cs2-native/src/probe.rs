//! The last readiness stage: the bridge's discovery file and one `control.hello` over loopback
//! TCP (the wire `cs2_mcp_common::bridge::client` speaks: one JSON-RPC object per line, the
//! token from the discovery file, protocol 1). Blocking std I/O with deadlines; the connection
//! is closed right after the answer (the bridge serves one client at a time, so the MCP server
//! can connect next).

use anyhow::{bail, Context};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The bridge protocol major this launcher speaks (`cs2_mcp_common::bridge::protocol::PROTOCOL`).
pub const PROTOCOL: u64 = 1;

/// `%LOCALAPPDATA%\cs2mcpbridge\session.json` / `$XDG_RUNTIME_DIR/cs2mcpbridge/session.json`
/// (fallback `~/.local/share/cs2mcpbridge/session.json`) - where the bridge writes it
/// (`crates/cs2-bridge/src/ipc/discovery.rs`).
pub fn default_discovery() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .filter(|v| !v.is_empty())
            .map(|b| PathBuf::from(b).join("cs2mcpbridge").join("session.json"))
    }
    #[cfg(not(windows))]
    {
        if let Some(x) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(x).join("cs2mcpbridge/session.json"));
        }
        std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .map(|h| PathBuf::from(h).join(".local/share/cs2mcpbridge/session.json"))
    }
}

#[derive(Debug, Clone)]
pub struct Discovery {
    pub pid: Option<u64>,
    pub host: String,
    pub port: u16,
    pub token: String,
    pub game_build: Option<String>,
    pub bridge_version: Option<String>,
    pub modified: Option<SystemTime>,
}

pub fn read_discovery(path: &Path) -> anyhow::Result<Discovery> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let v: Value = serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    if s("transport").as_deref().unwrap_or("tcp") != "tcp" {
        bail!("{}: transport is not tcp", path.display());
    }
    Ok(Discovery {
        pid: v.get("pid").and_then(Value::as_u64),
        host: s("host").unwrap_or_else(|| "127.0.0.1".into()),
        port: v.get("port").and_then(Value::as_u64).unwrap_or(0) as u16,
        token: s("token").unwrap_or_default(),
        game_build: v.get("gameBuild").map(|g| g.as_str().map(str::to_string).unwrap_or_else(|| g.to_string())),
        bridge_version: s("bridgeVersion"),
        modified: std::fs::metadata(path).and_then(|m| m.modified()).ok(),
    })
}

/// The hello answer, trimmed to what the launcher reports.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    pub protocol: u64,
    pub game_build: Option<String>,
    pub bridge_version: Option<String>,
    pub methods: usize,
}

/// Connect, send `control.hello`, read the answer.
pub fn hello(d: &Discovery, timeout: Duration) -> anyhow::Result<Hello> {
    if d.port == 0 {
        bail!("discovery has no port");
    }
    let addr = (d.host.as_str(), d.port)
        .to_socket_addrs()?
        .next()
        .context("discovery host does not resolve")?;
    let sock = TcpStream::connect_timeout(&addr, timeout).with_context(|| format!("connect {addr}"))?;
    sock.set_read_timeout(Some(timeout))?;
    sock.set_write_timeout(Some(timeout))?;
    let req = json!({
        "jsonrpc": "2.0", "id": "native-h1", "method": "control.hello",
        "params": {"protocol": PROTOCOL, "token": d.token,
                   "client": {"name": "cs2mcp-native", "version": env!("CARGO_PKG_VERSION")},
                   "deadlineMs": timeout.as_millis() as u64},
        "bridge": {"protocol": PROTOCOL}
    });
    let mut line = serde_json::to_vec(&req)?;
    line.push(b'\n');
    (&sock).write_all(&line)?;
    let mut reader = BufReader::new(&sock);
    loop {
        let mut buf = String::new();
        if reader.read_line(&mut buf)? == 0 {
            bail!("bridge closed the connection before answering hello");
        }
        if buf.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(buf.trim()).context("bridge sent a non-JSON line")?;
        let id = v.get("id").cloned().unwrap_or(Value::Null);
        if let Some(err) = v.get("error") {
            if id.is_null() || id == json!("native-h1") {
                bail!("bridge refused hello: {err}");
            }
        }
        if id != json!("native-h1") {
            continue; // a notification
        }
        let r = v.get("result").context("hello answer has no result")?;
        let protocol = r.get("protocol").and_then(Value::as_u64).unwrap_or(0);
        if protocol != PROTOCOL {
            bail!("bridge protocol {protocol} != {PROTOCOL}");
        }
        let _ = sock.shutdown(std::net::Shutdown::Both);
        return Ok(Hello {
            protocol,
            game_build: r.get("gameBuild").map(|g| g.as_str().map(str::to_string).unwrap_or_else(|| g.to_string())),
            bridge_version: r.get("bridgeVersion").and_then(Value::as_str).map(str::to_string)
                .or_else(|| d.bridge_version.clone()),
            methods: r.pointer("/capabilities/methods").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn hello_against_a_fake_bridge() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let t = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut r = BufReader::new(&s);
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            let v: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(v["method"], "control.hello");
            assert_eq!(v["params"]["token"], "tok");
            let note = "{\"jsonrpc\":\"2.0\",\"method\":\"event.x\"}\n";
            let ans = format!("{note}{{\"jsonrpc\":\"2.0\",\"id\":\"native-h1\",\"result\":{{\"protocol\":1,\"gameBuild\":\"2000940\",\"capabilities\":{{\"methods\":[\"a\",\"b\"]}}}}}}\n");
            (&s).write_all(ans.as_bytes()).unwrap();
        });
        let d = Discovery { pid: None, host: "127.0.0.1".into(), port, token: "tok".into(), game_build: None, bridge_version: Some("m3".into()), modified: None };
        let h = hello(&d, Duration::from_secs(5)).unwrap();
        t.join().unwrap();
        assert_eq!(h.game_build.as_deref(), Some("2000940"));
        assert_eq!(h.methods, 2);
        assert_eq!(h.bridge_version.as_deref(), Some("m3"));
    }

    #[test]
    fn discovery_file_fields() {
        let dir = crate::testutil::scratch("probe-disc");
        let p = dir.join("session.json");
        std::fs::write(&p, "{\n  \"pid\": 42,\n  \"protocol\": 1,\n  \"bridgeVersion\": \"m3.17\",\n  \"gameBuild\": \"2000940\",\n  \"transport\": \"tcp\",\n  \"host\": \"127.0.0.1\",\n  \"port\": 5555,\n  \"pipeName\": null,\n  \"token\": \"ab\",\n  \"startedAt\": 1\n}\n").unwrap();
        let d = read_discovery(&p).unwrap();
        assert_eq!((d.pid, d.port, d.token.as_str()), (Some(42), 5555, "ab"));
        assert!(d.modified.is_some());
    }
}
