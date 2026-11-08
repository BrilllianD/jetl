//! `.vscode/launch.json`: VS Code's debug configurations as jetl run configurations. JSONC (comments, trailing
//! commas) is stripped by `strip_jsonc`, then read by serde_json. Only gdb configurations get a debug block; other
//! types are listed as unsupported. Pure except `load`, which reads one file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::config::{DebugBlock, PICK_PROCESS, StartRequest};
use crate::run::{Origin, RunTask, TaskCommand, quote_word};

/// Where VS Code keeps its debug configurations, relative to the project root.
pub const FILE: &str = ".vscode/launch.json";

/// Start of every warning text.
const PREFIX: &str = ".vscode/launch.json";

pub fn path(root: &Path) -> PathBuf {
    root.join(FILE)
}

/// One configuration of the file.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// `origin` `LaunchJson`; `command` is display text only (`TaskCommand::Shell`), `cwd` and `env` are the launch's.
    pub task: RunTask,
    /// The block Debug starts it with, or why it cannot be debugged (the whole refusal text).
    pub debug: Result<DebugBlock, String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Imported {
    pub entries: Vec<Entry>,
    pub warnings: Vec<String>,
}

/// Comments blanked and trailing commas dropped; `Err` on an unterminated block comment.
pub fn strip_jsonc(text: &str) -> Result<String, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let chars: Vec<char> = text.chars().collect();
    let blanked = blank_comments(&chars)?;
    Ok(drop_trailing_commas(&blanked))
}

/// Copies a string literal starting at `chars[i]` (a `"`) into `out`; returns the index after it.
fn copy_string(chars: &[char], mut i: usize, out: &mut Vec<char>) -> usize {
    out.push(chars[i]);
    i += 1;
    while i < chars.len() {
        let c = chars[i];
        out.push(c);
        i += 1;
        if c == '\\' {
            if let Some(&n) = chars.get(i) {
                out.push(n);
                i += 1;
            }
        } else if c == '"' {
            break;
        }
    }
    i
}

fn blank(c: char) -> char {
    if c == '\n' || c == '\r' { c } else { ' ' }
}

fn blank_comments(chars: &[char]) -> Result<Vec<char>, String> {
    let mut out = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        match (chars[i], chars.get(i + 1)) {
            ('"', _) => i = copy_string(chars, i, &mut out),
            ('/', Some('/')) => {
                while i < chars.len() && chars[i] != '\n' && chars[i] != '\r' {
                    out.push(' ');
                    i += 1;
                }
            }
            ('/', Some('*')) => {
                let mut j = i + 2;
                while j + 1 < chars.len() && !(chars[j] == '*' && chars[j + 1] == '/') {
                    j += 1;
                }
                if j + 1 >= chars.len() {
                    return Err("unterminated /* comment".into());
                }
                out.extend(chars[i..j + 2].iter().map(|&c| blank(c)));
                i = j + 2;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(out)
}

fn drop_trailing_commas(chars: &[char]) -> String {
    let mut out = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '"' => i = copy_string(chars, i, &mut out),
            ',' => {
                let next = chars[i + 1..].iter().find(|c| !c.is_whitespace());
                out.push(if matches!(next, Some('}' | ']')) { ' ' } else { ',' });
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out.into_iter().collect()
}

/// The configurations of a file's text; `env` answers `${env:NAME}`.
pub fn parse(text: &str, env: &dyn Fn(&str) -> Option<String>) -> Imported {
    let mut out = Imported::default();
    let stripped = match strip_jsonc(text) {
        Ok(s) => s,
        Err(e) => {
            out.warnings.push(format!("{PREFIX}: {e}"));
            return out;
        }
    };
    let root: Value = match serde_json::from_str(&stripped) {
        Ok(v) => v,
        Err(e) => {
            out.warnings.push(format!("{PREFIX}: {e}"));
            return out;
        }
    };
    let Some(root) = root.as_object() else {
        out.warnings.push(format!("{PREFIX}: the top level is not an object"));
        return out;
    };
    let Some(configs) = root.get("configurations") else { return out };
    let Some(configs) = configs.as_array() else {
        out.warnings.push(format!("{PREFIX}: configurations is not a list"));
        return out;
    };
    for (i, c) in configs.iter().enumerate() {
        let name = c.get("name").and_then(Value::as_str).filter(|n| !n.is_empty());
        let (Some(obj), Some(name)) = (c.as_object(), name) else {
            out.warnings.push(format!("{PREFIX}: skipped configuration {}: it has no name", i + 1));
            continue;
        };
        match map_entry(name, obj, env) {
            Ok((entry, warning)) => {
                out.warnings.extend(warning);
                out.entries.push(entry);
            }
            Err(w) => out.warnings.push(w),
        }
    }
    out
}

/// `parse` of the file under `root`; a missing file is empty with no warning. Reads a file: off the main thread,
/// except in the Run Configurations window, which reads its small files on the main thread.
pub fn load(root: &Path, env: &dyn Fn(&str) -> Option<String>) -> Imported {
    match std::fs::read_to_string(path(root)) {
        Ok(text) => parse(&text, env),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Imported::default(),
        Err(e) => Imported { entries: Vec::new(), warnings: vec![format!("{PREFIX}: {e}")] },
    }
}

/// Appends, in file order, each entry's task whose name no task in `tasks` has (earlier ones win).
pub fn append_tasks(tasks: &mut Vec<RunTask>, entries: &[Entry]) {
    for e in entries {
        if !tasks.iter().any(|t| t.name == e.task.name) {
            tasks.push(e.task.clone());
        }
    }
}

/// The block of the entry named `name`: its refusal text when it is not supported.
pub fn block_for(imported: &Imported, name: &str) -> Option<Result<DebugBlock, String>> {
    imported.entries.iter().find(|e| e.task.name == name).map(|e| e.debug.clone())
}

fn skipped(name: &str, why: &str) -> String {
    format!("{PREFIX}: skipped '{name}': {why}")
}

/// Whether every `${...}` of `s` is one jetl knows: `W-var` text for the first that is not. A `${` with no `}`
/// after it is plain text. `pid_slot`: `s` is a processId, where `${command:pickProcess}` is the whole string.
fn check_vars(name: &str, s: &str, pid_slot: bool) -> Result<(), String> {
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        let Some(len) = rest[start + 2..].find('}') else { break };
        let var = &rest[start + 2..start + 2 + len];
        let ok = matches!(var, "workspaceFolder" | "file")
            || var.strip_prefix("env:").is_some_and(|n| !n.is_empty())
            || (pid_slot && s == PICK_PROCESS);
        if !ok {
            return Err(skipped(
                name,
                &format!(
                    "${{{var}}} is not supported (only ${{workspaceFolder}}, ${{file}}, ${{env:NAME}} and a processId of ${{command:pickProcess}})"
                ),
            ));
        }
        rest = &rest[start + 2 + len + 1..];
    }
    Ok(())
}

/// Each `${env:NAME}` replaced by `env(NAME)`, an unset one by nothing; the replaced text is not scanned again.
fn expand_env(s: &str, env: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("${env:") {
        let Some(len) = rest[start + 6..].find('}') else { break };
        out.push_str(&rest[..start]);
        out.push_str(&env(&rest[start + 6..start + 6 + len]).unwrap_or_default());
        rest = &rest[start + 6 + len + 1..];
    }
    out.push_str(rest);
    out
}

/// A string value checked for variables, then expanded.
fn read_str(name: &str, s: &str, pid_slot: bool, env: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    check_vars(name, s, pid_slot)?;
    Ok(expand_env(s, env))
}

/// The kind of a `type`: gdb, or the reason for the list ("cppdbg with MIMode lldb", "no type", the type).
fn kind_of(obj: &Map<String, Value>) -> Result<(), String> {
    let Some(t) = obj.get("type").and_then(Value::as_str) else { return Err("no type".into()) };
    match t {
        "gdb" => Ok(()),
        "cppdbg" => match obj.get("MIMode") {
            None => Ok(()),
            Some(Value::String(m)) if m == "gdb" => Ok(()),
            Some(Value::String(m)) => Err(format!("cppdbg with MIMode {m}")),
            Some(other) => Err(format!("cppdbg with MIMode {other}")),
        },
        other => Err(other.to_string()),
    }
}

fn task_of(name: &str, command: String, cwd: Option<String>, env: BTreeMap<String, String>) -> RunTask {
    RunTask {
        name: name.to_string(),
        command: TaskCommand::Shell(command),
        cwd,
        env,
        args: Vec::new(),
        matcher: None,
        origin: Origin::LaunchJson,
    }
}

/// One configuration: the entry and its warning, or the warning that says it was skipped.
fn map_entry(name: &str, obj: &Map<String, Value>, env: &dyn Fn(&str) -> Option<String>) -> Result<(Entry, Option<String>), String> {
    if let Err(kind) = kind_of(obj) {
        let task = task_of(name, format!("{kind} (not supported)"), None, BTreeMap::new());
        let why = format!(
            "Cannot debug '{name}': launch.json type {kind} is not supported; jetl debugs type cppdbg (MIMode gdb) and type gdb with gdb"
        );
        return Ok((Entry { task, debug: Err(why) }, None));
    }
    let request = match obj.get("request").and_then(Value::as_str) {
        Some("launch") => StartRequest::Launch,
        Some("attach") => StartRequest::Attach,
        _ => return Err(skipped(name, "request must be \"launch\" or \"attach\"")),
    };
    let (task, launch) = match request {
        StartRequest::Launch => map_launch(name, obj, env)?,
        StartRequest::Attach => map_attach(name, obj, env)?,
    };
    let block = DebugBlock { adapter: Some("gdb".into()), request, launch: Value::Object(launch), ..DebugBlock::default() };
    let warning =
        obj.contains_key("preLaunchTask").then(|| format!("{PREFIX}: '{name}': preLaunchTask is not run by jetl; build before debugging"));
    Ok((Entry { task, debug: Ok(block) }, warning))
}

fn map_launch(name: &str, obj: &Map<String, Value>, env: &dyn Fn(&str) -> Option<String>) -> Result<(RunTask, Map<String, Value>), String> {
    let mut launch = Map::new();
    let mut shown = String::new();
    if let Some(p) = obj.get("program").and_then(Value::as_str) {
        launch.insert("program".into(), read_str(name, p, false, env)?.into());
        shown.push_str(p);
    } else {
        shown.push_str("(no program)");
    }
    if let Some(args) = obj.get("args") {
        let words: Option<Vec<&str>> = args.as_array().and_then(|a| a.iter().map(Value::as_str).collect());
        let Some(words) = words else { return Err(skipped(name, "args must be a list of strings")) };
        let mut list = Vec::new();
        for w in &words {
            list.push(read_str(name, w, false, env)?);
            shown.push(' ');
            shown.push_str(&quote_word(w));
        }
        launch.insert("args".into(), json!(list));
    }
    if let Some(b) = obj.get("stopAtEntry").and_then(Value::as_bool) {
        launch.insert("stopAtBeginningOfMainSubprogram".into(), b.into());
    }
    for key in ["stopAtBeginningOfMainSubprogram", "stopOnEntry"] {
        if let Some(b) = obj.get(key).and_then(Value::as_bool) {
            launch.insert(key.into(), b.into());
        }
    }
    let cwd = match obj.get("cwd").and_then(Value::as_str) {
        Some(c) => Some(read_str(name, c, false, env)?),
        None => None,
    };
    let mut vars = BTreeMap::new();
    if let Some(list) = obj.get("environment").and_then(Value::as_array) {
        for e in list {
            if let (Some(k), Some(v)) = (e.get("name").and_then(Value::as_str), e.get("value").and_then(Value::as_str)) {
                vars.insert(k.to_string(), read_str(name, v, false, env)?);
            }
        }
    }
    if let Some(map) = obj.get("env").and_then(Value::as_object) {
        for (k, v) in map {
            if let Some(v) = v.as_str() {
                vars.insert(k.clone(), read_str(name, v, false, env)?);
            }
        }
    }
    Ok((task_of(name, shown, cwd, vars), launch))
}

fn map_attach(name: &str, obj: &Map<String, Value>, env: &dyn Fn(&str) -> Option<String>) -> Result<(RunTask, Map<String, Value>), String> {
    let mut launch = Map::new();
    let target = match obj.get("miDebuggerServerAddress").or_else(|| obj.get("target")).and_then(Value::as_str) {
        Some(t) => Some(read_str(name, t, false, env)?),
        None => None,
    };
    if let Some(p) = obj.get("program").and_then(Value::as_str) {
        launch.insert("program".into(), read_str(name, p, false, env)?.into());
    }
    let pid = obj.get("processId").filter(|v| !v.is_null()).or_else(|| obj.get("pid").filter(|v| !v.is_null()));
    let shown = match pid {
        Some(pid) => {
            let written = pid.as_str().map(str::to_string).unwrap_or_else(|| pid.to_string());
            let value = match pid {
                Value::String(s) => {
                    let s = read_str(name, s, true, env)?;
                    if s == PICK_PROCESS {
                        Some(Value::String(s))
                    } else {
                        s.chars().all(|c| c.is_ascii_digit()).then(|| s.parse::<u64>().ok()).flatten().and_then(pid_value)
                    }
                }
                Value::Number(n) => n.as_u64().and_then(pid_value),
                _ => None,
            };
            let Some(value) = value else { return Err(skipped(name, "processId must be a number or ${command:pickProcess}")) };
            launch.insert("pid".into(), value);
            format!("attach {written}")
        }
        None => match &target {
            Some(t) => format!("attach target {t}"),
            None => return Err(skipped(name, "an attach needs a processId")),
        },
    };
    if let Some(t) = target {
        launch.insert("target".into(), t.into());
    }
    Ok((task_of(name, shown, None, BTreeMap::new()), launch))
}

/// A pid in `1..=u32::MAX` as a JSON integer.
fn pid_value(n: u64) -> Option<Value> {
    (1..=u64::from(u32::MAX)).contains(&n).then(|| json!(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn read(text: &str) -> Imported {
        parse(text, &no_env)
    }

    fn one(text: &str) -> Entry {
        let i = read(text);
        assert_eq!(i.warnings, Vec::<String>::new(), "{text}");
        assert_eq!(i.entries.len(), 1, "{text}");
        i.entries.into_iter().next().unwrap()
    }

    fn cfg(body: &str) -> String {
        format!("{{\"configurations\": [{{{body}}}]}}")
    }

    fn var_warning(name: &str, var: &str) -> String {
        format!(
            ".vscode/launch.json: skipped '{name}': ${{{var}}} is not supported (only ${{workspaceFolder}}, ${{file}}, ${{env:NAME}} and a processId of ${{command:pickProcess}})"
        )
    }

    #[test]
    fn strip_keeps_strings_and_drops_comments_and_trailing_commas() {
        let text = "{\n  // c1 \"q\"\n  \"url\": \"http://x//y/*z*/\", /* c2 , ] */\n  \"q\": \"a\\\"//b\\\\\",\n  \"l\": [1, 2,],\n}\n";
        let stripped = strip_jsonc(text).unwrap();
        let v: Value = serde_json::from_str(&stripped).unwrap();
        assert_eq!(v, json!({"url": "http://x//y/*z*/", "q": "a\"//b\\", "l": [1, 2]}));
        assert_eq!(stripped.lines().count(), text.lines().count());
    }

    #[test]
    fn strip_handles_bom_crlf_and_a_comma_before_a_comment() {
        let stripped = strip_jsonc("\u{feff}{\r\n \"a\": 1, // x\r\n}\r\n").unwrap();
        assert_eq!(serde_json::from_str::<Value>(&stripped).unwrap(), json!({"a": 1}));
        let stripped = strip_jsonc("{\"a\": [1, /* x */ ]}").unwrap();
        assert_eq!(serde_json::from_str::<Value>(&stripped).unwrap(), json!({"a": [1]}));
    }

    #[test]
    fn an_unterminated_block_comment_is_an_error() {
        assert_eq!(strip_jsonc("{ /* x"), Err("unterminated /* comment".to_string()));
        assert!(strip_jsonc("{\"a\": \"/*\"}").is_ok());
    }

    #[test]
    fn vscode_cppdbg_template_maps_to_gdb() {
        let text = r#"{
    // Use IntelliSense to learn about possible attributes.
    "version": "0.2.0",
    "configurations": [
        {
            "name": "(gdb) Launch",
            "type": "cppdbg",
            "request": "launch",
            "program": "${workspaceFolder}/a.out",
            "args": ["x y"],
            "stopAtEntry": false,
            "cwd": "${workspaceFolder}",
            "environment": [{"name": "A", "value": "1"}],
            "externalConsole": false,
            "MIMode": "gdb",
            "miDebuggerPath": "/usr/bin/gdb",
            "setupCommands": [
                {
                    "description": "Enable pretty-printing for gdb",
                    "text": "-enable-pretty-printing",
                    "ignoreFailures": true
                },
            ]
        },
    ]
}"#;
        let e = one(text);
        assert_eq!(e.task.name, "(gdb) Launch");
        assert_eq!(e.task.origin, Origin::LaunchJson);
        assert_eq!(e.task.cwd.as_deref(), Some("${workspaceFolder}"));
        assert_eq!(e.task.env, BTreeMap::from([("A".to_string(), "1".to_string())]));
        assert_eq!(e.task.command.display(), "${workspaceFolder}/a.out 'x y'");
        let b = e.debug.unwrap();
        assert_eq!(b.adapter.as_deref(), Some("gdb"));
        assert_eq!(b.request, StartRequest::Launch);
        assert_eq!(b.launch, json!({"program": "${workspaceFolder}/a.out", "args": ["x y"], "stopAtBeginningOfMainSubprogram": false}));
        assert_eq!((b.language, b.build), (None, None));
    }

    #[test]
    fn gdb_type_takes_gdb_keys() {
        let e = one(&cfg(r#""name": "n", "type": "gdb", "request": "launch", "env": {"B": "2"},
            "environment": [{"name": "B", "value": "1"}, {"name": "C", "value": "3"}, 5],
            "stopOnEntry": true, "stopAtEntry": true, "stopAtBeginningOfMainSubprogram": false"#));
        assert_eq!(e.task.env, BTreeMap::from([("B".to_string(), "2".to_string()), ("C".to_string(), "3".to_string())]));
        let l = e.debug.unwrap().launch;
        assert_eq!(l["stopOnEntry"], json!(true));
        assert_eq!(l["stopAtBeginningOfMainSubprogram"], json!(false));
        assert_eq!(e.task.command.display(), "(no program)");
    }

    #[test]
    fn attach_entries() {
        let attach = |extra: &str| read(&cfg(&format!(r#""name": "a", "type": "cppdbg", "request": "attach", {extra}"#)));
        let e = attach(r#""processId": "${command:pickProcess}", "program": "/p", "cwd": "/w""#);
        let entry = &e.entries[0];
        assert!(e.warnings.is_empty());
        let b = entry.debug.clone().unwrap();
        assert_eq!(b.launch, json!({"pid": PICK_PROCESS, "program": "/p"}));
        assert_eq!(b.request, StartRequest::Attach);
        assert_eq!(entry.task.command.display(), "attach ${command:pickProcess}");
        assert_eq!(entry.task.cwd, None);
        for text in [r#""processId": "1234""#, r#""processId": 1234"#, r#""pid": 1234"#] {
            assert_eq!(attach(text).entries[0].debug.clone().unwrap().launch, json!({"pid": 1234}), "{text}");
        }
        let t = attach(r#""miDebuggerServerAddress": "localhost:9000""#);
        assert_eq!(t.entries[0].debug.clone().unwrap().launch, json!({"target": "localhost:9000"}));
        assert_eq!(t.entries[0].task.command.display(), "attach target localhost:9000");
        let w = attach(r#""MIMode": "gdb""#);
        assert_eq!((w.entries.len(), w.warnings), (0, vec![".vscode/launch.json: skipped 'a': an attach needs a processId".to_string()]));
        for bad in
            [r#""processId": "12a""#, r#""processId": 0"#, r#""processId": -1"#, r#""processId": 4294967296"#, r#""processId": true"#]
        {
            let w = attach(bad);
            assert_eq!(
                (w.entries.len(), w.warnings),
                (0, vec![".vscode/launch.json: skipped 'a': processId must be a number or ${command:pickProcess}".to_string()]),
                "{bad}"
            );
        }
        assert_eq!(attach(r#""processId": 4294967295"#).entries.len(), 1);
    }

    #[test]
    fn other_types_are_listed_unsupported() {
        let kinds = [
            ("lldb", r#""type": "lldb""#),
            ("codelldb", r#""type": "codelldb""#),
            ("debugpy", r#""type": "debugpy", "program": "${fileDirname}/x.py""#),
            ("python", r#""type": "python""#),
            ("go", r#""type": "go""#),
            ("node", r#""type": "node""#),
            ("pwa-node", r#""type": "pwa-node""#),
            ("cppdbg with MIMode lldb", r#""type": "cppdbg", "MIMode": "lldb""#),
            ("no type", r#""request": "launch""#),
        ];
        let body: Vec<String> = kinds.iter().map(|(_, b)| format!("{{\"name\": \"n\", {b}}}")).collect();
        let i = read(&format!("{{\"configurations\": [{}]}}", body.join(",")));
        assert_eq!(i.warnings, Vec::<String>::new());
        assert_eq!(i.entries.len(), 9);
        for (e, (kind, _)) in i.entries.iter().zip(kinds) {
            assert_eq!(
                e.debug,
                Err(format!(
                    "Cannot debug 'n': launch.json type {kind} is not supported; jetl debugs type cppdbg (MIMode gdb) and type gdb with gdb"
                ))
            );
            assert_eq!(e.task.command.display(), format!("{kind} (not supported)"));
            assert_eq!(e.task.origin, Origin::LaunchJson);
        }
    }

    #[test]
    fn unsupported_variables_skip_the_entry() {
        let gdb = |body: &str| read(&cfg(&format!(r#""name": "v", "type": "gdb", "request": "launch", {body}"#)));
        let attach = |body: &str| read(&cfg(&format!(r#""name": "v", "type": "gdb", "request": "attach", {body}"#)));
        let cases = [
            (gdb(r#""cwd": "${fileDirname}""#), "fileDirname"),
            (gdb(r#""program": "${workspaceFolderBasename}/a""#), "workspaceFolderBasename"),
            (attach(r#""processId": "${command:pickRemoteProcess}""#), "command:pickRemoteProcess"),
            (gdb(r#""program": "${command:pickProcess}""#), "command:pickProcess"),
            (gdb(r#""env": {"K": "${input:x}"}"#), "input:x"),
            (gdb(r#""args": ["${file}", "${env:}"]"#), "env:"),
        ];
        for (i, var) in cases {
            assert_eq!((i.entries.len(), i.warnings), (0, vec![var_warning("v", var)]), "{var}");
        }
        let env = |k: &str| (k == "HOME").then(|| "/h".to_string());
        let p = |s: &str| parse(&cfg(&format!(r#""name": "v", "type": "gdb", "request": "launch", "program": "{s}""#)), &env);
        assert_eq!(p("${env:HOME}/a").entries[0].debug.clone().unwrap().launch, json!({"program": "/h/a"}));
        assert_eq!(p("${env:NOPE}x").entries[0].debug.clone().unwrap().launch, json!({"program": "x"}));
        assert_eq!(p("a${ b").entries[0].debug.clone().unwrap().launch, json!({"program": "a${ b"}));
        assert_eq!(
            p("${workspaceFolder}/${file}").entries[0].debug.clone().unwrap().launch,
            json!({"program": "${workspaceFolder}/${file}"})
        );
        // The replaced text is not scanned again.
        let env2 = |_: &str| Some("${env:HOME}".to_string());
        let again = parse(&cfg(r#""name": "v", "type": "gdb", "request": "launch", "program": "${env:X}""#), &env2);
        assert_eq!(again.entries[0].debug.clone().unwrap().launch, json!({"program": "${env:HOME}"}));
    }

    #[test]
    fn prelaunch_task_warns_once_and_keeps_the_entry() {
        let i = read(&cfg(r#""name": "p", "type": "cppdbg", "request": "launch", "preLaunchTask": "build""#));
        assert_eq!(i.entries.len(), 1);
        assert_eq!(i.warnings, vec![".vscode/launch.json: 'p': preLaunchTask is not run by jetl; build before debugging".to_string()]);
    }

    #[test]
    fn a_nameless_entry_is_skipped() {
        let i = read(r#"{"configurations": [5, {"type": "gdb", "request": "launch"}, {"name": "", "type": "gdb"}, {"name": 3}]}"#);
        assert!(i.entries.is_empty());
        let want: Vec<String> = (1..=4).map(|n| format!(".vscode/launch.json: skipped configuration {n}: it has no name")).collect();
        assert_eq!(i.warnings, want);
    }

    #[test]
    fn bad_request_is_skipped() {
        for body in [r#""request": "run""#, r#""request": 1"#, r#""x": 1"#] {
            let i = read(&cfg(&format!(r#""name": "r", "type": "gdb", {body}"#)));
            assert_eq!(
                (i.entries.len(), i.warnings),
                (0, vec![r#".vscode/launch.json: skipped 'r': request must be "launch" or "attach""#.to_string()]),
                "{body}"
            );
        }
    }

    #[test]
    fn bad_args_are_skipped() {
        for args in [r#""a""#, r#"["a", 1]"#, "{}"] {
            let i = read(&cfg(&format!(r#""name": "r", "type": "gdb", "request": "launch", "args": {args}"#)));
            assert_eq!(
                (i.entries.len(), i.warnings),
                (0, vec![".vscode/launch.json: skipped 'r': args must be a list of strings".to_string()]),
                "{args}"
            );
        }
    }

    #[test]
    fn configurations_not_a_list_warns() {
        let i = read(r#"{"configurations": {}}"#);
        assert_eq!((i.entries.len(), i.warnings), (0, vec![".vscode/launch.json: configurations is not a list".to_string()]));
        let i = read("[]");
        assert_eq!(i.warnings.len(), 1);
    }

    #[test]
    fn no_configurations_is_empty() {
        assert_eq!(read(r#"{"version": "0.2.0", "compounds": [], "inputs": []}"#), Imported::default());
        assert_eq!(read("{}"), Imported::default());
    }

    #[test]
    fn invalid_json_gives_one_warning() {
        let i = read("{\"configurations\": [}");
        assert!(i.entries.is_empty());
        assert_eq!(i.warnings.len(), 1);
        assert!(i.warnings[0].starts_with(".vscode/launch.json: "), "{}", i.warnings[0]);
        assert!(i.warnings[0].contains("line 1"), "{}", i.warnings[0]);
        let i = read("{ /* x");
        assert_eq!(i.warnings, vec![".vscode/launch.json: unterminated /* comment".to_string()]);
    }

    #[test]
    fn load_of_a_missing_file_is_empty() {
        let dir = std::env::temp_dir().join(format!("jetl-launch-json-load-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(load(&dir, &no_env), Imported::default());
        std::fs::create_dir_all(dir.join(".vscode")).unwrap();
        std::fs::write(path(&dir), cfg(r#""name": "a", "type": "gdb", "request": "launch""#)).unwrap();
        let i = load(&dir, &no_env);
        assert_eq!(i.entries.iter().map(|e| e.task.name.as_str()).collect::<Vec<_>>(), ["a"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn append_tasks_keeps_the_first_name() {
        let entry = |n: &str| Entry { task: task_of(n, "x".into(), None, BTreeMap::new()), debug: Err("no".into()) };
        let mut tasks = vec![RunTask { origin: Origin::Config, ..task_of("a", "x".into(), None, BTreeMap::new()) }];
        append_tasks(&mut tasks, &[entry("a"), entry("b"), entry("b")]);
        assert_eq!(tasks.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!((tasks[0].origin, tasks[1].origin), (Origin::Config, Origin::LaunchJson));
    }

    #[test]
    fn block_for_finds_the_first_entry_by_name() {
        let i = read(&cfg(r#""name": "a", "type": "lldb""#));
        assert!(matches!(block_for(&i, "a"), Some(Err(_))));
        assert_eq!(block_for(&i, "zz"), None);
    }
}
