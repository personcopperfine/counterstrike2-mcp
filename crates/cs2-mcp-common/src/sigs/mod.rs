//! Consumer of the signature database `sigs/signatures/<module>.json`
//! (format: sigs/README.md).
//!
//! Rule: this crate contains NO addresses. Everything address-shaped lives in the bridge DLL,
//! which is generated from the same database (`pins.py` -> `bridge_pins.h`, fail-closed). What
//! the Rust side uses the database for:
//! * build pinning: the newest `builds` key = the build the repo is on; compared with the
//!   bridge's `gameBuild` (hello) and the game's `steam.inf` ClientVersion -> `BuildMismatch`;
//! * `doctor`/status reporting: which entries are `stale`/unverified for the running build;
//! * naming: tools and docs refer to symbols by entry name (e.g. `EVS_RS_GAME_WORLD`), never by RVA.
//!
//! Resolution against binaries stays in Python (`sigs/scripts/sigcheck.py resolve`), invoked only
//! by dev tooling, never by the server at runtime.
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Deserialize, Debug, Clone)]
pub struct Entry {
    pub kind: String,
    pub locator: serde_json::Value,
    pub via: String,
    pub tag: String,
    #[serde(default)]
    pub expect: BTreeMap<String, i64>,
    #[serde(default)]
    pub bridge: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ModuleDb {
    pub module: String,
    pub image_base: u64,
    #[serde(default)]
    pub builds: BTreeMap<String, serde_json::Value>,
    pub entries: BTreeMap<String, Entry>,
}

pub struct SigDb {
    pub modules: BTreeMap<String, ModuleDb>,
}

impl SigDb {
    pub fn load_dir(dir: &Path) -> anyhow::Result<SigDb> {
        let mut modules = BTreeMap::new();
        for e in std::fs::read_dir(dir)? {
            let p = e?.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            if p.file_name().and_then(|x| x.to_str()) == Some("SCHEMA.json") {
                continue;
            }
            let db: ModuleDb = serde_json::from_str(&std::fs::read_to_string(&p)?)?;
            modules.insert(db.module.clone(), db);
        }
        Ok(SigDb { modules })
    }

    /// Newest build recorded across modules (lexicographic on the numeric ClientVersion string).
    pub fn newest_build(&self) -> Option<String> {
        self.modules
            .values()
            .flat_map(|m| m.builds.keys().cloned())
            .max()
    }

    /// Entries with a `bridge` macro that are not `verified` on `build`: the update to-do list.
    pub fn unverified_bridge_entries(&self, build: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (m, db) in &self.modules {
            for (name, e) in &db.entries {
                if e.bridge.is_some() && !e.expect.contains_key(build) {
                    out.push((m.clone(), name.clone()));
                }
            }
        }
        out
    }
}
