//! Run Test at Caret: find the test (or block of tests) around a byte of a
//! parsed file, and build the command that runs only it. Pure: the app
//! supplies the tree, the rope and the paths.

use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use ropey::Rope;
use tree_sitter::{Node, Tree};

/// A test (or block of tests) found around the caret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Enclosing modules / classes / describe blocks, outermost first, then the test.
    pub names: Vec<String>,
    /// A single test; false for a module, class or describe block.
    pub leaf: bool,
}

static RUST_TEST_ATTR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^#\[\s*(?:\w+::)*test\b").unwrap());

fn text(node: Node, rope: &Rope) -> String {
    rope.byte_slice(node.start_byte()..node.end_byte()).to_string()
}

fn field_text(node: Node, field: &str, rope: &Rope) -> Option<String> {
    node.child_by_field_name(field).map(|n| text(n, rope))
}

/// The node at `byte`; a caret in the indentation of a line counts as on its first character.
fn start_node<'a>(tree: &'a Tree, rope: &Rope, byte: usize) -> Option<Node<'a>> {
    let char = rope.byte_to_char(byte.min(rope.len_bytes()));
    let line = rope.char_to_line(char);
    let first = rope.line_to_char(line);
    let blank_before = rope.slice(first..char).chars().all(|c| c == ' ' || c == '\t');
    let indent = rope.slice(char..).chars().take_while(|c| *c == ' ' || *c == '\t').count();
    let char = if blank_before && char + indent < rope.len_chars() && !matches!(rope.char(char + indent), '\n' | '\r') {
        char + indent
    } else {
        char
    };
    let byte = rope.char_to_byte(char);
    tree.root_node().descendant_for_byte_range(byte, byte)
}

fn is_comment(n: Node) -> bool {
    matches!(n.kind(), "line_comment" | "block_comment")
}

/// `function_item` with a `#[test]`-like attribute among the attributes and comments right before it.
fn is_rust_test(f: Node, rope: &Rope) -> bool {
    let mut prev = f.prev_named_sibling();
    while let Some(p) = prev {
        if p.kind() == "attribute_item" {
            if RUST_TEST_ATTR.is_match(&text(p, rope)) {
                return true;
            }
        } else if !is_comment(p) {
            return false;
        }
        prev = p.prev_named_sibling();
    }
    false
}

/// The names of the inline modules around `node`, outermost first.
fn rust_mods(node: Node, rope: &Rope) -> Vec<String> {
    let mut mods = Vec::new();
    let mut cur = node.parent();
    while let Some(n) = cur {
        if n.kind() == "mod_item"
            && let Some(name) = field_text(n, "name", rope)
        {
            mods.push(name);
        }
        cur = n.parent();
    }
    mods.reverse();
    mods
}

pub fn rust_test_at(tree: &Tree, rope: &Rope, byte: usize) -> Option<Found> {
    let mut module: Option<Node> = None;
    let mut cur = start_node(tree, rope, byte);
    while let Some(n) = cur {
        match n.kind() {
            "function_item" if is_rust_test(n, rope) => return Some(rust_fn_found(n, rope)),
            "attribute_item" => {
                // The caret is on an attribute line: the test is the function that follows.
                let mut next = n.next_named_sibling();
                while let Some(s) = next.filter(|s| s.kind() == "attribute_item" || is_comment(*s)) {
                    next = s.next_named_sibling();
                }
                if let Some(f) = next.filter(|f| f.kind() == "function_item" && is_rust_test(*f, rope)) {
                    return Some(rust_fn_found(f, rope));
                }
            }
            // `mod x;` has no body: nothing to run inline.
            "mod_item" if module.is_none() && n.child_by_field_name("body").is_some() => module = Some(n),
            _ => {}
        }
        cur = n.parent();
    }
    let m = module?;
    let mut names = rust_mods(m, rope);
    names.push(field_text(m, "name", rope)?);
    Some(Found { names, leaf: false })
}

fn rust_fn_found(f: Node, rope: &Rope) -> Found {
    let mut names = rust_mods(f, rope);
    names.extend(field_text(f, "name", rope));
    Found { names, leaf: true }
}

/// Names of the `class_definition`s around `node`, outermost first.
fn python_classes(node: Node, rope: &Rope) -> Vec<String> {
    let mut names = Vec::new();
    let mut cur = node.parent();
    while let Some(n) = cur {
        if n.kind() == "class_definition" {
            names.extend(field_text(n, "name", rope));
        }
        cur = n.parent();
    }
    names.reverse();
    names
}

pub fn python_test_at(tree: &Tree, rope: &Rope, byte: usize) -> Option<Found> {
    let mut class: Option<Node> = None;
    let mut cur = start_node(tree, rope, byte);
    while let Some(n) = cur {
        // A caret on a decorator line belongs to the function it decorates.
        let func = match n.kind() {
            "function_definition" => Some(n),
            "decorated_definition" => n.child_by_field_name("definition").filter(|d| d.kind() == "function_definition"),
            _ => None,
        };
        if let Some(f) = func
            && let Some(name) = field_text(f, "name", rope).filter(|s| s.starts_with("test"))
        {
            let mut names = python_classes(f, rope);
            names.push(name);
            return Some(Found { names, leaf: true });
        }
        match n.kind() {
            "class_definition" if class.is_none() && field_text(n, "name", rope).is_some_and(|s| s.starts_with("Test")) => class = Some(n),
            _ => {}
        }
        cur = n.parent();
    }
    let c = class?;
    let mut names = python_classes(c, rope);
    names.extend(field_text(c, "name", rope));
    Some(Found { names, leaf: false })
}

/// `it('x', ..)`, `test.only(`x`, ..)`, `describe('x', ..)`: (is a test, the title).
fn js_call(n: Node, rope: &Rope) -> Option<(bool, String)> {
    if n.kind() != "call_expression" {
        return None;
    }
    let f = n.child_by_field_name("function")?;
    let name = match f.kind() {
        "identifier" => text(f, rope),
        "member_expression" => {
            let prop = field_text(f, "property", rope)?;
            if !matches!(prop.as_str(), "only" | "skip" | "concurrent") {
                return None;
            }
            let obj = f.child_by_field_name("object").filter(|o| o.kind() == "identifier")?;
            text(obj, rope)
        }
        _ => return None,
    };
    let leaf = match name.as_str() {
        "it" | "test" => true,
        "describe" => false,
        _ => return None,
    };
    let first = n.child_by_field_name("arguments")?.named_child(0)?;
    let ok = match first.kind() {
        "string" => true,
        "template_string" => {
            let mut c = first.walk();
            !first.named_children(&mut c).any(|k| k.kind() == "template_substitution")
        }
        _ => false,
    };
    if !ok {
        return None;
    }
    let raw = text(first, rope);
    let inner = raw.get(1..raw.len().saturating_sub(1)).unwrap_or("");
    Some((leaf, inner.to_string()))
}

pub fn js_test_at(tree: &Tree, rope: &Rope, byte: usize) -> Option<Found> {
    let mut found: Vec<(bool, String)> = Vec::new();
    let mut cur = start_node(tree, rope, byte);
    while let Some(n) = cur {
        found.extend(js_call(n, rope));
        cur = n.parent();
    }
    let leaf = found.first()?.0;
    found.reverse();
    Some(Found { names: found.into_iter().map(|(_, t)| t).collect(), leaf })
}

/// The nearest ancestor directory of `file` (up to `root`) that holds `marker`.
fn project_dir(file: &Path, root: &Path, marker: &str) -> Option<PathBuf> {
    file.ancestors().skip(1).take_while(|d| d.starts_with(root)).find(|d| d.join(marker).is_file()).map(Path::to_path_buf)
}

fn words(w: &[&str]) -> Vec<String> {
    w.iter().map(|s| s.to_string()).collect()
}

/// The Rust test (or module of tests) around the caret, as cargo selects it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustTest {
    /// The package directory: the nearest `Cargo.toml` above the file.
    pub dir: PathBuf,
    /// `["--bin", x]`, `["--test", x]` or empty.
    pub target: Vec<String>,
    /// The module path joined by `::`, with a trailing `::` for a module.
    pub filter: String,
    /// True only for a leaf whose path is known: libtest matches the whole name.
    pub exact: bool,
    /// True only for a file under `src/` outside `src/bin/`: no target flag is
    /// given, so only the library's and binaries' test executables can hold it.
    pub in_src: bool,
}

impl RustTest {
    /// `cargo test [target] <filter> [-- --exact]`.
    pub fn command(&self) -> Vec<String> {
        let mut argv = words(&["cargo", "test"]);
        argv.extend(self.target.iter().cloned());
        argv.push(self.filter.clone());
        if self.exact {
            argv.extend(words(&["--", "--exact"]));
        }
        argv
    }
}

/// `cargo test` for `found`, run in the directory of the nearest `Cargo.toml` above `file`.
pub fn rust_command(file: &Path, root: &Path, found: &Found) -> (PathBuf, Vec<String>) {
    let t = rust_test(file, root, found);
    (t.dir.clone(), t.command())
}

/// The test `found` in `file`, located in its package.
pub fn rust_test(file: &Path, root: &Path, found: &Found) -> RustTest {
    let dir = project_dir(file, root, "Cargo.toml").unwrap_or_else(|| root.to_path_buf());
    let rel: Vec<String> = file
        .strip_prefix(&dir)
        .map(|p| p.components().filter_map(|c| if let Component::Normal(s) = c { s.to_str().map(String::from) } else { None }).collect())
        .unwrap_or_default();
    let stem = |s: &str| s.strip_suffix(".rs").unwrap_or(s).to_string();
    // (target flags, module path of the file, whether the path is exact)
    let mut in_src = false;
    let (target, mut path, exact): (Vec<String>, Vec<String>, bool) = match rel.first().map(String::as_str) {
        Some("src") if rel.len() >= 2 => {
            let rest = &rel[1..];
            if rest[0] == "bin" && rest.len() >= 2 {
                // `src/bin/x.rs` and `src/bin/x/main.rs` are the crate root.
                let mut path: Vec<String> = rest[2..].iter().map(|s| stem(s)).collect();
                if rest.len() == 2 || path == ["main"] {
                    path.clear();
                }
                if path.last().is_some_and(|l| l == "mod") {
                    path.pop();
                }
                (words(&["--bin", &stem(&rest[1])]), path, true)
            } else {
                let mut path: Vec<String> = rest.iter().map(|s| stem(s)).collect();
                if rest.len() == 1 && matches!(path[0].as_str(), "lib" | "main") {
                    path.clear();
                }
                if path.last().is_some_and(|l| l == "mod") {
                    path.pop();
                }
                in_src = true;
                (vec![], path, true)
            }
        }
        Some("tests") if rel.len() >= 2 => {
            let mut path: Vec<String> = rel[2..].iter().map(|s| stem(s)).collect();
            if path.last().is_some_and(|l| l == "mod") {
                path.pop();
            }
            (words(&["--test", &stem(&rel[1])]), path, true)
        }
        _ => (vec![], vec![], false),
    };
    path.extend(found.names.iter().cloned());
    let mut filter = path.join("::");
    if !found.leaf {
        filter.push_str("::");
    }
    RustTest { dir, target, filter, exact: found.leaf && exact, in_src }
}

/// `pytest -v <file>::<Class>::<test>` in the project root.
pub fn python_command(file: &Path, root: &Path, found: &Found) -> (PathBuf, Vec<String>) {
    let rel = file.strip_prefix(root).unwrap_or(file);
    let rel = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/");
    let id = std::iter::once(rel).chain(found.names.iter().cloned()).collect::<Vec<_>>().join("::");
    (root.to_path_buf(), vec!["pytest".into(), "-v".into(), id])
}

/// `vitest` or `jest` (whichever the nearest `package.json` lists) for `found`.
pub fn js_command(file: &Path, root: &Path, found: &Found) -> Result<(PathBuf, Vec<String>), String> {
    const NONE: &str = "No vitest or jest in package.json";
    let dir = project_dir(file, root, "package.json").ok_or(NONE)?;
    let json: serde_json::Value =
        std::fs::read_to_string(dir.join("package.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).ok_or(NONE)?;
    let has = |name: &str| ["dependencies", "devDependencies"].iter().any(|k| json.get(k).and_then(|d| d.get(name)).is_some());
    let rel = file.strip_prefix(&dir).unwrap_or(file);
    let rel = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/");
    let mut pattern = format!("^{}", regex::escape(&found.names.join(" ")));
    if found.leaf {
        pattern.push('$');
    }
    let argv = if has("vitest") {
        vec!["npx".into(), "vitest".into(), "run".into(), rel, "-t".into(), pattern]
    } else if has("jest") {
        vec!["npx".into(), "jest".into(), rel, "-t".into(), pattern]
    } else {
        return Err(NONE.into());
    };
    Ok((dir, argv))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(feature = "lang-rust", feature = "lang-python", feature = "lang-js", feature = "lang-ts"))]
    fn parse(lang: &str, src: &str) -> (Tree, Rope) {
        let def = crate::editor::syntax::language_for_name(lang).unwrap();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&def.language).unwrap();
        (parser.parse(src, None).unwrap(), Rope::from_str(src))
    }

    /// Byte offset of the `n`th (0-based) `needle` in `src`, plus `plus`.
    #[cfg(any(feature = "lang-rust", feature = "lang-python", feature = "lang-js", feature = "lang-ts"))]
    fn at(src: &str, needle: &str, plus: usize) -> usize {
        src.find(needle).unwrap() + plus
    }

    fn found(names: &[&str], leaf: bool) -> Found {
        Found { names: names.iter().map(|s| s.to_string()).collect(), leaf }
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn rust_test_fn_mod_and_attribute_line() {
        let src = "fn plain() {}\nmod tests {\n    mod inner {\n        #[tokio::test(flavor = \"multi_thread\")]\n        async fn deep() { assert!(true); }\n    }\n    #[test]\n    fn top() {}\n    fn helper() {}\n}\n";
        let (tree, rope) = parse("rust", src);
        let f = |n, p| rust_test_at(&tree, &rope, at(src, n, p));
        assert_eq!(f("assert!", 2), Some(found(&["tests", "inner", "deep"], true)));
        assert_eq!(f("#[test]", 2), Some(found(&["tests", "top"], true)));
        assert_eq!(f("#[test]", 0), Some(found(&["tests", "top"], true)), "first column");
        assert_eq!(rust_test_at(&tree, &rope, at(src, "    #[test]", 0)), Some(found(&["tests", "top"], true)), "indentation");
        assert_eq!(f("flavor", 0), Some(found(&["tests", "inner", "deep"], true)), "attribute argument");
        assert_eq!(f("fn helper", 4), Some(found(&["tests"], false)));
        assert_eq!(f("fn plain", 4), None);
    }

    #[test]
    fn rust_command_paths() {
        let root = std::env::temp_dir().join(format!("jetl-caret-rust-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["", "crates/core"] {
            std::fs::create_dir_all(root.join(d).join("src")).unwrap();
            std::fs::write(root.join(d).join("Cargo.toml"), "[package]\n").unwrap();
        }
        let top = found(&["tests", "top"], true);
        let cmd = |f: &str, found: &Found| rust_command(&root.join(f), &root, found);
        let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(cmd("src/lib.rs", &top), (root.clone(), words(&["cargo", "test", "tests::top", "--", "--exact"])));
        assert_eq!(cmd("src/run/matcher.rs", &top).1, words(&["cargo", "test", "run::matcher::tests::top", "--", "--exact"]));
        assert_eq!(cmd("src/a/mod.rs", &top).1, words(&["cargo", "test", "a::tests::top", "--", "--exact"]));
        assert_eq!(cmd("src/main.rs", &top).1, words(&["cargo", "test", "tests::top", "--", "--exact"]));
        assert_eq!(
            cmd("tests/cli.rs", &found(&["outside"], true)).1,
            words(&["cargo", "test", "--test", "cli", "outside", "--", "--exact"])
        );
        assert_eq!(cmd("src/bin/tool.rs", &top).1, words(&["cargo", "test", "--bin", "tool", "tests::top", "--", "--exact"]));
        assert_eq!(cmd("src/run/matcher.rs", &found(&["tests"], false)).1, words(&["cargo", "test", "run::matcher::tests::"]));
        assert_eq!(cmd("benches/b.rs", &top).1, words(&["cargo", "test", "tests::top"]), "outside src and tests: no --exact");
        let (dir, argv) = cmd("crates/core/src/lib.rs", &top);
        assert_eq!((dir, argv), (root.join("crates/core"), words(&["cargo", "test", "tests::top", "--", "--exact"])));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rust_test_fields() {
        let root = std::env::temp_dir().join(format!("jetl-caret-rust-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["", "crates/core"] {
            std::fs::create_dir_all(root.join(d).join("src")).unwrap();
            std::fs::write(root.join(d).join("Cargo.toml"), "[package]\n").unwrap();
        }
        let top = found(&["tests", "top"], true);
        let t = |f: &str, found: &Found| rust_test(&root.join(f), &root, found);
        let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let m = t("src/run/matcher.rs", &top);
        assert_eq!((m.filter.as_str(), m.exact, m.in_src, m.target.is_empty()), ("run::matcher::tests::top", true, true, true));
        assert_eq!(m.dir, root);
        let m = t("src/main.rs", &top);
        assert_eq!((m.filter.as_str(), m.in_src), ("tests::top", true));
        let c = t("tests/cli.rs", &found(&["outside"], true));
        assert_eq!((c.target, c.in_src, c.exact, c.filter.as_str()), (words(&["--test", "cli"]), false, true, "outside"));
        let b = t("src/bin/tool.rs", &top);
        assert_eq!((b.target, b.in_src), (words(&["--bin", "tool"]), false));
        let m = t("src/run/matcher.rs", &found(&["tests"], false));
        assert_eq!((m.filter.as_str(), m.exact), ("run::matcher::tests::", false));
        let b = t("benches/b.rs", &top);
        assert_eq!((b.exact, b.in_src, b.target.is_empty()), (false, false, true));
        assert_eq!(t("crates/core/src/lib.rs", &top).dir, root.join("crates/core"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(feature = "lang-python")]
    #[test]
    fn python_test_in_class_and_decorated() {
        let src = "class TestMath:\n    value = 1\n    @pytest.mark.slow\n    def test_fail(self):\n        assert 1 == 2\n\ndef helper():\n    pass\n";
        let (tree, rope) = parse("python", src);
        let f = |n, p| python_test_at(&tree, &rope, at(src, n, p));
        let want = found(&["TestMath", "test_fail"], true);
        assert_eq!(f("assert", 0), Some(want.clone()));
        assert_eq!(f("@pytest", 2), Some(want.clone()), "decorator line");
        assert_eq!(f("value", 0), Some(found(&["TestMath"], false)));
        assert_eq!(f("pass", 0), None);
        let root = PathBuf::from("/p");
        assert_eq!(
            python_command(&root.join("tests/test_a.py"), &root, &want),
            (root, vec!["pytest".to_string(), "-v".into(), "tests/test_a.py::TestMath::test_fail".into()])
        );
    }

    #[cfg(any(feature = "lang-js", feature = "lang-ts"))]
    fn check_js(lang: &str) {
        let src = "describe('math', () => {\n  describe(`nested`, () => {\n    it.only('fails (x)', () => {\n      expect(1).toBe(2)\n    })\n  })\n  const a = 1\n})\n";
        let (tree, rope) = parse(lang, src);
        let f = |n, p| js_test_at(&tree, &rope, at(src, n, p));
        let got = f("expect", 0).unwrap();
        assert_eq!(got, found(&["math", "nested", "fails (x)"], true), "{lang}");
        assert_eq!(f("it.only", 0), Some(got), "{lang}: caret on the call name");
        assert_eq!(f("const a", 0), Some(found(&["math"], false)), "{lang}");
        assert_eq!(f("`nested", 0), Some(found(&["math", "nested"], false)), "{lang}");
        assert_eq!(js_test_at(&tree, &rope, 0).map(|f| f.names), Some(vec!["math".to_string()]));
        let (tree, rope) = parse(lang, "it(`a ${b}`, () => { x() })\nit(name, () => { y() })\nfoo('z', () => { w() })\n");
        for needle in ["x()", "y()", "w()"] {
            let src = rope.to_string();
            assert_eq!(js_test_at(&tree, &rope, src.find(needle).unwrap()), None, "{lang}: {needle}");
        }
    }

    #[cfg(feature = "lang-js")]
    #[test]
    fn js_describe_and_it_javascript() {
        check_js("javascript");
    }

    #[cfg(feature = "lang-ts")]
    #[test]
    fn js_describe_and_it_typescript() {
        check_js("typescript");
        check_js("tsx");
    }

    #[test]
    fn js_pattern_and_runner_from_package_json() {
        let root = std::env::temp_dir().join(format!("jetl-caret-js-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let pkg = |name: &str, json: &str| {
            std::fs::create_dir_all(root.join(name).join("src")).unwrap();
            std::fs::write(root.join(name).join("package.json"), json).unwrap();
            root.join(name).join("src/math.test.ts")
        };
        let vitest = pkg("v", r#"{"devDependencies": {"vitest": "^1"}}"#);
        let jest = pkg("j", r#"{"dependencies": {"jest": "^29"}}"#);
        let none = pkg("n", r#"{"dependencies": {"left-pad": "1"}}"#);
        let test = found(&["math", "nested", "fails (x)"], true);
        let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            js_command(&vitest, &root, &test),
            Ok((root.join("v"), words(&["npx", "vitest", "run", "src/math.test.ts", "-t", r"^math nested fails \(x\)$"])))
        );
        assert_eq!(
            js_command(&jest, &root, &found(&["math"], false)).unwrap().1,
            words(&["npx", "jest", "src/math.test.ts", "-t", "^math"])
        );
        assert_eq!(js_command(&none, &root, &test), Err("No vitest or jest in package.json".to_string()));
        let (_, v) = js_command(&vitest, &root, &test).unwrap();
        let (_, j) = js_command(&jest, &root, &test).unwrap();
        let task = |argv: Vec<String>| crate::run::RunTask {
            name: "t".into(),
            command: crate::run::TaskCommand::Argv(argv),
            cwd: None,
            env: Default::default(),
            args: vec![],
            matcher: None,
            origin: Default::default(),
        };
        assert_eq!(crate::run::testing::Runner::detect(&task(v)), Some(crate::run::testing::Runner::Vitest));
        assert_eq!(crate::run::testing::Runner::detect(&task(j)), Some(crate::run::testing::Runner::Jest));
        let _ = std::fs::remove_dir_all(&root);
    }
}
