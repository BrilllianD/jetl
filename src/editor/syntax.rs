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
}

fn make(name: &'static str, extensions: &'static [&'static str], filenames: &'static [&'static str], language: Language, queries: &[&str], line_comment: Option<&'static str>) -> Option<LanguageDef> {
    // Try the full set first; some grammars ship queries that reference
    // nodes only present in a sibling grammar, so fall back progressively.
    for n in (1..=queries.len()).rev() {
        let src: String = queries[..n].join("\n");
        match Query::new(&language, &src) {
            Ok(query) => return Some(LanguageDef { name, extensions, filenames, language: language.clone(), query, line_comment, block_comment: block_comment(name), indent_after_colon: matches!(name, "python" | "yaml") }),
            Err(e) => tracing::warn!("highlight query for {name} ({n} parts): {e}"),
        }
    }
    None
}

fn block_comment(name: &str) -> Option<(&'static str, &'static str)> {
    match name {
        "rust" | "javascript" | "typescript" | "tsx" | "csharp" | "css" | "scss" => Some(("/*", "*/")),
        "html" | "xml" | "markdown" => Some(("<!--", "-->")),
        _ => None,
    }
}

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
        ];
        let names: Vec<&str> = LANGUAGES.iter().map(|l| l.name).collect();
        assert_eq!(names.len(), enabled.iter().filter(|e| **e).count(), "loaded: {names:?}");
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
}
