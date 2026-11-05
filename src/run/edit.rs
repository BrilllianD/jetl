//! Writing `.jetl/run.toml` (Run > Edit Configurations…). Edits go through
//! `toml_edit` so comments, order and spacing of untouched entries survive;
//! the result is validated with the real parser before it is written.

use std::path::{Path, PathBuf};

use toml_edit::{Array as TomlArray, ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value};

use super::{Origin, ProjectTasks, RunTask, TaskCommand};
use crate::config::FileText;
use crate::config::theme::error_summary;

/// Which array of `.jetl/run.toml` an entry is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Array {
    /// `[[tasks]]`, the older name.
    Tasks,
    /// `[[configs]]`.
    Configs,
}

impl Array {
    fn key(self) -> &'static str {
        match self {
            Array::Tasks => "tasks",
            Array::Configs => "configs",
        }
    }
}

/// One entry of the file: its array and its index in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    pub array: Array,
    pub index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigEdit {
    Update(Slot, RunTask),
    Remove(Slot),
    /// Appended to `[[configs]]`.
    Add(RunTask),
}

/// The header comment of a new file.
pub const RUN_STUB: &str =
    "# Run configurations of this project: Run > Edit Configurations…, or see the [run] section of jetl's docs/CONFIG.md\n";

const FILE: &str = ".jetl/run.toml";

pub fn project_file(root: &Path) -> PathBuf {
    root.join(".jetl").join("run.toml")
}

/// The entries of the file, each with its slot: `tasks` first, then `configs`.
pub fn load_project(text: &str) -> Result<Vec<(Slot, RunTask)>, String> {
    let cfg = toml::from_str::<ProjectTasks>(text).map_err(|e| format!("{FILE}: {}", error_summary(&e.to_string())))?;
    let slots = |array, v: Vec<RunTask>| {
        v.into_iter().enumerate().map(|(index, t)| (Slot { array, index }, RunTask { origin: Origin::Project, ..t })).collect::<Vec<_>>()
    };
    let mut all = slots(Array::Tasks, cfg.tasks);
    all.extend(slots(Array::Configs, cfg.configs));
    Ok(all)
}

fn str_array(v: &[String]) -> Value {
    Value::Array(v.iter().map(String::as_str).collect::<TomlArray>())
}

fn command_value(c: &TaskCommand) -> Value {
    match c {
        TaskCommand::Shell(s) => Value::from(s.as_str()),
        TaskCommand::Argv(v) => str_array(v),
    }
}

fn env_value(env: &std::collections::BTreeMap<String, String>) -> Value {
    let mut t = InlineTable::new();
    for (k, v) in env {
        t.insert(k, Value::from(v.as_str()));
    }
    Value::InlineTable(t)
}

/// The value of `field` in `t`, or `None` when it is the default (the key is
/// left out of the file).
fn field_value(t: &RunTask, field: &str) -> Option<Value> {
    match field {
        "name" => Some(Value::from(t.name.as_str())),
        "command" => Some(command_value(&t.command)),
        "args" => (!t.args.is_empty()).then(|| str_array(&t.args)),
        "cwd" => t.cwd.as_deref().filter(|c| !c.is_empty()).map(Value::from),
        "env" => (!t.env.is_empty()).then(|| env_value(&t.env)),
        "matcher" => t.matcher.map(|m| Value::from(m.label())),
        _ => None,
    }
}

const FIELDS: [&str; 6] = ["name", "command", "args", "cwd", "env", "matcher"];

/// Whether `field` is written the same in both, defaults counting as equal.
fn same_field(a: &RunTask, b: &RunTask, field: &str) -> bool {
    match (field_value(a, field), field_value(b, field)) {
        (None, None) => true,
        (Some(x), Some(y)) => x.to_string().trim() == y.to_string().trim(),
        _ => false,
    }
}

fn update_table(table: &mut Table, old: &RunTask, new: &RunTask) {
    for field in FIELDS {
        if same_field(old, new, field) {
            continue;
        }
        match field_value(new, field) {
            Some(mut v) => {
                // Replace in place: the key keeps its leading comment, the
                // value its spacing and trailing comment.
                if let Some(slot) = table.get_mut(field) {
                    if let Some(old) = slot.as_value() {
                        *v.decor_mut() = old.decor().clone();
                    }
                    *slot = Item::Value(v);
                } else {
                    table.insert(field, Item::Value(v));
                }
            }
            None => {
                table.remove(field);
            }
        }
    }
}

/// The position in the file of the first table of either array.
fn first_position(doc: &DocumentMut) -> Option<isize> {
    [Array::Tasks, Array::Configs]
        .into_iter()
        .filter_map(|a| doc.get(a.key()).and_then(Item::as_array_of_tables))
        .flat_map(|a| a.iter().filter_map(Table::position))
        .min()
}

/// The comments and blank lines in front of the table at `slot`.
fn prefix_of(doc: &DocumentMut, slot: &Slot) -> String {
    let table = doc.get(slot.array.key()).and_then(Item::as_array_of_tables).and_then(|a| a.get(slot.index));
    table.and_then(|t| t.decor().prefix()).and_then(|p| p.as_str()).unwrap_or_default().to_string()
}

/// The first table of the file was removed: its leading text (the file's
/// header comments) goes to the table now first, or to the end of the file.
fn hand_on_header(doc: &mut DocumentMut, lead: &str) {
    let first = first_position(doc);
    for a in [Array::Tasks, Array::Configs] {
        let Some(tables) = doc.get_mut(a.key()).and_then(Item::as_array_of_tables_mut) else { continue };
        let Some(i) = (0..tables.len()).find(|&i| tables.get(i).and_then(Table::position) == first) else { continue };
        let Some(table) = tables.get_mut(i) else { continue };
        let own = table.decor().prefix().and_then(|p| p.as_str()).unwrap_or_default().trim_start_matches('\n').to_string();
        table.decor_mut().set_prefix(format!("{lead}{own}"));
        return;
    }
    if first.is_none() {
        let rest = doc.trailing().as_str().unwrap_or_default().to_string();
        doc.set_trailing(format!("{lead}{rest}"));
    }
}

fn new_table(t: &RunTask) -> Table {
    let mut table = Table::new();
    for field in FIELDS {
        if let Some(v) = field_value(t, field) {
            table.insert(field, Item::Value(v));
        }
    }
    table
}

/// Apply `edits` to the text of `.jetl/run.toml`: updates, then removals
/// (highest index first), then additions at the end of `[[configs]]`. Empty
/// `text` starts from `RUN_STUB`.
pub fn apply_config_edits(text: &str, edits: &[ConfigEdit]) -> Result<String, String> {
    let text = if text.is_empty() { RUN_STUB } else { text };
    let mut doc = text.parse::<DocumentMut>().map_err(|e| format!("{FILE} is not valid TOML: {}", error_summary(&e.to_string())))?;
    let old = load_project(text)?;
    for array in [Array::Tasks, Array::Configs] {
        if doc.get(array.key()).is_some_and(|i| !i.is_array_of_tables()) {
            return Err(format!("{FILE}: `{}` is an inline array; edit it by hand", array.key()));
        }
    }
    let missing = |slot: &Slot| format!("{FILE}: no {} entry {}", slot.array.key(), slot.index + 1);
    for e in edits {
        if let ConfigEdit::Update(slot, new) = e {
            let (_, was) = old.iter().find(|(s, _)| s == slot).ok_or_else(|| missing(slot))?;
            let table = doc
                .get_mut(slot.array.key())
                .and_then(Item::as_array_of_tables_mut)
                .and_then(|a| a.get_mut(slot.index))
                .ok_or_else(|| missing(slot))?;
            update_table(table, was, new);
        }
    }
    let mut removed: Vec<Slot> = edits.iter().filter_map(|e| if let ConfigEdit::Remove(s) = e { Some(*s) } else { None }).collect();
    removed.sort_by_key(|s| std::cmp::Reverse(s.index));
    for slot in removed {
        let array = doc.get_mut(slot.array.key()).and_then(Item::as_array_of_tables_mut).ok_or_else(|| missing(&slot))?;
        if slot.index >= array.len() {
            return Err(missing(&slot));
        }
        let first = array.get(slot.index).and_then(Table::position) == first_position(&doc);
        let lead = if first { prefix_of(&doc, &slot) } else { String::new() };
        let Some(array) = doc.get_mut(slot.array.key()).and_then(Item::as_array_of_tables_mut) else {
            return Err(missing(&slot));
        };
        array.remove(slot.index);
        if first {
            hand_on_header(&mut doc, &lead);
        }
    }
    for e in edits {
        if let ConfigEdit::Add(t) = e {
            let mut table = new_table(t);
            // Trailing comments (the stub) stay above the first new table.
            let mut lead = String::new();
            if doc.get("configs").is_none() {
                lead = doc.trailing().as_str().unwrap_or_default().to_string();
                doc.set_trailing("");
                doc.insert("configs", Item::ArrayOfTables(ArrayOfTables::new()));
            }
            let before = format!("{doc}{lead}");
            if !before.trim().is_empty() && !before.ends_with("\n\n") {
                lead.push('\n');
            }
            if !lead.is_empty() {
                table.decor_mut().set_prefix(lead);
            }
            if let Some(a) = doc.get_mut("configs").and_then(Item::as_array_of_tables_mut) {
                a.push(table);
            }
        }
    }
    let out = doc.to_string();
    toml::from_str::<ProjectTasks>(&out).map_err(|e| format!("{FILE}: {}", error_summary(&e.to_string())))?;
    Ok(out)
}

/// Apply `edits` to the file under `root` and write it; `base` is the text
/// the edits were made against. Returns the text written. Creates only
/// `root/.jetl`.
pub fn write_project(root: &Path, base: &str, edits: &[ConfigEdit]) -> Result<String, String> {
    let file = project_file(root);
    let current = match FileText::read(&file) {
        FileText::Missing => String::new(),
        FileText::Unreadable(e) => return Err(format!("{FILE}: {e}")),
        FileText::Text(t) => t,
    };
    if current != base {
        return Err(format!("{FILE} changed on disk; reopen Edit Configurations"));
    }
    let out = apply_config_edits(&current, edits)?;
    let dir = root.join(".jetl");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    match crate::util::fs::write_atomic(&file, &out) {
        Ok(Some(perm_err)) => tracing::warn!("{FILE} permissions not kept: {perm_err}"),
        Ok(None) => {}
        Err(e) => return Err(format!("write {}: {e}", file.display())),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::run::{MatcherKind, RunConfig};

    fn task(name: &str, command: TaskCommand) -> RunTask {
        RunTask { name: name.into(), command, cwd: None, env: BTreeMap::new(), args: Vec::new(), matcher: None, origin: Origin::Project }
    }

    fn argv(words: &[&str]) -> TaskCommand {
        TaskCommand::Argv(words.iter().map(|s| s.to_string()).collect())
    }

    const INPUT: &str = "# top\n[[tasks]]\nname = \"old\"   # keep\ncommand = \"make\"\n\n[[configs]]\n# web\nname = \"web\"\ncommand = [\"npm\", \"run\", \"dev\"]\nenv = { PORT = \"1\" }\n";

    fn slot(array: Array, index: usize) -> Slot {
        Slot { array, index }
    }

    #[test]
    fn round_trip_keeps_comments_and_untouched_entries() {
        let mut web = load_project(INPUT).unwrap()[1].1.clone();
        web.cwd = Some("web".into());
        web.env.insert("PORT".into(), "2".into());
        let mut added = task("t", argv(&["cargo", "test"]));
        added.args = vec!["--".into(), "--nocapture".into()];
        added.matcher = Some(MatcherKind::Plain);
        let out =
            apply_config_edits(INPUT, &[ConfigEdit::Update(slot(Array::Configs, 0), web.clone()), ConfigEdit::Add(added.clone())]).unwrap();
        let expected = "# top\n[[tasks]]\nname = \"old\"   # keep\ncommand = \"make\"\n\n[[configs]]\n# web\nname = \"web\"\ncommand = [\"npm\", \"run\", \"dev\"]\nenv = { PORT = \"2\" }\ncwd = \"web\"\n\n[[configs]]\nname = \"t\"\ncommand = [\"cargo\", \"test\"]\nargs = [\"--\", \"--nocapture\"]\nmatcher = \"none\"\n";
        assert_eq!(out, expected);
        let loaded = load_project(&out).unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[1], (slot(Array::Configs, 0), web));
        assert_eq!(loaded[2], (slot(Array::Configs, 1), added));
        assert_eq!(loaded[0].1.name, "old");
    }

    #[test]
    fn remove_and_reset_fields() {
        let text = "[[tasks]]\nname = \"a\"\ncommand = \"make\"\n\n[[tasks]]\nname = \"b\"\ncommand = \"make b\"\n\n[[configs]]\nname = \"c\"\ncommand = \"x\"\nenv = { A = \"1\" }\nmatcher = \"none\"\n";
        let out = apply_config_edits(text, &[ConfigEdit::Remove(slot(Array::Tasks, 0))]).unwrap();
        assert_eq!(
            out,
            "[[tasks]]\nname = \"b\"\ncommand = \"make b\"\n\n[[configs]]\nname = \"c\"\ncommand = \"x\"\nenv = { A = \"1\" }\nmatcher = \"none\"\n"
        );
        let mut c = load_project(text).unwrap()[2].1.clone();
        c.env.clear();
        c.matcher = None;
        let out = apply_config_edits(text, &[ConfigEdit::Update(slot(Array::Configs, 0), c)]).unwrap();
        assert_eq!(
            out,
            "[[tasks]]\nname = \"a\"\ncommand = \"make\"\n\n[[tasks]]\nname = \"b\"\ncommand = \"make b\"\n\n[[configs]]\nname = \"c\"\ncommand = \"x\"\n"
        );
    }

    #[test]
    fn removing_the_first_table_keeps_the_header() {
        let text = "# header\n\n[[tasks]]\nname = \"a\"\ncommand = \"make\"\n\n[[configs]]\n# c\nname = \"c\"\ncommand = \"x\"\n";
        let out = apply_config_edits(text, &[ConfigEdit::Remove(slot(Array::Tasks, 0))]).unwrap();
        assert_eq!(out, "# header\n\n[[configs]]\n# c\nname = \"c\"\ncommand = \"x\"\n");
        let out =
            apply_config_edits(text, &[ConfigEdit::Remove(slot(Array::Tasks, 0)), ConfigEdit::Remove(slot(Array::Configs, 0))]).unwrap();
        assert_eq!(out, "# header\n\n");
        // The stub of a file jetl created survives removing its only entry.
        let made = apply_config_edits("", &[ConfigEdit::Add(task("a", TaskCommand::Shell("make".into())))]).unwrap();
        let out = apply_config_edits(&made, &[ConfigEdit::Remove(slot(Array::Configs, 0))]).unwrap();
        assert!(out.starts_with(RUN_STUB), "{out}");
    }

    #[test]
    fn new_file_starts_with_the_stub() {
        let out = apply_config_edits("", &[ConfigEdit::Add(task("a", TaskCommand::Shell("make".into())))]).unwrap();
        assert!(out.starts_with(RUN_STUB), "{out}");
        let loaded = load_project(&out).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].1.command, TaskCommand::Shell("make".into()));
    }

    #[test]
    fn inline_array_is_refused() {
        let text = "configs = [{ name = \"a\", command = \"x\" }]\n";
        let err = apply_config_edits(text, &[ConfigEdit::Add(task("b", TaskCommand::Shell("y".into())))]).unwrap_err();
        assert_eq!(err, ".jetl/run.toml: `configs` is an inline array; edit it by hand");
    }

    #[test]
    fn invalid_result_is_refused() {
        let err = apply_config_edits("[[tasks]\nname = 1", &[]).unwrap_err();
        assert!(err.starts_with(".jetl/run.toml is not valid TOML: "), "{err}");
        let err = apply_config_edits("[[tasks]]\nname = 3\n", &[]).unwrap_err();
        assert!(err.starts_with(".jetl/run.toml: "), "{err}");
    }

    #[test]
    fn write_project_detects_outside_change() {
        let root = std::env::temp_dir().join(format!("jetl-test-run-edit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let add = ConfigEdit::Add(task("a", TaskCommand::Shell("make".into())));
        let first = write_project(&root, "", std::slice::from_ref(&add)).unwrap();
        assert!(root.join(".jetl").is_dir());
        assert_eq!(std::fs::read_to_string(project_file(&root)).unwrap(), first);
        let err = write_project(&root, "stale", std::slice::from_ref(&add)).unwrap_err();
        assert_eq!(err, ".jetl/run.toml changed on disk; reopen Edit Configurations");
        assert_eq!(std::fs::read_to_string(project_file(&root)).unwrap(), first);
        let second = write_project(&root, &first, &[add]).unwrap();
        assert_eq!(load_project(&second).unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    const WITH_DEBUG: &str = "# top\n[[configs]]\nname = \"demo\"\ncommand = [\"./demo\"]\n\n# how to debug it\n[debug.demo]\nadapter = \"gdb\"   # keep\nlaunch = { program = \"${workspaceFolder}/demo\", args = [\"x\"] }\n\n[debug.other.launch]\nprogram = \"a\"\n\n[[configs]]\nname = \"b\"\ncommand = \"make\"\n";
    const BLOCKS: &str = "# how to debug it\n[debug.demo]\nadapter = \"gdb\"   # keep\nlaunch = { program = \"${workspaceFolder}/demo\", args = [\"x\"] }\n\n[debug.other.launch]\nprogram = \"a\"\n";

    #[test]
    fn debug_tables_do_not_disturb_load_project() {
        let loaded = load_project(WITH_DEBUG).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!((loaded[0].0, loaded[0].1.name.as_str()), (slot(Array::Configs, 0), "demo"));
        assert_eq!((loaded[1].0, loaded[1].1.name.as_str()), (slot(Array::Configs, 1), "b"));
    }

    #[test]
    fn edits_leave_debug_tables_byte_for_byte() {
        let mut demo = load_project(WITH_DEBUG).unwrap()[0].1.clone();
        demo.cwd = Some("x".into());
        let out = apply_config_edits(WITH_DEBUG, &[ConfigEdit::Update(slot(Array::Configs, 0), demo)]).unwrap();
        assert_eq!(out, WITH_DEBUG.replacen("command = [\"./demo\"]\n", "command = [\"./demo\"]\ncwd = \"x\"\n", 1));
        let want = toml::from_str::<RunConfig>(WITH_DEBUG).unwrap().debug;
        let edits = [
            vec![ConfigEdit::Remove(slot(Array::Configs, 1))],
            vec![ConfigEdit::Remove(slot(Array::Configs, 0))],
            vec![ConfigEdit::Add(task("t", argv(&["make", "t"])))],
        ];
        for e in edits {
            let out = apply_config_edits(WITH_DEBUG, &e).unwrap();
            assert!(out.contains(BLOCKS), "{out}");
            assert_eq!(toml::from_str::<RunConfig>(&out).unwrap().debug, want, "{out}");
        }
        let only = "[debug.demo]\nadapter = \"gdb\"\n";
        let out = apply_config_edits(only, &[ConfigEdit::Add(task("t", argv(&["make"])))]).unwrap();
        assert!(out.contains(only) && out.contains("[[configs]]\nname = \"t\""), "{out}");
        assert_eq!(load_project(&out).unwrap().len(), 1);
    }

    #[test]
    fn a_bad_debug_block_does_not_stop_edit_configurations() {
        for bad in ["[debug.demo]\nlaunch = \"s\"\n", "[debug]\nport = 1\n"] {
            let text = format!("[[configs]]\nname = \"demo\"\ncommand = \"make\"\n\n{bad}");
            let loaded = load_project(&text).unwrap();
            assert_eq!(loaded.len(), 1, "{bad}");
            let mut demo = loaded[0].1.clone();
            demo.cwd = Some("x".into());
            let out = apply_config_edits(&text, &[ConfigEdit::Update(slot(Array::Configs, 0), demo)]).unwrap();
            assert_eq!(out, text.replacen("command = \"make\"\n", "command = \"make\"\ncwd = \"x\"\n", 1));
            let out = apply_config_edits(&text, &[ConfigEdit::Add(task("t", argv(&["make", "t"])))]).unwrap();
            assert!(out.contains(bad) && out.contains("name = \"t\""), "{out}");
        }
    }
}
