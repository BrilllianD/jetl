//! Tree-sitter based highlighting. Each buffer with a known language owns a
//! parser and tree; edits are fed incrementally and the visible byte range
//! is queried on render.

use std::ops::Range;
use std::path::Path;
use std::sync::LazyLock;

use ratatui::style::Style;
use ropey::Rope;
use tree_sitter::{InputEdit, Language, Node, Parser, Point, Query, QueryCursor, StreamingIterator, TextProvider, Tree};

use crate::config::theme::Theme;

pub struct LanguageDef {
    pub name: &'static str,
    pub extensions: &'static [&'static str],
    pub filenames: &'static [&'static str],
    pub language: Language,
    pub query: Query,
    /// Line comment token for "comment with line comment".
    pub line_comment: Option<&'static str>,
    /// Block comment delimiters for "comment with block comment".
    pub block_comment: Option<(&'static str, &'static str)>,
    /// Enter after a trailing `:` indents one level (Python, YAML).
    pub indent_after_colon: bool,
    /// Whether every query part compiled (no fallback to a partial set).
    #[cfg(test)]
    full_query: bool,
}

impl std::fmt::Debug for LanguageDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name)
    }
}

fn make(name: &'static str, extensions: &'static [&'static str], filenames: &'static [&'static str], language: Language, queries: &[&str], line_comment: Option<&'static str>) -> Option<LanguageDef> {
    // Try the full set first; some grammars ship queries that reference
    // nodes only present in a sibling grammar, so fall back progressively.
    for n in (1..=queries.len()).rev() {
        let src: String = queries[..n].join("\n");
        match Query::new(&language, &src) {
            Ok(query) => {
                return Some(LanguageDef {
                    name,
                    extensions,
                    filenames,
                    language: language.clone(),
                    query,
                    line_comment,
                    block_comment: block_comment(name),
                    indent_after_colon: matches!(name, "python" | "yaml"),
                    #[cfg(test)]
                    full_query: n == queries.len(),
                })
            }
            Err(e) => tracing::warn!("highlight query for {name} ({n} parts): {e}"),
        }
    }
    None
}

fn block_comment(name: &str) -> Option<(&'static str, &'static str)> {
    match name {
        "rust" | "javascript" | "typescript" | "tsx" | "csharp" | "css" | "scss" | "go" | "c" | "cpp" | "java" | "php" => Some(("/*", "*/")),
        "lua" => Some(("--[[", "]]")),
        "html" | "xml" | "markdown" => Some(("<!--", "-->")),
        _ => None,
    }
}

/// Prepended to the C and C++ queries: upstream captures a called struct
/// field as `@property` before `@function`, primitive types only as
/// `@type` (which Darcula leaves unstyled), and booleans not at all.
#[cfg(any(feature = "lang-c", feature = "lang-cpp"))]
const C_QUERY_FIXES: &str = "(call_expression function: (field_expression field: (field_identifier) @function)) [(primitive_type) (sized_type_specifier)] @type.builtin [(true) (false)] @boolean";

/// Prepended to the C++ query: upstream finds function names only one
/// scope deep (`A::run`, not `ns::A::run`) and leaves `auto` as `@type`.
#[cfg(feature = "lang-cpp")]
const CPP_QUERY_FIXES: &str = "(function_declarator declarator: (qualified_identifier name: (qualified_identifier name: (identifier) @function))) (call_expression function: (qualified_identifier name: (qualified_identifier name: (identifier) @function))) (auto) @type.builtin";

pub static LANGUAGES: LazyLock<Vec<LanguageDef>> = LazyLock::new(|| {
    let mut v = Vec::new();
    #[cfg(feature = "lang-rust")]
    // Upstream captures numbers as `constant.builtin` (like `true`); Darcula
    // colors them differently, and the first pattern for a node wins.
    v.extend(make("rust", &["rs"], &[], tree_sitter_rust::LANGUAGE.into(), &["[(integer_literal) (float_literal)] @number", tree_sitter_rust::HIGHLIGHTS_QUERY], Some("//")));
    #[cfg(feature = "lang-js")]
    v.extend(make("javascript", &["js", "mjs", "cjs", "jsx"], &[], tree_sitter_javascript::LANGUAGE.into(), &[tree_sitter_javascript::HIGHLIGHT_QUERY], Some("//")));
    #[cfg(feature = "lang-ts")]
    {
        #[cfg(feature = "lang-js")]
        let js = tree_sitter_javascript::HIGHLIGHT_QUERY;
        #[cfg(not(feature = "lang-js"))]
        let js = "";
        v.extend(make("typescript", &["ts", "mts", "cts"], &[], tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), &[tree_sitter_typescript::HIGHLIGHTS_QUERY, js], Some("//")));
        v.extend(make("tsx", &["tsx"], &[], tree_sitter_typescript::LANGUAGE_TSX.into(), &[tree_sitter_typescript::HIGHLIGHTS_QUERY, js], Some("//")));
    }
    #[cfg(feature = "lang-python")]
    v.extend(make("python", &["py", "pyi"], &[], tree_sitter_python::LANGUAGE.into(), &[tree_sitter_python::HIGHLIGHTS_QUERY], Some("#")));
    #[cfg(feature = "lang-json")]
    v.extend(make("json", &["json", "jsonc", "json5"], &[], tree_sitter_json::LANGUAGE.into(), &[tree_sitter_json::HIGHLIGHTS_QUERY], None));
    #[cfg(feature = "lang-toml")]
    v.extend(make("toml", &["toml"], &["Cargo.lock"], tree_sitter_toml_ng::LANGUAGE.into(), &[tree_sitter_toml_ng::HIGHLIGHTS_QUERY], Some("#")));
    #[cfg(feature = "lang-bash")]
    v.extend(make("bash", &["sh", "bash", "zsh"], &[".bashrc", ".zshrc", ".profile", ".bash_profile", "PKGBUILD"], tree_sitter_bash::LANGUAGE.into(), &[tree_sitter_bash::HIGHLIGHT_QUERY], Some("#")));
    #[cfg(feature = "lang-md")]
    v.extend(make("markdown", &["md", "markdown"], &[], tree_sitter_md::LANGUAGE.into(), &[tree_sitter_md::HIGHLIGHT_QUERY_BLOCK], None));
    #[cfg(feature = "lang-csharp")]
    v.extend(make("csharp", &["cs", "csx"], &[], tree_sitter_c_sharp::LANGUAGE.into(), &[tree_sitter_c_sharp::HIGHLIGHTS_QUERY], Some("//")));
    #[cfg(feature = "lang-yaml")]
    // Upstream captures mapping keys as `@string` before `@property`, and
    // the first pattern for a node wins; lift the key patterns to the front.
    v.extend(make("yaml", &["yml", "yaml"], &[], tree_sitter_yaml::LANGUAGE.into(), &["(_ key: (flow_node [(double_quote_scalar) (single_quote_scalar)] @property)) (_ key: (flow_node (plain_scalar (string_scalar) @property)))", tree_sitter_yaml::HIGHLIGHTS_QUERY], Some("#")));
    // Vue single-file components get HTML highlighting until injections
    // exist; the Vue grammar crate is unmaintained.
    #[cfg(feature = "lang-html")]
    v.extend(make("html", &["html", "htm", "xhtml", "vue"], &[], tree_sitter_html::LANGUAGE.into(), &[tree_sitter_html::HIGHLIGHTS_QUERY], None));
    #[cfg(feature = "lang-css")]
    v.extend(make("css", &["css"], &[], tree_sitter_css::LANGUAGE.into(), &[tree_sitter_css::HIGHLIGHTS_QUERY], None));
    #[cfg(feature = "lang-scss")]
    {
        // The SCSS query only covers what it adds to CSS (no strings, no
        // selectors); the CSS query fills in the rest.
        #[cfg(feature = "lang-css")]
        let css = tree_sitter_css::HIGHLIGHTS_QUERY;
        #[cfg(not(feature = "lang-css"))]
        let css = "";
        v.extend(make("scss", &["scss"], &[], tree_sitter_scss::language(), &[tree_sitter_scss::HIGHLIGHTS_QUERY, css], Some("//")));
    }
    #[cfg(feature = "lang-xml")]
    v.extend(make("xml", &["xml", "xsd", "xsl", "xslt", "svg", "csproj", "fsproj", "vbproj", "props", "targets", "plist", "resx", "xaml"], &[], tree_sitter_xml::LANGUAGE_XML.into(), &[tree_sitter_xml::XML_HIGHLIGHT_QUERY], None));
    #[cfg(feature = "lang-go")]
    // Upstream lists builtin calls after plain calls, and the first pattern
    // for a node wins; lift them to the front (Lua too).
    v.extend(make("go", &["go"], &[], tree_sitter_go::LANGUAGE.into(), &["(call_expression function: (identifier) @function.builtin (#match? @function.builtin \"^(append|cap|clear|close|complex|copy|delete|imag|len|make|max|min|new|panic|print|println|real|recover)$\"))", tree_sitter_go::HIGHLIGHTS_QUERY], Some("//")));
    #[cfg(feature = "lang-c")]
    v.extend(make("c", &["c", "h"], &[], tree_sitter_c::LANGUAGE.into(), &[C_QUERY_FIXES, tree_sitter_c::HIGHLIGHT_QUERY], Some("//")));
    // The C++ query only covers what C++ adds to C (no comments, numbers or
    // calls); the C query fills in the rest.
    #[cfg(feature = "lang-cpp")]
    v.extend(make("cpp", &["cpp", "cc", "cxx", "hpp", "hh"], &[], tree_sitter_cpp::LANGUAGE.into(), &[C_QUERY_FIXES, CPP_QUERY_FIXES, tree_sitter_cpp::HIGHLIGHT_QUERY, tree_sitter_c::HIGHLIGHT_QUERY], Some("//")));
    #[cfg(feature = "lang-java")]
    v.extend(make("java", &["java"], &[], tree_sitter_java::LANGUAGE.into(), &[tree_sitter_java::HIGHLIGHTS_QUERY], Some("//")));
    #[cfg(feature = "lang-php")]
    v.extend(make("php", &["php"], &[], tree_sitter_php::LANGUAGE_PHP.into(), &[tree_sitter_php::HIGHLIGHTS_QUERY], Some("//")));
    #[cfg(feature = "lang-lua")]
    v.extend(make("lua", &["lua"], &[], tree_sitter_lua::LANGUAGE.into(), &["(function_call name: (identifier) @function.builtin (#any-of? @function.builtin \"assert\" \"collectgarbage\" \"dofile\" \"error\" \"getfenv\" \"getmetatable\" \"ipairs\" \"load\" \"loadfile\" \"loadstring\" \"module\" \"next\" \"pairs\" \"pcall\" \"print\" \"rawequal\" \"rawget\" \"rawset\" \"require\" \"select\" \"setfenv\" \"setmetatable\" \"tonumber\" \"tostring\" \"type\" \"unpack\" \"xpcall\"))", tree_sitter_lua::HIGHLIGHTS_QUERY], Some("--")));
    // tree-sitter-containerfile is the maintained fork of the Dockerfile
    // grammar (the tree-sitter-dockerfile crate targets tree-sitter 0.20).
    #[cfg(feature = "lang-dockerfile")]
    v.extend(make("dockerfile", &["dockerfile"], &["Dockerfile", "Containerfile"], tree_sitter_containerfile::LANGUAGE.into(), &[tree_sitter_containerfile::HIGHLIGHTS_QUERY], Some("#")));
    v
});

pub fn detect(path: &Path) -> Option<&'static LanguageDef> {
    let name = path.file_name()?.to_str()?;
    let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
    LANGUAGES.iter().find(|l| l.filenames.contains(&name) || ext.as_deref().map(|e| l.extensions.contains(&e)).unwrap_or(false))
}

pub fn language_for_name(name: &str) -> Option<&'static LanguageDef> {
    LANGUAGES.iter().find(|l| l.name == name)
}

/// [`detect`], else the shebang on the first line, else a vim or emacs
/// modeline in the first or last 5 lines. `text` is the file's text (only
/// its ends are read) or at least its first and last few lines. A known
/// extension or filename always wins.
pub fn detect_text(path: &Path, text: &str) -> Option<&'static LanguageDef> {
    if let Some(def) = detect(path) {
        return Some(def);
    }
    if let Some(def) = text.lines().next().and_then(shebang) {
        return Some(def);
    }
    text.lines().take(5).chain(text.lines().rev().take(5)).find_map(modeline)
}

/// The language a `#!` line runs: the interpreter after `env`, its flags
/// and variable assignments, without a version suffix (`python3.12`).
fn shebang(line: &str) -> Option<&'static LanguageDef> {
    fn base(w: &str) -> &str {
        w.rsplit('/').next().unwrap_or(w)
    }
    let mut words = line.strip_prefix("#!")?.split_whitespace();
    let mut prog = base(words.next()?);
    if prog == "env" {
        // `env -S deno run` splits the rest itself; flags and NAME=value
        // pairs come before the command.
        prog = base(words.find(|w| !w.starts_with('-') && !w.contains('='))?);
    }
    let name = prog.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    let lang = match name {
        "sh" | "bash" | "zsh" | "dash" => "bash",
        n if n.starts_with("python") || n == "pypy" => "python",
        "node" | "nodejs" | "deno" | "bun" => "javascript",
        "ts-node" => "typescript",
        "lua" | "luajit" => "lua",
        "php" => "php",
        _ => return None,
    };
    language_for_name(lang)
}

/// The language a vim (`vim: set ft=yaml:`, `vi: ft=sh`, `ex:`) or emacs
/// (`-*- mode: python -*-`, `-*- python -*-`) modeline names.
fn modeline(line: &str) -> Option<&'static LanguageDef> {
    if let Some(start) = line.find("-*-") {
        let inner = &line[start + 3..];
        let inner = &inner[..inner.find("-*-")?];
        let mode = if inner.contains(':') {
            inner.split(';').find_map(|kv| kv.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case("mode")).map(|(_, v)| v))?
        } else {
            inner
        };
        return language_for_alias(mode.trim());
    }
    // `vi:` and `ex:` need a blank before them (vim's own rule), so
    // `regex:` or `index:` are not modelines.
    let at = ["vim:", "vi:", "ex:"].iter().filter_map(|m| line.match_indices(m).find(|(i, _)| *m == "vim:" || *i == 0 || line[..*i].ends_with(char::is_whitespace)).map(|(i, _)| i + m.len())).min()?;
    line[at..].split(|c: char| c.is_whitespace() || c == ':').find_map(|opt| match opt.split_once('=')? {
        ("ft" | "filetype" | "syn" | "syntax", v) => language_for_alias(v),
        _ => None,
    })
}

/// Our language for a vim filetype or emacs mode name, else the language
/// of that exact name.
fn language_for_alias(name: &str) -> Option<&'static LanguageDef> {
    let lower = name.to_ascii_lowercase();
    let name = lower.strip_suffix("-mode").unwrap_or(&lower);
    let name = match name {
        "sh" | "shell" | "shell-script" | "zsh" => "bash",
        "js" | "js2" | "jsx" | "node" => "javascript",
        "ts" => "typescript",
        "py" | "python3" => "python",
        "cs" | "c#" => "csharp",
        "c++" => "cpp",
        "yml" => "yaml",
        "md" => "markdown",
        "rs" => "rust",
        "golang" => "go",
        "docker" | "containerfile" => "dockerfile",
        "htm" | "xhtml" => "html",
        n => n,
    };
    language_for_name(name)
}

struct RopeProvider<'a>(&'a Rope);

impl<'a> TextProvider<RopeChunk<'a>> for RopeProvider<'a> {
    type I = RopeChunks<'a>;
    fn text(&mut self, node: Node) -> Self::I {
        let r = node.byte_range();
        let end = r.end.min(self.0.len_bytes());
        let start = r.start.min(end);
        RopeChunks(self.0.byte_slice(start..end).chunks())
    }
}

pub struct RopeChunk<'a>(&'a str);
impl AsRef<[u8]> for RopeChunk<'_> {
    fn as_ref(&self) -> &[u8] {
        self.0.as_bytes()
    }
}
pub struct RopeChunks<'a>(ropey::iter::Chunks<'a>);
impl<'a> Iterator for RopeChunks<'a> {
    type Item = RopeChunk<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(RopeChunk)
    }
}

#[derive(Debug, Default, Clone)]
pub struct Highlights {
    /// Non-overlapping, sorted char ranges.
    pub spans: Vec<(Range<usize>, Style)>,
}

impl Highlights {
    pub fn style_at(&self, char_idx: usize) -> Option<Style> {
        let i = self.spans.partition_point(|(r, _)| r.end <= char_idx);
        let (r, s) = self.spans.get(i)?;
        r.contains(&char_idx).then_some(*s)
    }

    /// Flatten possibly nested/overlapping spans; later (inner) spans win.
    fn flatten(mut raw: Vec<(Range<usize>, Style)>) -> Self {
        // Stable sort: outer spans first, ties keep their incoming order.
        raw.sort_by(|a, b| a.0.start.cmp(&b.0.start).then(b.0.end.cmp(&a.0.end)));
        let mut out: Vec<(Range<usize>, Style)> = Vec::with_capacity(raw.len());
        let mut stack: Vec<(usize, Style)> = Vec::new();
        let mut pos = 0usize;
        let mut iter = raw.into_iter().peekable();
        loop {
            // Emit the innermost open span up to the next span start.
            let target = iter.peek().map(|(r, _)| r.start).unwrap_or(usize::MAX);
            while pos < target {
                while stack.last().is_some_and(|(e, _)| *e <= pos) {
                    stack.pop();
                }
                let Some(&(end, st)) = stack.last() else { break };
                let next = end.min(target);
                match out.last_mut() {
                    Some(last) if last.0.end == pos && last.1 == st => last.0.end = next,
                    _ => out.push((pos..next, st)),
                }
                pos = next;
            }
            let Some((r, st)) = iter.next() else { break };
            pos = pos.max(r.start);
            stack.push((r.end, st));
        }
        Self { spans: out }
    }
}

pub struct Syntax {
    pub def: &'static LanguageDef,
    parser: Parser,
    tree: Option<Tree>,
    styles: Vec<Option<Style>>,
    dirty: bool,
    /// Highlights of the last queried window, keyed by first line and rows.
    cache: Option<(u64, Highlights)>,
}

impl std::fmt::Debug for Syntax {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Syntax").field("language", &self.def.name).field("parsed", &self.tree.is_some()).finish()
    }
}

impl Syntax {
    pub fn new(def: &'static LanguageDef, theme: &Theme) -> Option<Self> {
        let mut parser = Parser::new();
        parser.set_language(&def.language).map_err(|e| tracing::error!("set_language {}: {e}", def.name)).ok()?;
        let styles = def.query.capture_names().iter().map(|n| theme.capture_style(n)).collect();
        Some(Self { def, parser, tree: None, styles, dirty: true, cache: None })
    }

    #[cfg(test)]
    pub fn for_path(path: &Path, theme: &Theme) -> Option<Self> {
        Self::new(detect(path)?, theme)
    }

    /// Record an edit already applied to `rope` (positions are chars).
    pub fn note_change(&mut self, rope: &Rope, at: usize, _old_len: usize, new_len: usize, old: &str, new: &str) {
        self.dirty = true;
        self.cache = None;
        let Some(tree) = &mut self.tree else { return };
        let start_byte = rope.char_to_byte(at.min(rope.len_chars()));
        let start_row = rope.byte_to_line(start_byte);
        let start_col = start_byte - rope.line_to_byte(start_row);
        let old_end_byte = start_byte + old.len();
        let new_end_byte = start_byte + new.len();
        let old_end_position = match old.rfind('\n') {
            Some(i) => Point::new(start_row + old.matches('\n').count(), old.len() - i - 1),
            None => Point::new(start_row, start_col + old.len()),
        };
        let new_end_char = (at + new_len).min(rope.len_chars());
        let new_end_byte_r = rope.char_to_byte(new_end_char);
        let new_row = rope.byte_to_line(new_end_byte_r);
        let new_end_position = Point::new(new_row, new_end_byte_r - rope.line_to_byte(new_row));
        tree.edit(&InputEdit { start_byte, old_end_byte, new_end_byte, start_position: Point::new(start_row, start_col), old_end_position, new_end_position });
    }

    pub fn reparse(&mut self, rope: &Rope) {
        if !self.dirty && self.tree.is_some() {
            return;
        }
        let tree = self.parser.parse_with_options(
            &mut |byte, _| {
                if byte >= rope.len_bytes() {
                    return &[][..];
                }
                let (chunk, start, _, _) = rope.chunk_at_byte(byte);
                &chunk.as_bytes()[byte - start..]
            },
            self.tree.as_ref(),
            None,
        );
        if tree.is_some() {
            self.tree = tree;
            self.dirty = false;
        }
    }

    pub fn tree(&self) -> Option<&Tree> {
        self.tree.as_ref()
    }

    /// Highlights for `rows` lines starting at `first_line`.
    pub fn highlights(&mut self, rope: &Rope, first_line: usize, rows: usize) -> Highlights {
        let key = (first_line as u64) << 32 | rows as u64;
        if let Some((k, h)) = &self.cache {
            if *k == key && !self.dirty {
                return h.clone();
            }
        }
        self.reparse(rope);
        let Some(tree) = &self.tree else { return Highlights::default() };
        let last_line = (first_line + rows).min(rope.len_lines());
        let start_byte = rope.line_to_byte(first_line.min(rope.len_lines()));
        let end_byte = if last_line >= rope.len_lines() { rope.len_bytes() } else { rope.line_to_byte(last_line) };
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(start_byte..end_byte);
        let mut raw = Vec::new();
        let mut it = cursor.captures(&self.def.query, tree.root_node(), RopeProvider(rope));
        while let Some((m, idx)) = it.next() {
            let cap = m.captures()[*idx];
            let r = cap.node.byte_range();
            if r.start >= rope.len_bytes() {
                continue;
            }
            // Captures the theme does not style never block other captures.
            let Some(style) = self.styles.get(cap.index as usize).copied().flatten() else { continue };
            raw.push((r, m.pattern_index, style));
        }
        // The first styled pattern matching a node wins (tree-sitter-highlight
        // semantics); keep only that capture per node range.
        raw.sort_by(|a, b| a.0.start.cmp(&b.0.start).then(b.0.end.cmp(&a.0.end)).then(a.1.cmp(&b.1)));
        raw.dedup_by(|b, a| a.0 == b.0);
        let spans = raw
            .into_iter()
            .filter_map(|(r, _, style)| {
                let s = rope.byte_to_char(r.start);
                let e = rope.byte_to_char(r.end.min(rope.len_bytes()));
                (e > s).then_some((s..e, style))
            })
            .collect();
        let h = Highlights::flatten(spans);
        self.cache = Some((key, h.clone()));
        h
    }

    pub fn invalidate(&mut self) {
        self.cache = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flatten_nested_spans_inner_wins() {
        let a = Style::default().fg(ratatui::style::Color::Red);
        let b = Style::default().fg(ratatui::style::Color::Blue);
        let h = Highlights::flatten(vec![(0..10, a), (3..5, b), (7..8, b)]);
        assert_eq!(h.style_at(0), Some(a));
        assert_eq!(h.style_at(3), Some(b));
        assert_eq!(h.style_at(5), Some(a));
        assert_eq!(h.style_at(7), Some(b));
        assert_eq!(h.style_at(9), Some(a));
        assert_eq!(h.style_at(10), None);
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn rust_keywords_highlighted_and_incremental() {
        let theme = Theme::default();
        let def = language_for_name("rust").expect("rust grammar");
        let mut syn = Syntax::new(def, &theme).unwrap();
        let mut rope = Rope::from_str("fn main() { let x = \"hi\"; }\n");
        let h = syn.highlights(&rope, 0, 1);
        let kw = theme.capture_style("keyword").unwrap();
        let st = theme.capture_style("string").unwrap();
        assert_eq!(h.style_at(0), Some(kw), "fn is a keyword");
        assert_eq!(h.style_at(21), Some(st), "string literal");
        // Insert text before the string and check the string still highlights.
        let at = 12;
        rope.insert(at, "let y = 1; ");
        syn.note_change(&rope, at, 0, 11, "", "let y = 1; ");
        let h = syn.highlights(&rope, 0, 1);
        assert_eq!(h.style_at(21 + 11), Some(st));
        assert_eq!(h.style_at(12), Some(kw), "inserted let is a keyword");
        assert_eq!(h.style_at(20), theme.capture_style("number"), "integer literal");
    }

    #[test]
    fn every_enabled_grammar_compiles_its_query() {
        let enabled = [
            cfg!(feature = "lang-rust"),
            cfg!(feature = "lang-js"),
            cfg!(feature = "lang-ts"),
            cfg!(feature = "lang-ts"),
            cfg!(feature = "lang-python"),
            cfg!(feature = "lang-json"),
            cfg!(feature = "lang-toml"),
            cfg!(feature = "lang-bash"),
            cfg!(feature = "lang-md"),
            cfg!(feature = "lang-csharp"),
            cfg!(feature = "lang-yaml"),
            cfg!(feature = "lang-html"),
            cfg!(feature = "lang-css"),
            cfg!(feature = "lang-scss"),
            cfg!(feature = "lang-xml"),
            cfg!(feature = "lang-go"),
            cfg!(feature = "lang-c"),
            cfg!(feature = "lang-cpp"),
            cfg!(feature = "lang-java"),
            cfg!(feature = "lang-php"),
            cfg!(feature = "lang-lua"),
            cfg!(feature = "lang-dockerfile"),
        ];
        let names: Vec<&str> = LANGUAGES.iter().map(|l| l.name).collect();
        assert_eq!(names.len(), enabled.iter().filter(|e| **e).count(), "loaded: {names:?}");
        // A loaded grammar may still have dropped query parts; that is a bug too.
        let partial: Vec<&str> = LANGUAGES.iter().filter(|l| !l.full_query).map(|l| l.name).collect();
        assert!(partial.is_empty(), "query fell back to a partial set for {partial:?}");
    }

    #[cfg(feature = "lang-json")]
    #[test]
    fn json_strings_highlighted() {
        let theme = Theme::default();
        let mut syn = Syntax::for_path(Path::new("a.json"), &theme).expect("json grammar");
        let rope = Rope::from_str("{\"key\": \"value\", \"n\": 1}\n");
        let h = syn.highlights(&rope, 0, 1);
        let st = theme.capture_style("string").unwrap();
        // The value string is a plain string; the key may use a more specific capture.
        assert_eq!(h.style_at(10), Some(st));
        assert!(h.style_at(2).is_some(), "key highlighted");
        assert_eq!(h.style_at(22), theme.capture_style("number"));
    }

    #[cfg(feature = "lang-toml")]
    #[test]
    fn toml_strings_highlighted() {
        let theme = Theme::default();
        let mut syn = Syntax::for_path(Path::new("Cargo.toml"), &theme).expect("toml grammar");
        let rope = Rope::from_str("[package]\nname = \"jetl\"\n");
        let h = syn.highlights(&rope, 0, 2);
        assert_eq!(h.style_at(18), theme.capture_style("string"));
    }

    /// Style at the first char of `needle` in `src`, highlighted as `file`.
    #[allow(dead_code)]
    fn style_of(file: &str, src: &str, needle: &str) -> Option<Style> {
        let theme = Theme::default();
        let mut syn = Syntax::for_path(Path::new(file), &theme).unwrap_or_else(|| panic!("grammar for {file}"));
        let rope = Rope::from_str(src);
        let h = syn.highlights(&rope, 0, rope.len_lines());
        let at = src.find(needle).unwrap_or_else(|| panic!("{needle:?} in {src:?}"));
        h.style_at(rope.byte_to_char(at))
    }

    #[allow(dead_code)]
    fn cap(name: &str) -> Option<Style> {
        Theme::default().capture_style(name)
    }

    #[cfg(feature = "lang-csharp")]
    #[test]
    fn csharp_highlighted() {
        let src = "// note\npublic class A { string s = \"hi\"; int n = 42; }\n";
        assert_eq!(style_of("A.cs", src, "// note"), cap("comment"));
        assert_eq!(style_of("A.cs", src, "public"), cap("keyword"));
        assert_eq!(style_of("A.cs", src, "\"hi"), cap("string"));
        assert_eq!(style_of("A.cs", src, "42"), cap("number"));
    }

    #[cfg(feature = "lang-yaml")]
    #[test]
    fn yaml_highlighted() {
        let src = "# c\nname: \"jetl\"\ncount: 3\n";
        assert_eq!(style_of("a.yml", src, "# c"), cap("comment"));
        assert_eq!(style_of("a.yml", src, "\"jetl"), cap("string"));
        assert_eq!(style_of("a.yml", src, "name"), cap("property"), "key is not a string");
        assert_eq!(style_of("a.yml", src, "3"), cap("number"));
    }

    #[cfg(feature = "lang-html")]
    #[test]
    fn html_highlighted() {
        let src = "<!-- c -->\n<div class=\"a\">x</div>\n";
        assert_eq!(style_of("a.html", src, "<!--"), cap("comment"));
        assert_eq!(style_of("a.html", src, "div"), cap("tag"));
        assert_eq!(style_of("a.html", src, "class"), cap("attribute"));
        assert_eq!(style_of("a.vue", src, "a\">"), cap("string"));
    }

    #[cfg(feature = "lang-css")]
    #[test]
    fn css_highlighted() {
        let src = "/* c */\n@media print { a { color: red; content: \"x\"; } }\n";
        assert_eq!(style_of("a.css", src, "/*"), cap("comment"));
        assert_eq!(style_of("a.css", src, "@media"), cap("media"));
        assert_eq!(style_of("a.css", src, "a {"), cap("tag"));
        assert_eq!(style_of("a.css", src, "color"), cap("property"));
        assert_eq!(style_of("a.css", src, "\"x"), cap("string"));
    }

    #[cfg(feature = "lang-scss")]
    #[test]
    fn scss_highlighted() {
        let src = "// c\n@use \"base\";\n.a { @include m; }\n";
        assert_eq!(style_of("a.scss", src, "// c"), cap("comment"));
        if cfg!(feature = "lang-css") {
            assert_eq!(style_of("a.scss", src, "\"base"), cap("string"), "string from the CSS query");
        }
        assert_eq!(style_of("a.scss", src, "@include"), cap("include"));
        assert!(cap("include").is_some());
    }

    #[cfg(feature = "lang-xml")]
    #[test]
    fn xml_highlighted() {
        let src = "<?xml version=\"1.0\"?>\n<!-- c -->\n<Project Sdk=\"x\"><a/></Project>\n";
        assert_eq!(style_of("a.csproj", src, "<!--"), cap("comment"));
        assert_eq!(style_of("a.csproj", src, "Project"), cap("tag"));
        assert!(style_of("a.csproj", src, "Sdk").is_some(), "attribute name highlighted");
        assert_eq!(style_of("a.csproj", src, "x\"><a"), cap("string"));
    }

    #[cfg(feature = "lang-go")]
    #[test]
    fn go_highlighted() {
        let src = "// c\npackage main\nfunc (t *T) Get() int { return len(\"hi\") + 42 }\nfunc main() { fmt.Println(nil) }\n";
        assert_eq!(style_of("main.go", src, "// c"), cap("comment"));
        assert_eq!(style_of("main.go", src, "func"), cap("keyword"));
        assert_eq!(style_of("main.go", src, "Get"), cap("function"));
        assert_eq!(style_of("main.go", src, "main()"), cap("function"));
        assert_eq!(style_of("main.go", src, "len"), cap("function.builtin"), "builtin wins over plain call");
        assert_eq!(style_of("main.go", src, "\"hi"), cap("string"));
        assert_eq!(style_of("main.go", src, "42"), cap("number"));
    }

    #[cfg(feature = "lang-c")]
    #[test]
    fn c_highlighted() {
        let src = "/* b */\n#include <stdio.h>\nstatic unsigned long add(int a) { s.f(); return printf(\"%d\", 1); }\n";
        assert_eq!(style_of("a.h", src, "/* b"), cap("comment"));
        assert_eq!(style_of("a.c", src, "#include"), cap("keyword"));
        assert_eq!(style_of("a.c", src, "static"), cap("keyword"));
        assert_eq!(style_of("a.c", src, "unsigned"), cap("type.builtin"));
        assert_eq!(style_of("a.c", src, "int a"), cap("type.builtin"));
        assert_eq!(style_of("a.c", src, "add"), cap("function"));
        assert_eq!(style_of("a.c", src, "f()"), cap("function"), "called field is a function, not a property");
        assert_eq!(style_of("a.c", src, "\"%d"), cap("string"));
        assert_eq!(style_of("a.c", src, "1)"), cap("number"));
    }

    #[cfg(feature = "lang-cpp")]
    #[test]
    fn cpp_highlighted() {
        let src = "// c\nnamespace ns { class A { public: void run(); }; }\nvoid ns::A::run() { auto v = std::vector<int>{}; v.push_back(\"x\"); a::b::go(); throw nullptr; }\n";
        assert_eq!(style_of("a.cpp", src, "// c"), cap("comment"), "comment from the C query");
        assert_eq!(style_of("a.hpp", src, "namespace"), cap("keyword"));
        assert_eq!(style_of("a.cc", src, "class"), cap("keyword"));
        assert_eq!(style_of("a.cpp", src, "run()"), cap("function"));
        assert_eq!(style_of("a.cpp", src, "run() {"), cap("function"), "two scopes deep");
        assert_eq!(style_of("a.cpp", src, "go()"), cap("function"));
        assert_eq!(style_of("a.cpp", src, "auto"), cap("type.builtin"));
        assert_eq!(style_of("a.cpp", src, "push_back"), cap("function"));
        assert_eq!(style_of("a.cpp", src, "\"x"), cap("string"));
    }

    #[cfg(feature = "lang-java")]
    #[test]
    fn java_highlighted() {
        let src = "// c\n@Override\npublic class A { void run() { System.out.println(\"hi\" + 42); } }\n";
        assert_eq!(style_of("A.java", src, "// c"), cap("comment"));
        assert_eq!(style_of("A.java", src, "Override"), cap("attribute"));
        assert_eq!(style_of("A.java", src, "public"), cap("keyword"));
        assert_eq!(style_of("A.java", src, "void"), cap("type.builtin"));
        assert_eq!(style_of("A.java", src, "run"), cap("function"));
        assert_eq!(style_of("A.java", src, "println"), cap("function"));
        assert_eq!(style_of("A.java", src, "\"hi"), cap("string"));
        assert_eq!(style_of("A.java", src, "42"), cap("number"));
    }

    #[cfg(feature = "lang-php")]
    #[test]
    fn php_highlighted() {
        let src = "<?php\n// c\nclass A { public function run() { echo strlen(\"hi\"); return 42; } }\n";
        assert_eq!(style_of("a.php", src, "<?php"), cap("tag"));
        assert_eq!(style_of("a.php", src, "// c"), cap("comment"));
        assert_eq!(style_of("a.php", src, "class"), cap("keyword"));
        assert_eq!(style_of("a.php", src, "function"), cap("keyword"));
        assert_eq!(style_of("a.php", src, "run"), cap("function"));
        assert_eq!(style_of("a.php", src, "strlen"), cap("function"));
        assert_eq!(style_of("a.php", src, "\"hi"), cap("string"));
        assert_eq!(style_of("a.php", src, "42"), cap("number"));
    }

    #[cfg(feature = "lang-lua")]
    #[test]
    fn lua_highlighted() {
        let src = "-- c\nlocal function f(x)\n  if x then print(\"hi\") end\n  for i = 1, 3 do x:go() end\n  return 42\nend\n";
        assert_eq!(style_of("a.lua", src, "-- c"), cap("comment"));
        assert_eq!(style_of("a.lua", src, "local"), cap("keyword"));
        assert_eq!(style_of("a.lua", src, "if"), cap("keyword"));
        assert_eq!(style_of("a.lua", src, "for"), cap("keyword"));
        assert_eq!(style_of("a.lua", src, "return"), cap("keyword"));
        assert_eq!(style_of("a.lua", src, "f("), cap("function"));
        assert_eq!(style_of("a.lua", src, "go"), cap("function"));
        assert_eq!(style_of("a.lua", src, "print"), cap("function.builtin"), "builtin wins over plain call");
        assert_eq!(style_of("a.lua", src, "\"hi"), cap("string"));
        assert_eq!(style_of("a.lua", src, "42"), cap("number"));
        assert!(cap("if").is_none() && cap("conditional") == cap("keyword"));
    }

    #[cfg(feature = "lang-dockerfile")]
    #[test]
    fn dockerfile_highlighted() {
        let src = "# c\nFROM rust:1.80 AS build\nENV A=b\nEXPOSE 8080\nCMD [\"jetl\"]\n";
        for file in ["Dockerfile", "Containerfile", "dev.Dockerfile"] {
            assert_eq!(detect(Path::new(file)).map(|l| l.name), Some("dockerfile"), "{file}");
        }
        assert_eq!(style_of("Dockerfile", src, "# c"), cap("comment"));
        assert_eq!(style_of("Dockerfile", src, "FROM"), cap("keyword"));
        assert_eq!(style_of("Dockerfile", src, "AS"), cap("keyword"));
        assert_eq!(style_of("Dockerfile", src, "A=b"), cap("property"));
        assert_eq!(style_of("Dockerfile", src, "8080"), cap("number"));
        assert_eq!(style_of("Dockerfile", src, "\"jetl"), cap("string"));
    }

    fn lang(path: &str, text: &str) -> Option<&'static str> {
        detect_text(Path::new(path), text).map(|l| l.name)
    }

    #[cfg(all(feature = "lang-python", feature = "lang-bash", feature = "lang-js", feature = "lang-ts", feature = "lang-lua", feature = "lang-php"))]
    #[test]
    fn shebang_names_the_interpreter() {
        assert_eq!(lang("/tmp/run", "#!/usr/bin/env python3\nprint(1)\n"), Some("python"));
        assert_eq!(lang("/tmp/run", "#!/usr/bin/python3.12 -u\n"), Some("python"));
        assert_eq!(lang("/tmp/run", "#!/bin/bash -e\nset -u\n"), Some("bash"));
        assert_eq!(lang("/tmp/run", "#! /bin/sh\n"), Some("bash"));
        assert_eq!(lang("/tmp/run", "#!/usr/bin/env zsh"), Some("bash"));
        assert_eq!(lang("/tmp/run", "#!/usr/bin/env -S deno run --allow-net\n"), Some("javascript"));
        assert_eq!(lang("/tmp/run", "#!/usr/bin/env -S NODE_ENV=prod node\n"), Some("javascript"));
        assert_eq!(lang("/tmp/run", "#!/usr/bin/env bun\n"), Some("javascript"));
        assert_eq!(lang("/tmp/run", "#!/usr/bin/env ts-node\n"), Some("typescript"));
        assert_eq!(lang("/tmp/run", "#!/usr/bin/env lua5.4\n"), Some("lua"));
        assert_eq!(lang("/tmp/run", "#!/usr/bin/php\n<?php\n"), Some("php"));
        assert_eq!(lang("/tmp/run", "#!/usr/bin/env ruby\n"), None);
        assert_eq!(lang("/tmp/run", "#!/usr/bin/env\n"), None);
        assert_eq!(lang("/tmp/run", "\n#!/bin/bash\n"), None, "only on the first line");
    }

    #[cfg(all(feature = "lang-yaml", feature = "lang-python", feature = "lang-bash", feature = "lang-cpp", feature = "lang-csharp"))]
    #[test]
    fn vim_modeline_in_first_or_last_lines() {
        assert_eq!(lang("/tmp/notes", "# vim: set ft=yaml:\na: 1\n"), Some("yaml"));
        assert_eq!(lang("/tmp/notes", "a = 1\n\n\n\n\n\n\n# vim: ft=python\n"), Some("python"));
        assert_eq!(lang("/tmp/notes", "x\n# vi: ts=4 filetype=sh\n"), Some("bash"));
        assert_eq!(lang("/tmp/notes", "// ex:syntax=cpp:\n"), Some("cpp"));
        assert_eq!(lang("/tmp/notes", "/* vim: set sw=4 ft=cs: */\n"), Some("csharp"));
        assert_eq!(lang("/tmp/notes", "regex: ft=python\nindex:ft=sh\n"), None, "vi: and ex: need a blank before them");
        assert_eq!(lang("/tmp/notes", "# vim: ts=4\n"), None);
        let middle = format!("{}# vim: ft=yaml\n{}", "x\n".repeat(5), "y\n".repeat(5));
        assert_eq!(lang("/tmp/notes", &middle), None, "only the first and last 5 lines");
    }

    #[cfg(all(feature = "lang-python", feature = "lang-cpp", feature = "lang-bash"))]
    #[test]
    fn emacs_modeline() {
        assert_eq!(lang("/tmp/notes", "# -*- mode: python; coding: utf-8 -*-\n"), Some("python"));
        assert_eq!(lang("/tmp/notes", "# -*- Python -*-\n"), Some("python"));
        assert_eq!(lang("/tmp/notes", "// -*- c++ -*-\n"), Some("cpp"));
        assert_eq!(lang("/tmp/notes", "#!/usr/bin/foo\n# -*- mode: shell-script -*-\n"), Some("bash"));
        assert_eq!(lang("/tmp/notes", "# -*- coding: utf-8 -*-\n"), None);
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn known_extension_beats_shebang_and_modeline() {
        assert_eq!(lang("/tmp/main.rs", "#!/usr/bin/env python3\n"), Some("rust"));
        assert_eq!(lang("/tmp/main.rs", "// vim: ft=python\n"), Some("rust"));
        assert_eq!(lang("/tmp/notes.txt", "plain\n"), None);
    }
}
