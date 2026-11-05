//! Local-install backend (phase 2, not implemented). See workflows/10-local-install.md.
//! Same `Runtime` contract; differences are only where the game tree, prefix and display live.

use super::*;

pub struct LocalRuntime {
    pub install_root: PathBuf,
}

impl Runtime for LocalRuntime {
    fn name(&self) -> &'static str {
        "local"
    }
    async fn status(&self) -> anyhow::Result<RuntimeStatus> {
        anyhow::bail!("local runtime: not implemented (phase 2)")
    }
    async fn up(&self, _spec: &LaunchSpec) -> anyhow::Result<SessionPaths> {
        anyhow::bail!("local runtime: not implemented (phase 2)")
    }
    async fn down(&self) -> anyhow::Result<()> {
        anyhow::bail!("local runtime: not implemented (phase 2)")
    }
    async fn exec(&self, _argv: &[String]) -> anyhow::Result<std::process::Output> {
        anyhow::bail!("local runtime: not implemented (phase 2)")
    }
    fn paths(&self) -> SessionPaths {
        SessionPaths {
            app_root: self.install_root.clone(),
            discovery: None,
            console_log: None,
            pfx: None,
        }
    }
    async fn logs(&self, _lines: usize) -> anyhow::Result<String> {
        anyhow::bail!("local runtime: not implemented (phase 2)")
    }
}
