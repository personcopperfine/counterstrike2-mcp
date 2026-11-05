//! Snapshots of the local player for `game_snapshot` and `game_restore`: the data and its JSON form
//! (inline snapshots are validated), the bounded store kept in `ClientState`, and the pure
//! comparison helpers the restore uses. The bridge reads and the restore steps are in
//! `tools/game_snapshot.rs`.
use crate::tools::game_state::is_weapon_name;
use crate::tools::game_wait::vec3;
use cs2_mcp_common::tools::ToolError;
use serde_json::{json, Map, Value};
use std::ops::RangeInclusive;

/// Snapshots kept per client process; storing one more drops the least recently stored.
pub const STORE_CAP: usize = 32;
/// Weapons in an inline snapshot (a player carries a handful; the bound keeps the input small).
pub const MAX_WEAPONS: usize = 64;
/// Pose tolerances: the setpos defaults (the game_wait `at` predicate).
pub const XY_TOLERANCE: f64 = 2.0;
pub const Z_TOLERANCE: f64 = 4.0;
pub const ANGLE_TOLERANCE: f64 = 1.0;

#[derive(Debug, Clone, PartialEq)]
pub struct WeaponSnap {
    pub name: String,
    pub def_index: Option<i64>,
    pub clip1: Option<i64>,
    pub reserve: Option<i64>,
}

/// The round state a snapshot keeps (`game.rules`): the summary only.
#[derive(Debug, Clone, PartialEq)]
pub struct RulesSnap {
    pub round_start_time: Option<f64>,
    pub rounds_played: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub name: Option<String>,
    pub taken_at: Option<f64>,
    pub map: String,
    pub tick: Option<u64>,
    pub origin: [f64; 3],
    /// the view angles (`viewAngles`): pitch, yaw, roll
    pub angles: [f64; 3],
    pub velocity: Option<[f64; 3]>,
    pub health: i64,
    pub armor: Option<i64>,
    pub helmet: Option<bool>,
    pub money: Option<i64>,
    pub ducked: Option<bool>,
    pub team: Option<i64>,
    pub active_weapon: Option<String>,
    pub weapons: Vec<WeaponSnap>,
    pub rules: Option<RulesSnap>,
}

/// Live values a snapshot may hold: health 1..=100 (a dead player is refused before capture), armor
/// 0..=100. The same bounds apply at capture and to inline snapshots.
pub const HEALTH_RANGE: RangeInclusive<i64> = 1..=100;
pub const ARMOR_RANGE: RangeInclusive<i64> = 0..=100;

/// Snapshot names (also the export file stem): 1..=64 of letters, digits, `_` and `-`.
pub fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn schema_missing(field: &str) -> ToolError {
    ToolError::new(
        "SchemaUnavailable",
        "SchemaUnavailable",
        format!("game state sent no {field}; a snapshot needs it"),
    )
    .retry_safe(true)
    .with_data(json!({"field": field}))
}

/// InvalidParams naming the path of the bad snapshot field.
fn bad(path: &str, what: &str) -> ToolError {
    ToolError::invalid_arguments(format!("snapshot.{path} must be {what}"))
}

/// The last segment of a dotted path (`player.armor` -> `armor`), the JSON key.
fn key_of(path: &str) -> &str {
    path.rsplit('.').next().unwrap_or(path)
}

/// A present, non-null field (null counts as absent).
fn opt<'a>(obj: &'a Map<String, Value>, path: &str) -> Option<&'a Value> {
    obj.get(key_of(path)).filter(|v| !v.is_null())
}

fn opt_int(
    obj: &Map<String, Value>,
    path: &str,
    range: RangeInclusive<i64>,
) -> Result<Option<i64>, ToolError> {
    match opt(obj, path) {
        None => Ok(None),
        Some(v) => v
            .as_i64()
            .filter(|n| range.contains(n))
            .map(Some)
            .ok_or_else(|| {
                bad(
                    path,
                    &format!("an integer {}..={}", range.start(), range.end()),
                )
            }),
    }
}

fn opt_num(obj: &Map<String, Value>, path: &str) -> Result<Option<f64>, ToolError> {
    match opt(obj, path) {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|n| n.is_finite())
            .map(Some)
            .ok_or_else(|| bad(path, "a finite number")),
    }
}

fn opt_bool(obj: &Map<String, Value>, path: &str) -> Result<Option<bool>, ToolError> {
    match opt(obj, path) {
        None => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| bad(path, "true or false")),
    }
}

fn opt_vec(obj: &Map<String, Value>, path: &str) -> Result<Option<[f64; 3]>, ToolError> {
    match opt(obj, path) {
        None => Ok(None),
        Some(v) => vec3(v)
            .map(Some)
            .ok_or_else(|| bad(path, "[x, y, z] finite numbers")),
    }
}

impl Snapshot {
    /// The snapshot of a live local player, from the bridge replies: `game.local`, `game.weapons` and
    /// `game.rules` when the bridge has it. The caller has checked that the player is alive. A field
    /// the bridge could not resolve is SchemaUnavailable (retry-safe): a snapshot without the pose,
    /// health or a weapon name would restore the wrong thing.
    pub fn from_reads(
        name: &str,
        taken_at: f64,
        local: &Value,
        weapons: &Value,
        rules: Option<&Value>,
    ) -> Result<Snapshot, ToolError> {
        let player = local
            .get("player")
            .filter(|p| p.is_object())
            .ok_or_else(|| schema_missing("player"))?;
        let map = local
            .get("map")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
            .ok_or_else(|| schema_missing("map"))?;
        let origin = player
            .get("origin")
            .and_then(vec3)
            .ok_or_else(|| schema_missing("player.origin"))?;
        let angles = local
            .get("viewAngles")
            .and_then(vec3)
            .ok_or_else(|| schema_missing("viewAngles"))?;
        let health = player
            .get("health")
            .and_then(Value::as_i64)
            .filter(|h| HEALTH_RANGE.contains(h))
            .ok_or_else(|| schema_missing("player.health (1..=100)"))?;
        let armor = match player.get("armor") {
            None | Some(Value::Null) => None,
            Some(v) => Some(
                v.as_i64()
                    .filter(|a| ARMOR_RANGE.contains(a))
                    .ok_or_else(|| schema_missing("player.armor (0..=100)"))?,
            ),
        };
        let active_weapon = match player.get("activeWeapon") {
            None | Some(Value::Null) => None,
            Some(w) => Some(
                w.get("name")
                    .and_then(Value::as_str)
                    .filter(|n| is_weapon_name(n))
                    .ok_or_else(|| schema_missing("player.activeWeapon.name"))?
                    .to_string(),
            ),
        };
        let items = weapons
            .get("weapons")
            .and_then(Value::as_array)
            .ok_or_else(|| schema_missing("weapons"))?;
        let mut held = Vec::with_capacity(items.len());
        for w in items {
            let wn = w
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| is_weapon_name(n))
                .ok_or_else(|| schema_missing("weapons[].name"))?;
            held.push(WeaponSnap {
                name: wn.to_string(),
                def_index: w.get("defIndex").and_then(Value::as_i64),
                clip1: w.get("clip1").and_then(Value::as_i64),
                reserve: w.get("reserve").and_then(Value::as_i64),
            });
        }
        Ok(Snapshot {
            name: Some(name.to_string()),
            taken_at: Some(taken_at),
            map: map.to_string(),
            tick: local.get("tick").and_then(Value::as_u64),
            origin,
            angles,
            velocity: player.get("velocity").and_then(vec3),
            health,
            armor,
            helmet: player.get("helmet").and_then(Value::as_bool),
            money: player.get("money").and_then(Value::as_i64),
            ducked: player.get("ducked").and_then(Value::as_bool),
            team: player.get("team").and_then(Value::as_i64),
            active_weapon,
            weapons: held,
            rules: rules.map(|r| RulesSnap {
                round_start_time: r.get("roundStartTime").and_then(Value::as_f64),
                rounds_played: r.get("roundsPlayed").and_then(Value::as_i64),
            }),
        })
    }

    /// The JSON form: what `game_snapshot` returns and what an inline `game_restore` takes back.
    pub fn to_json(&self) -> Value {
        let mut out = Map::new();
        if let Some(n) = &self.name {
            out.insert("name".into(), json!(n));
        }
        out.insert("takenAt".into(), json!(self.taken_at));
        out.insert("map".into(), json!(self.map));
        out.insert("tick".into(), json!(self.tick));
        out.insert(
            "player".into(),
            json!({
                "origin": self.origin,
                "angles": self.angles,
                "velocity": self.velocity,
                "health": self.health,
                "armor": self.armor,
                "helmet": self.helmet,
                "money": self.money,
                "ducked": self.ducked,
                "team": self.team,
                "activeWeapon": self.active_weapon,
            }),
        );
        let weapons: Vec<Value> = self
            .weapons
            .iter()
            .map(|w| {
                json!({"name": w.name, "defIndex": w.def_index, "clip1": w.clip1, "reserve": w.reserve})
            })
            .collect();
        out.insert("weapons".into(), Value::Array(weapons));
        out.insert(
            "rules".into(),
            json!(self.rules.as_ref().map(
                |r| json!({"roundStartTime": r.round_start_time, "roundsPlayed": r.rounds_played})
            )),
        );
        Value::Object(out)
    }

    /// A snapshot from its JSON form (an inline `game_restore` argument). Unknown keys are ignored;
    /// a wrong type or an out-of-range value is InvalidParams naming the field. `name` is optional
    /// here (inline snapshots need none); `health` must be a live value 1..=100.
    pub fn from_json(v: &Value) -> Result<Snapshot, ToolError> {
        let obj = v
            .as_object()
            .ok_or_else(|| ToolError::invalid_arguments("snapshot must be an object"))?;
        let name = match obj.get("name") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if valid_name(s) => Some(s.clone()),
            _ => return Err(bad("name", "1..64 letters, digits, _ or -")),
        };
        let map = obj
            .get("map")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty() && m.len() <= 128)
            .ok_or_else(|| bad("map", "a non-empty map name"))?
            .to_string();
        let player = obj
            .get("player")
            .and_then(Value::as_object)
            .ok_or_else(|| bad("player", "an object"))?;
        let origin = opt_vec(player, "player.origin")?
            .ok_or_else(|| bad("player.origin", "[x, y, z] finite numbers"))?;
        let angles = opt_vec(player, "player.angles")?
            .ok_or_else(|| bad("player.angles", "[pitch, yaw, roll] finite numbers"))?;
        let health = opt_int(player, "player.health", HEALTH_RANGE)?
            .ok_or_else(|| bad("player.health", "an integer 1..=100"))?;
        let active_weapon = match opt(player, "player.activeWeapon") {
            None => None,
            Some(Value::String(w)) if is_weapon_name(w) => Some(w.clone()),
            Some(_) => return Err(bad("player.activeWeapon", "a weapon name or null")),
        };
        let items = obj
            .get("weapons")
            .and_then(Value::as_array)
            .ok_or_else(|| bad("weapons", "an array"))?;
        if items.len() > MAX_WEAPONS {
            return Err(bad("weapons", "at most 64 entries"));
        }
        let mut weapons = Vec::with_capacity(items.len());
        for (i, w) in items.iter().enumerate() {
            let w = w
                .as_object()
                .ok_or_else(|| bad(&format!("weapons[{i}]"), "an object"))?;
            let wn = match w.get("name") {
                Some(Value::String(n)) if is_weapon_name(n) => n.clone(),
                _ => {
                    return Err(bad(
                        &format!("weapons[{i}].name"),
                        "a weapon name like weapon_ak47",
                    ))
                }
            };
            weapons.push(WeaponSnap {
                name: wn,
                def_index: opt_int(w, &format!("weapons[{i}].defIndex"), 0..=65535)?,
                // -1: a weapon without ammo (the knife)
                clip1: opt_int(w, &format!("weapons[{i}].clip1"), -1..=i64::MAX)?,
                reserve: opt_int(w, &format!("weapons[{i}].reserve"), 0..=i64::MAX)?,
            });
        }
        let rules = match opt(obj, "rules") {
            None => None,
            Some(r) => {
                let r = r
                    .as_object()
                    .ok_or_else(|| bad("rules", "an object or null"))?;
                Some(RulesSnap {
                    round_start_time: opt_num(r, "rules.roundStartTime")?,
                    rounds_played: opt_int(r, "rules.roundsPlayed", 0..=i64::MAX)?,
                })
            }
        };
        Ok(Snapshot {
            name,
            taken_at: opt_num(obj, "takenAt")?,
            map,
            tick: opt_int(obj, "tick", 0..=i64::MAX)?.map(|t| t as u64),
            origin,
            angles,
            velocity: opt_vec(player, "player.velocity")?,
            health,
            armor: opt_int(player, "player.armor", ARMOR_RANGE)?,
            helmet: opt_bool(player, "player.helmet")?,
            money: opt_int(player, "player.money", 0..=i64::MAX)?,
            ducked: opt_bool(player, "player.ducked")?,
            team: opt_int(player, "player.team", i64::MIN..=i64::MAX)?,
            active_weapon,
            weapons,
            rules,
        })
    }
}

/// The snapshots of this process, oldest first. Storing a name again replaces it (and makes it the
/// newest); storing past `STORE_CAP` drops the oldest.
#[derive(Debug, Default)]
pub struct SnapshotStore {
    items: Vec<Snapshot>,
}

impl SnapshotStore {
    /// Store a named snapshot. Returns the name of the snapshot dropped to stay within the cap.
    pub fn insert(&mut self, s: Snapshot) -> Option<String> {
        self.items.retain(|x| x.name != s.name);
        self.items.push(s);
        if self.items.len() > STORE_CAP {
            return self.items.remove(0).name;
        }
        None
    }

    pub fn get(&self, name: &str) -> Option<&Snapshot> {
        self.items.iter().find(|s| s.name.as_deref() == Some(name))
    }

    /// Stored names, oldest first.
    pub fn names(&self) -> Vec<String> {
        self.items.iter().filter_map(|s| s.name.clone()).collect()
    }
}

/// Signed yaw difference `got - want`, wrapped to [-180, 180).
pub fn yaw_delta(got: f64, want: f64) -> f64 {
    let d = (got - want).rem_euclid(360.0);
    if d >= 180.0 {
        d - 360.0
    } else {
        d
    }
}

/// Horizontal (xy) and vertical (z) distance between two origins.
pub fn origin_gap(want: [f64; 3], got: [f64; 3]) -> (f64, f64) {
    (
        (got[0] - want[0]).hypot(got[1] - want[1]),
        (got[2] - want[2]).abs(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(origin: Value, health: Value) -> Value {
        json!({
            "tick": 42, "inGame": true, "map": "editor/zoo/smartprop_zoo",
            "viewAngles": [-5.0, 179.5, 0.0],
            "player": {
                "alive": true, "origin": origin, "velocity": [0.0, 0.0, 0.0],
                "health": health, "armor": 100, "helmet": true, "money": 800,
                "ducked": false, "team": 3,
                "activeWeapon": {"name": "weapon_ak47", "defIndex": 7}
            }
        })
    }

    fn weapons() -> Value {
        json!({"weapons": [
            {"name": "weapon_knife", "defIndex": 42, "clip1": -1, "clip2": -1, "reserve": 0},
            {"name": "weapon_ak47", "defIndex": 7, "clip1": 30, "clip2": 0, "reserve": 90}
        ]})
    }

    fn rules() -> Value {
        json!({"roundStartTime": 12.5, "roundsPlayed": 3})
    }

    #[test]
    fn yaw_delta_wraps_at_180() {
        assert_eq!(yaw_delta(179.0, -179.0), -2.0);
        assert_eq!(yaw_delta(-179.0, 179.0), 2.0);
        assert_eq!(yaw_delta(90.0, 0.0), 90.0);
        assert!((yaw_delta(0.0, 0.0)).abs() < 1e-12);
    }

    #[test]
    fn origin_gap_splits_xy_and_z() {
        let (xy, dz) = origin_gap([0.0, 0.0, 0.0], [3.0, 4.0, -2.5]);
        assert_eq!(xy, 5.0);
        assert_eq!(dz, 2.5);
    }

    #[test]
    fn from_reads_captures_every_field() {
        let s = Snapshot::from_reads(
            "base",
            100.25,
            &local(json!([1.0, 2.0, 3.0]), json!(87)),
            &weapons(),
            Some(&rules()),
        )
        .unwrap();
        assert_eq!(s.name.as_deref(), Some("base"));
        assert_eq!(s.taken_at, Some(100.25));
        assert_eq!(s.map, "editor/zoo/smartprop_zoo");
        assert_eq!(s.tick, Some(42));
        assert_eq!(s.origin, [1.0, 2.0, 3.0]);
        assert_eq!(s.angles, [-5.0, 179.5, 0.0]);
        assert_eq!(s.velocity, Some([0.0, 0.0, 0.0]));
        assert_eq!(s.health, 87);
        assert_eq!(s.armor, Some(100));
        assert_eq!(s.helmet, Some(true));
        assert_eq!(s.money, Some(800));
        assert_eq!(s.ducked, Some(false));
        assert_eq!(s.team, Some(3));
        assert_eq!(s.active_weapon.as_deref(), Some("weapon_ak47"));
        assert_eq!(s.weapons.len(), 2);
        assert_eq!(s.weapons[1].clip1, Some(30));
        assert_eq!(s.weapons[1].reserve, Some(90));
        assert_eq!(s.weapons[1].def_index, Some(7));
        assert_eq!(
            s.rules,
            Some(RulesSnap {
                round_start_time: Some(12.5),
                rounds_played: Some(3)
            })
        );
    }

    #[test]
    fn from_reads_refuses_unresolved_pose_health_and_weapon_names() {
        for (l, w) in [
            (local(Value::Null, json!(87)), weapons()),
            (local(json!([0.0, 0.0, 0.0]), Value::Null), weapons()),
            (
                local(json!([0.0, 0.0, 0.0]), json!(87)),
                json!({"weapons": [{"name": null}]}),
            ),
        ] {
            let e = Snapshot::from_reads("x", 0.0, &l, &w, None).unwrap_err();
            assert_eq!(e.name, "SchemaUnavailable");
            assert!(e.retry_safe);
        }
    }

    #[test]
    fn from_reads_bounds_health_armor_and_the_active_weapon() {
        let w = weapons();
        let mut l = local(json!([0.0, 0.0, 0.0]), json!(101));
        assert!(Snapshot::from_reads("x", 0.0, &l, &w, None).is_err());
        l = local(json!([0.0, 0.0, 0.0]), json!(100));
        l["player"]["armor"] = json!(150);
        assert!(Snapshot::from_reads("x", 0.0, &l, &w, None).is_err());
        l["player"]["armor"] = json!(100);
        l["player"]["activeWeapon"] = json!({"name": "ak47"});
        assert!(Snapshot::from_reads("x", 0.0, &l, &w, None).is_err());
        l["player"]["activeWeapon"] = json!({"name": "weapon_ak47"});
        assert!(Snapshot::from_reads("x", 0.0, &l, &w, None).is_ok());
    }

    #[test]
    fn json_round_trip_is_lossless() {
        let s = Snapshot::from_reads(
            "rt",
            7.0,
            &local(json!([1.0, 2.0, 3.0]), json!(50)),
            &weapons(),
            Some(&rules()),
        )
        .unwrap();
        let back = Snapshot::from_json(&s.to_json()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn inline_snapshot_needs_only_the_pose_health_and_map() {
        let s = Snapshot::from_json(&json!({
            "map": "editor/zoo/smartprop_zoo",
            "player": {"origin": [1, 2, 3], "angles": [0, 90, 0], "health": 100},
            "weapons": [],
            "extra": "ignored"
        }))
        .unwrap();
        assert_eq!(s.name, None);
        assert_eq!(s.armor, None);
        assert_eq!(s.rules, None);
        assert!(s.weapons.is_empty());
    }

    #[test]
    fn inline_snapshot_validation_names_the_field() {
        let good = json!({
            "map": "m", "player": {"origin": [1, 2, 3], "angles": [0, 0, 0], "health": 100},
            "weapons": []
        });
        assert!(Snapshot::from_json(&good).is_ok());
        let cases: Vec<(&str, Value)> = vec![
            ("snapshot must", json!([1, 2])),
            (
                "name",
                json!({"name": "a/b", "map": "m", "player": {"origin": [1,2,3], "angles": [0,0,0], "health": 1}, "weapons": []}),
            ),
            (
                "map",
                json!({"map": "", "player": {"origin": [1,2,3], "angles": [0,0,0], "health": 1}, "weapons": []}),
            ),
            (
                "player.health",
                json!({"map": "m", "player": {"origin": [1,2,3], "angles": [0,0,0], "health": 0}, "weapons": []}),
            ),
            (
                "player.health",
                json!({"map": "m", "player": {"origin": [1,2,3], "angles": [0,0,0], "health": 101}, "weapons": []}),
            ),
            (
                "player.origin",
                json!({"map": "m", "player": {"origin": [1,2], "angles": [0,0,0], "health": 1}, "weapons": []}),
            ),
            (
                "player.origin",
                json!({"map": "m", "player": {"origin": [1,2,"x"], "angles": [0,0,0], "health": 1}, "weapons": []}),
            ),
            (
                "player.angles",
                json!({"map": "m", "player": {"origin": [1,2,3], "health": 1}, "weapons": []}),
            ),
            (
                "player.armor",
                json!({"map": "m", "player": {"origin": [1,2,3], "angles": [0,0,0], "health": 1, "armor": 101}, "weapons": []}),
            ),
            (
                "player.helmet",
                json!({"map": "m", "player": {"origin": [1,2,3], "angles": [0,0,0], "health": 1, "helmet": "yes"}, "weapons": []}),
            ),
            (
                "player.activeWeapon",
                json!({"map": "m", "player": {"origin": [1,2,3], "angles": [0,0,0], "health": 1, "activeWeapon": "ak47"}, "weapons": []}),
            ),
            (
                "weapons",
                json!({"map": "m", "player": {"origin": [1,2,3], "angles": [0,0,0], "health": 1}}),
            ),
            (
                "weapons[0].name",
                json!({"map": "m", "player": {"origin": [1,2,3], "angles": [0,0,0], "health": 1}, "weapons": [{"name": "weapon_ak47; quit"}]}),
            ),
            (
                "weapons[0].clip1",
                json!({"map": "m", "player": {"origin": [1,2,3], "angles": [0,0,0], "health": 1}, "weapons": [{"name": "weapon_ak47", "clip1": -2}]}),
            ),
            (
                "rules",
                json!({"map": "m", "player": {"origin": [1,2,3], "angles": [0,0,0], "health": 1}, "weapons": [], "rules": 3}),
            ),
        ];
        for (path, v) in cases {
            let e = Snapshot::from_json(&v).unwrap_err();
            assert_eq!(e.name, "InvalidParams", "{v}");
            assert!(e.message.contains(path), "{path}: {}", e.message);
        }
        let many: Vec<Value> = (0..=MAX_WEAPONS)
            .map(|_| json!({"name": "weapon_ak47"}))
            .collect();
        let v = json!({"map": "m", "player": {"origin": [1,2,3], "angles": [0,0,0], "health": 1}, "weapons": many});
        assert!(Snapshot::from_json(&v)
            .unwrap_err()
            .message
            .contains("weapons"));
    }

    #[test]
    fn store_replaces_by_name_and_drops_the_oldest_past_the_cap() {
        let mk = |n: &str| {
            Snapshot::from_json(&json!({
            "name": n, "map": "m", "player": {"origin": [1,2,3], "angles": [0,0,0], "health": 1}, "weapons": []
        })).unwrap()
        };
        let mut store = SnapshotStore::default();
        for i in 0..STORE_CAP {
            assert_eq!(store.insert(mk(&format!("s{i}"))), None);
        }
        // a name again replaces: no drop, the count stays
        assert_eq!(store.insert(mk("s0")), None);
        assert_eq!(store.names().len(), STORE_CAP);
        assert_eq!(store.names().last().map(String::as_str), Some("s0"));
        // one more drops the oldest (now s1)
        assert_eq!(store.insert(mk("extra")), Some("s1".to_string()));
        assert!(store.get("s1").is_none());
        assert!(store.get("s0").is_some());
        assert_eq!(store.names().len(), STORE_CAP);
    }
}
