//! Tree-sitter based highlighting. Each buffer with a known language owns a
//! parser and tree; edits are fed incrementally and the visible byte range
//! is queried on render.

use std::ops::Range;
use std::path::Path;
use std::sync::{LazyLock, OnceLock};

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
    /// Tags query (`@definition.*` and `@name` captures) for symbols:
    /// the Structure window and the symbol index.
    pub tags: Option<&'static str>,
    /// `tags` compiled on first use.
    tags_query: OnceLock<Option<Query>>,
    /// Injection query (`@injection.content` with `@injection.language` or
    /// `#set! injection.language`): languages embedded in this one
    /// (Markdown fences, HTML `<script>` and `<style>`).
    pub injections: Option<&'static str>,
    /// `injections` compiled on first use.
    injections_query: OnceLock<Option<Query>>,
    /// Whether every query part compiled (no fallback to a partial set).
    #[cfg(test)]
    full_query: bool,
}

impl std::fmt::Debug for LanguageDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name)
    }
}

fn make(
    name: &'static str,
    extensions: &'static [&'static str],
    filenames: &'static [&'static str],
    language: Language,
    queries: &[&str],
    line_comment: Option<&'static str>,
) -> Option<LanguageDef> {
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
                    tags: None,
                    tags_query: OnceLock::new(),
                    injections: None,
                    injections_query: OnceLock::new(),
                    #[cfg(test)]
                    full_query: n == queries.len(),
                });
            }
            Err(e) => tracing::warn!("highlight query for {name} ({n} parts): {e}"),
        }
    }
    None
}

impl LanguageDef {
    #[cfg_attr(
        not(any(
            feature = "lang-rust",
            feature = "lang-js",
            feature = "lang-ts",
            feature = "lang-python",
            feature = "lang-go",
            feature = "lang-c",
            feature = "lang-cpp",
            feature = "lang-java"
        )),
        allow(dead_code)
    )]
    fn with_tags(mut self, tags: &'static str) -> Self {
        self.tags = Some(tags);
        self
    }

    /// The compiled tags query; `None` without one or when it does not
    /// compile (logged once).
    pub fn tags_query(&self) -> Option<&Query> {
        self.tags_query
            .get_or_init(|| {
                let src = self.tags?;
                Query::new(&self.language, src).map_err(|e| tracing::warn!("tags query for {}: {e}", self.name)).ok()
            })
            .as_ref()
    }

    #[cfg(any(feature = "lang-md", feature = "lang-html"))]
    fn with_injections(mut self, q: &'static str) -> Self {
        self.injections = Some(q);
        self
    }

    /// The compiled injections query; `None` without one or when it does
    /// not compile (logged once).
    pub fn injections_query(&self) -> Option<&Query> {
        self.injections_query
            .get_or_init(|| {
                let src = self.injections?;
                Query::new(&self.language, src).map_err(|e| tracing::warn!("injections query for {}: {e}", self.name)).ok()
            })
            .as_ref()
    }
}

/// Query parts joined into one static string (built once per language).
#[allow(dead_code)]
fn join_static(parts: &[&str]) -> &'static str {
    Box::leak(parts.join("\n").into_boxed_str())
}

/// Prepended to the Rust tags: upstream lists `impl` blocks as references
/// only, so methods would have no parent in the Structure window.
#[cfg(feature = "lang-rust")]
const RUST_TAGS_EXTRA: &str = "(impl_item trait: (_) @trait type: (_) @name) @definition.impl (impl_item type: (_) @name !trait) @definition.impl (const_item name: (identifier) @name) @definition.constant (static_item name: (identifier) @name) @definition.constant";

/// Prepended to the HTML injections: `<script lang="ts">` and
/// `<style lang="scss">` (Vue) name their language; the first pattern
/// whose language is compiled in wins for a content node.
#[cfg(feature = "lang-html")]
const HTML_INJECTIONS_LANG: &str = "((script_element (start_tag (attribute (attribute_name) @_attr (quoted_attribute_value (attribute_value) @injection.language))) (raw_text) @injection.content) (#eq? @_attr \"lang\")) ((style_element (start_tag (attribute (attribute_name) @_attr (quoted_attribute_value (attribute_value) @injection.language))) (raw_text) @injection.content) (#eq? @_attr \"lang\"))";

fn block_comment(name: &str) -> Option<(&'static str, &'static str)> {
    match name {
        "rust" | "javascript" | "typescript" | "tsx" | "csharp" | "css" | "scss" | "go" | "c" | "cpp" | "java" | "php" => {
            Some(("/*", "*/"))
        }
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
    v.extend(
        make(
            "rust",
            &["rs"],
            &[],
            tree_sitter_rust::LANGUAGE.into(),
            &["[(integer_literal) (float_literal)] @number", tree_sitter_rust::HIGHLIGHTS_QUERY],
            Some("//"),
        )
        .map(|d| d.with_tags(join_static(&[RUST_TAGS_EXTRA, tree_sitter_rust::TAGS_QUERY]))),
    );
    #[cfg(feature = "lang-js")]
    v.extend(
        make(
            "javascript",
            &["js", "mjs", "cjs", "jsx"],
            &[],
            tree_sitter_javascript::LANGUAGE.into(),
            &[tree_sitter_javascript::HIGHLIGHT_QUERY],
            Some("//"),
        )
        .map(|d| d.with_tags(tree_sitter_javascript::TAGS_QUERY)),
    );
    #[cfg(feature = "lang-ts")]
    {
        #[cfg(feature = "lang-js")]
        let (js, js_tags) = (tree_sitter_javascript::HIGHLIGHT_QUERY, tree_sitter_javascript::TAGS_QUERY);
        #[cfg(not(feature = "lang-js"))]
        let (js, js_tags) = ("", "");
        // The TypeScript tags only cover what TypeScript adds (signatures,
        // interfaces); the JavaScript tags find functions and classes.
        let tags = join_static(&[tree_sitter_typescript::TAGS_QUERY, js_tags]);
        v.extend(
            make(
                "typescript",
                &["ts", "mts", "cts"],
                &[],
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                &[tree_sitter_typescript::HIGHLIGHTS_QUERY, js],
                Some("//"),
            )
            .map(|d| d.with_tags(tags)),
        );
        v.extend(
            make(
                "tsx",
                &["tsx"],
                &[],
                tree_sitter_typescript::LANGUAGE_TSX.into(),
                &[tree_sitter_typescript::HIGHLIGHTS_QUERY, js],
                Some("//"),
            )
            .map(|d| d.with_tags(tags)),
        );
    }
    #[cfg(feature = "lang-python")]
    v.extend(
        make("python", &["py", "pyi"], &[], tree_sitter_python::LANGUAGE.into(), &[tree_sitter_python::HIGHLIGHTS_QUERY], Some("#"))
            .map(|d| d.with_tags(tree_sitter_python::TAGS_QUERY)),
    );
    #[cfg(feature = "lang-json")]
    v.extend(make(
        "json",
        &["json", "jsonc", "json5"],
        &[],
        tree_sitter_json::LANGUAGE.into(),
        &[tree_sitter_json::HIGHLIGHTS_QUERY],
        None,
    ));
    #[cfg(feature = "lang-toml")]
    v.extend(make(
        "toml",
        &["toml"],
        &["Cargo.lock"],
        tree_sitter_toml_ng::LANGUAGE.into(),
        &[tree_sitter_toml_ng::HIGHLIGHTS_QUERY],
        Some("#"),
    ));
    #[cfg(feature = "lang-bash")]
    v.extend(make(
        "bash",
        &["sh", "bash", "zsh"],
        &[".bashrc", ".zshrc", ".profile", ".bash_profile", "PKGBUILD"],
        tree_sitter_bash::LANGUAGE.into(),
        &[tree_sitter_bash::HIGHLIGHT_QUERY],
        Some("#"),
    ));
    #[cfg(feature = "lang-md")]
    v.extend(
        make("markdown", &["md", "markdown"], &[], tree_sitter_md::LANGUAGE.into(), &[tree_sitter_md::HIGHLIGHT_QUERY_BLOCK], None)
            .map(|d| d.with_injections(tree_sitter_md::INJECTION_QUERY_BLOCK)),
    );
    #[cfg(feature = "lang-csharp")]
    v.extend(make(
        "csharp",
        &["cs", "csx"],
        &[],
        tree_sitter_c_sharp::LANGUAGE.into(),
        &[tree_sitter_c_sharp::HIGHLIGHTS_QUERY],
        Some("//"),
    ));
    #[cfg(feature = "lang-yaml")]
    // Upstream captures mapping keys as `@string` before `@property`, and
    // the first pattern for a node wins; lift the key patterns to the front.
    v.extend(make("yaml", &["yml", "yaml"], &[], tree_sitter_yaml::LANGUAGE.into(), &["(_ key: (flow_node [(double_quote_scalar) (single_quote_scalar)] @property)) (_ key: (flow_node (plain_scalar (string_scalar) @property)))", tree_sitter_yaml::HIGHLIGHTS_QUERY], Some("#")));
    // Vue single-file components use the HTML grammar; their `<script>` and
    // `<style>` blocks are injections (`lang` picks TypeScript or SCSS). The
    // Vue grammar crate is unmaintained.
    #[cfg(feature = "lang-html")]
    v.extend(
        make("html", &["html", "htm", "xhtml", "vue"], &[], tree_sitter_html::LANGUAGE.into(), &[tree_sitter_html::HIGHLIGHTS_QUERY], None)
            .map(|d| d.with_injections(join_static(&[HTML_INJECTIONS_LANG, tree_sitter_html::INJECTIONS_QUERY]))),
    );
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
    v.extend(make(
        "xml",
        &["xml", "xsd", "xsl", "xslt", "svg", "csproj", "fsproj", "vbproj", "props", "targets", "plist", "resx", "xaml"],
        &[],
        tree_sitter_xml::LANGUAGE_XML.into(),
        &[tree_sitter_xml::XML_HIGHLIGHT_QUERY],
        None,
    ));
    #[cfg(feature = "lang-go")]
    // Upstream lists builtin calls after plain calls, and the first pattern
    // for a node wins; lift them to the front (Lua too).
    v.extend(make("go", &["go"], &[], tree_sitter_go::LANGUAGE.into(), &["(call_expression function: (identifier) @function.builtin (#match? @function.builtin \"^(append|cap|clear|close|complex|copy|delete|imag|len|make|max|min|new|panic|print|println|real|recover)$\"))", tree_sitter_go::HIGHLIGHTS_QUERY], Some("//")).map(|d| d.with_tags(tree_sitter_go::TAGS_QUERY)));
    #[cfg(feature = "lang-c")]
    v.extend(
        make("c", &["c", "h"], &[], tree_sitter_c::LANGUAGE.into(), &[C_QUERY_FIXES, tree_sitter_c::HIGHLIGHT_QUERY], Some("//"))
            .map(|d| d.with_tags(tree_sitter_c::TAGS_QUERY)),
    );
    // The C++ query only covers what C++ adds to C (no comments, numbers or
    // calls); the C query fills in the rest.
    #[cfg(feature = "lang-cpp")]
    v.extend(
        make(
            "cpp",
            &["cpp", "cc", "cxx", "hpp", "hh"],
            &[],
            tree_sitter_cpp::LANGUAGE.into(),
            &[C_QUERY_FIXES, CPP_QUERY_FIXES, tree_sitter_cpp::HIGHLIGHT_QUERY, tree_sitter_c::HIGHLIGHT_QUERY],
            Some("//"),
        )
        .map(|d| d.with_tags(tree_sitter_cpp::TAGS_QUERY)),
    );
    #[cfg(feature = "lang-java")]
    v.extend(
        make("java", &["java"], &[], tree_sitter_java::LANGUAGE.into(), &[tree_sitter_java::HIGHLIGHTS_QUERY], Some("//"))
            .map(|d| d.with_tags(tree_sitter_java::TAGS_QUERY)),
    );
    #[cfg(feature = "lang-php")]
    v.extend(make("php", &["php"], &[], tree_sitter_php::LANGUAGE_PHP.into(), &[tree_sitter_php::HIGHLIGHTS_QUERY], Some("//")));
    #[cfg(feature = "lang-lua")]
    v.extend(make("lua", &["lua"], &[], tree_sitter_lua::LANGUAGE.into(), &["(function_call name: (identifier) @function.builtin (#any-of? @function.builtin \"assert\" \"collectgarbage\" \"dofile\" \"error\" \"getfenv\" \"getmetatable\" \"ipairs\" \"load\" \"loadfile\" \"loadstring\" \"module\" \"next\" \"pairs\" \"pcall\" \"print\" \"rawequal\" \"rawget\" \"rawset\" \"require\" \"select\" \"setfenv\" \"setmetatable\" \"tonumber\" \"tostring\" \"type\" \"unpack\" \"xpcall\"))", tree_sitter_lua::HIGHLIGHTS_QUERY], Some("--")));
    // tree-sitter-containerfile is the maintained fork of the Dockerfile
    // grammar (the tree-sitter-dockerfile crate targets tree-sitter 0.20).
    #[cfg(feature = "lang-dockerfile")]
    v.extend(make(
        "dockerfile",
        &["dockerfile"],
        &["Dockerfile", "Containerfile"],
        tree_sitter_containerfile::LANGUAGE.into(),
        &[tree_sitter_containerfile::HIGHLIGHTS_QUERY],
        Some("#"),
    ));
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
    let at = ["vim:", "vi:", "ex:"]
        .iter()
        .filter_map(|m| {
            line.match_indices(m)
                .find(|(i, _)| *m == "vim:" || *i == 0 || line[..*i].ends_with(char::is_whitespace))
                .map(|(i, _)| i + m.len())
        })
        .min()?;
    line[at..].split(|c: char| c.is_whitespace() || c == ':').find_map(|opt| match opt.split_once('=')? {
        ("ft" | "filetype" | "syn" | "syntax", v) => language_for_alias(v),
        _ => None,
    })
}

/// Our language for a vim filetype or emacs mode name, else the language
/// of that exact name.
pub fn language_for_alias(name: &str) -> Option<&'static LanguageDef> {
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

pub(crate) struct RopeProvider<'a>(pub &'a Rope);

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

/// Highlight a standalone text (documentation code) with `def`: per line of `text`
/// (split at '\n'), the styled char ranges within that line. Empty lists when the
/// grammar cannot be loaded.
pub fn highlight_text(def: &'static LanguageDef, text: &str, theme: &Theme) -> Vec<Vec<(Range<usize>, Style)>> {
    let rope = Rope::from_str(text);
    let lines = rope.len_lines();
    let mut out = vec![Vec::new(); lines];
    let Some(mut syn) = Syntax::new(def, theme) else { return out };
    let spans = syn.highlights(&rope, 0, lines).spans;
    for (i, line) in out.iter_mut().enumerate() {
        let start = rope.line_to_char(i);
        let end = if i + 1 < lines { rope.line_to_char(i + 1) - 1 } else { rope.len_chars() };
        for (r, st) in &spans {
            let (s, e) = (r.start.max(start), r.end.min(end));
            if s < e {
                line.push((s - start..e - start, *st));
            }
        }
    }
    out
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

/// The theme's style for each capture of the language's query, by capture
/// index.
fn styles_for(def: &LanguageDef, theme: &Theme) -> Vec<Option<Style>> {
    def.query.capture_names().iter().map(|n| theme.capture_style(n)).collect()
}

/// Most injections one buffer keeps; further embedded blocks keep the
/// host's highlighting.
const MAX_INJECTIONS: usize = 1000;

/// A language embedded in the host text, with its own tree. Its nodes
/// carry document byte offsets (parsed with included ranges).
struct Injection {
    /// Index into `LANGUAGES`.
    lang: usize,
    /// The document bytes the inner parser read: the content node
    /// minus its children, sorted, non-empty.
    bytes: Vec<Range<usize>>,
    tree: Tree,
    /// An edit touched it since its last parse.
    dirty: bool,
}

/// An `@injection.content` match found in the host tree.
struct Candidate {
    lang: usize,
    bytes: Vec<Range<usize>>,
    ranges: Vec<tree_sitter::Range>,
}

/// First start .. last end of sorted ranges.
fn span(bytes: &[Range<usize>]) -> Range<usize> {
    match (bytes.first(), bytes.last()) {
        (Some(a), Some(b)) => a.start..b.end,
        _ => 0..0,
    }
}

/// Whether the ranges meet or overlap (an edit at a block's edge counts).
fn touches(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start <= b.end && b.start <= a.end
}

fn overlaps(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && b.start < a.end
}

/// Where byte `p` lands after `start..old_end` was replaced by `start..new_end`.
fn map_byte(p: usize, start: usize, old_end: usize, new_end: usize) -> usize {
    if p <= start {
        p
    } else if p >= old_end {
        p - old_end + new_end
    } else {
        new_end
    }
}

/// The node's range with each named child's range cut out (anonymous
/// tokens such as the `(` in fenced code are text; the Markdown
/// `block_continuation` children are the container prefixes); built from node
/// fields only (the rope's line count differs from tree-sitter's for lone
/// `\r` and U+2028). Empty pieces are dropped.
fn content_ranges(node: Node) -> Vec<tree_sitter::Range> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    let (mut from_byte, mut from_point) = (node.start_byte(), node.start_position());
    for child in node.named_children(&mut cursor) {
        if child.start_byte() > from_byte {
            out.push(tree_sitter::Range {
                start_byte: from_byte,
                end_byte: child.start_byte(),
                start_point: from_point,
                end_point: child.start_position(),
            });
        }
        if child.end_byte() > from_byte {
            from_byte = child.end_byte();
            from_point = child.end_position();
        }
    }
    if node.end_byte() > from_byte {
        out.push(tree_sitter::Range {
            start_byte: from_byte,
            end_byte: node.end_byte(),
            start_point: from_point,
            end_point: node.end_position(),
        });
    }
    out
}

/// The front matter node without its delimiter lines (`+++`, `---`): the
/// upstream query has no `#offset!`, and `+++` is not TOML. Empty without a
/// body.
fn metadata_body(node: Node, rope: &Rope) -> Vec<tree_sitter::Range> {
    let (start, end) = (node.start_byte(), node.end_byte().min(rope.len_bytes()));
    let text = rope.byte_slice(start..end).to_string();
    let Some(first) = text.find('\n').map(|i| i + 1) else { return Vec::new() };
    let trimmed = text.trim_end_matches(['\n', '\r']);
    let Some(last) = trimmed.rfind('\n').map(|i| i + 1) else { return Vec::new() };
    if last <= first {
        return Vec::new();
    }
    let row = node.start_position().row;
    let point = |at: usize| tree_sitter::Point { row: row + text[..at].matches('\n').count(), column: 0 };
    vec![tree_sitter::Range { start_byte: start + first, end_byte: start + last, start_point: point(first), end_point: point(last) }]
}

/// The included range a Markdown host parse restarts with when the
/// document's front matter block is not front matter: a line in it opens a
/// code fence (```` ``` ```` or `~~~` at column 0, never valid YAML or TOML
/// there). It starts at the opening line's line break, so the parser sees a
/// blank first line and no front matter. `None` for real front matter and
/// every other tree.
fn rejected_front_matter(tree: &Tree, rope: &Rope) -> Option<tree_sitter::Range> {
    let node = tree.root_node().named_child(0)?;
    if !matches!(node.kind(), "minus_metadata" | "plus_metadata") {
        return None;
    }
    let (start, end) = (node.start_byte(), node.end_byte().min(rope.len_bytes()));
    let text = rope.byte_slice(start..end).to_string();
    let first = text.find(['\r', '\n'])?;
    let plus = node.kind() == "plus_metadata";
    let mut offset = first;
    let mut fenced = false;
    while offset < text.len() {
        // Skip the line break: `\r\n`, `\n` or a lone `\r`, as tree-sitter-md ends lines.
        offset += if text[offset..].starts_with("\r\n") { 2 } else { 1 };
        let line = &text[offset..];
        if line.starts_with("```") || line.starts_with("~~~") {
            // A TOML multi-line string may hold a column-0 fence.
            let before = &text[..offset];
            if !(plus && (before.matches("'''").count() % 2 == 1 || before.matches("\"\"\"").count() % 2 == 1)) {
                fenced = true;
            }
            break;
        }
        match line.find(['\r', '\n']) {
            Some(i) => offset += i,
            None => break,
        }
    }
    if !fenced {
        return None;
    }
    Some(tree_sitter::Range {
        start_byte: start + first,
        end_byte: usize::MAX,
        start_point: Point::new(node.start_position().row, node.start_position().column + first),
        end_point: Point::new(usize::MAX, usize::MAX),
    })
}

/// Index into `LANGUAGES` of the language an injection names (a fence info
/// string such as `rust {.numberLines}`, or `ts`).
fn language_index(name: &str) -> Option<usize> {
    let word = name.trim().split(|c: char| c.is_whitespace() || c == ',' || c == '{').next()?;
    let def = language_for_alias(word)?;
    LANGUAGES.iter().position(|l| std::ptr::eq(l, def))
}

/// The injections whose content node meets `region`: one per content byte
/// range (the lowest pattern wins), sorted by start. Unknown languages and
/// empty content are skipped.
fn collect_injections(query: &Query, tree: &Tree, rope: &Rope, region: Range<usize>) -> Vec<Candidate> {
    let Some(content_ix) = query.capture_index_for_name("injection.content") else { return Vec::new() };
    let lang_ix = query.capture_index_for_name("injection.language");
    let mut cursor = QueryCursor::new();
    cursor.set_byte_range(region);
    let mut found: Vec<(Range<usize>, usize, Candidate)> = Vec::new();
    let mut it = cursor.matches(query, tree.root_node(), RopeProvider(rope));
    while let Some(m) = it.next() {
        let Some(content) = m.captures().iter().find(|c| c.index == content_ix) else { continue };
        let name = lang_ix
            .and_then(|ix| m.captures().iter().find(|c| c.index == ix))
            .map(|c| {
                let r = c.node.byte_range();
                let end = r.end.min(rope.len_bytes());
                rope.byte_slice(r.start.min(end)..end).to_string()
            })
            .or_else(|| {
                query
                    .property_settings(m.pattern_index)
                    .iter()
                    .find(|p| &*p.key == "injection.language")
                    .and_then(|p| p.value.as_deref().map(str::to_owned))
            });
        let Some(lang) = name.as_deref().and_then(language_index) else { continue };
        let ranges = match content.node.kind() {
            "plus_metadata" | "minus_metadata" => metadata_body(content.node, rope),
            _ => content_ranges(content.node),
        };
        if ranges.is_empty() {
            continue;
        }
        let bytes = ranges.iter().map(|r| r.start_byte..r.end_byte).collect();
        found.push((content.node.byte_range(), m.pattern_index, Candidate { lang, bytes, ranges }));
    }
    found.sort_by(|a, b| a.0.start.cmp(&b.0.start).then(a.0.end.cmp(&b.0.end)).then(a.1.cmp(&b.1)));
    keep_disjoint(&mut found);
    found.into_iter().map(|(_, _, c)| c).collect()
}

/// Drop the entries (sorted by start) that overlap an earlier kept one,
/// identical or nested: `tree_at` binary-searches disjoint spans.
fn keep_disjoint<T>(found: &mut Vec<(Range<usize>, usize, T)>) {
    let mut kept_end = 0;
    found.retain(|(r, _, _)| {
        let keep = r.start >= kept_end;
        if keep {
            kept_end = r.end;
        }
        keep
    });
}

/// Styled char ranges of `query` over `bytes` in `tree`. The first styled
/// pattern matching a node wins (tree-sitter-highlight semantics). With
/// `clip`, node ranges are cut to those byte ranges (an injection's
/// included ranges).
fn query_spans(
    query: &Query,
    styles: &[Option<Style>],
    tree: &Tree,
    rope: &Rope,
    bytes: Range<usize>,
    clip: Option<&[Range<usize>]>,
) -> Vec<(Range<usize>, Style)> {
    let mut cursor = QueryCursor::new();
    cursor.set_byte_range(bytes);
    let mut raw = Vec::new();
    let mut it = cursor.captures(query, tree.root_node(), RopeProvider(rope));
    while let Some((m, idx)) = it.next() {
        let cap = m.captures()[*idx];
        let r = cap.node.byte_range();
        if r.start >= rope.len_bytes() {
            continue;
        }
        // Captures the theme does not style never block other captures.
        let Some(style) = styles.get(cap.index as usize).copied().flatten() else { continue };
        raw.push((r, m.pattern_index, style));
    }
    raw.sort_by(|a, b| a.0.start.cmp(&b.0.start).then(b.0.end.cmp(&a.0.end)).then(a.1.cmp(&b.1)));
    raw.dedup_by(|b, a| a.0 == b.0);
    let mut spans = Vec::with_capacity(raw.len());
    for (r, _, style) in raw {
        let mut push = |r: Range<usize>| {
            let s = rope.byte_to_char(r.start);
            let e = rope.byte_to_char(r.end.min(rope.len_bytes()));
            if e > s {
                spans.push((s..e, style));
            }
        };
        match clip {
            None => push(r),
            Some(clip) => {
                for c in clip {
                    let piece = r.start.max(c.start)..r.end.min(c.end);
                    if piece.start < piece.end {
                        push(piece);
                    }
                }
            }
        }
    }
    spans
}

/// The rope's bytes from `byte` on, as the parser input callback wants them.
fn read_chunk(rope: &Rope, byte: usize) -> &[u8] {
    if byte >= rope.len_bytes() {
        return &[];
    }
    let (chunk, start, _, _) = rope.chunk_at_byte(byte);
    &chunk.as_bytes()[byte - start..]
}

/// `styles_for` of every language in `LANGUAGES`; empty when `def` has no
/// injection query.
fn inner_styles_for(def: &LanguageDef, theme: &Theme) -> Vec<Vec<Option<Style>>> {
    if def.injections.is_some() { LANGUAGES.iter().map(|d| styles_for(d, theme)).collect() } else { Vec::new() }
}

pub struct Syntax {
    pub def: &'static LanguageDef,
    parser: Parser,
    tree: Option<Tree>,
    styles: Vec<Option<Style>>,
    dirty: bool,
    /// Highlights of the last queried window, keyed by first line and rows.
    cache: Option<(u64, Highlights)>,
    /// Parses the injections (its language and included ranges are set per parse).
    inner: Parser,
    /// Sorted by start, disjoint.
    injections: Vec<Injection>,
    /// `styles_for` of every language in `LANGUAGES`, by index; empty when
    /// the language has no injection query.
    inner_styles: Vec<Vec<Option<Style>>>,
    /// Bytes edited since the last reparse, in the current text.
    edited: Option<Range<usize>>,
    #[cfg(test)]
    inner_parses: usize,
}

impl std::fmt::Debug for Syntax {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Syntax")
            .field("language", &self.def.name)
            .field("parsed", &self.tree.is_some())
            .field("injections", &self.injections.len())
            .finish()
    }
}

impl Syntax {
    pub fn new(def: &'static LanguageDef, theme: &Theme) -> Option<Self> {
        let mut parser = Parser::new();
        parser.set_language(&def.language).map_err(|e| tracing::error!("set_language {}: {e}", def.name)).ok()?;
        Some(Self {
            def,
            parser,
            tree: None,
            styles: styles_for(def, theme),
            dirty: true,
            cache: None,
            inner: Parser::new(),
            injections: Vec::new(),
            inner_styles: inner_styles_for(def, theme),
            edited: None,
            #[cfg(test)]
            inner_parses: 0,
        })
    }

    /// Take the capture styles from `theme` again (hot reload); the parse
    /// tree stays.
    pub fn restyle(&mut self, theme: &Theme) {
        self.styles = styles_for(self.def, theme);
        self.inner_styles = inner_styles_for(self.def, theme);
        self.cache = None;
    }

    #[cfg(test)]
    pub fn for_path(path: &Path, theme: &Theme) -> Option<Self> {
        Self::new(detect(path)?, theme)
    }

    /// Record an edit already applied to `rope` (positions are chars).
    pub fn note_change(&mut self, rope: &Rope, at: usize, _old_len: usize, new_len: usize, old: &str, new: &str) {
        self.dirty = true;
        self.cache = None;
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
        let edit = InputEdit {
            start_byte,
            old_end_byte,
            new_end_byte,
            start_position: Point::new(start_row, start_col),
            old_end_position,
            new_end_position,
        };
        if self.def.injections.is_some() {
            self.edited = Some(match self.edited.take() {
                Some(r) => {
                    start_byte.min(map_byte(r.start, start_byte, old_end_byte, new_end_byte))
                        ..map_byte(r.end, start_byte, old_end_byte, new_end_byte).max(new_end_byte)
                }
                None => start_byte..new_end_byte,
            });
            let changed = start_byte..old_end_byte;
            for inj in &mut self.injections {
                inj.dirty |= touches(&span(&inj.bytes), &changed);
                for b in &mut inj.bytes {
                    *b = map_byte(b.start, start_byte, old_end_byte, new_end_byte)..map_byte(b.end, start_byte, old_end_byte, new_end_byte);
                }
                inj.tree.edit(&edit);
            }
        }
        if let Some(tree) = &mut self.tree {
            tree.edit(&edit);
        }
    }

    pub fn reparse(&mut self, rope: &Rope) {
        if !self.dirty && self.tree.is_some() {
            return;
        }
        let old = if self.def.injections.is_some() { self.tree.clone() } else { None };
        let mut tree = self.parser.parse_with_options(&mut |byte, _| read_chunk(rope, byte), self.tree.as_ref(), None);
        if self.def.name == "markdown"
            && let Some(range) = tree.as_ref().and_then(|t| rejected_front_matter(t, rope))
            && self.parser.set_included_ranges(&[range]).is_ok()
        {
            let again = self.parser.parse_with_options(&mut |byte, _| read_chunk(rope, byte), tree.as_ref(), None);
            let _ = self.parser.set_included_ranges(&[]);
            if again.is_some() {
                tree = again;
            }
        }
        if tree.is_some() {
            self.tree = tree;
            self.dirty = false;
            self.update_injections(rope, old.as_ref());
        }
    }

    /// Bring the injections up to date with the new host tree: only the
    /// region that changed is queried, and an inner tree is parsed again
    /// only when an edit touched it or its ranges changed.
    fn update_injections(&mut self, rope: &Rope, old: Option<&Tree>) {
        let def = self.def;
        let Some(query) = def.injections_query() else { return };
        let edited = self.edited.take();
        let Some(new) = self.tree.as_ref() else { return };
        let len = rope.len_bytes();
        let region = match old {
            None => 0..len,
            Some(old) => {
                let (mut lo, mut hi) = (usize::MAX, 0usize);
                let mut add = |r: Range<usize>| {
                    lo = lo.min(r.start);
                    hi = hi.max(r.end);
                };
                if let Some(e) = edited {
                    add(e);
                }
                for r in old.changed_ranges(new) {
                    add(r.start_byte..r.end_byte);
                }
                if lo > hi {
                    return;
                }
                lo.saturating_sub(1).min(len)..(hi + 1).min(len)
            }
        };
        let found = collect_injections(query, new, rope, region.clone());
        let (mut kept, mut pool): (Vec<Injection>, Vec<Injection>) = std::mem::take(&mut self.injections).into_iter().partition(|inj| {
            let s = span(&inj.bytes);
            s.start < s.end && !touches(&s, &region) && !found.iter().any(|c| overlaps(&s, &span(&c.bytes)))
        });
        let mut fresh: Vec<Injection> = Vec::new();
        for c in found {
            if kept.len() + fresh.len() >= MAX_INJECTIONS {
                tracing::debug!("more than {MAX_INJECTIONS} injections; the rest keep the host highlighting");
                break;
            }
            let cs = span(&c.bytes);
            let reused = pool.iter().position(|p| p.lang == c.lang && overlaps(&span(&p.bytes), &cs)).map(|i| pool.remove(i));
            if let Some(p) = &reused
                && !p.dirty
                && p.bytes == c.bytes
            {
                fresh.push(reused.unwrap());
                continue;
            }
            if let Err(e) = self.inner.set_language(&LANGUAGES[c.lang].language) {
                tracing::debug!("injection set_language: {e}");
                continue;
            }
            if let Err(e) = self.inner.set_included_ranges(&c.ranges) {
                tracing::debug!("injection included ranges: {e}");
                continue;
            }
            #[cfg(test)]
            {
                self.inner_parses += 1;
            }
            let old_tree = reused.as_ref().map(|p| &p.tree);
            let Some(tree) = self.inner.parse_with_options(&mut |byte, _| read_chunk(rope, byte), old_tree, None) else { continue };
            fresh.push(Injection { lang: c.lang, bytes: c.bytes, tree, dirty: false });
        }
        kept.append(&mut fresh);
        kept.sort_by_key(|inj| span(&inj.bytes).start);
        for inj in &mut kept {
            inj.dirty = false;
        }
        self.injections = kept;
    }

    /// The host tree.
    pub fn tree(&self) -> Option<&Tree> {
        self.tree.as_ref()
    }

    /// The tree that owns byte `byte`: the injection whose ranges hold it
    /// (`start <= byte < end`), else the host tree.
    pub fn tree_at(&self, byte: usize) -> Option<&Tree> {
        let i = self.injections.partition_point(|inj| span(&inj.bytes).start <= byte);
        if let Some(inj) = i.checked_sub(1).map(|i| &self.injections[i])
            && inj.bytes.iter().any(|r| r.contains(&byte))
        {
            return Some(&inj.tree);
        }
        self.tree.as_ref()
    }

    /// `tree_at` for a char position (clamped to the text).
    pub fn tree_at_char(&self, rope: &Rope, pos: usize) -> Option<&Tree> {
        self.tree_at(rope.char_to_byte(pos.min(rope.len_chars())))
    }

    /// The host tree, then every injection's.
    pub fn trees(&self) -> impl Iterator<Item = &Tree> {
        self.tree.iter().chain(self.injections.iter().map(|inj| &inj.tree))
    }

    /// Language names of the injections, in text order.
    #[cfg(test)]
    pub fn injection_langs(&self) -> Vec<&'static str> {
        self.injections.iter().map(|inj| LANGUAGES[inj.lang].name).collect()
    }

    /// How many inner parses ran so far.
    #[cfg(test)]
    pub fn inner_parses(&self) -> usize {
        self.inner_parses
    }

    /// Highlights for `rows` lines starting at `first_line`.
    pub fn highlights(&mut self, rope: &Rope, first_line: usize, rows: usize) -> Highlights {
        let key = (first_line as u64) << 32 | rows as u64;
        if let Some((k, h)) = &self.cache
            && *k == key
            && !self.dirty
        {
            return h.clone();
        }
        self.reparse(rope);
        let Some(tree) = &self.tree else { return Highlights::default() };
        let last_line = (first_line + rows).min(rope.len_lines());
        let start_byte = rope.line_to_byte(first_line.min(rope.len_lines()));
        let end_byte = if last_line >= rope.len_lines() { rope.len_bytes() } else { rope.line_to_byte(last_line) };
        let mut spans = query_spans(&self.def.query, &self.styles, tree, rope, start_byte..end_byte, None);
        for inj in &self.injections {
            let s = span(&inj.bytes);
            if s.end <= start_byte || s.start >= end_byte {
                continue;
            }
            let Some(styles) = self.inner_styles.get(inj.lang) else { continue };
            spans.extend(query_spans(
                &LANGUAGES[inj.lang].query,
                styles,
                &inj.tree,
                rope,
                s.start.max(start_byte)..s.end.min(end_byte),
                Some(&inj.bytes),
            ));
        }
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

    #[cfg(feature = "lang-rust")]
    #[test]
    fn highlight_text_splits_spans_per_line() {
        let theme = Theme::default();
        let def = language_for_name("rust").expect("rust grammar");
        let lines = highlight_text(def, "fn a() {}\nlet x = 1;", &theme);
        assert_eq!(lines.len(), 2);
        let kw = theme.capture_style("keyword").unwrap();
        assert!(lines[0].contains(&(0..2, kw)));
        assert_eq!(lines[1][0].0.start, 0);
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn restyle_changes_highlight_styles() {
        use ratatui::style::Color;
        let def = language_for_name("rust").expect("rust grammar");
        let mut syn = Syntax::new(def, &Theme::darcula()).unwrap();
        let rope = Rope::from_str("fn main() {}\n");
        let before = syn.highlights(&rope, 0, 1).style_at(0).unwrap();
        assert_ne!(before.fg, Some(Color::Rgb(1, 2, 3)));
        let mut theme = Theme::darcula();
        theme.apply_toml("[syntax]\nkeyword = \"#010203\"\n", false).unwrap();
        syn.restyle(&theme);
        // Same window as before: the cache must not serve the old style.
        assert_eq!(syn.highlights(&rope, 0, 1).style_at(0).unwrap().fg, Some(Color::Rgb(1, 2, 3)));
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
        let tagged: Vec<&str> = LANGUAGES.iter().filter(|l| l.tags.is_some()).map(|l| l.name).collect();
        let broken: Vec<&str> = LANGUAGES.iter().filter(|l| l.tags.is_some() && l.tags_query().is_none()).map(|l| l.name).collect();
        assert!(broken.is_empty(), "tags query does not compile for {broken:?}");
        for name in ["rust", "javascript", "typescript", "tsx", "python", "go", "c", "cpp", "java"] {
            assert_eq!(tagged.contains(&name), names.contains(&name), "{name} has tags when enabled");
        }
        let injecting: Vec<&str> = LANGUAGES.iter().filter(|l| l.injections.is_some()).map(|l| l.name).collect();
        let broken: Vec<&str> =
            LANGUAGES.iter().filter(|l| l.injections.is_some() && l.injections_query().is_none()).map(|l| l.name).collect();
        assert!(broken.is_empty(), "injections query does not compile for {broken:?}");
        for name in ["markdown", "html"] {
            assert_eq!(injecting.contains(&name), names.contains(&name), "{name} has injections when enabled");
        }
        assert!(injecting.iter().all(|n| ["markdown", "html"].contains(n)), "no other language injects: {injecting:?}");
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

    #[cfg_attr(not(feature = "lang-rust"), allow(dead_code))]
    fn lang(path: &str, text: &str) -> Option<&'static str> {
        detect_text(Path::new(path), text).map(|l| l.name)
    }

    #[cfg(all(
        feature = "lang-python",
        feature = "lang-bash",
        feature = "lang-js",
        feature = "lang-ts",
        feature = "lang-lua",
        feature = "lang-php"
    ))]
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

    /// Style at byte `at` of `src`, highlighted as `file`.
    #[cfg(all(feature = "lang-md", feature = "lang-rust"))]
    fn style_at(file: &str, src: &str, at: usize) -> Option<Style> {
        let theme = Theme::default();
        let mut syn = Syntax::for_path(Path::new(file), &theme).unwrap_or_else(|| panic!("grammar for {file}"));
        let rope = Rope::from_str(src);
        let h = syn.highlights(&rope, 0, rope.len_lines());
        h.style_at(rope.byte_to_char(at))
    }

    /// A Markdown `Syntax` for `src`, highlighted once.
    #[cfg(feature = "lang-md")]
    fn md_syntax(src: &str) -> (Syntax, Rope) {
        let mut syn = Syntax::for_path(Path::new("a.md"), &Theme::default()).expect("markdown grammar");
        let rope = Rope::from_str(src);
        syn.highlights(&rope, 0, rope.len_lines());
        (syn, rope)
    }

    /// Insert `text` at byte `at` of `rope` and tell `syn`.
    #[cfg(feature = "lang-md")]
    fn insert_at(syn: &mut Syntax, rope: &mut Rope, at: usize, text: &str) {
        let c = rope.byte_to_char(at);
        rope.insert(c, text);
        syn.note_change(rope, c, 0, text.chars().count(), "", text);
    }

    #[cfg(all(feature = "lang-md", feature = "lang-rust"))]
    #[test]
    fn markdown_fence_is_highlighted_as_its_language() {
        let src = "# T\n\n```rust\nfn main() { let s = \"hi\"; }\n```\n\nprose fn here\n";
        assert_eq!(style_of("a.md", src, "fn main"), cap("keyword"));
        assert_eq!(style_of("a.md", src, "let"), cap("keyword"));
        assert_eq!(style_of("a.md", src, "\"hi"), cap("string"));
        assert_ne!(style_at("a.md", src, src.rfind("fn").unwrap()), cap("keyword"), "prose is not code");
    }

    #[cfg(feature = "lang-md")]
    #[test]
    fn unknown_fence_language_keeps_host_highlighting() {
        let src = "```nosuchlang\nfn x\n```\n";
        assert_eq!(style_of("a.md", src, "fn x"), cap("none"));
        let mut syn = Syntax::for_path(Path::new("a.md"), &Theme::default()).unwrap();
        let rope = Rope::from_str(src);
        syn.highlights(&rope, 0, rope.len_lines());
        assert!(syn.injection_langs().is_empty());
    }

    #[cfg(all(feature = "lang-html", feature = "lang-js", feature = "lang-css"))]
    #[test]
    fn html_script_style_and_vue_lang() {
        let src = "<script>let x = 1;</script>\n<style>a { color: red; }</style>\n";
        assert_eq!(style_of("a.html", src, "let"), cap("keyword"));
        assert_eq!(style_of("a.html", src, "color"), cap("property"));
        #[cfg(feature = "lang-ts")]
        {
            let vue = "<script lang=\"ts\">let n: number = 1;</script>\n";
            assert_eq!(style_of("a.vue", vue, "number"), cap("type.builtin"));
            let mut syn = Syntax::for_path(Path::new("a.vue"), &Theme::default()).unwrap();
            let rope = Rope::from_str(vue);
            syn.highlights(&rope, 0, rope.len_lines());
            assert_eq!(syn.injection_langs(), ["typescript"]);
        }
    }

    #[cfg(all(feature = "lang-md", feature = "lang-rust"))]
    #[test]
    fn editing_one_fence_reparses_only_that_fence() {
        let src = "```rust\nfn a() {}\n```\n\nprose\n\n```rust\nfn b() {}\n```\n";
        let (mut syn, mut rope) = md_syntax(src);
        let rows = rope.len_lines();
        assert_eq!(syn.inner_parses(), 2);
        let kw = cap("keyword");
        let at = src.find("{}").unwrap() + 1;
        insert_at(&mut syn, &mut rope, at, "let y = 2; ");
        let h = syn.highlights(&rope, 0, rows);
        assert_eq!(syn.inner_parses(), 3, "only the edited fence parses again");
        assert_eq!(h.style_at(rope.byte_to_char(at)), kw, "inserted let");
        let b_at = rope.to_string().find("fn b").unwrap();
        assert_eq!(h.style_at(rope.byte_to_char(b_at)), kw);
        let prose = rope.to_string().find("prose").unwrap();
        insert_at(&mut syn, &mut rope, prose, "more ");
        let h = syn.highlights(&rope, 0, rows);
        assert_eq!(syn.inner_parses(), 3, "prose edit parses no fence");
        let text = rope.to_string();
        assert_eq!(h.style_at(rope.byte_to_char(text.find("let y").unwrap())), kw);
        assert_eq!(h.style_at(rope.byte_to_char(text.find("fn b").unwrap())), kw, "shifted fence keeps its styles");
        assert_ne!(h.style_at(rope.byte_to_char(text.find("more").unwrap())), kw);
    }

    #[cfg(all(feature = "lang-md", feature = "lang-rust", feature = "lang-python"))]
    #[test]
    fn changing_the_fence_language_replaces_the_injection() {
        let src = "```rust\ndef f(): pass\n```\n";
        let (mut syn, mut rope) = md_syntax(src);
        assert_eq!(syn.injection_langs(), ["rust"]);
        let h = syn.highlights(&rope, 0, rope.len_lines());
        assert_ne!(h.style_at(rope.byte_to_char(src.find("def").unwrap())), cap("keyword"));
        let c = rope.byte_to_char(3);
        rope.remove(c..c + 4);
        syn.note_change(&rope, c, 4, 0, "rust", "");
        rope.insert(c, "python");
        syn.note_change(&rope, c, 0, 6, "", "python");
        let h = syn.highlights(&rope, 0, rope.len_lines());
        assert_eq!(syn.injection_langs(), ["python"]);
        assert_eq!(h.style_at(rope.byte_to_char(rope.to_string().find("def").unwrap())), cap("keyword"));
    }

    #[cfg(all(feature = "lang-md", feature = "lang-rust"))]
    #[test]
    fn deleting_a_fence_drops_its_injection() {
        let src = "```rust\nfn a() {}\n```\n";
        let (mut syn, mut rope) = md_syntax(src);
        assert_eq!(syn.injection_langs(), ["rust"]);
        let n = rope.len_chars();
        rope.remove(0..n);
        syn.note_change(&rope, 0, n, 0, src, "");
        let h = syn.highlights(&rope, 0, 1);
        assert!(syn.injection_langs().is_empty());
        assert!(h.spans.is_empty(), "{:?}", h.spans);
    }

    #[test]
    fn map_byte_follows_an_edit() {
        // Replace bytes 4..6 by 3 bytes.
        assert_eq!(map_byte(2, 4, 6, 7), 2, "before");
        assert_eq!(map_byte(5, 4, 6, 7), 7, "inside");
        assert_eq!(map_byte(9, 4, 6, 7), 10, "after");
        // Insertion at 4: start == old_end.
        assert_eq!(map_byte(4, 4, 4, 7), 4);
        assert_eq!(map_byte(5, 4, 4, 7), 8);
    }

    #[cfg(all(feature = "lang-md", feature = "lang-toml", feature = "lang-yaml"))]
    #[test]
    fn front_matter_delimiters_stay_out_of_the_inner_parser() {
        let toml = "+++\ntitle = \"x\"\n+++\n\n# H\n";
        let (syn, _) = md_syntax(toml);
        assert_eq!(syn.injection_langs(), ["toml"]);
        let inner = syn.trees().nth(1).unwrap();
        assert!(!inner.root_node().has_error(), "{}", inner.root_node().to_sexp());
        assert_eq!(style_of("a.md", toml, "title"), cap("property"));
        let yaml = "---\ntitle: x\n---\n\n# H\n";
        let (syn, _) = md_syntax(yaml);
        assert_eq!(syn.injection_langs(), ["yaml"]);
        assert!(!syn.trees().nth(1).unwrap().root_node().has_error());
        assert_eq!(style_of("a.md", yaml, "title"), cap("property"));
    }

    #[cfg(feature = "lang-md")]
    #[test]
    fn a_fence_in_front_matter_makes_it_markdown() {
        let src = "---\nintro\n\n```rust\nfn main() {}\n```\n\n---\n\n# H\n";
        let (syn, _) = md_syntax(src);
        let sexp = syn.tree().unwrap().root_node().to_sexp();
        assert!(sexp.contains("fenced_code_block") && sexp.contains("thematic_break"), "{sexp}");
        assert!(!sexp.contains("minus_metadata"), "{sexp}");
        assert!(cap("punctuation.delimiter").is_some());
        assert_eq!(style_of("a.md", src, "```rust"), cap("punctuation.delimiter"));
        assert_eq!(style_of("a.md", src, "# H"), cap("punctuation.special"));
        #[cfg(feature = "lang-rust")]
        {
            assert_eq!(syn.injection_langs(), ["rust"]);
            assert_eq!(style_of("a.md", src, "fn main"), cap("keyword"));
        }
        let toml = "+++\ntitle = \"x\"\n~~~\ny\n~~~\n+++\n";
        let (syn, _) = md_syntax(toml);
        let sexp = syn.tree().unwrap().root_node().to_sexp();
        assert!(!sexp.contains("plus_metadata") && sexp.contains("fenced_code_block"), "{sexp}");
    }

    #[cfg(all(feature = "lang-md", feature = "lang-yaml"))]
    #[test]
    fn front_matter_with_an_indented_fence_stays_front_matter() {
        let src = "---\nd: |\n  ```rust\n  x\n  ```\n---\n\n# H\n";
        let (syn, _) = md_syntax(src);
        assert_eq!(syn.tree().unwrap().root_node().named_child(0).unwrap().kind(), "minus_metadata");
        assert_eq!(syn.injection_langs(), ["yaml"]);
    }

    #[cfg(feature = "lang-md")]
    #[test]
    fn typing_a_fence_into_front_matter_reparses_it() {
        let src = "---\ntitle: x\n---\n\n# H\n";
        let (mut syn, mut rope) = md_syntax(src);
        let first = |syn: &Syntax| syn.tree().unwrap().root_node().named_child(0).unwrap().kind().to_string();
        assert_eq!(first(&syn), "minus_metadata");
        let at = src.find("---\n\n").unwrap();
        insert_at(&mut syn, &mut rope, at, "```\n");
        syn.highlights(&rope, 0, rope.len_lines());
        assert_eq!(first(&syn), "section");
        assert!(syn.tree().unwrap().root_node().to_sexp().contains("fenced_code_block"));
        let c = rope.byte_to_char(at);
        rope.remove(c..c + 4);
        syn.note_change(&rope, c, 4, 0, "```\n", "");
        syn.highlights(&rope, 0, rope.len_lines());
        assert_eq!(first(&syn), "minus_metadata");
        #[cfg(feature = "lang-yaml")]
        assert_eq!(syn.injection_langs(), ["yaml"]);
    }

    #[cfg(feature = "lang-md")]
    #[test]
    fn a_toml_multiline_string_may_hold_a_fence() {
        let src = "+++\ndesc = '''\n```\nx\n```\n'''\n+++\n\n# H\n";
        let (syn, _) = md_syntax(src);
        assert_eq!(syn.tree().unwrap().root_node().named_child(0).unwrap().kind(), "plus_metadata");
    }

    #[cfg(feature = "lang-md")]
    #[test]
    fn a_lone_cr_line_break_ends_a_line_for_the_fence_check() {
        let src = "---\rintro\r```\rx\r```\r---\r\r# H\r";
        let (syn, _) = md_syntax(src);
        let sexp = syn.tree().unwrap().root_node().to_sexp();
        assert!(!sexp.contains("minus_metadata") && sexp.contains("fenced_code_block"), "{sexp}");
    }

    #[cfg(feature = "lang-md")]
    #[test]
    fn the_closing_rule_toggles_a_rejected_block() {
        let src = "---\nintro\n```\nx\n```\n---\n\n# H\n";
        let (mut syn, mut rope) = md_syntax(src);
        let sexp = |syn: &Syntax| syn.tree().unwrap().root_node().to_sexp();
        let state = |syn: &Syntax| (sexp(syn).starts_with("(document (section (thematic_break)"), sexp(syn).contains("metadata"));
        assert_eq!(state(&syn), (false, false), "{}", sexp(&syn));
        let at = src.rfind("---\n").unwrap();
        let c = rope.byte_to_char(at);
        rope.remove(c..c + 4);
        syn.note_change(&rope, c, 4, 0, "---\n", "");
        syn.highlights(&rope, 0, rope.len_lines());
        assert_eq!(state(&syn), (true, false), "{}", sexp(&syn));
        insert_at(&mut syn, &mut rope, at, "---\n");
        syn.highlights(&rope, 0, rope.len_lines());
        assert_eq!(state(&syn), (false, false), "{}", sexp(&syn));
    }

    #[test]
    fn nested_injection_candidates_stay_disjoint() {
        let mut found = vec![(0..10, 0, 'a'), (0..10, 1, 'b'), (2..5, 0, 'c'), (10..12, 0, 'd'), (11..20, 0, 'e'), (20..30, 0, 'f')];
        keep_disjoint(&mut found);
        assert_eq!(found.iter().map(|f| f.2).collect::<String>(), "adf");
    }
}
