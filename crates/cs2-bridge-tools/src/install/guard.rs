//! The busy-container guard. The real game trees are shared with running containers: install
//! must not rewrite files a live engine is reading. Wine: a running container with a bind whose
//! source is the app dir or a parent/child of it. Linux: a running container using the
//! `cs2-home` volume (a named-volume mount, or a bind of its mountpoint or a parent/child).
//! Every podman call is read-only; a FAILING podman refuses - the guard never guesses "nothing
//! is running". The guard applies to the real trees only: an overridden (test) tree skips it
//! unless `GUARD_FAKE=1`.

use std::path::Path;
use std::process::{Command, Stdio};

use super::fsx::{die, die4, realpath_m, say, R};
use super::{Ctx, Platform, LINUX_VOLUME};

/// `podman <args>` with the runtime's XDG dir; `Some(stdout)` on exit 0.
pub fn podman_out(ctx: &Ctx, args: &[&str], quiet: bool) -> Option<String> {
    let out = Command::new(&ctx.podman)
        .args(args)
        .env("XDG_RUNTIME_DIR", &ctx.xdg)
        .stdin(Stdio::null())
        .stderr(if quiet {
            Stdio::null()
        } else {
            Stdio::inherit()
        })
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The `cs2-home` volume's mountpoint (read-only podman call).
pub fn linux_vmp(ctx: &Ctx) -> Option<String> {
    podman_out(
        ctx,
        &[
            "volume",
            "inspect",
            LINUX_VOLUME,
            "--format",
            "{{.Mountpoint}}",
        ],
        false,
    )
    .map(|s| s.trim_end_matches('\n').to_string())
}

/// The game dir inside a volume mountpoint.
pub fn volume_game_dir(vmp: &str) -> String {
    format!("{vmp}/steamapps/common/Counter-Strike Global Offensive/game")
}

/// Must the guard run? Decided on `realpath -m` of both sides, so a trailing slash, `..`
/// segments or a symlink spelling of the real tree still gets the guard.
pub fn guard_needed(ctx: &Ctx) -> bool {
    if ctx.guard_fake {
        return true;
    }
    match ctx.platform {
        Platform::Wine => realpath_m(&ctx.game) == realpath_m(&ctx.real_game),
        Platform::Linux => {
            let Some(lg) = &ctx.linux_game_override else {
                return true;
            };
            // the volume cannot be resolved: the guard must run (its own podman failure then
            // refuses loudly)
            let Some(vmp) = linux_vmp(ctx) else {
                return true;
            };
            realpath_m(lg) == realpath_m(Path::new(&volume_game_dir(&vmp)))
        }
    }
}

/// The running containers that mount the tree. `Err(rc)`: podman failed (3: ps/volume, 4: an
/// inspect of a listed container, 5: unreadable mounts) - the caller refuses.
pub fn busy_probe(ctx: &Ctx) -> Result<Vec<String>, i32> {
    let probe = match ctx.platform {
        Platform::Wine => realpath_m(&ctx.app).to_string_lossy().into_owned(),
        Platform::Linux => linux_vmp(ctx).ok_or(3)?,
    };
    let mut ids_out = podman_out(ctx, &["ps", "-q"], false).ok_or(3)?;
    if ctx.platform == Platform::Linux {
        let filter = format!("volume={LINUX_VOLUME}");
        let extra = podman_out(ctx, &["ps", "-q", "--filter", &filter], false).ok_or(3)?;
        ids_out.push(' ');
        ids_out.push_str(&extra);
    }
    let mut ids: Vec<&str> = Vec::new();
    for id in ids_out.split_whitespace() {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    let mut busy = Vec::new();
    for id in ids {
        let name = podman_out(ctx, &["inspect", "--format", "{{.Name}}", id], true)
            .map(|s| s.trim_end_matches('\n').to_string())
            .unwrap_or_default();
        if name.is_empty() {
            // a container that exited between `ps` and `inspect` is skipped; one that is still
            // listed but cannot be inspected stays a refusal (never guess)
            let now = podman_out(ctx, &["ps", "-q"], false).ok_or(3)?;
            if now.contains(id) {
                return Err(4);
            }
            say(format_args!(
                "  (container {id} exited between ps and inspect - skipped)"
            ));
            continue;
        }
        let mounts =
            podman_out(ctx, &["inspect", "--format", "{{json .Mounts}}", id], false).ok_or(4)?;
        if mounts_match(ctx.platform, &probe, &mounts)? {
            busy.push(name);
        }
    }
    Ok(busy)
}

/// Does a container's `{{json .Mounts}}` hold the tree? `Err(5)`: not a JSON list of objects.
pub fn mounts_match(platform: Platform, probe: &str, mounts_json: &str) -> Result<bool, i32> {
    let mounts: Vec<serde_json::Value> = serde_json::from_str(mounts_json.trim()).map_err(|_| 5)?;
    let field = |m: &serde_json::Map<String, serde_json::Value>, keys: &[&str]| -> String {
        keys.iter()
            .filter_map(|k| m.get(*k).and_then(|v| v.as_str()))
            .find(|s| !s.is_empty())
            .unwrap_or("")
            .to_string()
    };
    let probe = rpath(probe);
    for m in &mounts {
        let m = m.as_object().ok_or(5)?;
        let t = field(m, &["Type", "MountType"]);
        let s = rpath(&field(m, &["Source"]));
        match platform {
            Platform::Wine => {
                if !s.is_empty() && (t == "bind" || t.is_empty()) && related(&s, &probe) {
                    return Ok(true);
                }
            }
            Platform::Linux => {
                let name = field(m, &["Name"]);
                if t == "volume" && (name == LINUX_VOLUME || s == LINUX_VOLUME) {
                    return Ok(true);
                }
                if t == "bind" && !s.is_empty() && !probe.is_empty() && related(&s, &probe) {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

/// Symlink-resolved; `/` (and an empty path) match nothing: a `/` bind is the whole host, not a
/// marker for this tree (a `/`-matching rule would refuse on every container).
fn rpath(p: &str) -> String {
    if p.is_empty() {
        return String::new();
    }
    let r = realpath_m(Path::new(p)).to_string_lossy().into_owned();
    if r == "/" {
        String::new()
    } else {
        r
    }
}

/// The same path, or a parent/child of it (a mount of the tree's parent also holds the tree).
fn related(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() || a == "/" || b == "/" {
        return false;
    }
    a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

/// install/uninstall (and ensure's repair) refuse while a running container mounts the tree.
pub fn guard_live_game(ctx: &Ctx) -> R<()> {
    if !guard_needed(ctx) {
        return Ok(());
    }
    say(format_args!(
        "checking for running containers mounting the tree (read-only podman){}",
        if ctx.guard_fake_set {
            " [GUARD_FAKE]"
        } else {
            ""
        }
    ));
    let busy = busy_probe(ctx).map_err(|rc| {
        die(format_args!(
            "podman failed while probing running containers (rc {rc}); cannot see whether a container mounts the tree - refusing to touch the game tree (set CS2_GAME / CS2_LINUX_GAME for a test tree)"
        ))
    })?;
    if !busy.is_empty() {
        say("these running containers mount the tree:");
        for n in &busy {
            say(format_args!("  - {n}"));
        }
        return Err(die4(
            "refusing to touch the game tree while the containers above mount it (stop them first; for tests use CS2_GAME / CS2_LINUX_GAME)",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wine_matches_binds_of_the_tree_its_parents_and_children_only() {
        let probe = "/nonexistent-cs2/proj/workshop/run/app";
        let bind = |src: &str| {
            format!(r#"[{{"Type":"volume","Name":"cs2-home"}},{{"Type":"bind","Source":"{src}"}}]"#)
        };
        assert_eq!(mounts_match(Platform::Wine, probe, &bind(probe)), Ok(true));
        assert_eq!(
            mounts_match(Platform::Wine, probe, &bind("/nonexistent-cs2/proj")),
            Ok(true)
        );
        assert_eq!(
            mounts_match(Platform::Wine, probe, &bind(&format!("{probe}/game"))),
            Ok(true)
        );
        assert_eq!(
            mounts_match(Platform::Wine, probe, &bind("/nonexistent-cs2/other")),
            Ok(false)
        );
        assert_eq!(mounts_match(Platform::Wine, probe, &bind("/")), Ok(false));
        assert_eq!(mounts_match(Platform::Wine, probe, "[]"), Ok(false));
        assert_eq!(mounts_match(Platform::Wine, probe, "not json"), Err(5));
    }

    #[test]
    fn linux_matches_the_volume_by_name_or_its_mountpoint() {
        let vmp = "/nonexistent-cs2/volumes/cs2-home/_data";
        assert_eq!(
            mounts_match(
                Platform::Linux,
                vmp,
                r#"[{"Type":"volume","Name":"cs2-home"}]"#
            ),
            Ok(true)
        );
        assert_eq!(
            mounts_match(
                Platform::Linux,
                vmp,
                r#"[{"Type":"volume","Name":"other"}]"#
            ),
            Ok(false)
        );
        let bind = format!(r#"[{{"Type":"bind","Source":"{vmp}/steamapps"}}]"#);
        assert_eq!(mounts_match(Platform::Linux, vmp, &bind), Ok(true));
    }
}
