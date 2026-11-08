//! Debug adapter configuration: which adapters exist (`[debug.adapters.<name>]` in config.toml over a
//! built-in table), how a run configuration is debugged (`[debug.<name>]` in `.jetl/run.toml`, or
//! `[run.debug.<name>]` in config.toml), which adapter is chosen and what `launch` carries.
//!
//! Pure over an injected `doctor::Find` and environment. Only `load_debug_blocks` touches the disk, and
//! nothing here starts a process: `resolve_build` only describes the build step.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value, json};

use crate::doctor::Find;
use crate::run::caret::RustTest;
use crate::run::process::Spec;
use crate::run::{Origin, RunTask};

/// `launch` as written: a table, or refused when the file is parsed.
fn table<'de, D: Deserializer<'de>>(d: D) -> Result<Value, D::Error> {
    let v = Value::deserialize(d)?;
    if v.is_object() { Ok(v) } else { Err(serde::de::Error::custom("launch must be a table")) }
}

fn table_opt<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    table(d).map(Some)
}

/// `[debug]` in config.toml.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct DebugConfig {
    /// `[debug.adapters.<name>]`: changes to a built-in adapter, or a new one.
    pub adapters: BTreeMap<String, AdapterOverride>,
}

/// A `[debug.adapters.<name>]` table; omitted keys keep the built-in values.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct AdapterOverride {
    /// The command and its arguments; empty turns the adapter off.
    pub command: Option<Vec<String>>,
    /// jetl language ids (`LanguageDef::name`) the adapter is used for.
    pub languages: Option<Vec<String>>,
    /// Arguments of the adapter's `launch` request; replaces the built-in ones whole. `${language}` becomes the language in use.
    #[serde(deserialize_with = "table_opt")]
    pub launch: Option<Value>,
}

/// An adapter as started: the built-in values with the user's changes.
#[derive(Debug, Clone, PartialEq)]
pub struct AdapterConfig {
    pub name: String,
    pub command: Vec<String>,
    pub languages: Vec<String>,
    /// Base of the `launch` arguments; may hold `${language}` until `choose_adapter` fills it.
    pub launch: Value,
    /// The adapter takes `env` as the exact environment, so jetl's own goes beneath the configuration's.
    pub exact_env: bool,
    /// Where to get it, shown when it is missing; empty for a user-defined adapter.
    pub install_hint: &'static str,
}

/// How one run configuration is debugged.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct DebugBlock {
    /// An adapter by name; unset, the first installed for the language.
    pub adapter: Option<String>,
    /// A jetl language id; unset, the active buffer's.
    pub language: Option<String>,
    /// The command to run first, as words; empty means none.
    pub build: Option<Vec<String>>,
    /// The adapter's `launch` arguments, passed on as they stand (after variables); `Null` when omitted.
    #[serde(deserialize_with = "table")]
    pub launch: Value,
}

/// Values for `${workspaceFolder}`, `${file}` and `${artifact}`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchVars {
    pub workspace_folder: String,
    pub file: Option<String>,
    pub artifact: Option<String>,
}

/// Fills in the language an adapter runs for; see `choose_adapter`.
const LANGUAGE_VAR: &str = "${language}";
/// Filled in from the build's executable; see `fill_artifact`.
const ARTIFACT_VAR: &str = "${artifact}";

struct Builtin {
    name: &'static str,
    command: &'static [&'static str],
    languages: &'static [&'static str],
    launch: fn() -> Value,
    exact_env: bool,
    install_hint: &'static str,
}

/// In order: the first one found is the default for a language.
const BUILTIN: &[Builtin] = &[
    Builtin {
        name: "codelldb",
        command: &["codelldb"],
        languages: &["rust", "c", "cpp"],
        launch: || json!({"terminal": "console", "sourceLanguages": [LANGUAGE_VAR]}),
        exact_env: false,
        install_hint: "the VSIX from github.com/vadimcn/codelldb releases, or AUR codelldb",
    },
    Builtin {
        name: "lldb-dap",
        command: &["lldb-dap"],
        languages: &["rust", "c", "cpp"],
        launch: || json!({"console": "internalConsole"}),
        exact_env: false,
        install_hint: "the LLVM packages, Arch lldb",
    },
    Builtin {
        name: "gdb",
        command: &["gdb", "-q", "-i=dap"],
        languages: &["c", "cpp", "rust"],
        launch: || json!({}),
        exact_env: true,
        install_hint: "your distribution's gdb package",
    },
    Builtin {
        name: "debugpy",
        command: &["python3", "-P", "-m", "debugpy.adapter"],
        languages: &["python"],
        launch: || json!({"console": "internalConsole", "subProcess": false}),
        exact_env: false,
        install_hint: "pip install debugpy, Arch python-debugpy",
    },
];

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

impl DebugConfig {
    /// Every adapter, user-defined ones first (by name), then the built-in ones with the user's changes;
    /// adapters turned off are left out.
    pub fn adapters(&self) -> Vec<AdapterConfig> {
        let builtin = |name: &str| BUILTIN.iter().find(|b| b.name == name);
        let none = AdapterOverride::default();
        let user = self.adapters.iter().filter(|(name, _)| builtin(name).is_none()).map(|(name, o)| (name.as_str(), o, None));
        let known = BUILTIN.iter().map(|b| (b.name, self.adapters.get(b.name).unwrap_or(&none), Some(b)));
        user.chain(known)
            .map(|(name, o, b)| AdapterConfig {
                name: name.to_string(),
                command: o.command.clone().unwrap_or_else(|| b.map(|b| strings(b.command)).unwrap_or_default()),
                languages: o.languages.clone().unwrap_or_else(|| b.map(|b| strings(b.languages)).unwrap_or_default()),
                launch: o.launch.clone().unwrap_or_else(|| b.map_or_else(|| json!({}), |b| (b.launch)())),
                exact_env: b.is_some_and(|b| b.exact_env),
                install_hint: b.map_or("", |b| b.install_hint),
            })
            .filter(|a| a.command.first().is_some_and(|c| !c.is_empty()))
            .collect()
    }
}

/// Replaces each `from` in every string of the tree; keys stay as written.
fn substitute(v: &mut Value, vars: &[(&str, &str)]) {
    match v {
        Value::String(s) => {
            for (from, to) in vars {
                *s = s.replace(from, to);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|e| substitute(e, vars)),
        Value::Object(o) => o.values_mut().for_each(|e| substitute(e, vars)),
        _ => {}
    }
}

fn mentions(v: &Value, var: &str) -> bool {
    match v {
        Value::String(s) => s.contains(var),
        Value::Array(a) => a.iter().any(|e| mentions(e, var)),
        Value::Object(o) => o.values().any(|e| mentions(e, var)),
        _ => false,
    }
}

/// Whether `launch` holds `${artifact}` anywhere.
pub fn needs_artifact(launch: &Value) -> bool {
    mentions(launch, ARTIFACT_VAR)
}

/// Replaces `${artifact}` in every string of `launch`.
pub fn fill_artifact(launch: &mut Value, path: &str) {
    substitute(launch, &[(ARTIFACT_VAR, path)]);
}

/// With a language, `${language}` becomes it; without one, the top-level keys that mention it are removed.
fn fill_language(launch: &mut Value, language: Option<&str>) {
    match language {
        Some(l) => substitute(launch, &[(LANGUAGE_VAR, l)]),
        None => {
            if let Some(o) = launch.as_object_mut() {
                o.retain(|_, v| !mentions(v, LANGUAGE_VAR));
            }
        }
    }
}

/// The adapter for a block: the one it names, else the first installed one that lists the language
/// (`block.language`, else `language`). The result's `launch` has `${language}` filled in, so send
/// only this to `resolve_launch`, never an entry of `DebugConfig::adapters` directly.
pub fn choose_adapter(cfg: &DebugConfig, block: &DebugBlock, language: Option<&str>, find: Find) -> Result<AdapterConfig, String> {
    let adapters = cfg.adapters();
    let language = block.language.as_deref().or(language);
    let mut chosen = if let Some(name) = &block.adapter {
        let a = adapters.into_iter().find(|a| &a.name == name).ok_or_else(|| {
            format!("Debug adapter '{name}' is not configured, or its command is empty: see [debug.adapters] in config.toml")
        })?;
        if find(&a.command[0]).is_none() {
            let hint = if a.install_hint.is_empty() { String::new() } else { format!(". Install: {}", a.install_hint) };
            return Err(format!("Debug adapter '{name}': {} not found{hint}", a.command[0]));
        }
        a
    } else {
        let language = language.ok_or(
            "No language for this debug configuration: open a file of its language, or set language or adapter in its debug table",
        )?;
        let candidates: Vec<AdapterConfig> = adapters.into_iter().filter(|a| a.languages.iter().any(|l| l == language)).collect();
        if candidates.is_empty() {
            return Err(format!("No debug adapter for {language}: add one as [debug.adapters.<name>] in config.toml"));
        }
        match candidates.iter().position(|a| find(&a.command[0]).is_some()) {
            Some(i) => candidates.into_iter().nth(i).expect("position is in range"),
            None => {
                let list: Vec<String> = candidates
                    .iter()
                    .map(|a| if a.install_hint.is_empty() { a.name.clone() } else { format!("{} ({})", a.name, a.install_hint) })
                    .collect();
                return Err(format!("No debug adapter found for {language}. Install one of: {}", list.join("; ")));
            }
        }
    };
    fill_language(&mut chosen.launch, language);
    Ok(chosen)
}

fn extend(out: &mut Map<String, Value>, v: &Value) {
    if let Some(o) = v.as_object() {
        out.extend(o.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
}

fn var_pairs(vars: &LaunchVars) -> Vec<(&str, &str)> {
    let mut pairs = vec![("${workspaceFolder}", vars.workspace_folder.as_str())];
    pairs.extend(vars.file.as_deref().map(|f| ("${file}", f)));
    pairs.extend(vars.artifact.as_deref().map(|a| (ARTIFACT_VAR, a)));
    pairs
}

/// The task's `cwd` with the variables replaced, joined to the root (an absolute one replaces it); else the root.
fn task_cwd(task: &RunTask, vars: &LaunchVars, pairs: &[(&str, &str)]) -> String {
    match task.cwd.as_deref().filter(|c| !c.is_empty()) {
        Some(c) => {
            let mut c = Value::String(c.to_string());
            substitute(&mut c, pairs);
            Path::new(&vars.workspace_folder).join(c.as_str().unwrap_or_default()).to_string_lossy().into_owned()
        }
        None => vars.workspace_folder.clone(),
    }
}

/// The step to run before the adapter: the block's `build` words with `${workspaceFolder}` and `${file}`
/// replaced, run in the task's `cwd` with its `env`. `None` when the block has no build or its program is empty.
pub fn resolve_build(block: &DebugBlock, task: &RunTask, vars: &LaunchVars) -> Option<Spec> {
    let words = block.build.as_ref().filter(|w| w.first().is_some_and(|p| !p.is_empty()))?;
    let pairs = var_pairs(vars);
    let argv = words
        .iter()
        .map(|w| {
            let mut v = Value::String(w.clone());
            substitute(&mut v, &pairs);
            v.as_str().unwrap_or_default().to_string()
        })
        .collect();
    Some(Spec {
        argv,
        cwd: PathBuf::from(task_cwd(task, vars, &pairs)),
        env: task.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
    })
}

/// The block a detected Cargo task is debugged with when none is written: build it with JSON messages
/// and debug what it names as `${artifact}`.
pub fn default_block(task: &RunTask) -> Option<DebugBlock> {
    if task.origin != Origin::Detected {
        return None;
    }
    let build: &[&str] = match task.command.display().as_str() {
        "cargo build" | "cargo run" => &["cargo", "build", "--message-format=json"],
        "cargo test" => &["cargo", "test", "--no-run", "--message-format=json"],
        _ => return None,
    };
    Some(DebugBlock { adapter: None, language: Some("rust".into()), build: Some(strings(build)), launch: json!({"program": ARTIFACT_VAR}) })
}

/// The block Debug Test at Caret debugs with. It is not a configuration: nothing names it, writes it or
/// selects it. It builds the package's tests with the Run version's target flag and launches
/// `${artifact}` (the test executable that holds the test) with only that test (or module) selected.
pub fn test_block(t: &RustTest) -> DebugBlock {
    let mut build = strings(&["cargo", "test", "--no-run", "--message-format=json"]);
    build.extend(t.target.iter().cloned());
    let mut args = vec![t.filter.clone()];
    if t.exact {
        args.push("--exact".into());
    }
    args.push("--nocapture".into());
    DebugBlock { adapter: None, language: Some("rust".into()), build: Some(build), launch: json!({"program": ARTIFACT_VAR, "args": args}) }
}

/// The `launch` arguments for a session. Later wins, by top-level key: the adapter's `launch`; `cwd`
/// (the task's, with the variables replaced, joined to the root, so an absolute result replaces the root; else the root); `env` (the task's, with jetl's beneath it when the adapter
/// takes the exact environment; left out when empty); the block's `launch`. Then `${workspaceFolder}`,
/// `${file}` and `${artifact}` are replaced in every string; an unset one stays as written. The task's
/// `command` and `args` are not used.
pub fn resolve_launch(
    adapter: &AdapterConfig,
    block: &DebugBlock,
    task: &RunTask,
    vars: &LaunchVars,
    env: &dyn Fn() -> BTreeMap<String, String>,
) -> Value {
    let mut out = Map::new();
    extend(&mut out, &adapter.launch);
    let pairs = var_pairs(vars);
    out.insert("cwd".into(), task_cwd(task, vars, &pairs).into());
    let mut merged = if adapter.exact_env { env() } else { BTreeMap::new() };
    merged.extend(task.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    if !merged.is_empty() {
        out.insert("env".into(), json!(merged));
    }
    extend(&mut out, &block.launch);
    let mut v = Value::Object(out);
    substitute(&mut v, &pairs);
    v
}

/// The `debug` tables of `.jetl/run.toml` and nothing else: a bad task is not this reader's to report.
#[derive(Default, Deserialize)]
#[serde(default)]
struct ProjectBlocks {
    debug: BTreeMap<String, DebugBlock>,
}

/// The blocks of `configured` with those of `.jetl/run.toml` over them, by name. On a bad project file, the
/// `configured` ones and the error text. Reads a file, so it runs off the main thread.
pub fn load_debug_blocks(root: &Path, configured: &BTreeMap<String, DebugBlock>) -> (BTreeMap<String, DebugBlock>, Option<String>) {
    let mut blocks = configured.clone();
    let mut error = None;
    match std::fs::read_to_string(crate::run::edit::project_file(root)) {
        Ok(text) => match toml::from_str::<ProjectBlocks>(&text) {
            Ok(c) => blocks.extend(c.debug),
            Err(e) => error = Some(format!(".jetl/run.toml: {}", crate::config::theme::error_summary(&e.to_string()))),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => error = Some(format!(".jetl/run.toml: {e}")),
    }
    (blocks, error)
}

/// jetl's environment, without a variable that is not UTF-8 (`std::env::vars` would panic on one).
pub fn process_env() -> BTreeMap<String, String> {
    std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))).collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::Config;
    use crate::run::{Origin, RunConfig, TaskCommand};

    fn cfg(text: &str) -> DebugConfig {
        Config::parse(text).unwrap().debug
    }

    fn finder(known: &'static [&'static str]) -> impl Fn(&str) -> Option<PathBuf> {
        move |n| known.contains(&n).then(|| PathBuf::from(format!("/usr/bin/{n}")))
    }

    fn task(cwd: Option<&str>, env: &[(&str, &str)]) -> RunTask {
        RunTask {
            name: "demo".into(),
            command: TaskCommand::Argv(vec!["./demo".into()]),
            cwd: cwd.map(String::from),
            env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            args: vec!["ignored".into()],
            matcher: None,
            origin: Origin::Config,
        }
    }

    fn vars() -> LaunchVars {
        LaunchVars { workspace_folder: "/w".into(), file: Some("/w/a.c".into()), artifact: None }
    }

    fn no_env() -> BTreeMap<String, String> {
        panic!("jetl's environment was read")
    }

    fn block(adapter: Option<&str>, language: Option<&str>, launch: Value) -> DebugBlock {
        DebugBlock { adapter: adapter.map(String::from), language: language.map(String::from), build: None, launch }
    }

    fn names(cfg: &DebugConfig) -> Vec<String> {
        cfg.adapters().into_iter().map(|a| a.name).collect()
    }

    const ALL: &[&str] = &["codelldb", "lldb-dap", "gdb", "debugpy", "python3"];

    #[test]
    fn builtins_come_in_order_with_their_values() {
        let all = DebugConfig::default().adapters();
        assert_eq!(all.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["codelldb", "lldb-dap", "gdb", "debugpy"]);
        let expect: [(&[&str], &[&str], Value); 4] = [
            (&["codelldb"], &["rust", "c", "cpp"], json!({"terminal": "console", "sourceLanguages": ["${language}"]})),
            (&["lldb-dap"], &["rust", "c", "cpp"], json!({"console": "internalConsole"})),
            (&["gdb", "-q", "-i=dap"], &["c", "cpp", "rust"], json!({})),
            (&["python3", "-P", "-m", "debugpy.adapter"], &["python"], json!({"console": "internalConsole", "subProcess": false})),
        ];
        for (a, (command, languages, launch)) in all.iter().zip(expect) {
            assert_eq!(a.command, command, "{}", a.name);
            assert_eq!(a.languages, languages, "{}", a.name);
            assert_eq!(a.launch, launch, "{}", a.name);
            assert_eq!(a.exact_env, a.name == "gdb", "{}", a.name);
            assert!(!a.install_hint.is_empty(), "{}", a.name);
        }
    }

    #[test]
    fn an_override_keeps_omitted_keys() {
        let c = cfg("[debug.adapters.gdb]\nlanguages = [\"c\"]\n");
        let all = c.adapters();
        assert_eq!(names(&c), ["codelldb", "lldb-dap", "gdb", "debugpy"]);
        let gdb = &all[2];
        assert_eq!(gdb.command, ["gdb", "-q", "-i=dap"]);
        assert!(gdb.exact_env);
        assert!(!gdb.install_hint.is_empty());
        assert_eq!(gdb.languages, ["c"]);
    }

    #[test]
    fn a_launch_override_replaces_the_whole_launch() {
        let c = cfg("[debug.adapters.debugpy]\nlaunch = { justMyCode = false }\n");
        let py = c.adapters().into_iter().find(|a| a.name == "debugpy").unwrap();
        assert_eq!(py.launch, json!({"justMyCode": false}));
    }

    #[test]
    fn an_empty_command_turns_an_adapter_off() {
        for text in ["[debug.adapters.codelldb]\ncommand = []\n", "[debug.adapters.codelldb]\ncommand = [\"\"]\n"] {
            assert_eq!(names(&cfg(text)), ["lldb-dap", "gdb", "debugpy"], "{text}");
        }
        assert_eq!(names(&cfg("[debug.adapters.nocmd]\nlanguages = [\"go\"]\n")), ["codelldb", "lldb-dap", "gdb", "debugpy"]);
    }

    #[test]
    fn user_defined_come_first_by_name() {
        let c = cfg(
            "[debug.adapters.zeta]\ncommand = [\"z\"]\nlanguages = [\"go\"]\n[debug.adapters.alpha]\ncommand = [\"a\"]\nlanguages = [\"go\"]\n",
        );
        assert_eq!(names(&c), ["alpha", "zeta", "codelldb", "lldb-dap", "gdb", "debugpy"]);
        let alpha = &c.adapters()[0];
        assert!(!alpha.exact_env);
        assert_eq!(alpha.install_hint, "");
        assert_eq!(alpha.launch, json!({}));
    }

    #[test]
    fn the_block_names_the_adapter() {
        let c = DebugConfig::default();
        let found = finder(&["gdb"]);
        let a = choose_adapter(&c, &block(Some("gdb"), None, Value::Null), None, &found).unwrap();
        assert_eq!(a.name, "gdb");
        let unknown = "Debug adapter 'nope' is not configured, or its command is empty: see [debug.adapters] in config.toml";
        assert_eq!(choose_adapter(&c, &block(Some("nope"), None, Value::Null), None, &found).unwrap_err(), unknown);
        let off = cfg("[debug.adapters.gdb]\ncommand = []\n");
        let err = choose_adapter(&off, &block(Some("gdb"), None, Value::Null), None, &found).unwrap_err();
        assert_eq!(err, unknown.replace("nope", "gdb"));
        let err = choose_adapter(&c, &block(Some("gdb"), None, Value::Null), None, &finder(&[])).unwrap_err();
        assert_eq!(err, "Debug adapter 'gdb': gdb not found. Install: your distribution's gdb package");
        let mine = cfg("[debug.adapters.mine]\ncommand = [\"mine\"]\nlanguages = [\"go\"]\n");
        let err = choose_adapter(&mine, &block(Some("mine"), None, Value::Null), None, &finder(&[])).unwrap_err();
        assert_eq!(err, "Debug adapter 'mine': mine not found");
    }

    #[test]
    fn the_language_picks_the_first_found() {
        let c = DebugConfig::default();
        let table = [
            (ALL, None, Some("rust"), "codelldb"),
            (&["lldb-dap", "gdb"], None, Some("rust"), "lldb-dap"),
            (&["gdb"], None, Some("c"), "gdb"),
            (ALL, Some("c"), Some("python"), "codelldb"),
            (&["python3"], None, Some("python"), "debugpy"),
        ];
        for (known, block_lang, active, want) in table {
            let a = choose_adapter(&c, &block(None, block_lang, Value::Null), active, &finder(known)).unwrap();
            assert_eq!(a.name, want, "{known:?} {block_lang:?} {active:?}");
        }
        let mine = cfg("[debug.adapters.mine]\ncommand = [\"mine\"]\nlanguages = [\"rust\"]\n");
        let a = choose_adapter(&mine, &block(None, None, Value::Null), Some("rust"), &finder(&["mine", "codelldb"])).unwrap();
        assert_eq!(a.name, "mine");
    }

    #[test]
    fn the_no_adapter_message_names_the_language_and_hints() {
        let c = DebugConfig::default();
        let none = block(None, None, Value::Null);
        let err = choose_adapter(&c, &none, Some("rust"), &finder(&[])).unwrap_err();
        let hints: Vec<String> = c
            .adapters()
            .iter()
            .filter(|a| a.languages.iter().any(|l| l == "rust"))
            .map(|a| format!("{} ({})", a.name, a.install_hint))
            .collect();
        assert_eq!(hints.len(), 3);
        assert_eq!(err, format!("No debug adapter found for rust. Install one of: {}", hints.join("; ")));
        let err = choose_adapter(&c, &none, Some("go"), &finder(&[])).unwrap_err();
        assert_eq!(err, "No debug adapter for go: add one as [debug.adapters.<name>] in config.toml");
        let err = choose_adapter(&c, &none, None, &finder(&[])).unwrap_err();
        assert_eq!(
            err,
            "No language for this debug configuration: open a file of its language, or set language or adapter in its debug table"
        );
        let off = cfg("[debug.adapters.lldb-dap]\ncommand = []\n");
        let err = choose_adapter(&off, &none, Some("rust"), &finder(&[])).unwrap_err();
        assert!(!err.contains("lldb-dap") && err.contains("codelldb") && err.contains("gdb"), "{err}");
        let bare = cfg("[debug.adapters.mine]\ncommand = [\"mine\"]\nlanguages = [\"go\"]\n");
        let err = choose_adapter(&bare, &none, Some("go"), &finder(&[])).unwrap_err();
        assert_eq!(err, "No debug adapter found for go. Install one of: mine");
    }

    #[test]
    fn choose_adapter_fills_the_language() {
        let c = DebugConfig::default();
        let found = finder(ALL);
        let a = choose_adapter(&c, &block(None, None, Value::Null), Some("rust"), &found).unwrap();
        assert_eq!(a.launch["sourceLanguages"], json!(["rust"]));
        let a = choose_adapter(&c, &block(Some("codelldb"), Some("c"), Value::Null), Some("rust"), &found).unwrap();
        assert_eq!(a.launch["sourceLanguages"], json!(["c"]));
        let a = choose_adapter(&c, &block(Some("codelldb"), None, Value::Null), None, &found).unwrap();
        assert!(a.launch.get("terminal").is_some() && a.launch.get("sourceLanguages").is_none(), "{}", a.launch);
        let user = cfg("[debug.adapters.gdb]\nlaunch = { x = \"${language}\", y = \"z\" }\n");
        let a = choose_adapter(&user, &block(Some("gdb"), None, Value::Null), Some("c"), &found).unwrap();
        assert_eq!(a.launch, json!({"x": "c", "y": "z"}));
    }

    fn adapter(launch: Value, exact_env: bool) -> AdapterConfig {
        AdapterConfig { name: "t".into(), command: vec!["t".into()], languages: vec![], launch, exact_env, install_hint: "" }
    }

    #[test]
    fn test_block_builds_and_passes_the_filter() {
        let t = RustTest { dir: "/w".into(), target: strings(&["--test", "it"]), filter: "integration".into(), exact: true, in_src: false };
        let b = test_block(&t);
        assert_eq!(b.build, Some(strings(&["cargo", "test", "--no-run", "--message-format=json", "--test", "it"])));
        assert_eq!((b.language.as_deref(), b.adapter.as_deref()), (Some("rust"), None));
        assert_eq!(b.launch, json!({"program": "${artifact}", "args": ["integration", "--exact", "--nocapture"]}));
        let all = DebugConfig::default().adapters();
        let gdb = all.iter().find(|a| a.name == "gdb").unwrap();
        let v = LaunchVars { artifact: Some("/t/exe".into()), ..vars() };
        let v = resolve_launch(gdb, &b, &task(None, &[]), &v, &BTreeMap::new);
        assert_eq!(v["program"], "/t/exe");
        assert_eq!(v["args"], json!(["integration", "--exact", "--nocapture"]));
        let m = RustTest { dir: "/w".into(), target: vec![], filter: "util::tests::".into(), exact: false, in_src: true };
        let b = test_block(&m);
        assert_eq!(b.build, Some(strings(&["cargo", "test", "--no-run", "--message-format=json"])));
        assert_eq!(b.launch["args"], json!(["util::tests::", "--nocapture"]));
    }

    #[test]
    fn resolve_launch_merges_later_wins() {
        let a = adapter(json!({"a": "adapter", "cwd": "adapter", "env": {"E": "adapter"}, "b": "adapter"}), false);
        let t = task(Some("sub"), &[("E", "task")]);
        let v = resolve_launch(&a, &block(None, None, json!({"b": "block"})), &t, &vars(), &no_env);
        assert_eq!(v, json!({"a": "adapter", "b": "block", "cwd": "/w/sub", "env": {"E": "task"}}));
        let v = resolve_launch(&a, &block(None, None, json!({"cwd": "/own", "env": {"Z": "1"}})), &t, &vars(), &no_env);
        assert_eq!(v["cwd"], "/own");
        assert_eq!(v["env"], json!({"Z": "1"}));
        for (cwd, want) in
            [(None, "/w"), (Some(""), "/w"), (Some("/abs"), "/abs"), (Some("${workspaceFolder}/sub"), "/w/sub"), (Some("sub"), "/w/sub")]
        {
            let v = resolve_launch(&a, &block(None, None, Value::Null), &task(cwd, &[]), &vars(), &no_env);
            assert_eq!(v["cwd"], want, "{cwd:?}");
        }
    }

    #[test]
    fn resolve_launch_replaces_variables_in_every_nested_string() {
        let a = adapter(json!({}), false);
        let launch = json!({
            "program": "${workspaceFolder}/demo",
            "args": ["${file}", {"deep": ["x${workspaceFolder}y"]}],
            "n": 3,
            "t": true,
            "${file}": "key",
            "art": "${artifact}",
        });
        let v = resolve_launch(&a, &block(None, None, launch.clone()), &task(Some("${file}"), &[]), &vars(), &no_env);
        assert_eq!(v["program"], "/w/demo");
        assert_eq!(v["args"][0], "/w/a.c");
        assert_eq!(v["args"][1]["deep"][0], "x/wy");
        assert_eq!(v["n"], 3);
        assert_eq!(v["t"], true);
        assert_eq!(v["${file}"], "key");
        assert_eq!(v["cwd"], "/w/a.c");
        assert_eq!(v["art"], "${artifact}");
        let with = LaunchVars { artifact: Some("/w/bin".into()), ..vars() };
        let v = resolve_launch(&a, &block(None, None, launch), &task(None, &[]), &with, &no_env);
        assert_eq!(v["art"], "/w/bin");
    }

    #[test]
    fn exact_env_only_for_gdb() {
        let c = DebugConfig::default();
        let found = finder(ALL);
        let own = || BTreeMap::from([("PATH".to_string(), "/bin".to_string()), ("E".to_string(), "jetl".to_string())]);
        let pick = |name: &str| choose_adapter(&c, &block(Some(name), Some("c"), Value::Null), None, &found).unwrap();
        let empty = block(None, None, Value::Null);
        let v = resolve_launch(&pick("gdb"), &empty, &task(None, &[("E", "task")]), &vars(), &own);
        assert_eq!(v["env"], json!({"PATH": "/bin", "E": "task"}));
        for name in ["lldb-dap", "codelldb"] {
            let v = resolve_launch(&pick(name), &empty, &task(None, &[("E", "task")]), &vars(), &no_env);
            assert_eq!(v["env"], json!({"E": "task"}), "{name}");
        }
        let v = resolve_launch(&pick("lldb-dap"), &empty, &task(None, &[]), &vars(), &no_env);
        assert!(v.get("env").is_none());
    }

    #[test]
    fn the_task_command_and_args_are_not_used() {
        let lldb = DebugConfig::default().adapters().into_iter().find(|a| a.name == "lldb-dap").unwrap();
        let v = resolve_launch(&lldb, &block(None, None, Value::Null), &task(None, &[]), &vars(), &no_env);
        assert_eq!(v, json!({"console": "internalConsole", "cwd": "/w"}));
    }

    #[test]
    fn the_outline_fixture_parses() {
        let text = "[[configs]]\nname = \"demo\"\ncommand = [\"./demo\"]\n\n[debug.demo]\nadapter = \"gdb\"\nlanguage = \"c\"\nlaunch = { program = \"${workspaceFolder}/demo\", args = [\"x\"] }\n";
        let c: RunConfig = toml::from_str(text).unwrap();
        assert_eq!(c.configs.len(), 1);
        let b = &c.debug["demo"];
        assert_eq!((b.adapter.as_deref(), b.language.as_deref()), (Some("gdb"), Some("c")));
        assert_eq!(b.launch, json!({"program": "${workspaceFolder}/demo", "args": ["x"]}));
        let c = Config::parse("[run.debug.demo]\nadapter = \"gdb\"\n").unwrap().run;
        assert_eq!(c.debug["demo"].adapter.as_deref(), Some("gdb"));
        assert_eq!(c.debug["demo"].launch, Value::Null);
        let c: RunConfig = toml::from_str("[debug.demo.launch]\nprogram = \"a\"\n").unwrap();
        assert_eq!(c.debug["demo"].launch, json!({"program": "a"}));
        let c: RunConfig = toml::from_str("[debug.\"web tests\"]\nadapter = \"gdb\"\n").unwrap();
        assert!(c.debug.contains_key("web tests"));
    }

    #[test]
    fn a_launch_that_is_not_a_table_is_refused() {
        let e = toml::from_str::<RunConfig>("[debug.x]\nlaunch = \"s\"\n").unwrap_err().to_string();
        assert!(e.contains("launch must be a table"), "{e}");
        let e = Config::parse("[debug.adapters.x]\nlaunch = 1\n").unwrap_err().to_string();
        assert!(e.contains("launch must be a table"), "{e}");
    }

    #[test]
    fn a_project_file_cannot_define_an_adapter() {
        let c: RunConfig = toml::from_str("[debug.adapters.evil]\ncommand = [\"evil\"]\n").unwrap();
        let b = &c.debug["adapters"];
        assert_eq!((b.adapter.as_ref(), &b.launch), (None, &Value::Null));
        let dir = std::env::temp_dir().join(format!("jetl-test-debug-evil-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".jetl")).unwrap();
        std::fs::write(dir.join(".jetl/run.toml"), "[debug.adapters.evil]\ncommand = [\"evil\"]\n").unwrap();
        let (blocks, error) = load_debug_blocks(&dir, &BTreeMap::new());
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(error.is_none());
        assert!(!blocks.contains_key("evil"));
        let known = names(&DebugConfig::default());
        assert!(blocks.keys().all(|k| !known.contains(k)));
        assert!(!names(&DebugConfig::default()).contains(&"evil".to_string()));
    }

    #[test]
    fn load_debug_blocks_project_wins_by_name() {
        let dir = std::env::temp_dir().join(format!("jetl-test-debug-blocks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let configured = BTreeMap::from([
            ("a".to_string(), block(Some("g"), Some("c"), Value::Null)),
            ("b".to_string(), block(Some("b"), None, Value::Null)),
        ]);
        let (blocks, error) = load_debug_blocks(&dir, &configured);
        assert_eq!((&blocks, &error), (&configured, &None));
        std::fs::create_dir_all(dir.join(".jetl")).unwrap();
        std::fs::write(dir.join(".jetl/run.toml"), "[debug.a]\nadapter = \"p\"\n[debug.c]\nadapter = \"pc\"\n").unwrap();
        let (blocks, error) = load_debug_blocks(&dir, &configured);
        assert!(error.is_none(), "{error:?}");
        assert_eq!(blocks["a"], block(Some("p"), None, Value::Null));
        assert_eq!(blocks["b"], configured["b"]);
        assert_eq!(blocks["c"].adapter.as_deref(), Some("pc"));
        std::fs::write(dir.join(".jetl/run.toml"), "[debug.a\n").unwrap();
        let (blocks, error) = load_debug_blocks(&dir, &configured);
        assert_eq!(blocks, configured);
        assert!(error.unwrap().starts_with(".jetl/run.toml: "));
        std::fs::write(dir.join(".jetl/run.toml"), "[debug.a]\nlaunch = \"s\"\n").unwrap();
        let (_, error) = load_debug_blocks(&dir, &configured);
        assert!(error.unwrap().contains("launch must be a table"));
        std::fs::write(dir.join(".jetl/run.toml"), "[debug]\nport = 1\n").unwrap();
        let (blocks, error) = load_debug_blocks(&dir, &configured);
        assert_eq!(blocks, configured);
        assert!(error.is_some_and(|e| e.starts_with(".jetl/run.toml: ")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_bad_task_does_not_take_the_debug_blocks() {
        let dir = std::env::temp_dir().join(format!("jetl-test-debug-bad-task-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".jetl")).unwrap();
        std::fs::write(dir.join(".jetl/run.toml"), "[[configs]]\nname = 3\n[debug.a]\nadapter = \"p\"\n").unwrap();
        let (blocks, error) = load_debug_blocks(&dir, &BTreeMap::new());
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(error.is_none(), "{error:?}");
        assert_eq!(blocks["a"].adapter.as_deref(), Some("p"));
    }

    #[test]
    fn process_env_reads_the_environment() {
        if let Ok(path) = std::env::var("PATH") {
            assert_eq!(process_env().get("PATH"), Some(&path));
        }
    }

    fn cargo_task(name: &str, command: &[&str], origin: Origin) -> RunTask {
        RunTask {
            name: name.into(),
            command: TaskCommand::Argv(command.iter().map(|s| s.to_string()).collect()),
            origin,
            ..task(None, &[])
        }
    }

    #[test]
    fn default_block_only_for_detected_cargo() {
        let get = |cmd: &[&str], origin| default_block(&cargo_task("t", cmd, origin));
        let cargo = |build: &[&str]| DebugBlock {
            adapter: None,
            language: Some("rust".into()),
            build: Some(strings(build)),
            launch: json!({"program": "${artifact}"}),
        };
        let json_build = &["cargo", "build", "--message-format=json"];
        assert_eq!(get(&["cargo", "build"], Origin::Detected), Some(cargo(json_build)));
        assert_eq!(get(&["cargo", "run"], Origin::Detected), Some(cargo(json_build)));
        assert_eq!(get(&["cargo", "test"], Origin::Detected), Some(cargo(&["cargo", "test", "--no-run", "--message-format=json"])));
        for other in [&["make"][..], &["npm", "run", "x"], &["go", "test", "./..."]] {
            assert_eq!(get(other, Origin::Detected), None, "{other:?}");
        }
        for origin in [Origin::Project, Origin::Config] {
            assert_eq!(get(&["cargo", "build"], origin), None, "{origin:?}");
        }
    }

    #[test]
    fn resolve_build_substitutes_and_uses_the_tasks_cwd() {
        let mut b = block(None, None, json!({"cwd": "/elsewhere"}));
        b.build = Some(vec!["make".into(), "-C".into(), "${workspaceFolder}/sub".into(), "F=${file}".into()]);
        let spec = resolve_build(&b, &task(Some("proj"), &[("A", "1")]), &vars()).unwrap();
        assert_eq!(spec.argv, ["make", "-C", "/w/sub", "F=/w/a.c"]);
        assert_eq!(spec.cwd, Path::new("/w/proj"), "the task's cwd, not the launch's");
        assert_eq!(spec.env, [("A".to_string(), "1".to_string())]);
        assert_eq!(resolve_build(&b, &task(None, &[]), &vars()).unwrap().cwd, Path::new("/w"));
        // An absolute cwd replaces the root, as for launch.
        assert_eq!(resolve_build(&b, &task(Some("/abs"), &[]), &vars()).unwrap().cwd, Path::new("/abs"));
        for none in [None, Some(vec![]), Some(vec![String::new()]), Some(vec![String::new(), "x".into()])] {
            b.build = none.clone();
            assert!(resolve_build(&b, &task(None, &[]), &vars()).is_none(), "{none:?}");
        }
        // `${artifact}` in a build word stays as written.
        b.build = Some(vec!["echo".into(), "${artifact}".into()]);
        assert_eq!(resolve_build(&b, &task(None, &[]), &vars()).unwrap().argv, ["echo", "${artifact}"]);
    }

    #[test]
    fn artifact_is_found_and_filled_anywhere() {
        let mut v = json!({"program": "${artifact}", "args": ["-x", {"deep": "a${artifact}b"}], "n": 3});
        assert!(needs_artifact(&v));
        assert!(!needs_artifact(&json!({"program": "x", "args": ["${file}"]})));
        fill_artifact(&mut v, "/t/prog");
        assert_eq!(v, json!({"program": "/t/prog", "args": ["-x", {"deep": "a/t/progb"}], "n": 3}));
        assert!(!needs_artifact(&v));
    }

    #[test]
    fn a_build_that_is_not_a_list_is_refused() {
        let dir = std::env::temp_dir().join(format!("jetl-test-debug-build-type-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".jetl")).unwrap();
        std::fs::write(dir.join(".jetl/run.toml"), "[debug.a]\nbuild = \"make\"\n").unwrap();
        let (blocks, error) = load_debug_blocks(&dir, &BTreeMap::new());
        assert!(blocks.is_empty());
        assert!(error.is_some_and(|e| e.starts_with(".jetl/run.toml: ")));
        std::fs::write(dir.join(".jetl/run.toml"), "[debug.a]\nbuild = [\"make\", \"x\"]\n").unwrap();
        let (blocks, error) = load_debug_blocks(&dir, &BTreeMap::new());
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(error.is_none(), "{error:?}");
        assert_eq!(blocks["a"].build, Some(vec!["make".to_string(), "x".to_string()]));
    }
}
