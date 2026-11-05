//! Play-test building blocks shared by client-mcp and hammer-mcp: the bridge calls and output
//! shapes of `game_state`, `input_sequence` and the `game.local` waits, over a `&mut BridgeClient`
//! the caller already locked (each server owns its own state type, so no trait is needed).
pub mod entities;
pub mod input;
pub mod predicate;
pub mod sequence;
pub mod state;
pub mod wait;
