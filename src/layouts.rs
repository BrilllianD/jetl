//! Named tool window layouts (View | Switch Layout…), app-wide in
//! `state/layouts.json`: the placement part of the session layout under a name.
//! Entries are kept as raw JSON on disk, so one this build cannot read (a newer
//! build's window, a hand edit) survives every write; a file that is not valid
//! JSON is never overwritten.

use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::ui::focus::{Side, ToolWindowId};
use crate::util::fs::write_atomic;

/// Version written to a new file.
pub const FORMAT: u64 = 1;

/// The placement part of [`crate::workspace::SessionLayout`]: same field names
/// and serde attributes (a parity test in `workspace.rs` keeps them equal).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutPlacement {
    /// Tool window open on each side.
    pub left: Option<ToolWindowId>,
    pub left_w: u16,
    pub bottom: Option<ToolWindowId>,
    pub bottom_h: u16,
    /// Right-side window (none before side choice).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub right: Option<ToolWindowId>,
    /// Its width; missing keeps the current one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub right_w: Option<u16>,
    /// Windows moved off their default side, in strip order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sides: Vec<(ToolWindowId, Side)>,
    /// Tool strip order when it is not the default.
    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "crate::workspace::known_tool_windows")]
    pub order: Vec<ToolWindowId>,
    /// Unpinned tool windows.
    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "crate::workspace::known_tool_windows")]
    pub unpinned: Vec<ToolWindowId>,
    /// Windows in their side's second pane.
    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "crate::workspace::known_tool_windows")]
    pub split: Vec<ToolWindowId>,
    /// Left Bottom window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left2: Option<ToolWindowId>,
    /// Bottom Right window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bottom2: Option<ToolWindowId>,
    /// Right Bottom window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub right2: Option<ToolWindowId>,
    /// Rows of the Left Bottom pane; missing: half the side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left2_h: Option<u16>,
    /// Columns of the Bottom Right pane; missing: half the side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bottom2_w: Option<u16>,
    /// Rows of the Right Bottom pane; missing: half the side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub right2_h: Option<u16>,
    /// Undocked (slide-over) tool windows.
    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "crate::workspace::known_tool_windows")]
    pub undocked: Vec<ToolWindowId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedLayout {
    pub name: String,
    pub layout: LayoutPlacement,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutOp {
    /// Replace the entry of that name in place, or append one.
    Put(NamedLayout),
    Rename {
        from: String,
        to: String,
    },
    Delete(String),
}

/// Serializes read-modify-write of the file between background tasks. The
/// value counts the file states handed out: each [`update`] bumps it while
/// holding the guard, so a higher version saw a later state.
static FILE_LOCK: Mutex<u64> = Mutex::new(0);

fn entry_name(v: &serde_json::Value) -> Option<&str> {
    v.get("name")?.as_str()
}

/// The entries this build can read, in file order; of a repeated name the
/// first wins, and empty names are skipped.
pub fn parse(raw: &[serde_json::Value]) -> Vec<NamedLayout> {
    let mut out: Vec<NamedLayout> = Vec::new();
    for v in raw {
        let Some(l) = serde_json::from_value::<NamedLayout>(v.clone()).ok() else { continue };
        if l.name.trim().is_empty() || out.iter().any(|o| o.name == l.name) {
            continue;
        }
        out.push(l);
    }
    out
}

pub fn apply_op(raw: &mut Vec<serde_json::Value>, op: &LayoutOp) -> Result<(), String> {
    match op {
        LayoutOp::Put(l) => {
            let v = serde_json::to_value(l).map_err(|e| e.to_string())?;
            match raw.iter().position(|e| entry_name(e) == Some(&l.name)) {
                Some(i) => raw[i] = v,
                None => raw.push(v),
            }
            Ok(())
        }
        LayoutOp::Rename { from, to } => {
            if from == to {
                return Ok(());
            }
            if raw.iter().any(|e| entry_name(e) == Some(to)) {
                return Err(format!("A layout named '{to}' already exists"));
            }
            let Some(entry) = raw.iter_mut().find(|e| entry_name(e) == Some(from)) else {
                return Err(format!("No layout named '{from}'"));
            };
            if let Some(o) = entry.as_object_mut() {
                o.insert("name".into(), serde_json::Value::String(to.clone()));
            }
            Ok(())
        }
        LayoutOp::Delete(name) => {
            raw.retain(|v| entry_name(v) != Some(name));
            Ok(())
        }
    }
}

fn read(file: &Path) -> Result<(u64, Vec<serde_json::Value>), String> {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((FORMAT, vec![])),
        Err(e) => return Err(format!("layouts {}: {e}", file.display())),
    };
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("layouts {}: not valid JSON ({e})", file.display()))?;
    if !v.is_object() {
        return Err(format!("layouts {}: not valid JSON (not an object)", file.display()));
    }
    let version = v["version"].as_u64().unwrap_or(FORMAT);
    let layouts = v["layouts"].as_array().cloned().unwrap_or_default();
    Ok((version, layouts))
}

/// Read the file, apply `op` (when given) and write it back, under the file
/// lock; returns the version taken under the lock (a higher one saw a later
/// state), the layouts that parse, and an error to show. A file that cannot be
/// read is listed as empty and not written; a failed op or write lists the
/// file as it was.
pub fn update(file: &Path, op: Option<&LayoutOp>) -> (u64, Vec<NamedLayout>, Option<String>) {
    let mut guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    *guard += 1;
    let version = *guard;
    let (file_version, raw) = match read(file) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("{e}");
            return (version, vec![], Some(e));
        }
    };
    let Some(op) = op else { return (version, parse(&raw), None) };
    let mut new_raw = raw.clone();
    if let Err(e) = apply_op(&mut new_raw, op) {
        return (version, parse(&raw), Some(e));
    }
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let doc = serde_json::json!({"version": file_version.max(FORMAT), "layouts": new_raw});
    let text = match serde_json::to_string_pretty(&doc) {
        Ok(t) => t,
        Err(e) => return (version, parse(&raw), Some(format!("Saving layouts: {e}"))),
    };
    match write_atomic(file, &text) {
        Err(e) => (version, parse(&raw), Some(format!("Saving layouts: {e}"))),
        Ok(perm) => {
            if let Some(p) = perm {
                tracing::debug!("layouts: permissions not copied: {p}");
            }
            (version, parse(&new_raw), None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn placement() -> LayoutPlacement {
        LayoutPlacement {
            left: Some(ToolWindowId::Project),
            left_w: 30,
            bottom: None,
            bottom_h: 10,
            right: None,
            right_w: None,
            sides: vec![],
            order: vec![],
            unpinned: vec![],
            split: vec![],
            left2: None,
            bottom2: None,
            right2: None,
            left2_h: None,
            bottom2_w: None,
            right2_h: None,
            undocked: vec![],
        }
    }

    fn named(name: &str) -> NamedLayout {
        NamedLayout { name: name.into(), layout: placement() }
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("jetl-test-layouts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn names(raw: &[Value]) -> Vec<String> {
        parse(raw).into_iter().map(|l| l.name).collect()
    }

    #[test]
    fn put_rename_delete_edit_entries_by_name() {
        let mut raw = vec![];
        apply_op(&mut raw, &LayoutOp::Put(named("A"))).unwrap();
        apply_op(&mut raw, &LayoutOp::Put(named("B"))).unwrap();
        let mut a2 = named("A");
        a2.layout.left_w = 40;
        apply_op(&mut raw, &LayoutOp::Put(a2)).unwrap();
        assert_eq!(names(&raw), ["A", "B"]);
        assert_eq!(parse(&raw)[0].layout.left_w, 40);
        apply_op(&mut raw, &LayoutOp::Rename { from: "A".into(), to: "C".into() }).unwrap();
        assert_eq!(names(&raw), ["C", "B"]);
        apply_op(&mut raw, &LayoutOp::Delete("B".into())).unwrap();
        apply_op(&mut raw, &LayoutOp::Delete("missing".into())).unwrap();
        assert_eq!(names(&raw), ["C"]);
    }

    #[test]
    fn rename_refuses_a_taken_or_missing_name() {
        let mut raw = vec![];
        apply_op(&mut raw, &LayoutOp::Put(named("A"))).unwrap();
        apply_op(&mut raw, &LayoutOp::Put(named("B"))).unwrap();
        let before = raw.clone();
        let e = apply_op(&mut raw, &LayoutOp::Rename { from: "A".into(), to: "B".into() }).unwrap_err();
        assert_eq!(e, "A layout named 'B' already exists");
        let e = apply_op(&mut raw, &LayoutOp::Rename { from: "X".into(), to: "Y".into() }).unwrap_err();
        assert_eq!(e, "No layout named 'X'");
        assert_eq!(raw, before);
    }

    #[test]
    fn parse_skips_unreadable_entries_and_keeps_the_first_of_a_name() {
        let a = serde_json::to_value(named("A")).unwrap();
        let mut a2 = named("A");
        a2.layout.left_w = 99;
        let raw = vec![
            json!({"name": "bad", "layout": {"left": "Nope", "left_w": 1, "bottom": null, "bottom_h": 1}}),
            a.clone(),
            serde_json::to_value(a2).unwrap(),
            json!({"name": "  ", "layout": a["layout"]}),
        ];
        assert_eq!(parse(&raw), [named("A")]);
    }

    #[test]
    fn update_writes_version_and_keeps_entries_it_cannot_read() {
        let d = tmp("update");
        let f = d.join("layouts.json");
        let (v0, list, err) = update(&f, None);
        assert!(list.is_empty() && err.is_none() && !f.exists());
        let mut x = serde_json::to_value(named("X")).unwrap();
        x["future"] = json!(1);
        let bad = json!({"name": "bad", "layout": {"left": "Nope", "left_w": 1, "bottom": null, "bottom_h": 1}});
        std::fs::write(&f, json!({"version": 2, "layouts": [bad, x]}).to_string()).unwrap();
        let (v1, list, err) = update(&f, Some(&LayoutOp::Put(named("Y"))));
        assert!(err.is_none());
        assert!(v1 > v0);
        assert_eq!(list.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["X", "Y"]);
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        assert_eq!(v["version"], 2);
        assert_eq!(v["layouts"][0], bad);
        assert_eq!(v["layouts"][1]["future"], 1);
        let (v2, _, err) = update(&f, Some(&LayoutOp::Rename { from: "X".into(), to: "Z".into() }));
        assert!(err.is_none() && v2 > v1);
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        assert_eq!(v["layouts"][1]["name"], "Z");
        assert_eq!(v["layouts"][1]["future"], 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_invalid_file_is_never_overwritten() {
        let d = tmp("invalid");
        let f = d.join("layouts.json");
        std::fs::write(&f, "{not json").unwrap();
        let (_, list, err) = update(&f, Some(&LayoutOp::Put(named("A"))));
        assert!(list.is_empty());
        assert!(err.unwrap().contains("not valid JSON"));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "{not json");
        let _ = std::fs::remove_dir_all(&d);
    }
}
