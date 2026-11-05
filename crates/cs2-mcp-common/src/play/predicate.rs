//! A field predicate over a JSON reply: `{path: "player.origin[0]", gt: 1200}`. Used by
//! `play_test`'s `wait_state` and `expect_state` steps over `game_state`.
//!
//! The path is dot-separated keys with `[n]` array indices (`player.alive`, `player.origin[2]`,
//! `player.activeWeapon.name`). Exactly one operator: `eq`, `ne`, `gt`, `ge`, `lt`, `le` (numbers),
//! `near` (a number, with `tolerance`, default 0.5), `contains` (substring of a string field),
//! `exists` (bool). A missing field never satisfies an operator except `exists: false`.
use crate::tools::ToolError;
use serde_json::{json, Value};

const OPS: [&str; 9] = ["eq", "ne", "gt", "ge", "lt", "le", "near", "contains", "exists"];

#[derive(Debug, Clone, PartialEq)]
enum Seg {
    Key(String),
    Index(usize),
}

#[derive(Debug, Clone, PartialEq)]
enum Op {
    Eq(Value),
    Ne(Value),
    Gt(f64),
    Ge(f64),
    Lt(f64),
    Le(f64),
    Near(f64, f64),
    Contains(String),
    Exists(bool),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Predicate {
    pub path: String,
    segs: Vec<Seg>,
    op: Op,
    /// the operator's name and wanted value, for reports
    pub op_name: &'static str,
    pub wanted: Value,
}

/// What a predicate saw: `ok` and the field's value (null when missing).
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub ok: bool,
    pub got: Value,
}

fn invalid(m: impl Into<String>) -> ToolError {
    ToolError::invalid_arguments(m)
}

fn parse_path(path: &str) -> Result<Vec<Seg>, ToolError> {
    let bad = || invalid(format!("path {path:?} is not a field path like player.origin[0]"));
    if path.is_empty() {
        return Err(bad());
    }
    let mut segs = Vec::new();
    for part in path.split('.') {
        let (key, mut rest) = match part.find('[') {
            Some(i) => (&part[..i], &part[i..]),
            None => (part, ""),
        };
        if key.is_empty() && segs.is_empty() {
            return Err(bad());
        }
        if !key.is_empty() {
            if key.contains(']') {
                return Err(bad());
            }
            segs.push(Seg::Key(key.to_string()));
        } else if rest.is_empty() {
            return Err(bad());
        }
        while !rest.is_empty() {
            let close = rest.find(']').ok_or_else(bad)?;
            let n: usize = rest[1..close].parse().map_err(|_| bad())?;
            segs.push(Seg::Index(n));
            rest = &rest[close + 1..];
            if !rest.is_empty() && !rest.starts_with('[') {
                return Err(bad());
            }
        }
    }
    Ok(segs)
}

impl Predicate {
    /// Parse `{path, <one operator>, tolerance?}`; unknown keys are refused.
    pub fn parse(v: &Value) -> Result<Predicate, ToolError> {
        let obj = v.as_object().ok_or_else(|| invalid("a predicate must be an object {path, eq|ne|gt|ge|lt|le|near|contains|exists}"))?;
        if let Some(k) = obj
            .keys()
            .find(|k| k.as_str() != "path" && k.as_str() != "tolerance" && !OPS.contains(&k.as_str()))
        {
            return Err(invalid(format!(
                "unknown key {k:?} in a predicate (allowed: path, tolerance, {})",
                OPS.join(", ")
            )));
        }
        let path = obj
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("a predicate needs path (e.g. \"player.origin[0]\")"))?;
        let segs = parse_path(path)?;
        let ops: Vec<&str> = OPS.iter().copied().filter(|o| obj.contains_key(*o)).collect();
        let [name] = ops[..] else {
            return Err(invalid(format!(
                "predicate on {path:?} needs exactly one operator of {} (got {})",
                OPS.join(", "),
                ops.len()
            )));
        };
        if obj.contains_key("tolerance") && name != "near" {
            return Err(invalid("tolerance belongs to near"));
        }
        let wanted = obj[name].clone();
        let num = || {
            wanted
                .as_f64()
                .filter(|n| n.is_finite())
                .ok_or_else(|| invalid(format!("{name} on {path:?} needs a number")))
        };
        let op = match name {
            "eq" => Op::Eq(wanted.clone()),
            "ne" => Op::Ne(wanted.clone()),
            "gt" => Op::Gt(num()?),
            "ge" => Op::Ge(num()?),
            "lt" => Op::Lt(num()?),
            "le" => Op::Le(num()?),
            "near" => {
                let tol = match obj.get("tolerance") {
                    None => 0.5,
                    Some(t) => t
                        .as_f64()
                        .filter(|t| t.is_finite() && *t >= 0.0)
                        .ok_or_else(|| invalid("tolerance must be a number >= 0"))?,
                };
                Op::Near(num()?, tol)
            }
            "contains" => Op::Contains(
                wanted
                    .as_str()
                    .ok_or_else(|| invalid(format!("contains on {path:?} needs a string")))?
                    .to_string(),
            ),
            "exists" => Op::Exists(
                wanted
                    .as_bool()
                    .ok_or_else(|| invalid(format!("exists on {path:?} needs true or false")))?,
            ),
            _ => unreachable!("OPS is exhaustive"),
        };
        Ok(Predicate {
            path: path.to_string(),
            segs,
            op,
            op_name: OPS.iter().copied().find(|o| *o == name).unwrap_or("eq"),
            wanted,
        })
    }

    /// The field at the path, if the reply has one (null counts as missing).
    pub fn lookup<'a>(&self, root: &'a Value) -> Option<&'a Value> {
        let mut cur = root;
        for seg in &self.segs {
            cur = match seg {
                Seg::Key(k) => cur.get(k)?,
                Seg::Index(i) => cur.get(*i)?,
            };
        }
        (!cur.is_null()).then_some(cur)
    }

    pub fn check(&self, root: &Value) -> Verdict {
        let found = self.lookup(root);
        let got = found.cloned().unwrap_or(Value::Null);
        let ok = match (&self.op, found) {
            (Op::Exists(want), f) => f.is_some() == *want,
            (_, None) => false,
            (Op::Eq(w), Some(v)) => same(v, w),
            (Op::Ne(w), Some(v)) => !same(v, w),
            (Op::Gt(w), Some(v)) => v.as_f64().is_some_and(|n| n > *w),
            (Op::Ge(w), Some(v)) => v.as_f64().is_some_and(|n| n >= *w),
            (Op::Lt(w), Some(v)) => v.as_f64().is_some_and(|n| n < *w),
            (Op::Le(w), Some(v)) => v.as_f64().is_some_and(|n| n <= *w),
            (Op::Near(w, tol), Some(v)) => v.as_f64().is_some_and(|n| (n - w).abs() <= *tol),
            (Op::Contains(w), Some(v)) => v.as_str().is_some_and(|s| s.contains(w.as_str())),
        };
        Verdict { ok, got }
    }

    pub fn describe(&self) -> Value {
        json!({"path": self.path, "op": self.op_name, "wanted": self.wanted})
    }
}

fn same(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) if a.is_number() && b.is_number() => x == y,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(v: Value) -> Predicate {
        Predicate::parse(&v).unwrap()
    }

    #[test]
    fn paths_reach_keys_and_indices() {
        let state = json!({"player": {"alive": false, "origin": [1300.5, 2.0, 3.0], "activeWeapon": {"name": "weapon_ak47"}}});
        assert!(p(json!({"path": "player.alive", "eq": false})).check(&state).ok);
        assert!(!p(json!({"path": "player.alive", "eq": true})).check(&state).ok);
        assert!(p(json!({"path": "player.origin[0]", "gt": 1200})).check(&state).ok);
        assert!(!p(json!({"path": "player.origin[0]", "lt": 1200})).check(&state).ok);
        assert!(p(json!({"path": "player.origin[1]", "near": 2.2, "tolerance": 0.5})).check(&state).ok);
        assert!(p(json!({"path": "player.activeWeapon.name", "contains": "ak47"})).check(&state).ok);
        assert!(p(json!({"path": "player.nope", "exists": false})).check(&state).ok);
        assert!(p(json!({"path": "player.origin", "exists": true})).check(&state).ok);
        assert_eq!(p(json!({"path": "player.origin[9]", "eq": 1})).check(&state), Verdict { ok: false, got: Value::Null });
        // integers and floats compare as numbers
        assert!(p(json!({"path": "player.origin[1]", "eq": 2})).check(&state).ok);
    }

    #[test]
    fn a_missing_or_null_field_satisfies_nothing_but_exists_false() {
        let state = json!({"player": null, "x": 1});
        for q in [json!({"path": "player.alive", "eq": false}), json!({"path": "player.alive", "ne": true}), json!({"path": "player", "ge": 0})] {
            assert!(!p(q).check(&state).ok);
        }
        assert!(p(json!({"path": "player", "exists": false})).check(&state).ok);
    }

    #[test]
    fn bad_predicates_are_refused() {
        for bad in [
            json!({"eq": 1}),
            json!({"path": "a", "eq": 1, "gt": 0}),
            json!({"path": "a"}),
            json!({"path": "a", "gt": "x"}),
            json!({"path": "", "eq": 1}),
            json!({"path": "a[", "eq": 1}),
            json!({"path": "a[x]", "eq": 1}),
            json!({"path": "a..b", "eq": 1}),
            json!({"path": "a", "eq": 1, "tolerance": 1}),
            json!({"path": "a", "eq": 1, "other": 1}),
            json!("a"),
        ] {
            assert!(Predicate::parse(&bad).is_err(), "{bad}");
        }
    }
}
