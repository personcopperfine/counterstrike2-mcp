//! The sound-key check shared by `create_entities` / `set_entity_keyvalues` /
//! `create_sound_entities`: every keyvalue whose **FGD type is `sound`** goes through
//! `soundevents::check_sound_name` **before** anything is written.
//!
//! Why a refusal and not a warning (the WP-R1 model check's argument, plan §1 "Sound-key
//! validation in `create_entities` / `set_entity_keyvalues`"): a misspelt soundevent name is
//! **silent** at runtime — the engine hashes the name, finds nothing and plays nothing, with no
//! console line and no engine error — and the resourcecompiler does not check references either
//! (`re-snd.md` R1). It is the hardest sound bug to find, so an unknown name is refused.
//!
//! The matrix, exactly:
//!
//! | verdict | outcome |
//! |---|---|
//! | `Known` | nothing (the value is written) |
//! | `SourceOnly` | warning: not compiled yet (`compile_soundevents`) |
//! | `Stale` | warning: the source removed/renamed it and the `_c` is from before (`compile_soundevents`) |
//! | `FilePath` | warning: a direct `.vsnd`/`.wav` path is not looked up |
//! | `Empty` (`""`) | warning: it clears the event, nothing to look up |
//! | `Unknown` | **refusal** `SoundEventNotFound` (+ suggestions) unless `allow_unknown` |
//! | index unbuildable (`VpkMissing`) | **refusal** naming it, unless `allow_unknown` |
//!
//! `allow_unknown` is `create_entities`' `allowUnknownSoundEvent:true` and
//! `set_entity_keyvalues`' `force:true`; with it every refusal becomes a warning and the value is
//! written unchecked. The `exempt` keys (a `set_entity_keyvalues` revert: the submitted value
//! equals the live one) are skipped entirely — writing the entity's own value back needs no
//! lookup. An unavailable FGD cannot name the sound keys, so the check does not run and says so
//! (never a silent "checked and fine") — but only when a key that carries a sound is actually
//! present: with no FGD the best evidence is [`SOUND_KEY_NAMES`], every key name `core/base.fgd`
//! types as `sound` (an `info_target` with a `targetname` is not about sound at all).
//!
//! The verdict source is a `HammerState` field (review L1): production installs nothing and reads
//! [`SoundCheckSource::Index`], the real `soundevents::check_sound_name`; the integration tests
//! install `SoundCheckSource::{Fixed, Unbuildable}` through
//! `HammerState::set_sound_check_source` — the state, not a thread local, so the override is
//! visible to every task and thread of that server.
use crate::soundevents::{check_sound_name, SoundNameVerdict, SoundSource};
use crate::state::HammerState;
use crate::tools::entities_live::FgdSource;
use cs2_mcp_common::tools::ToolError;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// Where a `sound`-typed value's verdict comes from.
#[derive(Debug, Clone, Default)]
pub enum SoundCheckSource {
    /// The real soundevent index (`soundevents::check_sound_name`, WP-S1).
    #[default]
    Index,
    /// Test seam: name → verdict, answered without touching the game tree. A name the map does not
    /// carry is `Unknown { suggestions: [] }`.
    Fixed(BTreeMap<String, SoundNameVerdict>),
    /// Test seam: the index cannot be built (`IndexError::VpkMissing` on the real path). The
    /// string is the detail the refusal names. Every `sound`-typed value becomes a `VpkMissing`
    /// refusal, unless `allow_unknown`.
    Unbuildable(String),
}

/// **Every** keyvalue name the real FGD gives the FGD type `sound`, lowercased (matching is
/// case-insensitive, like every FGD key lookup): the event names (`point_soundevent.soundName`,
/// `env_soundscape.soundevent`, `ambient_generic.message`, ...) and the plain sound keys of the
/// brush/logic classes (`noise1`/`noise2` of `func_door`/`func_rotating`, `startsound`/
/// `stopsound`, `use_sound`/`locked_sound`/`unlocked_sound`, `startclosesound`/`closesound`,
/// `messagesound`, `explosion_custom_sound`, ...). Without an FGD these names are all the check
/// knows about which keys carry a sound, so they decide whether the "the check did not run"
/// warning is even about sound (an `info_target` with a `targetname` is not a sound edit).
///
/// Verified 2026-10-16 against the installed tree — no hand-maintained subset:
/// `grep -hE '^[[:space:]]*[A-Za-z_][A-Za-z0-9_]*\(sound\)' game/core/base.fgd game/csgo/csgo.fgd`
/// yields 38 distinct names (`csgo/csgo.fgd` types none as `sound`). A key that Valve types
/// `sound` on some class and something else on another (e.g. `message`) is listed: without the
/// FGD the check cannot tell the two apart, and a missed sound key means a silently unchecked
/// write, while an extra one only adds a warning.
const SOUND_KEY_NAMES: [&str; 38] = [
    "breaksound",
    "closesound",
    "commentaryfile",
    "enginesound",
    "explosion_custom_sound",
    "locked_sound",
    "message",
    "messagesound",
    "movementsound",
    "movepingsound",
    "movesound",
    "noise1",
    "noise2",
    "puntsound",
    "reversalsoundlarge",
    "reversalsoundmedium",
    "reversalsoundsmall",
    "slidesoundback",
    "slidesoundfwd",
    "soundcloseoverride",
    "sounddisengage",
    "soundengage",
    "soundevent",
    "soundjiggleoverride",
    "soundlatchoverride",
    "soundlockedoverride",
    "soundmoveoverride",
    "soundmovingloop",
    "soundname",
    "soundopenoverride",
    "soundreachedvalueone",
    "soundreachedvaluezero",
    "soundunlockedoverride",
    "startclosesound",
    "startsound",
    "stopsound",
    "unlocked_sound",
    "use_sound",
];

/// True when `key` is one of [`SOUND_KEY_NAMES`] (case-insensitively: FGD keys are matched that
/// way everywhere) — the FGD-free best guess at "this keyvalue carries a sound".
fn looks_like_a_sound_key(key: &str) -> bool {
    SOUND_KEY_NAMES
        .iter()
        .any(|known| key.eq_ignore_ascii_case(known))
}

/// Every source rank the soundevent index consults, best first (WP-S1's precedence order) —
/// reported in a refusal's `data.sourcesChecked` so the caller sees where the name was looked for.
fn sources_checked() -> Vec<&'static str> {
    [
        SoundSource::AddonGame,
        SoundSource::AddonSource,
        SoundSource::VpkCsgo,
        SoundSource::VpkCore,
    ]
    .iter()
    .map(|source| source.as_str())
    .collect()
}

/// An unknown soundevent name: a local refusal, nothing sent. The suggestions come from the index
/// (WP-S1's ranking); the hint names both bypass spellings, since this refusal is raised from two
/// tools. A value with leading/trailing whitespace says so and points at the trimmed name the
/// index *does* know (review L2: `"mcp.snd.point "` is not a name the engine registers).
fn sound_event_not_found(key: &str, name: &str, suggestions: Vec<String>) -> ToolError {
    let closest = if suggestions.is_empty() {
        String::new()
    } else {
        format!("; closest soundevents: {}", suggestions.join(", "))
    };
    let whitespace = if name != name.trim() {
        format!(
            "; the name has leading/trailing whitespace (the event names are '{}')",
            name.trim()
        )
    } else {
        String::new()
    };
    ToolError::new(
        "SoundEventNotFound",
        "SoundEventNotFound",
        format!(
            "no soundevent named '{name}' ({key}) in the soundevent index{whitespace}{closest} — a \
             misspelt name is silent at runtime, so nothing was written (pass \
             allowUnknownSoundEvent:true on create_entities, or force:true on set_entity_keyvalues, \
             to write it unchecked)"
        ),
    )
    .with_data(json!({
        "name": name,
        "key": key,
        "sourcesChecked": sources_checked(),
    }))
    .did_you_mean(suggestions)
    .retry_safe(true)
}

/// The index could not be built: never a silent pass. `detail` is the reason (`VpkMissing` naming
/// the missing file on the real path).
fn index_unbuildable(detail: &str) -> ToolError {
    ToolError::new(
        "VpkMissing",
        "VpkMissing",
        format!(
            "the soundevent index could not be built ({detail}); a soundevent name cannot be \
             checked against it, so nothing was written (pass allowUnknownSoundEvent:true on \
             create_entities, or force:true on set_entity_keyvalues, to write it unchecked)"
        ),
    )
    .with_data(json!({"reason": detail, "sourcesChecked": sources_checked()}))
    .retry_safe(true)
}

/// The verdict on `name` from `state`'s installed source (production: the real index). Shared by
/// every sound-name caller: `create_entities` / `set_entity_keyvalues` read it through
/// [`check_sound_keyvalues`], `create_sound_entities` reads it directly — one seam
/// ([`SoundCheckSource`] on the state), not one per tool.
pub(crate) async fn verdict(state: &HammerState, name: &str) -> Result<SoundNameVerdict, ToolError> {
    match state.sound_check_source() {
        SoundCheckSource::Index => check_sound_name(state, name).await,
        SoundCheckSource::Fixed(map) => Ok(map.get(name).cloned().unwrap_or(
            SoundNameVerdict::Unknown {
                suggestions: Vec::new(),
            },
        )),
        SoundCheckSource::Unbuildable(detail) => Err(index_unbuildable(&detail)),
    }
}

/// The warning for an unavailable FGD: the check needs the FGD to know which keys carry a sound
/// event, so it cannot run — said out loud instead of reporting a silent pass. `keys` are the
/// keyvalue names that carry a sound (the [`SOUND_KEY_NAMES`] ones present in this edit).
fn fgd_unavailable_warning(keys: &[&str]) -> String {
    format!(
        "fgd unavailable: the sound-event check did not run (the FGD names which keyvalues carry a \
         sound event; without it the check only knows the key names core/base.fgd types as \
         `sound`, here: {}); the value(s) were NOT checked",
        keys.iter()
            .map(|key| format!("{key:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Check every `sound`-typed keyvalue of `class` (per `keyvalues`) against the soundevent index.
/// `Ok(warnings)` when the write may proceed, `Err(refusal)` when nothing must be written:
/// `SoundEventNotFound` for an unknown name, `VpkMissing` when the index cannot be built, both
/// `retrySafe`. `exempt` keys (the caller's reverts) and the values of `allow_unknown` are skipped
/// with a warning instead of a refusal.
pub(crate) async fn check_sound_keyvalues(
    state: &HammerState,
    fgd: &FgdSource,
    class: &str,
    keyvalues: &Map<String, Value>,
    exempt: &[String],
    allow_unknown: bool,
) -> Result<Vec<String>, ToolError> {
    let mut warnings: Vec<String> = Vec::new();
    let Some(db) = fgd.db() else {
        // No FGD, no knowledge of which keys carry a sound — but every key name the real
        // `core/base.fgd` types as `sound` is listed in `SOUND_KEY_NAMES`, so the warning is only
        // about sound when one of those is actually being written (a `targetname`-only edit on an
        // `info_target` is not a sound edit and must not be warned about).
        let sound_keys: Vec<&str> = keyvalues
            .keys()
            .filter(|key| looks_like_a_sound_key(key))
            .map(String::as_str)
            .collect();
        if !sound_keys.is_empty() {
            warnings.push(fgd_unavailable_warning(&sound_keys));
        }
        return Ok(warnings);
    };
    // The class's sound keys, in declaration order (base keys first: `snd_event_point` resolves
    // `point_soundevent`'s `soundName`).
    let sound_keys: Vec<String> = db
        .sound_keys(class)
        .iter()
        .map(|key| key.name.clone())
        .collect();
    for key in sound_keys {
        if exempt.iter().any(|seen| seen.eq_ignore_ascii_case(&key)) {
            continue; // a revert writes the entity's own value back: nothing to look up
        }
        // The caller's spelling of the key (FGD keys are matched case-insensitively everywhere).
        let Some((_, value)) = keyvalues.iter().find(|(name, _)| name.eq_ignore_ascii_case(&key))
        else {
            continue;
        };
        // `keyvalues_arg` stores every value as text, so a non-string here is a caller of this
        // module that skipped it; skip rather than invent a verdict.
        let Some(name) = value.as_str() else {
            continue;
        };
        let verdict = match verdict(state, name).await {
            Ok(verdict) => verdict,
            Err(error) => {
                if allow_unknown {
                    warnings.push(format!(
                        "{key} = {name:?} was written unchecked: the soundevent index could not be \
                         built ({error})"
                    ));
                    continue;
                }
                return Err(error);
            }
        };
        match verdict {
            SoundNameVerdict::Known { .. } => {}
            SoundNameVerdict::SourceOnly { file } => warnings.push(format!(
                "{key} = {name:?} is only in the addon's soundevent source ({file}): not compiled \
                 yet — run compile_soundevents (and run_map) before it can play"
            )),
            SoundNameVerdict::Stale { file } => warnings.push(format!(
                "{key} = {name:?} was removed or renamed in the addon's soundevent source ({file}) \
                 but the compiled file still carries it: the name plays until the next compile and \
                 is gone after it — run compile_soundevents and use the current name"
            )),
            SoundNameVerdict::FilePath => warnings.push(format!(
                "{key} = {name:?} is a sound file path, not a soundevent name: it was not looked \
                 up (ambient_generic.message accepts direct .vsnd/.wav names)"
            )),
            SoundNameVerdict::Empty => warnings.push(format!(
                "{key} is set to \"\" (the sound event is cleared): nothing was looked up"
            )),
            SoundNameVerdict::Unknown { suggestions } => {
                if allow_unknown {
                    warnings.push(format!(
                        "{key} = {name:?} is not in the soundevent index: written unchecked \
                         (allowUnknownSoundEvent/force); a misspelt name is silent at runtime"
                    ));
                } else {
                    return Err(sound_event_not_found(&key, name, suggestions));
                }
            }
        }
    }
    Ok(warnings)
}

// -------------------------------------------------------------------------------------------
// Unit tests: the matrix without a bridge or a game tree (the source seam is exercised there; the
// tool-level behaviour is `tests/entities_live.rs`).
// -------------------------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::fgd::Database;

    #[test]
    fn the_refusals_name_the_key_the_name_and_the_sources() {
        let error = sound_event_not_found("soundName", "Nope.Event", vec!["Nope.Eventx".into()]);
        assert_eq!(error.type_, "SoundEventNotFound");
        assert_eq!(error.name, "SoundEventNotFound");
        assert!(error.retry_safe, "nothing was dispatched");
        assert!(!error.outcome_unknown);
        assert!(error.code.is_none(), "a local refusal carries no wire code");
        assert_eq!(error.data["name"], json!("Nope.Event"));
        assert_eq!(error.data["key"], json!("soundName"));
        assert_eq!(error.data["didYouMean"], json!(["Nope.Eventx"]));
        assert_eq!(
            error.data["sourcesChecked"],
            json!(["addon-game", "addon-source", "vpk:csgo", "vpk:core"])
        );
        assert!(error.message.contains("allowUnknownSoundEvent:true"), "{error}");
        assert!(error.message.contains("force:true"), "{error}");

        let error = index_unbuildable("csgo/pak01_dir.vpk is missing");
        assert_eq!(error.type_, "VpkMissing");
        assert!(error.retry_safe);
        assert!(
            error.message.contains("csgo/pak01_dir.vpk is missing"),
            "{error}"
        );
    }

    /// Review L2: a whitespace-padded name is refused *and* the refusal says why, naming the
    /// trimmed spelling the index knows. A clean name carries no such sentence.
    #[test]
    fn a_whitespace_padded_name_says_so() {
        let padded = sound_event_not_found("soundName", "mcp.snd.point ", vec!["mcp.snd.point".into()]);
        assert!(
            padded.message.contains("leading/trailing whitespace"),
            "{padded}"
        );
        assert!(padded.message.contains("'mcp.snd.point'"), "{padded}");
        assert_eq!(padded.data["didYouMean"], json!(["mcp.snd.point"]));

        let clean = sound_event_not_found("soundName", "mcp.snd.point", Vec::new());
        assert!(
            !clean.message.contains("whitespace"),
            "an unpadded name must not mention whitespace: {clean}"
        );
    }

    /// The FGD-free key list: the warning is only about sound when a key the real FGD types as
    /// `sound` is present; `targetname`/`health`/anything else is not, and the list is the FGD's
    /// own (`noise1`, `startsound`, `use_sound`, ... — not just the three event keys).
    #[test]
    fn only_the_sound_key_names_trigger_the_fgd_free_warning() {
        for key in [
            "soundName",
            "soundevent",
            "message",
            "SOUNDNAME",
            "Message",
            // the plain sound keys of the brush/logic classes
            "noise1",
            "NOISE2",
            "startsound",
            "stopsound",
            "use_sound",
            "locked_sound",
            "unlocked_sound",
            "startclosesound",
            "closesound",
            "messagesound",
            "explosion_custom_sound",
            "MovePingSound",
        ] {
            assert!(looks_like_a_sound_key(key), "{key}");
        }
        for key in [
            "targetname",
            "radius",
            "model",
            "soundscape",
            "message2",
            "noise3",
            "startsound2",
            "mainSoundscapeName",
            "",
        ] {
            assert!(!looks_like_a_sound_key(key), "{key}");
        }
        let warning = fgd_unavailable_warning(&["message"]);
        assert!(warning.contains("the sound-event check did not run"), "{warning}");
        assert!(warning.contains("\"message\""), "{warning}");
        let warning = fgd_unavailable_warning(&["noise1", "noise2"]);
        assert!(warning.contains("\"noise1\""), "{warning}");
        assert!(warning.contains("\"noise2\""), "{warning}");
    }

    /// The FGD fixture is the real database over `fixtures/fgd`; the check reads the class's sound
    /// keys from it (this pins the adapter's key selection without a bridge).
    #[test]
    fn the_adapter_finds_the_sound_keys_through_the_base_chain() {
        let db = Database::load(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/fgd"))
            .expect("the FGD fixture loads");
        let names: Vec<&str> = db
            .sound_keys("snd_event_point")
            .iter()
            .map(|key| key.name.as_str())
            .collect();
        assert_eq!(names, vec!["soundName"], "inherited from point_soundevent");
        assert!(db.is_sound_class("snd_event_point"));
        assert!(!db.is_sound_class("trigger_soundscape"));
    }
}
