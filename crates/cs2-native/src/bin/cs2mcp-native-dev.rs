//! `cs2mcp-native-dev`: the live-check helpers that used to hide in `cs2mcp-native` (FIXME 15).
//! A developer tool: the packaging step (`scripts/package.sh`) does not ship it. Results go to
//! stdout as JSON, progress to stderr.

use clap::{Parser, Subcommand};
use cs2_native::manifest;
use cs2_native::events::Events;
use cs2_native::session::emit;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(name = "cs2mcp-native-dev", version, about = "cs2mcp-native's diagnostics for live checks (tree hashes, VToolMessage posts, steam_interfaces.txt)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Print steam_interfaces.txt as generated from a Steam API library.
    GbeInterfaces { library: PathBuf },
    /// Hash a tree: `{rel: [size, sha256|null, mtime_ns]}`, sha for files up to --sha-max-mb.
    TreeHash {
        root: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = 64)]
        sha_max_mb: u64,
    },
    /// Compare two tree-hash files (exit 3 when they differ).
    TreeDiff { a: PathBuf, b: PathBuf },
    /// Post a VToolMessage console command into the running -tools session.
    VtmCommand { command: String },
    /// Post the `tool_request cs2_bridge` the readiness ladder posts.
    VtmBridgeRequest,
    /// `win::wait_pid` on one pid (FIXME 8: only a missing pid is gone; one this token may not
    /// wait on is polled through a query handle). Windows only.
    WaitPid {
        pid: u32,
        #[arg(long, default_value_t = 2000)]
        ms: u32,
    },
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli.cmd) {
        Events::stderr().info(format!("error: {e:#}"));
        std::process::exit(1);
    }
}

fn run(cmd: Cmd) -> anyhow::Result<()> {
    match cmd {
        Cmd::GbeInterfaces { library } => {
            print!("{}", cs2_native::layout::steam_interfaces(&std::fs::read(&library)?));
            Ok(())
        }
        Cmd::TreeHash { root, out, sha_max_mb } => tree_hash(&root, &out, sha_max_mb << 20),
        Cmd::TreeDiff { a, b } => tree_diff(&a, &b),
        Cmd::VtmCommand { command } => {
            let seq = cs2_native::vtm::post(&cs2_native::vtm::command(&command)).map_err(anyhow::Error::msg)?;
            emit(&json!({"posted": seq}))
        }
        Cmd::VtmBridgeRequest => {
            let seq = cs2_native::vtm::post(&cs2_native::vtm::bridge_request()).map_err(anyhow::Error::msg)?;
            emit(&json!({"posted": seq}))
        }
        Cmd::WaitPid { pid, ms } => wait_pid(pid, ms),
    }
}

#[cfg(windows)]
fn wait_pid(pid: u32, ms: u32) -> anyhow::Result<()> {
    let t = std::time::Instant::now();
    let gone = cs2_native::win::wait_pid(pid, ms);
    emit(&json!({"pid": pid, "gone": gone, "elapsedMs": t.elapsed().as_millis() as u64}))
}

#[cfg(not(windows))]
fn wait_pid(_pid: u32, _ms: u32) -> anyhow::Result<()> {
    anyhow::bail!("wait-pid is Windows-only")
}

fn tree_hash(root: &Path, out: &Path, sha_max: u64) -> anyhow::Result<()> {
    let mut m = serde_json::Map::new();
    cs2_native::tree::walk(root, &mut |rel, p, md| {
        if md.is_dir() {
            m.insert(rel.to_string(), json!(null));
        } else {
            let sha = if md.len() <= sha_max { manifest::hash_file(p)?.map(|h| h.sha256) } else { None };
            m.insert(rel.to_string(), json!([md.len(), sha, cs2_native::tree::mtime_ns(md)]));
        }
        Ok(())
    })?;
    Events::stderr().info(format!("{} entries", m.len()));
    std::fs::write(out, serde_json::to_vec(&Value::Object(m))?)?;
    Ok(())
}

fn tree_diff(a: &Path, b: &Path) -> anyhow::Result<()> {
    let ra: serde_json::Map<String, Value> = serde_json::from_slice(&std::fs::read(a)?)?;
    let rb: serde_json::Map<String, Value> = serde_json::from_slice(&std::fs::read(b)?)?;
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    for (k, v) in &rb {
        match ra.get(k) {
            None => added.push(k.clone()),
            Some(o) if o != v => changed.push(json!({"path": k, "before": o, "after": v})),
            _ => {}
        }
    }
    for k in ra.keys() {
        if !rb.contains_key(k) {
            removed.push(k.clone());
        }
    }
    let same = added.is_empty() && removed.is_empty() && changed.is_empty();
    emit(&json!({"identical": same, "entries": [ra.len(), rb.len()], "added": added, "removed": removed, "changed": changed}))?;
    if !same {
        std::process::exit(3);
    }
    Ok(())
}
