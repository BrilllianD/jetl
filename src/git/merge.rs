//! Three-way merge of a conflicted file for the conflict resolver: the
//! stages read from the index (`repo::conflict_sides`), or the conflict
//! markers in the work file when the index has none, split into chunks of
//! lines where yours or theirs differ from the base. Pure: no git, no
//! editor.

use std::ops::Range;

use anyhow::{Result, bail};

use super::diff::{hunks, is_binary, strip_eol};

/// A conflicted path that is not a regular file on some side: only
/// taking a side whole makes sense.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Special {
    Symlink,
    Submodule,
}

/// What the index and the work tree hold for a conflicted path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConflictSides {
    /// The path relative to the work tree, `/`-separated.
    pub rel: String,
    /// Stage 1 (the common ancestor), 2 (yours, HEAD) and 3 (theirs);
    /// `None` when the stage is missing (added on both sides, deleted on
    /// one side).
    pub base: Option<Vec<u8>>,
    pub ours: Option<Vec<u8>>,
    pub theirs: Option<Vec<u8>>,
    /// The work tree file (with git's conflict markers); empty when gone.
    pub work: Vec<u8>,
    /// What yours and theirs are: a branch name or a short commit id, with
    /// what the operation makes of it during a rebase or cherry-pick.
    pub ours_label: String,
    pub theirs_label: String,
    /// Set when a stage is a symbolic link or a submodule; then
    /// `ours`/`theirs` hold the link target or the commit id in hex.
    pub special: Option<Special>,
}

/// The three texts the resolver shows, line endings made alike.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeTexts {
    pub base: String,
    pub ours: String,
    pub theirs: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeInput {
    Text(MergeTexts),
    /// No line merge possible (a binary side, a side deleted): only taking
    /// yours or theirs whole. The reason is shown.
    SidesOnly(String),
}

impl MergeInput {
    /// Decide how `s` can be merged. A missing base (added on both sides)
    /// is empty. Without any stage the work file's conflict markers are
    /// parsed. When the sides disagree on CRLF and LF, all three take the
    /// work file's line ending, which the result keeps.
    pub fn classify(s: &ConflictSides) -> Result<MergeInput> {
        match s.special {
            Some(Special::Symlink) => return Ok(MergeInput::SidesOnly(format!("{} is a symbolic link", s.rel))),
            Some(Special::Submodule) => return Ok(MergeInput::SidesOnly(format!("{} is a submodule", s.rel))),
            None => {}
        }
        if s.base.is_none() && s.ours.is_none() && s.theirs.is_none() {
            if is_binary(&s.work) {
                return Ok(MergeInput::SidesOnly(format!("{} is binary", s.rel)));
            }
            let Ok(work) = std::str::from_utf8(&s.work) else {
                return Ok(MergeInput::SidesOnly(format!("{} is not UTF-8 text", s.rel)));
            };
            return parse_markers(work).map(MergeInput::Text);
        }
        let (Some(ours), Some(theirs)) = (&s.ours, &s.theirs) else {
            let side = if s.ours.is_none() { "yours" } else { "theirs" };
            return Ok(MergeInput::SidesOnly(format!("{} was deleted in {side}", s.rel)));
        };
        let base = s.base.as_deref().unwrap_or_default();
        if [base, ours, theirs].into_iter().any(is_binary) {
            return Ok(MergeInput::SidesOnly(format!("{} is binary", s.rel)));
        }
        let text = |b: &[u8]| std::str::from_utf8(b).map(str::to_string);
        let (Ok(base), Ok(ours), Ok(theirs)) = (text(base), text(ours), text(theirs)) else {
            return Ok(MergeInput::SidesOnly(format!("{} is not UTF-8 text", s.rel)));
        };
        let mut t = MergeTexts { base, ours, theirs };
        let styles: Vec<Eol> = [&t.base, &t.ours, &t.theirs].into_iter().filter_map(|x| eol_style(x.as_str())).collect();
        if styles.windows(2).any(|w| w[0] != w[1]) {
            let crlf = std::str::from_utf8(&s.work).ok().and_then(eol_style) == Some(Eol::CrLf);
            for x in [&mut t.base, &mut t.ours, &mut t.theirs] {
                *x = with_eol(x, crlf);
            }
        }
        Ok(MergeInput::Text(t))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Eol {
    Lf,
    CrLf,
    Mixed,
}

/// The line endings of `s`, `None` without any.
fn eol_style(s: &str) -> Option<Eol> {
    let crlf = s.matches("\r\n").count();
    let lf = s.matches('\n').count() - crlf;
    match (crlf, lf) {
        (0, 0) => None,
        (_, 0) => Some(Eol::CrLf),
        (0, _) => Some(Eol::Lf),
        _ => Some(Eol::Mixed),
    }
}

fn with_eol(s: &str, crlf: bool) -> String {
    let lf = s.replace("\r\n", "\n");
    if crlf { lf.replace('\n', "\r\n") } else { lf }
}

/// Whether `line` (without its terminator) is a conflict marker of `c`:
/// seven of it, alone or followed by a space and a label.
fn is_marker(line: &str, c: char) -> bool {
    let n = line.chars().take_while(|x| *x == c).count();
    n == 7 && (line.len() == 7 || line[7..].starts_with(' '))
}

/// The base, yours and theirs of a work file with conflict markers. A
/// region with a `|||||||` part (diff3 style) has that base; one without
/// has an empty base, so both sides count as added there.
pub fn parse_markers(work: &str) -> Result<MergeTexts> {
    #[derive(PartialEq)]
    enum At {
        Out,
        Ours,
        Base,
        Theirs,
    }
    let (mut base, mut ours, mut theirs) = (String::new(), String::new(), String::new());
    let (mut at, mut found) = (At::Out, false);
    for line in work.split_inclusive('\n') {
        let bare = strip_eol(line);
        match at {
            At::Out if is_marker(bare, '<') => (at, found) = (At::Ours, true),
            At::Out => {
                base.push_str(line);
                ours.push_str(line);
                theirs.push_str(line);
            }
            At::Ours if is_marker(bare, '|') => at = At::Base,
            At::Ours | At::Base if is_marker(bare, '=') => at = At::Theirs,
            At::Ours => ours.push_str(line),
            At::Base => base.push_str(line),
            At::Theirs if is_marker(bare, '>') => at = At::Out,
            At::Theirs => theirs.push_str(line),
        }
    }
    if at != At::Out {
        bail!("unterminated conflict marker");
    }
    if !found {
        bail!("no conflict markers");
    }
    Ok(MergeTexts { base, ours, theirs })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkKind {
    /// Only yours changed the base here.
    OursOnly,
    TheirsOnly,
    /// Both made the same change.
    BothSame,
    /// Both changed it differently.
    Conflict,
}

/// Lines where yours or theirs differ from the base: a range of lines in
/// each text (empty where that text has none there).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub kind: ChunkKind,
    pub base: Range<usize>,
    pub ours: Range<usize>,
    pub theirs: Range<usize>,
}

impl ChunkKind {
    /// Whether yours (`ours`) or theirs changed the base in this chunk.
    pub fn changes(self, ours: bool) -> bool {
        match self {
            ChunkKind::OursOnly => ours,
            ChunkKind::TheirsOnly => !ours,
            ChunkKind::BothSame | ChunkKind::Conflict => true,
        }
    }
}

/// The lines of `s` with their terminators, as diffs count them.
pub fn lines(s: &str) -> Vec<&str> {
    s.split_inclusive('\n').collect()
}

/// The chunks of a three-way merge in base order: the changes of base to
/// yours and of base to theirs, grouped where they overlap or touch (so two
/// chunks always have an unchanged base line between them).
pub fn chunks(base: &str, ours: &str, theirs: &str) -> Vec<Chunk> {
    let range = |r: &Range<u32>| r.start as usize..r.end as usize;
    // (base lines, side: 0 yours, 1 theirs, side lines)
    let mut all: Vec<(Range<usize>, usize, Range<usize>)> = Vec::new();
    for (side, text) in [ours, theirs].into_iter().enumerate() {
        all.extend(hunks(base, text).iter().map(|h| (range(&h.before), side, range(&h.after))));
    }
    all.sort_by_key(|(b, _, _)| (b.start, b.end));
    let (ol, tl) = (lines(ours), lines(theirs));
    // How far each side's line numbers are ahead of the base's before the
    // group at hand.
    let mut delta = [0isize; 2];
    let mut out = Vec::new();
    let mut i = 0;
    while i < all.len() {
        let gs = all[i].0.start;
        let mut ge = all[i].0.end;
        let mut j = i + 1;
        while j < all.len() && all[j].0.start <= ge {
            ge = ge.max(all[j].0.end);
            j += 1;
        }
        let (mut changed, mut grow) = ([false; 2], [0isize; 2]);
        for (b, side, s) in &all[i..j] {
            changed[*side] = true;
            grow[*side] += s.len() as isize - b.len() as isize;
        }
        let side = |k: usize| (gs as isize + delta[k]) as usize..(ge as isize + delta[k] + grow[k]) as usize;
        let (o, t) = (side(0), side(1));
        let kind = match changed {
            [true, true] if ol[o.clone()] == tl[t.clone()] => ChunkKind::BothSame,
            [true, true] => ChunkKind::Conflict,
            [true, false] => ChunkKind::OursOnly,
            _ => ChunkKind::TheirsOnly,
        };
        out.push(Chunk { kind, base: gs..ge, ours: o, theirs: t });
        delta[0] += grow[0];
        delta[1] += grow[1];
        i = j;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ChunkKind::*;

    type Row = (ChunkKind, Range<usize>, Range<usize>, Range<usize>);

    fn kinds(base: &str, ours: &str, theirs: &str) -> Vec<Row> {
        chunks(base, ours, theirs).into_iter().map(|c| (c.kind, c.base, c.ours, c.theirs)).collect()
    }

    const BASE: &str = "a\nb\nc\nd\ne\nf\ng\n";

    #[test]
    fn one_sided_and_identical_changes() {
        assert_eq!(kinds(BASE, "a\nB\nc\nd\ne\nf\ng\n", BASE), [(OursOnly, 1..2, 1..2, 1..2)]);
        assert_eq!(kinds(BASE, BASE, "a\nb\nc\nd\ne\nf\nG\nh\n"), [(TheirsOnly, 6..7, 6..7, 6..8)]);
        let same = "a\nb\nX\nd\ne\nf\ng\n";
        assert_eq!(kinds(BASE, same, same), [(BothSame, 2..3, 2..3, 2..3)]);
        // A deletion on one side and an insertion on the other further on:
        // the line numbers after each shift.
        let ours = "a\nc\nd\ne\nf\ng\n";
        let theirs = "a\nb\nc\nd\ne\nf\nnew\ng\n";
        assert_eq!(kinds(BASE, ours, theirs), [(OursOnly, 1..2, 1..1, 1..2), (TheirsOnly, 6..6, 5..5, 6..7)]);
    }

    #[test]
    fn overlapping_and_touching_edits_conflict() {
        let ours = "a\nb\nOURS\nd\ne\nf\ng\n";
        let theirs = "a\nb\nTHEIRS\nTHEIRS 2\nd\ne\nf\ng\n";
        assert_eq!(kinds(BASE, ours, theirs), [(Conflict, 2..3, 2..3, 2..4)]);
        // Yours changes b, theirs c: adjacent lines touch.
        assert_eq!(kinds(BASE, "a\nB\nc\nd\ne\nf\ng\n", "a\nb\nC\nd\ne\nf\ng\n"), [(Conflict, 1..3, 1..3, 1..3)]);
        // An insertion right where the other side's change starts.
        assert_eq!(kinds(BASE, "a\nb\nnew\nc\nd\ne\nf\ng\n", "a\nb\nC\nd\ne\nf\ng\n"), [(Conflict, 2..3, 2..4, 2..3)]);
        // One unchanged line between them keeps two chunks.
        assert_eq!(
            kinds(BASE, "a\nB\nc\nd\ne\nf\ng\n", "a\nb\nc\nD\ne\nf\ng\n"),
            [(OursOnly, 1..2, 1..2, 1..2), (TheirsOnly, 3..4, 3..4, 3..4)]
        );
    }

    #[test]
    fn add_add_and_trailing_newline() {
        // Added on both sides: an empty base.
        assert_eq!(kinds("", "x\ny\n", "x\nz\n"), [(Conflict, 0..0, 0..2, 0..2)]);
        // Only one side drops the final newline.
        assert_eq!(kinds("a\nb\n", "a\nb", "a\nb\n"), [(OursOnly, 1..2, 1..2, 1..2)]);
        assert_eq!(kinds("a\nb\n", "a\nb", "a\nB\n"), [(Conflict, 1..2, 1..2, 1..2)]);
    }

    fn sides(base: &str, ours: &str, theirs: &str, work: &str) -> ConflictSides {
        ConflictSides {
            rel: "f.txt".into(),
            base: Some(base.into()),
            ours: Some(ours.into()),
            theirs: Some(theirs.into()),
            work: work.into(),
            ..Default::default()
        }
    }

    fn texts(s: &ConflictSides) -> MergeTexts {
        match MergeInput::classify(s).unwrap() {
            MergeInput::Text(t) => t,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn line_endings_are_made_alike_only_when_they_differ() {
        // All CRLF: kept, and the chunks are the same as with LF.
        let t = texts(&sides("a\r\nb\r\n", "a\r\nB\r\n", "A\r\nb\r\n", "w\r\n"));
        assert_eq!(t.ours, "a\r\nB\r\n");
        assert_eq!(kinds(&t.base, &t.ours, &t.theirs), [(Conflict, 0..2, 0..2, 0..2)]);
        // Theirs converted the file to LF; the work file is CRLF.
        let t = texts(&sides("a\r\nb\r\nc\r\n", "a\r\nB\r\nc\r\n", "a\nb\nc\n", "x\r\n"));
        assert_eq!((t.base.as_str(), t.theirs.as_str()), ("a\r\nb\r\nc\r\n", "a\r\nb\r\nc\r\n"));
        assert_eq!(kinds(&t.base, &t.ours, &t.theirs), [(OursOnly, 1..2, 1..2, 1..2)]);
        // Mixed endings in one side: LF when the work file is LF.
        let t = texts(&sides("a\nb\n", "a\r\nb\n", "a\nb\nc\n", "a\nb\n"));
        assert_eq!(t.ours, "a\nb\n");
        // The same mixed file on every side stays as it is.
        let t = texts(&sides("a\r\nb\n", "a\r\nb\nc\n", "a\r\nb\n", ""));
        assert_eq!(t.base, "a\r\nb\n");
    }

    #[test]
    fn classify_without_a_line_merge() {
        let mut s = sides("a\n", "b\n", "c\n", "");
        s.theirs = None;
        assert!(matches!(MergeInput::classify(&s).unwrap(), MergeInput::SidesOnly(r) if r.contains("deleted in theirs")));
        let mut s = sides("a\n", "b\0\n", "c\n", "");
        s.base = None;
        assert!(matches!(MergeInput::classify(&s).unwrap(), MergeInput::SidesOnly(r) if r.contains("binary")));
        // Added on both sides: an empty base.
        let mut s = sides("", "x\n", "y\n", "");
        s.base = None;
        assert_eq!(texts(&s).base, "");
        // A link or a submodule: sides only, whatever the stages hold.
        let mut s = sides("a\n", "b\n", "c\n", "");
        s.special = Some(Special::Symlink);
        assert!(matches!(MergeInput::classify(&s).unwrap(), MergeInput::SidesOnly(r) if r == "f.txt is a symbolic link"));
        s.special = Some(Special::Submodule);
        assert!(matches!(MergeInput::classify(&s).unwrap(), MergeInput::SidesOnly(r) if r == "f.txt is a submodule"));
    }

    #[test]
    fn markers_two_way_and_diff3() {
        let work = "top\n<<<<<<< HEAD\nmine\n=======\nyours\nmore\n>>>>>>> dev\nbottom\n";
        let t = parse_markers(work).unwrap();
        assert_eq!(
            (t.base.as_str(), t.ours.as_str(), t.theirs.as_str()),
            ("top\nbottom\n", "top\nmine\nbottom\n", "top\nyours\nmore\nbottom\n")
        );
        assert_eq!(kinds(&t.base, &t.ours, &t.theirs), [(Conflict, 1..1, 1..2, 1..3)]);
        let work = "<<<<<<< ours\nA\n||||||| base\na\n=======\nAA\n>>>>>>> theirs\nend\n";
        let t = parse_markers(work).unwrap();
        assert_eq!((t.base.as_str(), t.ours.as_str(), t.theirs.as_str()), ("a\nend\n", "A\nend\n", "AA\nend\n"));
        // Through classify when the index has no stages.
        let s = ConflictSides { rel: "f".into(), work: work.into(), ..Default::default() };
        assert_eq!(texts(&s), t);
    }

    #[test]
    fn marker_look_alikes_and_errors() {
        // Eight `<`, a `=======x` and markers inside a region are text.
        let work = "<<<<<<<< not\n=======x\n<<<<<<< a\n======= \nb\n>>>>>>>\n";
        let t = parse_markers(work).unwrap();
        assert_eq!(t.ours, "<<<<<<<< not\n=======x\n");
        assert_eq!(t.theirs, "<<<<<<<< not\n=======x\nb\n");
        assert!(parse_markers("<<<<<<< a\nx\n=======\ny\n").unwrap_err().to_string().contains("unterminated"));
        assert!(parse_markers("plain\n").unwrap_err().to_string().contains("no conflict markers"));
    }
}
