//! LSP semantic tokens: the capture name a token type (plus modifiers) maps
//! to, and the decoder of a server's relative `data` array against the
//! server's legend into char ranges of the document.

use std::collections::HashMap;
use std::ops::Range;

use lsp_types::{Position, SemanticToken, SemanticTokens, SemanticTokensLegend};
use ropey::Rope;
use serde_json::Value;

use super::pos;

/// The theme capture for a token type and its modifiers, or `None` for a
/// kind that gets no colour of its own (punctuation, unresolved names, ...).
/// A `comment` with the `documentation` modifier is `comment.documentation`.
pub fn capture_for(token_type: &str, modifiers: &[&str]) -> Option<String> {
    let base = match token_type {
        "function" => "function",
        "method" => "function.method",
        "macro" => "function.macro",
        "parameter" => "variable.parameter",
        "variable" => "variable",
        "property" => "property",
        "enumMember" => "constant",
        "namespace" => "module",
        "type" | "class" | "struct" | "enum" | "typeAlias" | "union" => "type",
        "interface" | "trait" => "type.interface",
        "typeParameter" => "type.parameter",
        "keyword" | "modifier" => "keyword",
        "comment" => "comment",
        "string" => "string",
        "number" => "number",
        "regexp" => "string.regexp",
        "operator" => "operator",
        "decorator" | "attribute" | "derive" | "builtinAttribute" => "attribute",
        "lifetime" | "label" => "label",
        "selfKeyword" => "variable.builtin",
        "builtinType" | "selfTypeKeyword" => "type.builtin",
        "boolean" => "boolean",
        "escapeSequence" | "formatSpecifier" => "string.escape",
        _ => return None,
    };
    // Doc comments keep the doc-comment colour tree-sitter gives them.
    if base == "comment" && modifiers.contains(&"documentation") {
        return Some("comment.documentation".into());
    }
    let suffix = [("mutable", ".mutable"), ("defaultLibrary", ".builtin"), ("readonly", ".readonly")]
        .into_iter()
        .find(|(m, _)| modifiers.contains(m))
        .map_or("", |(_, s)| s);
    Some(format!("{base}{suffix}"))
}

/// The tokens of a `SemanticTokens | null` answer; anything else is empty.
pub fn parse(value: Value) -> Vec<SemanticToken> {
    match serde_json::from_value::<Option<SemanticTokens>>(value) {
        Ok(t) => t.map(|t| t.data).unwrap_or_default(),
        Err(e) => {
            tracing::debug!("semantic tokens: {e}");
            Vec::new()
        }
    }
}

/// Decode relative `data` against `legend` into `(char range of rope,
/// capture)`, in data order. A token on a missing line, with a type past the
/// legend, without a capture or empty after clamping is dropped.
pub fn decode(rope: &Rope, data: &[SemanticToken], legend: &SemanticTokensLegend) -> Vec<(Range<usize>, String)> {
    let mut out = Vec::with_capacity(data.len());
    let mut cache: HashMap<(u32, u32), Option<String>> = HashMap::new();
    let (mut line, mut start) = (0u32, 0u32);
    for t in data {
        if t.delta_line > 0 {
            line = line.saturating_add(t.delta_line);
            start = t.delta_start;
        } else {
            start = start.saturating_add(t.delta_start);
        }
        if line as usize >= rope.len_lines() {
            continue;
        }
        let capture = cache.entry((t.token_type, t.token_modifiers_bitset)).or_insert_with(|| {
            let ty = legend.token_types.get(t.token_type as usize)?;
            let mods: Vec<&str> = (0..32)
                .filter(|i| t.token_modifiers_bitset & (1 << i) != 0)
                .filter_map(|i| legend.token_modifiers.get(i as usize))
                .map(|m| m.as_str())
                .collect();
            capture_for(ty.as_str(), &mods)
        });
        let Some(name) = capture else { continue };
        let from = pos::char_index(rope, Position { line, character: start });
        let to = pos::char_index(rope, Position { line, character: start.saturating_add(t.length) });
        if from < to {
            out.push((from..to, name.clone()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn capture_table() {
        let rows = [
            ("function", "function"),
            ("method", "function.method"),
            ("macro", "function.macro"),
            ("parameter", "variable.parameter"),
            ("variable", "variable"),
            ("property", "property"),
            ("enumMember", "constant"),
            ("namespace", "module"),
            ("type", "type"),
            ("class", "type"),
            ("struct", "type"),
            ("enum", "type"),
            ("typeAlias", "type"),
            ("union", "type"),
            ("interface", "type.interface"),
            ("trait", "type.interface"),
            ("typeParameter", "type.parameter"),
            ("keyword", "keyword"),
            ("modifier", "keyword"),
            ("comment", "comment"),
            ("string", "string"),
            ("number", "number"),
            ("regexp", "string.regexp"),
            ("operator", "operator"),
            ("decorator", "attribute"),
            ("attribute", "attribute"),
            ("derive", "attribute"),
            ("builtinAttribute", "attribute"),
            ("lifetime", "label"),
            ("label", "label"),
            ("selfKeyword", "variable.builtin"),
            ("builtinType", "type.builtin"),
            ("selfTypeKeyword", "type.builtin"),
            ("boolean", "boolean"),
            ("escapeSequence", "string.escape"),
            ("formatSpecifier", "string.escape"),
        ];
        for (ty, want) in rows {
            assert_eq!(capture_for(ty, &[]).as_deref(), Some(want), "{ty}");
        }
        for ty in ["punctuation", "generic", "unresolvedReference", "event", "whatever"] {
            assert_eq!(capture_for(ty, &[]), None, "{ty}");
        }
    }

    #[test]
    fn capture_modifier_suffix() {
        let rows: [(&str, &[&str], Option<&str>); 6] = [
            ("variable", &["mutable"], Some("variable.mutable")),
            ("function", &["defaultLibrary"], Some("function.builtin")),
            ("variable", &["readonly"], Some("variable.readonly")),
            ("variable", &["readonly", "mutable"], Some("variable.mutable")),
            ("variable", &["declaration"], Some("variable")),
            ("generic", &["mutable"], None),
        ];
        for (ty, mods, want) in rows {
            assert_eq!(capture_for(ty, mods).as_deref(), want, "{ty} {mods:?}");
        }
    }

    #[test]
    fn doc_comments_keep_their_capture() {
        let rows: [(&str, &[&str], Option<&str>); 4] = [
            ("comment", &["documentation"], Some("comment.documentation")),
            ("comment", &[], Some("comment")),
            ("comment", &["documentation", "readonly"], Some("comment.documentation")),
            ("function", &["documentation"], Some("function")),
        ];
        for (ty, mods, want) in rows {
            assert_eq!(capture_for(ty, mods).as_deref(), want, "{ty} {mods:?}");
        }
    }

    #[test]
    fn parse_answers() {
        assert!(parse(Value::Null).is_empty());
        let t = parse(json!({"data": [0, 3, 4, 0, 0]}));
        assert_eq!(t.len(), 1);
        assert_eq!((t[0].delta_line, t[0].delta_start, t[0].length, t[0].token_type, t[0].token_modifiers_bitset), (0, 3, 4, 0, 0));
        assert!(parse(json!({"resultId": "1", "data": []})).is_empty());
        assert!(parse(json!({"nope": 1})).is_empty());
    }

    fn legend() -> SemanticTokensLegend {
        serde_json::from_value(json!({"tokenTypes": ["function", "variable", "generic"], "tokenModifiers": ["declaration", "mutable"]}))
            .unwrap()
    }

    fn dec(text: &str, data: Value) -> Vec<(Range<usize>, String)> {
        let toks = parse(json!({ "data": data }));
        decode(&Rope::from_str(text), &toks, &legend())
    }

    fn s(r: Range<usize>, n: &str) -> (Range<usize>, String) {
        (r, n.to_string())
    }

    #[test]
    fn decode_utf16_columns() {
        let got = dec("a😀b c\nd\n", json!([0, 3, 1, 0, 0, 0, 2, 1, 1, 2, 1, 0, 1, 1, 0]));
        assert_eq!(got, [s(2..3, "function"), s(4..5, "variable.mutable"), s(6..7, "variable")]);
        assert_eq!(dec("a😀b c\nd\n", json!([0, 1, 2, 0, 0])), [s(1..2, "function")]);
    }

    #[test]
    fn decode_drops_bad_tokens_and_keeps_running_position() {
        // Line 5 past the end of a two-line rope is dropped.
        assert!(dec("ab\ncd\n", json!([5, 0, 1, 0, 0])).is_empty());
        // Type index 7 and a `generic` token are dropped, the others keep their place.
        let got = dec("abcdef\n", json!([0, 0, 1, 0, 0, 0, 1, 1, 7, 0, 0, 1, 1, 2, 0, 0, 1, 1, 1, 0]));
        assert_eq!(got, [s(0..1, "function"), s(3..4, "variable")]);
        // A start past the line end is an empty range: dropped, later tokens still counted.
        let got = dec("ab\ncd\n", json!([0, 9, 1, 0, 0, 1, 0, 1, 1, 0]));
        assert_eq!(got, [s(3..4, "variable")]);
    }

    #[test]
    fn decode_clamps_length_to_the_line_end() {
        assert_eq!(dec("abc\ndef\n", json!([0, 1, 50, 0, 0])), [s(1..3, "function")]);
    }
}
