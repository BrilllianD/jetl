//! Keyboard macros: the recorder, the saved format and the app-wide store of
//! named macros in `state/macros.json` (shared by every project). `app/macros.rs`
//! hooks the recorder into `App::perform`. Like `layouts.rs`, entries are kept
//! as raw JSON on disk, so one this build cannot read survives every write; a
//! file that is not valid JSON is never overwritten, and every write is a
//! read-modify-write under a process-wide lock published by `write_atomic`, so
//! two jetl processes never drop each other's macros unless they save within
//! the same few milliseconds (the lock is per process, as in `layouts.rs`).

use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::actions::{Action, EditorAction};
use crate::util::fs::write_atomic;

/// A recording stops by itself at this many steps.
pub const MAX_STEPS: usize = 10_000;
/// Version written to a new file.
pub const FORMAT: u64 = 1;

#[derive(Debug, Default)]
pub struct Recorder {
    recording: Option<Vec<Action>>,
    skipped: usize,
    last: Option<Vec<Action>>,
    /// A key or a bracketed paste is being handled.
    pub keyboard: bool,
    /// `App::perform` nesting depth.
    pub depth: u32,
    /// A macro is being played back.
    pub playing: bool,
}

/// What a finished recording held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stopped {
    pub steps: usize,
    /// Keys left out (dialogs, popups, find bar text).
    pub skipped: usize,
}

impl Recorder {
    pub fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    /// Begin an empty recording.
    pub fn start(&mut self) {
        self.recording = Some(Vec::new());
        self.skipped = 0;
    }

    /// End the recording. A non-empty one becomes the last macro; an empty one
    /// keeps the previous last macro.
    pub fn stop(&mut self) -> Stopped {
        let steps = self.recording.take().unwrap_or_default();
        let stopped = Stopped { steps: steps.len(), skipped: self.skipped };
        self.skipped = 0;
        if !steps.is_empty() {
            self.last = Some(steps);
        }
        stopped
    }

    /// Whether `a`, performed now, belongs in the recording.
    pub fn wants(&self, a: &Action) -> bool {
        self.recording.is_some() && self.keyboard && self.depth == 0 && !self.playing && !is_macro_action(a)
    }

    /// Record a step. False: the cap was reached, and recording stopped (the
    /// steps became the last macro).
    pub fn push(&mut self, a: Action) -> bool {
        let Some(steps) = &mut self.recording else { return true };
        steps.push(a);
        if steps.len() >= MAX_STEPS {
            self.stop();
            return false;
        }
        true
    }

    /// Count a key that was left out of the recording.
    pub fn skip(&mut self) {
        if self.recording.is_some() {
            self.skipped += 1;
        }
    }

    /// Keys left out of the recording so far (tests).
    #[cfg(test)]
    pub fn skipped_for_test(&self) -> usize {
        self.skipped
    }

    pub fn last(&self) -> Option<&[Action]> {
        self.last.as_deref()
    }

    pub fn set_last(&mut self, steps: Vec<Action>) {
        self.last = Some(steps);
    }
}

/// The four macro actions are never recorded and never played.
pub fn is_macro_action(a: &Action) -> bool {
    matches!(
        a,
        Action::Editor(EditorAction::MacroRecord | EditorAction::MacroPlay | EditorAction::MacroSaveAs | EditorAction::MacroPlayNamed(_))
    )
}

/// One step on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SavedStep {
    /// A registry action id.
    Action { action: String },
    /// Consecutive `InsertChar`s, each played as one.
    Chars { chars: String },
    /// One `InsertText`.
    Text { text: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedMacro {
    pub name: String,
    pub steps: Vec<SavedStep>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MacroOp {
    /// Replace the entry of that name in place, or append one.
    Put(NamedMacro),
}

/// The disk form of recorded steps, and how many were left out: those without
/// a registry id (parameterised actions) and macro actions.
pub fn to_saved(steps: &[Action]) -> (Vec<SavedStep>, usize) {
    let mut out: Vec<SavedStep> = Vec::new();
    let mut left_out = 0;
    for a in steps {
        match a {
            Action::Editor(EditorAction::InsertChar(c)) => match out.last_mut() {
                Some(SavedStep::Chars { chars }) => chars.push(*c),
                _ => out.push(SavedStep::Chars { chars: c.to_string() }),
            },
            Action::Editor(EditorAction::InsertText(t)) => out.push(SavedStep::Text { text: t.clone() }),
            _ if is_macro_action(a) => left_out += 1,
            _ => match a.id() {
                Some(id) => out.push(SavedStep::Action { action: id.to_string() }),
                None => left_out += 1,
            },
        }
    }
    (out, left_out)
}

/// The actions of saved steps; `Err(id)` names the first id this build does
/// not know, or a macro action's id.
pub fn from_saved(steps: &[SavedStep]) -> Result<Vec<Action>, String> {
    let mut out = Vec::new();
    for s in steps {
        match s {
            SavedStep::Action { action } => match Action::from_id(action) {
                Some(a) if !is_macro_action(&a) => out.push(a),
                _ => return Err(action.clone()),
            },
            SavedStep::Chars { chars } => out.extend(chars.chars().map(|c| Action::Editor(EditorAction::InsertChar(c)))),
            SavedStep::Text { text } => out.push(Action::Editor(EditorAction::InsertText(text.clone()))),
        }
    }
    Ok(out)
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
pub fn parse(raw: &[serde_json::Value]) -> Vec<NamedMacro> {
    let mut out: Vec<NamedMacro> = Vec::new();
    for v in raw {
        let Some(m) = serde_json::from_value::<NamedMacro>(v.clone()).ok() else { continue };
        if m.name.trim().is_empty() || out.iter().any(|o| o.name == m.name) {
            continue;
        }
        out.push(m);
    }
    out
}

pub fn apply_op(raw: &mut Vec<serde_json::Value>, op: &MacroOp) -> Result<(), String> {
    match op {
        MacroOp::Put(m) => {
            let v = serde_json::to_value(m).map_err(|e| e.to_string())?;
            match raw.iter().position(|e| entry_name(e) == Some(&m.name)) {
                Some(i) => raw[i] = v,
                None => raw.push(v),
            }
            Ok(())
        }
    }
}

fn read(file: &Path) -> Result<(u64, Vec<serde_json::Value>), String> {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((FORMAT, vec![])),
        Err(e) => return Err(format!("macros {}: {e}", file.display())),
    };
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("macros {}: not valid JSON ({e})", file.display()))?;
    if !v.is_object() {
        return Err(format!("macros {}: not valid JSON (not an object)", file.display()));
    }
    let version = v["version"].as_u64().unwrap_or(FORMAT);
    let macros = match v.get("macros") {
        None => vec![],
        Some(l) => l.as_array().cloned().ok_or_else(|| format!("macros {}: not valid JSON (macros is not a list)", file.display()))?,
    };
    Ok((version, macros))
}

/// Read the file, apply `op` (when given) and write it back, under the file
/// lock; returns the version taken under the lock (a higher one saw a later
/// state), the macros that parse, and an error to show. A file that cannot be
/// read is listed as empty and not written; a failed op or write lists the
/// file as it was.
pub fn update(file: &Path, op: Option<&MacroOp>) -> (u64, Vec<NamedMacro>, Option<String>) {
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
    let doc = serde_json::json!({"version": file_version.max(FORMAT), "macros": new_raw});
    let text = match serde_json::to_string_pretty(&doc) {
        Ok(t) => t,
        Err(e) => return (version, parse(&raw), Some(format!("Saving macros: {e}"))),
    };
    match write_atomic(file, &text) {
        Err(e) => (version, parse(&raw), Some(format!("Saving macros: {e}"))),
        Ok(perm) => {
            if let Some(p) = perm {
                tracing::debug!("macros: permissions not copied: {p}");
            }
            (version, parse(&new_raw), None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn ins(c: char) -> Action {
        Action::Editor(EditorAction::InsertChar(c))
    }

    fn named(name: &str, chars: &str) -> NamedMacro {
        NamedMacro { name: name.into(), steps: vec![SavedStep::Chars { chars: chars.into() }] }
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("jetl-test-macros-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn names(raw: &[Value]) -> Vec<String> {
        parse(raw).into_iter().map(|m| m.name).collect()
    }

    #[test]
    fn recorder_records_only_top_level_keyboard_non_macro_actions() {
        let a = ins('a');
        let mut r = Recorder { keyboard: true, ..Default::default() };
        assert!(!r.wants(&a), "not recording");
        r.start();
        assert!(r.wants(&a));
        r.keyboard = false;
        assert!(!r.wants(&a), "not from the keyboard");
        r.keyboard = true;
        r.depth = 1;
        assert!(!r.wants(&a), "nested");
        r.depth = 0;
        r.playing = true;
        assert!(!r.wants(&a), "playing");
        r.playing = false;
        for m in [EditorAction::MacroRecord, EditorAction::MacroPlay, EditorAction::MacroSaveAs, EditorAction::MacroPlayNamed("x".into())] {
            assert!(!r.wants(&Action::Editor(m)), "macro action");
        }
        assert!(r.wants(&a));
    }

    #[test]
    fn stop_keeps_the_last_macro_when_nothing_was_recorded() {
        let mut r = Recorder::default();
        r.start();
        assert!(r.push(ins('a')));
        assert_eq!(r.stop(), Stopped { steps: 1, skipped: 0 });
        assert_eq!(r.last(), Some(&[ins('a')][..]));
        r.start();
        r.skip();
        assert_eq!(r.stop(), Stopped { steps: 0, skipped: 1 });
        assert!(!r.is_recording());
        assert_eq!(r.last(), Some(&[ins('a')][..]), "the previous macro stays");
    }

    #[test]
    fn push_stops_at_the_cap() {
        let mut r = Recorder::default();
        r.start();
        for _ in 0..MAX_STEPS - 1 {
            assert!(r.push(ins('a')));
        }
        assert!(r.is_recording());
        assert!(!r.push(ins('a')));
        assert!(!r.is_recording());
        assert_eq!(r.last().map(<[Action]>::len), Some(MAX_STEPS));
    }

    #[test]
    fn to_saved_merges_chars_and_counts_unnamed() {
        let steps = vec![
            ins('a'),
            ins('b'),
            Action::Editor(EditorAction::DuplicateLine),
            Action::Editor(EditorAction::InsertText("x".into())),
            Action::Run(crate::actions::RunAction::Select("t".into())),
            ins('c'),
        ];
        let (saved, left_out) = to_saved(&steps);
        assert_eq!(
            saved,
            [
                SavedStep::Chars { chars: "ab".into() },
                SavedStep::Action { action: "editor.duplicate_line".into() },
                SavedStep::Text { text: "x".into() },
                SavedStep::Chars { chars: "c".into() },
            ]
        );
        assert_eq!(left_out, 1);
    }

    #[test]
    fn from_saved_round_trips_and_refuses_unknown_and_macro_ids() {
        let steps =
            vec![ins('a'), ins('b'), Action::Editor(EditorAction::DuplicateLine), Action::Editor(EditorAction::InsertText("x".into()))];
        let (saved, _) = to_saved(&steps);
        assert_eq!(from_saved(&saved), Ok(steps));
        let bad = [SavedStep::Chars { chars: "a".into() }, SavedStep::Action { action: "no.such_action".into() }];
        assert_eq!(from_saved(&bad), Err("no.such_action".to_string()));
        let mac = [SavedStep::Action { action: "editor.macro_play".into() }];
        assert_eq!(from_saved(&mac), Err("editor.macro_play".to_string()));
    }

    #[test]
    fn saved_json_shape() {
        let (steps, _) = to_saved(&[
            Action::Editor(EditorAction::Move(crate::actions::Motion::LineEnd, false)),
            ins(';'),
            ins(';'),
            Action::Editor(EditorAction::InsertText("pasted text".into())),
        ]);
        let m = NamedMacro { name: "semi".into(), steps };
        let doc = json!({"version": 1, "macros": [m]});
        assert_eq!(
            doc.to_string(),
            r#"{"macros":[{"name":"semi","steps":[{"action":"editor.line_end"},{"chars":";;"},{"text":"pasted text"}]}],"version":1}"#
        );
        let back: NamedMacro = serde_json::from_value(doc["macros"][0].clone()).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn put_replaces_by_name_and_keeps_unreadable_entries() {
        let mut raw = vec![json!({"name": "future", "steps": [{"kind": "new"}]}), json!("junk")];
        apply_op(&mut raw, &MacroOp::Put(named("A", "x"))).unwrap();
        apply_op(&mut raw, &MacroOp::Put(named("B", "y"))).unwrap();
        apply_op(&mut raw, &MacroOp::Put(named("A", "z"))).unwrap();
        assert_eq!(raw.len(), 4);
        assert_eq!(raw[0]["name"], "future");
        assert_eq!(names(&raw), ["A", "B"]);
        assert_eq!(parse(&raw)[0].steps, [SavedStep::Chars { chars: "z".into() }]);
    }

    #[test]
    fn parse_takes_the_first_of_a_repeated_name_and_skips_empty_names() {
        let raw = vec![json!(named("A", "1")), json!(named("A", "2")), json!(named(" ", "3"))];
        let got = parse(&raw);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].steps, [SavedStep::Chars { chars: "1".into() }]);
    }

    #[test]
    fn invalid_json_is_never_overwritten() {
        let dir = tmp("invalid");
        let file = dir.join("macros.json");
        std::fs::write(&file, "{bad").unwrap();
        let (_, list, err) = update(&file, Some(&MacroOp::Put(named("A", "x"))));
        assert!(list.is_empty());
        let e = err.expect("an error");
        assert!(e.starts_with("macros ") && e.contains("not valid JSON"), "{e}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{bad");
    }

    #[test]
    fn two_updates_keep_each_others_macros() {
        let dir = tmp("two");
        let file = dir.join("state").join("macros.json");
        let (v1, list, err) = update(&file, Some(&MacroOp::Put(named("first", "a"))));
        assert!(err.is_none() && list.len() == 1);
        // A second process has not seen `first`; its write must keep it.
        let (v2, list, err) = update(&file, Some(&MacroOp::Put(named("second", "b"))));
        assert!(err.is_none());
        assert!(v2 > v1);
        assert_eq!(list.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["first", "second"]);
        let (_, list, _) = update(&file, None);
        assert_eq!(list.len(), 2);
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(v["version"], 1);
    }
}
