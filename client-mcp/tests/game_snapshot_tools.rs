//! `game_snapshot` and `game_restore` against the mock bridge. The mock serves a small world (a
//! player with a pose, health, armor, weapons and an active weapon) that the console and selection
//! replies change, so the restore's effects are what the comparison reads. Covered: the capture of
//! every field, the store (replace by name, the cap), export to artifacts, the refusals that send
//! nothing (not in game, another map, an unknown name, bad inline snapshots), the restore's command
//! order and comparison, a weapon the game never gives (restored false), and parts.
#![allow(clippy::result_large_err)]
use client_mcp::state::ClientConfig;
use cs2_mcp_common::mock::MockBridge;
use cs2_mcp_common::tools::{CallFailure, ToolError};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const METHODS: [&str; 15] = [
    "control.ping",
    "control.status",
    "control.capabilities",
    "control.version",
    "doc.active",
    "game.tick",
    "game.local",
    "game.weapons",
    "game.rules",
    "game.selectWeapon",
    "console.exec",
    "input.status",
    "sound.drain",
    "sound.status",
    "control.shutdown",
];

const MAP: &str = "editor/zoo/smartprop_zoo";

#[derive(Clone, Debug)]
struct Held {
    name: String,
    clip1: i64,
    reserve: i64,
}

/// The simulated player. `ignore_give` lists weapons the game never gives (a mock gap).
#[derive(Clone, Debug)]
struct World {
    map: String,
    tick: u64,
    origin: [f64; 3],
    view: [f64; 3],
    health: i64,
    armor: i64,
    helmet: bool,
    money: i64,
    team: i64,
    weapons: Vec<Held>,
    active: Option<String>,
    ignore_give: Vec<String>,
    commands: Vec<String>,
    /// false: game.local reports not in game (the player is gone)
    in_game: bool,
    /// setpos_exact is accepted but the player stays where it is (a pose that never settles)
    freeze_pose: bool,
    /// after a command containing this text, the player is no longer in game
    die_on: Option<String>,
}

impl World {
    fn new(map: &str) -> World {
        World {
            map: map.to_string(),
            tick: 100,
            origin: [0.0; 3],
            view: [0.0; 3],
            health: 100,
            armor: 0,
            helmet: false,
            money: 800,
            team: 3,
            weapons: Vec::new(),
            active: None,
            ignore_give: Vec::new(),
            commands: Vec::new(),
            in_game: true,
            freeze_pose: false,
            die_on: None,
        }
    }

    fn held(&self, name: &str) -> Option<&Held> {
        self.weapons.iter().find(|w| w.name == name)
    }

    fn active_json(&self) -> Value {
        match self.active.as_deref().and_then(|a| self.held(a)) {
            Some(w) => {
                json!({"entindex": 9, "name": w.name, "defIndex": 1, "clip1": w.clip1, "clip2": 0, "reserve": w.reserve})
            }
            None => Value::Null,
        }
    }

    fn local(&mut self) -> Value {
        self.tick += 1;
        json!({
            "tick": self.tick, "inGame": self.in_game, "map": self.map, "playerSlot": 0,
            "viewAngles": self.view, "inputOverride": null,
            "player": {
                "alive": self.health > 0, "origin": self.origin, "velocity": [0.0, 0.0, 0.0],
                "health": self.health, "armor": self.armor, "helmet": self.helmet,
                "money": self.money, "ducked": false, "team": self.team,
                "activeWeapon": self.active_json(),
            }
        })
    }

    fn weapons(&mut self) -> Value {
        self.tick += 1;
        let list: Vec<Value> = self
            .weapons
            .iter()
            .map(|w| json!({"entindex": 9, "name": w.name, "defIndex": 1, "clip1": w.clip1, "clip2": 0, "reserve": w.reserve}))
            .collect();
        json!({
            "tick": self.tick, "inGame": true, "map": self.map,
            "active": self.active_json(), "count": list.len(), "weapons": list,
        })
    }

    /// One console command, as the engine runs it (`;` separates commands).
    fn run(&mut self, command: &str) {
        self.commands.push(command.to_string());
        for part in command.split(';').map(str::trim).filter(|p| !p.is_empty()) {
            let words: Vec<&str> = part.split_whitespace().collect();
            match words.as_slice() {
                ["setpos_exact", x, y, z] => {
                    if !self.freeze_pose {
                        self.origin = [x.parse().unwrap(), y.parse().unwrap(), z.parse().unwrap()];
                    }
                }
                ["setang_exact", p, y, r] => {
                    self.view = [p.parse().unwrap(), y.parse().unwrap(), r.parse().unwrap()];
                }
                ["ent_fire", "!player", "sethealth", n] => self.health = n.parse().unwrap(),
                ["give", "item_assaultsuit"] => {
                    self.armor = 100;
                    self.helmet = true;
                }
                ["give", "item_kevlar"] => self.armor = 100,
                ["give", name] if !self.ignore_give.iter().any(|i| i == name) => {
                    self.weapons.push(Held {
                        name: name.to_string(),
                        clip1: 12,
                        reserve: 24,
                    });
                    self.active = Some(name.to_string());
                }
                _ => {}
            }
        }
        if self.die_on.as_deref().is_some_and(|d| command.contains(d)) {
            self.in_game = false;
        }
    }
}

type Shared = Arc<Mutex<World>>;

/// A mock bridge serving `world`: game.local/weapons/rules read it, console.exec changes it, and
/// game.selectWeapon makes a held weapon active.
async fn world_bridge(world: Shared) -> MockBridge {
    world_bridge_with(world, &[], None).await
}

/// As `world_bridge`, with extra advertised methods, and the `nth` console.exec call failing with
/// a wire error when given.
async fn world_bridge_with(world: Shared, extra: &[&str], fail_nth: Option<usize>) -> MockBridge {
    let (l, w, c, s) = (world.clone(), world.clone(), world.clone(), world.clone());
    let methods: Vec<&str> = METHODS
        .iter()
        .copied()
        .chain(extra.iter().copied())
        .collect();
    let mut b = MockBridge::builder()
        .methods(methods)
        .responder("game.local", move |_| l.lock().unwrap().local())
        .responder("game.weapons", move |_| w.lock().unwrap().weapons())
        .responder_value(
            "game.rules",
            json!({"tick": 1, "inGame": true, "map": MAP, "roundStartTime": 12.5, "roundsPlayed": 2, "schemaMissing": []}),
        )
        .responder("console.exec", move |params| {
            let command = params["command"].as_str().unwrap_or_default().to_string();
            c.lock().unwrap().run(&command);
            json!({"queued": true})
        })
        .responder("game.selectWeapon", move |params| {
            let name = params["weapon"].as_str().unwrap_or_default().to_string();
            let mut world = s.lock().unwrap();
            if world.held(&name).is_some() {
                world.active = Some(name.clone());
                json!({"selected": true, "weapon": name})
            } else {
                json!({"selected": false, "weapon": null})
            }
        })
        .responder_value("input.status", json!({"angles": null}));
    if extra.contains(&"input.clear") {
        b = b.responder_value("input.clear", json!({"cleared": true}));
    }
    if let Some(n) = fail_nth {
        b = b.error_on_call(
            "console.exec",
            n,
            -32000,
            "console refused the command",
            None,
        );
    }
    b.start().await.unwrap()
}

/// The base player: a pistol-free loadout (knife, ak47, glock), armored, active ak47.
fn base_world(map: &str) -> World {
    let mut w = World::new(map);
    w.origin = [10.0, 20.0, 30.0];
    w.view = [0.0, 90.0, 0.0];
    w.health = 80;
    w.armor = 100;
    w.helmet = true;
    w.weapons = vec![
        Held {
            name: "weapon_knife".into(),
            clip1: -1,
            reserve: 0,
        },
        Held {
            name: "weapon_ak47".into(),
            clip1: 30,
            reserve: 90,
        },
        Held {
            name: "weapon_glock".into(),
            clip1: 17,
            reserve: 120,
        },
    ];
    w.active = Some("weapon_ak47".into());
    w
}

/// The player after the snapshot moved on: elsewhere, hurt, unarmored, knife only, with a deagle.
fn moved_on(map: &str) -> World {
    let mut w = World::new(map);
    w.origin = [-500.0, 0.0, 0.0];
    w.view = [0.0, 0.0, 0.0];
    w.health = 30;
    w.weapons = vec![
        Held {
            name: "weapon_knife".into(),
            clip1: -1,
            reserve: 0,
        },
        Held {
            name: "weapon_deagle".into(),
            clip1: 7,
            reserve: 35,
        },
    ];
    w.active = Some("weapon_knife".into());
    w
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "cs2mcp-snapshot-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

mod common;

fn config(args: &[&str]) -> ClientConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: ClientConfig,
    }
    let mut argv = vec!["t"];
    argv.extend(common::hermetic(args));
    W::parse_from(argv).c
}

type Server = cs2_mcp_common::mcp::McpServer<client_mcp::state::ClientState>;

async fn server_for(mb: &MockBridge, extra: &[&str]) -> Server {
    let mut args = vec![
        "--discovery",
        mb.discovery_path.to_str().unwrap(),
        "--deadline-ms",
        "7000",
    ];
    args.extend_from_slice(extra);
    client_mcp::build_server(&config(&args)).unwrap()
}

async fn call(server: &Server, name: &str, args: Value) -> Result<Value, ToolError> {
    match server.call(name, args).await {
        Ok(out) => Ok(out.json),
        Err(CallFailure::Tool(e)) => Err(e),
        Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
    }
}

fn sent(mb: &MockBridge, method: &str) -> Vec<Value> {
    mb.method_calls(method)
        .into_iter()
        .map(|r| r.params)
        .collect()
}

fn console_commands(mb: &MockBridge) -> Vec<String> {
    sent(mb, "console.exec")
        .iter()
        .map(|p| p["command"].as_str().unwrap().to_string())
        .collect()
}

fn entry<'a>(out: &'a Value, field: &str) -> &'a Value {
    out["comparison"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["field"] == json!(field))
        .unwrap_or_else(|| panic!("no comparison entry {field}: {out}"))
}

/// Capture the base player under `name` and return the snapshot reply.
async fn snapshot_base(server: &Server, name: &str) -> Value {
    call(server, "game_snapshot", json!({"name": name}))
        .await
        .unwrap()
}

#[tokio::test]
async fn snapshot_captures_every_field_and_reads_only() {
    let world = Arc::new(Mutex::new(base_world(MAP)));
    let mb = world_bridge(world.clone()).await;
    let server = server_for(&mb, &[]).await;
    let out = snapshot_base(&server, "base").await;
    let s = &out["snapshot"];
    assert_eq!(s["name"], json!("base"));
    assert_eq!(s["map"], json!(MAP));
    assert!(s["takenAt"].as_f64().unwrap() > 0.0);
    assert!(s["tick"].as_u64().is_some());
    assert_eq!(s["player"]["origin"], json!([10.0, 20.0, 30.0]));
    assert_eq!(s["player"]["angles"], json!([0.0, 90.0, 0.0]));
    assert_eq!(s["player"]["velocity"], json!([0.0, 0.0, 0.0]));
    assert_eq!(s["player"]["health"], json!(80));
    assert_eq!(s["player"]["armor"], json!(100));
    assert_eq!(s["player"]["helmet"], json!(true));
    assert_eq!(s["player"]["money"], json!(800));
    assert_eq!(s["player"]["ducked"], json!(false));
    assert_eq!(s["player"]["team"], json!(3));
    assert_eq!(s["player"]["activeWeapon"], json!("weapon_ak47"));
    assert_eq!(
        s["weapons"],
        json!([
            {"name": "weapon_knife", "defIndex": 1, "clip1": -1, "reserve": 0},
            {"name": "weapon_ak47", "defIndex": 1, "clip1": 30, "reserve": 90},
            {"name": "weapon_glock", "defIndex": 1, "clip1": 17, "reserve": 120},
        ])
    );
    assert_eq!(
        s["rules"],
        json!({"roundStartTime": 12.5, "roundsPlayed": 2})
    );
    assert_eq!(
        out["stored"],
        json!({"name": "base", "count": 1, "cap": 32})
    );
    assert!(out.get("file").is_none());
    for m in ["game.local", "game.weapons", "game.rules"] {
        assert_eq!(sent(&mb, m).len(), 1, "{m} once");
    }
    assert!(
        sent(&mb, "console.exec").is_empty(),
        "a snapshot sends no commands"
    );

    // the same name again replaces: the count stays at one
    let again = snapshot_base(&server, "base").await;
    assert_eq!(again["stored"]["count"], json!(1));
}

#[tokio::test]
async fn snapshot_refuses_when_not_in_game_and_validates_arguments() {
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder_value(
            "game.local",
            json!({"tick": 5, "inGame": false, "map": "", "player": null}),
        )
        .responder_value("game.weapons", json!({"weapons": []}))
        .start()
        .await
        .unwrap();
    let server = server_for(&mb, &[]).await;
    let err = call(&server, "game_snapshot", json!({"name": "x"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "PrerequisiteFailed");
    assert!(err.retry_safe);
    assert!(sent(&mb, "game.weapons").is_empty());
    for bad in [
        json!({}),
        json!({"name": "a/b"}),
        json!({"name": "x", "export": "yes"}),
        json!({"name": "x", "bogus": 1}),
    ] {
        let err = call(&server, "game_snapshot", bad.clone())
            .await
            .unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{bad}");
    }
}

#[tokio::test]
async fn store_is_bounded_and_names_the_dropped_snapshot() {
    let world = Arc::new(Mutex::new(base_world(MAP)));
    let mb = world_bridge(world).await;
    let server = server_for(&mb, &[]).await;
    let mut last = Value::Null;
    for i in 0..33 {
        last = snapshot_base(&server, &format!("s{i}")).await;
        if i < 32 {
            assert!(last.get("dropped").is_none());
        }
    }
    assert_eq!(
        last["stored"],
        json!({"name": "s32", "count": 32, "cap": 32})
    );
    assert_eq!(last["dropped"], json!("s0"));
    // s0 is gone: restoring it is an unknown name, s1 is still there
    let err = call(&server, "game_restore", json!({"name": "s0"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "InvalidParams");
    assert!(err.message.contains("s32") && !err.message.contains("s0,"));
    call(&server, "game_restore", json!({"name": "s1"}))
        .await
        .unwrap();
}

#[tokio::test]
async fn export_writes_the_snapshot_under_artifacts() {
    let world = Arc::new(Mutex::new(base_world(MAP)));
    let mb = world_bridge(world).await;
    let dir = TempDir::new("export");
    let server = server_for(&mb, &["--project-root", dir.0.to_str().unwrap()]).await;
    let out = call(
        &server,
        "game_snapshot",
        json!({"name": "exp", "export": true}),
    )
    .await
    .unwrap();
    let file = PathBuf::from(out["file"].as_str().unwrap());
    assert_eq!(file, dir.0.join("artifacts/snapshots/exp.json"));
    let written: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    // takenAt is a float: the file's text reads back to within float noise, the rest is exact
    let (mut w, mut o) = (written.clone(), out["snapshot"].clone());
    w.as_object_mut().unwrap().remove("takenAt");
    o.as_object_mut().unwrap().remove("takenAt");
    assert_eq!(w, o);
    assert!(
        (written["takenAt"].as_f64().unwrap() - out["snapshot"]["takenAt"].as_f64().unwrap()).abs()
            < 1e-3
    );
    // the temp files are uniquely named and renamed away: none is left in the directory
    let leftovers: Vec<String> = std::fs::read_dir(dir.0.join("artifacts/snapshots"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[tokio::test]
async fn restore_on_another_map_refuses_and_sends_nothing() {
    let world = Arc::new(Mutex::new(base_world(MAP)));
    let mb = world_bridge(world.clone()).await;
    let server = server_for(&mb, &[]).await;
    snapshot_base(&server, "base").await;
    world.lock().unwrap().map = "de_dust2".into();
    mb.reset_trace();
    let err = call(&server, "game_restore", json!({"name": "base"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "PrerequisiteFailed");
    assert!(err.retry_safe);
    assert_eq!(err.data["snapshotMap"], json!(MAP));
    assert_eq!(err.data["currentMap"], json!("de_dust2"));
    assert!(console_commands(&mb).is_empty());
    assert!(sent(&mb, "game.selectWeapon").is_empty());
}

#[tokio::test]
async fn restore_sends_pose_health_armor_weapons_and_selects_in_order() {
    let world = Arc::new(Mutex::new(base_world(MAP)));
    let mb = world_bridge(world.clone()).await;
    let server = server_for(&mb, &[]).await;
    snapshot_base(&server, "base").await;
    *world.lock().unwrap() = moved_on(MAP);
    mb.reset_trace();
    let out = call(&server, "game_restore", json!({"name": "base"}))
        .await
        .unwrap();
    assert_eq!(
        console_commands(&mb),
        vec![
            "setpos_exact 10.000 20.000 30.000; setang_exact 0.000 90.000 0.000",
            "ent_fire !player sethealth 80",
            "give item_assaultsuit",
            "give weapon_ak47",
            "give weapon_glock",
        ]
    );
    // the active weapon: the last give made the glock active, so the snapshot's ak47 is selected
    let selects = sent(&mb, "game.selectWeapon");
    assert_eq!(selects.len(), 1);
    assert_eq!(selects[0]["weapon"], json!("weapon_ak47"));
    assert_eq!(
        out["commands"].as_array().unwrap().last(),
        Some(&json!("selectWeapon weapon_ak47"))
    );
    assert_eq!(out["restored"], json!(true), "{out}");
    assert_eq!(out["name"], json!("base"));
    assert_eq!(out["map"], json!(MAP));
    assert_eq!(out["parts"], json!(["pose", "health", "armor", "weapons"]));
    for (field, ok) in [
        ("origin", true),
        ("angles", true),
        ("health", true),
        ("armor", true),
        ("helmet", true),
        ("weapons.weapon_knife", true),
        ("weapons.weapon_ak47", true),
        ("weapons.weapon_glock", true),
        ("activeWeapon", true),
    ] {
        assert_eq!(entry(&out, field)["ok"], json!(ok), "{field}: {out}");
    }
    assert_eq!(entry(&out, "origin")["got"], json!([10.0, 20.0, 30.0]));
    assert_eq!(entry(&out, "health")["got"], json!(80));
    assert_eq!(out["approx"], json!(["armor"]));
    assert_eq!(
        out["extraWeapons"],
        json!(["weapon_deagle"]),
        "extras are reported, not dropped"
    );
    // the world really holds the deagle still
    assert!(world.lock().unwrap().held("weapon_deagle").is_some());
    // only the requested parts' fields, each with got and whether it differs
    let not: Vec<(&str, Value)> = out["notRestored"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| (n["field"].as_str().unwrap(), n["mismatch"].clone()))
        .collect();
    let names: Vec<&str> = not.iter().map(|(f, _)| *f).collect();
    assert_eq!(
        names,
        vec![
            "velocity",
            "ducked",
            "clip1.weapon_knife",
            "reserve.weapon_knife",
            "clip1.weapon_ak47",
            "reserve.weapon_ak47",
            "clip1.weapon_glock",
            "reserve.weapon_glock",
        ]
    );
    let mismatch = |f: &str| not.iter().find(|(n, _)| *n == f).unwrap().1.clone();
    assert_eq!(mismatch("velocity"), json!(false));
    assert_eq!(mismatch("clip1.weapon_knife"), json!(false));
    assert_eq!(
        mismatch("clip1.weapon_ak47"),
        json!(true),
        "the give loaded 12, the snapshot had 30"
    );
    assert_eq!(
        mismatch("reserve.weapon_ak47"),
        json!(true),
        "the give loaded 24 reserve, the snapshot had 90"
    );
    // every command went out once: no retry inside the tool
    assert_eq!(console_commands(&mb).len(), 5);
}

#[tokio::test]
async fn a_weapon_the_game_never_gives_is_restored_false() {
    let world = Arc::new(Mutex::new(base_world(MAP)));
    let mb = world_bridge(world.clone()).await;
    let server = server_for(&mb, &[]).await;
    snapshot_base(&server, "base").await;
    {
        let mut w = world.lock().unwrap();
        *w = moved_on(MAP);
        w.ignore_give.push("weapon_glock".into());
    }
    mb.reset_trace();
    let out = call(
        &server,
        "game_restore",
        json!({"name": "base", "timeoutMs": 300}),
    )
    .await
    .unwrap();
    assert_eq!(out["restored"], json!(false));
    let glock = entry(&out, "weapons.weapon_glock");
    assert_eq!(glock["ok"], json!(false));
    assert_eq!(glock["wanted"], json!(1));
    assert_eq!(glock["got"], json!(0));
    // the others still match: only the missing weapon fails
    assert_eq!(entry(&out, "weapons.weapon_ak47")["ok"], json!(true));
    assert_eq!(entry(&out, "health")["ok"], json!(true));
    // the snapshot's active weapon (ak47) is held and already active: no select is sent
    // a select is sent after the gives even though the active weapon is already right: the give
    // into an occupied slot may have changed it
    assert_eq!(sent(&mb, "game.selectWeapon").len(), 1);
    assert_eq!(entry(&out, "activeWeapon")["ok"], json!(true));
}

#[tokio::test]
async fn restore_of_selected_parts_sends_only_those() {
    let world = Arc::new(Mutex::new(base_world(MAP)));
    let mb = world_bridge(world.clone()).await;
    let server = server_for(&mb, &[]).await;
    snapshot_base(&server, "base").await;
    *world.lock().unwrap() = moved_on(MAP);
    mb.reset_trace();
    let out = call(
        &server,
        "game_restore",
        json!({"name": "base", "parts": ["health", "pose"]}),
    )
    .await
    .unwrap();
    // pose first (run order), then health; nothing else
    assert_eq!(
        console_commands(&mb),
        vec![
            "setpos_exact 10.000 20.000 30.000; setang_exact 0.000 90.000 0.000",
            "ent_fire !player sethealth 80",
        ]
    );
    assert_eq!(out["parts"], json!(["pose", "health"]));
    assert_eq!(out["restored"], json!(true));
    assert!(out["comparison"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["field"] != json!("weapons.weapon_ak47")));
}

#[tokio::test]
async fn an_inline_snapshot_restores_without_a_store_entry() {
    let world = Arc::new(Mutex::new(moved_on(MAP)));
    let mb = world_bridge(world.clone()).await;
    let server = server_for(&mb, &[]).await;
    let out = call(
        &server,
        "game_restore",
        json!({
            "snapshot": {
                "map": MAP,
                "player": {"origin": [1.0, 2.0, 3.0], "angles": [0.0, 0.0, 0.0], "health": 55},
                "weapons": []
            },
            "parts": ["pose", "health"]
        }),
    )
    .await
    .unwrap();
    assert_eq!(out["name"], Value::Null);
    assert_eq!(out["restored"], json!(true), "{out}");
    assert_eq!(world.lock().unwrap().health, 55);
}

#[tokio::test]
async fn invalid_restore_arguments_and_snapshots_send_nothing() {
    let world = Arc::new(Mutex::new(base_world(MAP)));
    let mb = world_bridge(world).await;
    let server = server_for(&mb, &[]).await;
    snapshot_base(&server, "base").await;
    mb.reset_trace();
    let good = json!({"map": MAP, "player": {"origin": [1, 2, 3], "angles": [0, 0, 0], "health": 50}, "weapons": []});
    let mut bad_health = good.clone();
    bad_health["player"]["health"] = json!(0);
    let mut bad_weapon = good.clone();
    bad_weapon["weapons"] = json!([{"name": "weapon_ak47; quit"}]);
    let mut bad_angles = good.clone();
    bad_angles["player"]["angles"] = json!([120, 0, 0]);
    for bad in [
        json!({}),
        json!({"name": "base", "snapshot": good}),
        json!({"name": "nope"}),
        json!({"name": "a b"}),
        json!({"snapshot": bad_health}),
        json!({"snapshot": bad_weapon}),
        json!({"snapshot": bad_angles}),
        json!({"snapshot": {"map": MAP}}),
        json!({"name": "base", "parts": ["money"]}),
        json!({"name": "base", "parts": []}),
        json!({"name": "base", "timeoutMs": 0}),
        json!({"name": "base", "bogus": true}),
    ] {
        let err = call(&server, "game_restore", bad.clone())
            .await
            .unwrap_err();
        assert_eq!(err.name, "InvalidParams", "{bad}: {}", err.message);
    }
    assert!(console_commands(&mb).is_empty());
    assert!(sent(&mb, "game.local").is_empty());
}

#[tokio::test]
async fn an_override_refuses_the_pose_before_any_command() {
    let world = Arc::new(Mutex::new(base_world(MAP)));
    let mb = MockBridge::builder()
        .methods(METHODS)
        .responder_value("input.status", json!({"angles": [10.0, 20.0, 0.0]}))
        .responder("game.local", {
            let w = world.clone();
            move |_| w.lock().unwrap().local()
        })
        .responder("game.weapons", {
            let w = world.clone();
            move |_| w.lock().unwrap().weapons()
        })
        .responder("console.exec", {
            let w = world.clone();
            move |p| {
                w.lock()
                    .unwrap()
                    .run(p["command"].as_str().unwrap_or_default());
                json!({"queued": true})
            }
        })
        .start()
        .await
        .unwrap();
    let server = server_for(&mb, &[]).await;
    let snap = json!({"map": MAP, "player": {"origin": [1, 2, 3], "angles": [0, 0, 0], "health": 50}, "weapons": []});
    let err = call(
        &server,
        "game_restore",
        json!({"snapshot": snap, "parts": ["pose"]}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "PrerequisiteFailed");
    assert!(err.retry_safe);
    assert!(err.message.contains("input_clear first"));
    assert!(console_commands(&mb).is_empty());
}

const POSE_CMD: &str = "setpos_exact 10.000 20.000 30.000; setang_exact 0.000 90.000 0.000";

/// The base snapshot taken, then the world moved on (the usual restore setup).
async fn base_then_moved_on(world: &Shared, mb: &MockBridge, server: &Server) {
    snapshot_base(server, "base").await;
    *world.lock().unwrap() = moved_on(MAP);
    mb.reset_trace();
}

#[tokio::test]
async fn a_pose_that_does_not_settle_is_recorded_and_the_restore_goes_on() {
    let world = Arc::new(Mutex::new(base_world(MAP)));
    let mb = world_bridge(world.clone()).await;
    let server = server_for(&mb, &[]).await;
    base_then_moved_on(&world, &mb, &server).await;
    world.lock().unwrap().freeze_pose = true;
    let out = call(
        &server,
        "game_restore",
        json!({"name": "base", "timeoutMs": 300}),
    )
    .await
    .unwrap();
    assert_eq!(out["pose"]["ok"], json!(false), "{out}");
    assert!(out["pose"]["toTarget"].is_object(), "{out}");
    assert_eq!(out["restored"], json!(false));
    assert_eq!(entry(&out, "origin")["ok"], json!(false));
    // the pose command went out once, and health and armor and the weapons still follow it
    let cmds = console_commands(&mb);
    assert_eq!(cmds[0], POSE_CMD);
    assert!(
        cmds.contains(&"ent_fire !player sethealth 80".to_string()),
        "{cmds:?}"
    );
    assert!(
        cmds.contains(&"give item_assaultsuit".to_string()),
        "{cmds:?}"
    );
    assert!(cmds.contains(&"give weapon_ak47".to_string()), "{cmds:?}");
    assert_eq!(entry(&out, "health")["ok"], json!(true));
    assert_eq!(out["budgetExhausted"], json!(false));
}

#[tokio::test]
async fn a_later_console_error_is_not_retry_safe_and_lists_what_was_sent() {
    let world = Arc::new(Mutex::new(base_world(MAP)));
    // call 1 is the pose, call 2 the health command, which fails
    let mb = world_bridge_with(world.clone(), &[], Some(2)).await;
    let server = server_for(&mb, &[]).await;
    base_then_moved_on(&world, &mb, &server).await;
    let err = call(&server, "game_restore", json!({"name": "base"}))
        .await
        .unwrap_err();
    assert!(!err.retry_safe, "{err:?}");
    assert_eq!(err.data["commandsSent"], json!(true));
    assert_eq!(err.data["commands"], json!([POSE_CMD]));
    assert_eq!(
        console_commands(&mb).len(),
        2,
        "nothing is sent after the failed command"
    );
}

#[tokio::test]
async fn a_first_command_error_is_passed_through_and_nothing_more_is_sent() {
    let world = Arc::new(Mutex::new(base_world(MAP)));
    let mb = world_bridge_with(world.clone(), &[], Some(1)).await;
    let server = server_for(&mb, &[]).await;
    base_then_moved_on(&world, &mb, &server).await;
    let err = call(&server, "game_restore", json!({"name": "base"}))
        .await
        .unwrap_err();
    // nothing had gone out before the failing command: no commandsSent, the error stands as it is
    assert!(err.data.get("commandsSent").is_none(), "{err:?}");
    assert_eq!(console_commands(&mb).len(), 1);
}

#[tokio::test]
async fn armor_and_helmet_removal_is_classified_without_waiting() {
    let world = Arc::new(Mutex::new(base_world(MAP)));
    let mb = world_bridge(world.clone()).await;
    let server = server_for(&mb, &[]).await;
    {
        let mut w = world.lock().unwrap();
        w.armor = 0;
        w.helmet = false;
    }
    snapshot_base(&server, "bare").await;
    {
        // the player has armor and a helmet the snapshot did not
        let mut w = world.lock().unwrap();
        w.armor = 100;
        w.helmet = true;
    }
    mb.reset_trace();
    let out = call(
        &server,
        "game_restore",
        json!({"name": "bare", "parts": ["pose", "armor"], "timeoutMs": 5000}),
    )
    .await
    .unwrap();
    assert_eq!(out["restored"], json!(false));
    assert!(entry(&out, "armor")["reason"]
        .as_str()
        .unwrap()
        .contains("removed"));
    assert!(entry(&out, "helmet")["reason"].is_string());
    assert!(!console_commands(&mb).iter().any(|c| c.starts_with("give")));
    assert!(
        out["waitedMs"].as_u64().unwrap() < 2000,
        "classified mismatches are not polled for: {out}"
    );
}

#[tokio::test]
async fn extra_copies_of_a_weapon_are_classified_without_waiting() {
    let world = Arc::new(Mutex::new(base_world(MAP)));
    let mb = world_bridge(world.clone()).await;
    let server = server_for(&mb, &[]).await;
    snapshot_base(&server, "base").await;
    world.lock().unwrap().weapons.push(Held {
        name: "weapon_ak47".into(),
        clip1: 30,
        reserve: 90,
    });
    mb.reset_trace();
    let out = call(
        &server,
        "game_restore",
        json!({"name": "base", "parts": ["weapons"], "timeoutMs": 5000}),
    )
    .await
    .unwrap();
    assert_eq!(out["restored"], json!(false));
    assert!(entry(&out, "weapons.weapon_ak47")["reason"]
        .as_str()
        .unwrap()
        .contains("cannot drop"));
    assert!(out["waitedMs"].as_u64().unwrap() < 2000, "{out}");
    assert!(
        console_commands(&mb).is_empty(),
        "nothing to give and nothing to select"
    );
}

#[tokio::test]
async fn the_player_dying_during_the_pose_is_an_error_after_the_send() {
    let world = Arc::new(Mutex::new(base_world(MAP)));
    let mb = world_bridge(world.clone()).await;
    let server = server_for(&mb, &[]).await;
    base_then_moved_on(&world, &mb, &server).await;
    world.lock().unwrap().die_on = Some("setpos_exact".into());
    let err = call(&server, "game_restore", json!({"name": "base"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "PrerequisiteFailed");
    assert!(!err.retry_safe);
    assert_eq!(err.data["commandsSent"], json!(true));
    assert_eq!(err.data["commands"], json!([POSE_CMD]));
}

#[tokio::test]
async fn not_in_game_during_the_settle_is_an_error_after_the_send() {
    let world = Arc::new(Mutex::new(base_world(MAP)));
    let mb = world_bridge(world.clone()).await;
    let server = server_for(&mb, &[]).await;
    base_then_moved_on(&world, &mb, &server).await;
    world.lock().unwrap().die_on = Some("sethealth".into());
    let err = call(&server, "game_restore", json!({"name": "base"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "PrerequisiteFailed");
    assert!(!err.retry_safe);
    assert_eq!(err.data["commandsSent"], json!(true));
    let cmds = err.data["commands"].as_array().unwrap();
    assert!(
        cmds.contains(&json!("ent_fire !player sethealth 80")),
        "{cmds:?}"
    );
}

#[tokio::test]
async fn input_clear_runs_first_when_the_bridge_advertises_it() {
    let world = Arc::new(Mutex::new(base_world(MAP)));
    let mb = world_bridge_with(world.clone(), &["input.clear"], None).await;
    let server = server_for(&mb, &[]).await;
    base_then_moved_on(&world, &mb, &server).await;
    let out = call(&server, "game_restore", json!({"name": "base"}))
        .await
        .unwrap();
    assert_eq!(out["inputCleared"], json!(true));
    assert_eq!(out["commands"][0], json!("input.clear"));
    assert_eq!(sent(&mb, "input.clear").len(), 1);
    assert_eq!(console_commands(&mb)[0], POSE_CMD);
}

#[tokio::test]
async fn a_team_mismatch_refuses_before_anything_is_sent() {
    let world = Arc::new(Mutex::new(base_world(MAP)));
    let mb = world_bridge(world.clone()).await;
    let server = server_for(&mb, &[]).await;
    snapshot_base(&server, "base").await;
    world.lock().unwrap().team = 2;
    mb.reset_trace();
    let err = call(&server, "game_restore", json!({"name": "base"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "PrerequisiteFailed");
    assert!(err.retry_safe);
    assert_eq!(err.data["snapshotTeam"], json!(3));
    assert_eq!(err.data["currentTeam"], json!(2));
    assert_eq!(err.data["commandsSent"], json!(false));
    assert!(console_commands(&mb).is_empty());
    assert!(sent(&mb, "game.selectWeapon").is_empty());
}

#[tokio::test]
async fn a_failed_export_leaves_the_store_unchanged() {
    let world = Arc::new(Mutex::new(base_world(MAP)));
    let mb = world_bridge(world).await;
    let dir = TempDir::new("blocked-export");
    // the project root is a file: the artifacts directory cannot be created
    let blocked = dir.0.join("not-a-dir");
    std::fs::write(&blocked, "x").unwrap();
    let server = server_for(&mb, &["--project-root", blocked.to_str().unwrap()]).await;
    let err = call(
        &server,
        "game_snapshot",
        json!({"name": "keep", "export": true}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "ExportFailed");
    let err = call(&server, "game_restore", json!({"name": "keep"}))
        .await
        .unwrap_err();
    assert_eq!(
        err.name, "InvalidParams",
        "the snapshot was not stored: {}",
        err.message
    );
}
