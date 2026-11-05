//! AI playtest loop: observe -> decide (the MCP client model) -> act (`input.set`) -> wait (`game.tick`).
//! Observations: screenshot (capture), console samples (`getpos`), tick, checkpoints. The action
//! vocabulary mirrors `CBaseUserCmdPB` semantics (buttons, view angles, move axes) so traces are
//! replayable against cs2-gym (`cs2gym.proto`, a separate project, not in this repo). See workflows/08-playtest-ai.md.
#[derive(Debug, Clone)]
pub struct Observation {
    pub tick: i64,
    pub png: Option<std::path::PathBuf>,
    pub ocr_text: Option<String>,
    pub console_tail: String,
}
