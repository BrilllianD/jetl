//! Snippet variable values (`$TM_FILENAME`, `$CURRENT_YEAR`, ...): what a
//! snippet body's variables stand for at one expansion. Pure: the clock, the
//! buffer and the workspace root come in as arguments, so the values are
//! testable with a fixed time. `app/completion.rs` (`completion_apply`)
//! builds it and passes `get` to the snippet parser; `app/snippets.rs`
//! supplies the clock.
//!
//! Names (case-sensitive; any other name, and a file name of an untitled
//! buffer, is unresolved and the snippet's default is used):
//! - `TM_SELECTED_TEXT`, `SELECTION`: the selected text.
//! - `TM_CURRENT_LINE`, `TM_CURRENT_WORD`: the line, and the word at the start
//!   of the replaced range, before the edit.
//! - `TM_LINE_INDEX` (from 0), `TM_LINE_NUMBER` (from 1).
//! - `TM_FILENAME`, `TM_FILENAME_BASE`, `TM_DIRECTORY`, `TM_FILEPATH`,
//!   `RELATIVE_FILEPATH`: the file.
//! - `WORKSPACE_NAME`, `WORKSPACE_FOLDER`: the project root.
//! - `CURSOR_INDEX` (from 0), `CURSOR_NUMBER` (from 1): which selection.
//! - `CURRENT_YEAR`, `CURRENT_YEAR_SHORT`, `CURRENT_MONTH`,
//!   `CURRENT_MONTH_NAME`, `CURRENT_MONTH_NAME_SHORT`, `CURRENT_DATE`,
//!   `CURRENT_DAY_NAME`, `CURRENT_DAY_NAME_SHORT`, `CURRENT_HOUR`,
//!   `CURRENT_MINUTE`, `CURRENT_SECOND`, `CURRENT_SECONDS_UNIX`,
//!   `CURRENT_TIMEZONE_OFFSET`: local time.
//! - `RANDOM`, `RANDOM_HEX`, `UUID`: fresh on every call.
//! - `LINE_COMMENT`, `BLOCK_COMMENT_START`, `BLOCK_COMMENT_END`: the
//!   language's comment tokens.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::blame::civil;
use super::buffer::Buffer;
use super::selection::{is_word_char, line_len_no_eol};

const MONTHS: [&str; 12] =
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnippetVars {
    pub path: Option<PathBuf>,
    pub root: PathBuf,
    pub line: usize,
    pub current_line: String,
    pub current_word: String,
    pub selected: String,
    pub cursor: usize,
    pub line_comment: Option<&'static str>,
    pub block_comment: Option<(&'static str, &'static str)>,
    /// Unix seconds.
    pub now: i64,
    /// Seconds east of UTC at `now`.
    pub offset: i64,
}

impl SnippetVars {
    /// Values for an expansion replacing `range` of `b` (clamped to the
    /// text). No selection: `selected` is empty and `cursor` 0; a caller with
    /// a selection sets them.
    pub fn new(b: &Buffer, root: &Path, range: std::ops::Range<usize>, now: i64, offset: i64) -> Self {
        let rope = &b.rope;
        let start = range.start.min(rope.len_chars());
        let line = rope.char_to_line(start);
        let line_start = rope.line_to_char(line);
        let len = line_len_no_eol(rope, line);
        let current_line: String = rope.line(line).chars().take(len).collect();
        let chars: Vec<char> = current_line.chars().collect();
        let col = (start - line_start).min(chars.len());
        let from = chars[..col].iter().rposition(|&c| !is_word_char(c)).map_or(0, |p| p + 1);
        let to = chars[col..].iter().position(|&c| !is_word_char(c)).map_or(chars.len(), |p| col + p);
        let lang = b.language();
        Self {
            path: b.path.clone(),
            root: root.to_path_buf(),
            line,
            current_line,
            current_word: chars[from..to].iter().collect(),
            selected: String::new(),
            cursor: 0,
            line_comment: lang.and_then(|l| l.line_comment),
            block_comment: lang.and_then(|l| l.block_comment),
            now,
            offset,
        }
    }

    /// The value of variable `name`; `None` when it is unknown or cannot be
    /// answered here.
    pub fn get(&self, name: &str) -> Option<String> {
        let path = || self.path.as_deref();
        let text = |p: &Path| p.to_string_lossy().into_owned();
        let t = self.now + self.offset;
        let days = t.div_euclid(86_400);
        let secs = t.rem_euclid(86_400);
        let (year, month, day) = civil(days);
        let weekday = (days + 4).rem_euclid(7) as usize;
        let month_name = MONTHS[(month - 1) as usize];
        let day_name = DAYS[weekday];
        Some(match name {
            "TM_SELECTED_TEXT" | "SELECTION" => self.selected.clone(),
            "TM_CURRENT_LINE" => self.current_line.clone(),
            "TM_CURRENT_WORD" => self.current_word.clone(),
            "TM_LINE_INDEX" => self.line.to_string(),
            "TM_LINE_NUMBER" => (self.line + 1).to_string(),
            "TM_FILENAME" => path()?.file_name().map(|n| n.to_string_lossy().into_owned())?,
            "TM_FILENAME_BASE" => path()?.file_stem().map(|n| n.to_string_lossy().into_owned())?,
            "TM_DIRECTORY" => text(path()?.parent()?),
            "TM_FILEPATH" => text(path()?),
            "RELATIVE_FILEPATH" => {
                let p = path()?;
                text(p.strip_prefix(&self.root).unwrap_or(p))
            }
            "WORKSPACE_NAME" => self.root.file_name().map(|n| n.to_string_lossy().into_owned())?,
            "WORKSPACE_FOLDER" => text(&self.root),
            "CURSOR_INDEX" => self.cursor.to_string(),
            "CURSOR_NUMBER" => (self.cursor + 1).to_string(),
            "CURRENT_YEAR" => format!("{year:04}"),
            "CURRENT_YEAR_SHORT" => format!("{:02}", year.rem_euclid(100)),
            "CURRENT_MONTH" => format!("{month:02}"),
            "CURRENT_MONTH_NAME" => month_name.to_string(),
            "CURRENT_MONTH_NAME_SHORT" => month_name[..3].to_string(),
            "CURRENT_DATE" => format!("{day:02}"),
            "CURRENT_DAY_NAME" => day_name.to_string(),
            "CURRENT_DAY_NAME_SHORT" => day_name[..3].to_string(),
            "CURRENT_HOUR" => format!("{:02}", secs / 3600),
            "CURRENT_MINUTE" => format!("{:02}", secs / 60 % 60),
            "CURRENT_SECOND" => format!("{:02}", secs % 60),
            "CURRENT_SECONDS_UNIX" => self.now.to_string(),
            "CURRENT_TIMEZONE_OFFSET" => {
                let m = self.offset.abs() / 60;
                format!("{}{:02}:{:02}", if self.offset < 0 { '-' } else { '+' }, m / 60, m % 60)
            }
            "RANDOM" => format!("{:06}", random_u64() % 1_000_000),
            "RANDOM_HEX" => format!("{:06x}", random_u64() & 0xff_ffff),
            "UUID" => uuid(),
            "LINE_COMMENT" => self.line_comment?.to_string(),
            "BLOCK_COMMENT_START" => self.block_comment?.0.to_string(),
            "BLOCK_COMMENT_END" => self.block_comment?.1.to_string(),
            _ => return None,
        })
    }
}

/// A version 4 UUID, lowercase, `8-4-4-4-12`.
fn uuid() -> String {
    let mut b = [random_u64().to_be_bytes(), random_u64().to_be_bytes()].concat();
    b[6] = b[6] & 0x0f | 0x40;
    b[8] = b[8] & 0x3f | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// A number that differs between calls: a hash of a counter and the clock's
/// nanoseconds under a randomly keyed hasher. Not cryptographic.
fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    h.write_u64(nanos);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "fn main() {\n    let foo_bar = 1;\n}\n";

    fn vars(path: Option<&str>, offset: i64) -> SnippetVars {
        let b = Buffer::from_text(1, path.map(PathBuf::from), TEXT, false);
        let at = TEXT.find("bar").unwrap();
        SnippetVars::new(&b, Path::new("/w"), at..at, 1_700_000_000, offset)
    }

    fn get(v: &SnippetVars, n: &str) -> String {
        v.get(n).unwrap_or_else(|| panic!("{n} is unresolved"))
    }

    #[test]
    fn file_line_and_word_names() {
        let v = vars(Some("/w/src/a.rs"), 3 * 3600);
        for (n, want) in [
            ("TM_FILENAME", "a.rs"),
            ("TM_FILENAME_BASE", "a"),
            ("TM_DIRECTORY", "/w/src"),
            ("TM_FILEPATH", "/w/src/a.rs"),
            ("RELATIVE_FILEPATH", "src/a.rs"),
            ("WORKSPACE_NAME", "w"),
            ("WORKSPACE_FOLDER", "/w"),
            ("TM_LINE_INDEX", "1"),
            ("TM_LINE_NUMBER", "2"),
            ("TM_CURRENT_LINE", "    let foo_bar = 1;"),
            ("TM_CURRENT_WORD", "foo_bar"),
            ("TM_SELECTED_TEXT", ""),
            ("SELECTION", ""),
            ("CURSOR_INDEX", "0"),
            ("CURSOR_NUMBER", "1"),
        ] {
            assert_eq!(get(&v, n), want, "{n}");
        }
        let v = SnippetVars { cursor: 2, selected: "s".into(), ..v };
        assert_eq!((get(&v, "CURSOR_NUMBER"), get(&v, "SELECTION")), ("3".into(), "s".into()));
        assert_eq!(v.get("NOPE"), None);
    }

    #[test]
    fn date_and_time_names() {
        let v = vars(None, 3 * 3600);
        for (n, want) in [
            ("CURRENT_YEAR", "2023"),
            ("CURRENT_YEAR_SHORT", "23"),
            ("CURRENT_MONTH", "11"),
            ("CURRENT_MONTH_NAME", "November"),
            ("CURRENT_MONTH_NAME_SHORT", "Nov"),
            ("CURRENT_DATE", "15"),
            ("CURRENT_DAY_NAME", "Wednesday"),
            ("CURRENT_DAY_NAME_SHORT", "Wed"),
            ("CURRENT_HOUR", "01"),
            ("CURRENT_MINUTE", "13"),
            ("CURRENT_SECOND", "20"),
            ("CURRENT_SECONDS_UNIX", "1700000000"),
            ("CURRENT_TIMEZONE_OFFSET", "+03:00"),
        ] {
            assert_eq!(get(&v, n), want, "{n}");
        }
        let v = SnippetVars { offset: -(7 * 3600 + 30 * 60), ..v };
        assert_eq!(get(&v, "CURRENT_TIMEZONE_OFFSET"), "-07:30");
        assert_eq!((get(&v, "CURRENT_DATE"), get(&v, "CURRENT_DAY_NAME")), ("14".into(), "Tuesday".into()));
    }

    #[test]
    fn random_names_and_untitled_buffers() {
        let v = vars(None, 0);
        let r = get(&v, "RANDOM");
        assert!(r.len() == 6 && r.bytes().all(|c| c.is_ascii_digit()), "{r}");
        let h = get(&v, "RANDOM_HEX");
        assert!(h.len() == 6 && h.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)), "{h}");
        let re = regex::Regex::new("^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$").unwrap();
        let (a, b) = (get(&v, "UUID"), get(&v, "UUID"));
        assert!(re.is_match(&a) && re.is_match(&b), "{a} {b}");
        assert_ne!(a, b);
        for n in ["TM_FILENAME", "TM_FILENAME_BASE", "TM_DIRECTORY", "TM_FILEPATH", "RELATIVE_FILEPATH"] {
            assert_eq!(v.get(n), None, "{n}");
        }
        assert_eq!(v.get("LINE_COMMENT"), None);
        assert_eq!(vars(Some("/elsewhere/a.txt"), 0).get("RELATIVE_FILEPATH").as_deref(), Some("/elsewhere/a.txt"));
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn comment_tokens_follow_the_language() {
        let v = vars(Some("/w/src/a.rs"), 0);
        assert_eq!(get(&v, "LINE_COMMENT"), "//");
        assert_eq!((get(&v, "BLOCK_COMMENT_START"), get(&v, "BLOCK_COMMENT_END")), ("/*".into(), "*/".into()));
    }
}
