//! The conflict resolver (Git > Resolve Conflicts): yours, the result and
//! theirs side by side in a temporary editor layout. Opening it puts the
//! groups, the split tree and the focus aside (`Saved`) and shows three
//! scratch buffers (no path): yours and theirs read-only, the result
//! editable, starting from the base with every non-conflicting change
//! applied (one undo step, so undo shows the pure base). Below
//! [`TWO_PANE_BELOW`] columns yours and theirs share the left pane as two
//! tabs. Closing it (Apply, Cancel) drops the scratch buffers and puts the
//! layout back; Apply writes the result into the file's buffer (opened if
//! needed), saves it and stages it.
//!
//! Each chunk ([`crate::git::merge::chunks`]) has an anchor, its text in
//! the result in chars, mapped through every edit of the result like a
//! parked view's carets (`MergeSession::catch_up`, from `sync_parked` and
//! before every resolver command). The resolver's own edits set the
//! anchors exactly; each undo state remembers the anchors and chunk states
//! it had, so undo and redo restore them. An edit by hand inside a chunk
//! counts as resolving it.
//!
//! The panes scroll together: the pane that scrolled since the last frame
//! (else the focused one) leads, and the others show the row that matches
//! its top. Every pane is cut into blocks (the unchanged run before the
//! first chunk, each chunk, each run between chunks, the run after the
//! last); a pane with fewer lines in a block than the longest gets blank
//! filler rows after the block's last line (`merge_fill`,
//! [`super::rows::Filler`]), so a block takes the same rows in all three
//! panes and a top row maps by its block and its offset into it
//! (`block_offset`, `block_row`). A block that is empty at the very top of
//! a pane gets its filler rows before the pane's first line.

use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier};
use ropey::Rope;
use unicode_width::UnicodeWidthStr;

use super::buffer::{Buffer, BufferId};
use super::groups::{GroupId, TabGroup, View};
use super::rows::{Filler, Row, Rows};
use super::selection::Selection;
use super::split::{Axis, SplitNode};
use super::undo::{Change, TxKind};
use super::view::{DecorLine, LineDecor};
use super::view_state::LogEdit;
use super::{Editor, indent, syntax};
use crate::actions::{EditorAction, MergeAction};
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::{Effect, Level};
use crate::git::merge::{Chunk, ChunkKind, MergeTexts, chunks, lines};
use crate::ui::dialog::{Dialog, DialogId};

/// Below this many columns of editor area the resolver shows two panes:
/// yours and theirs as tabs of the left one.
pub const TWO_PANE_BELOW: u16 = 90;

/// What layout commands answer while the resolver is open.
pub const BUSY: &str = "Finish or cancel the merge first";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Ours,
    Result,
    Theirs,
}

impl Pane {
    const ALL: [Pane; 3] = [Pane::Ours, Pane::Result, Pane::Theirs];

    /// Cells of the glyph column: `»«×` in the result, `»×` and `«×` beside.
    fn action_w(self) -> u16 {
        if self == Pane::Result { 3 } else { 2 }
    }
}

/// What became of one side's change in a chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Pending,
    Applied,
    Ignored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkState {
    pub ours: Side,
    pub theirs: Side,
    /// Edited by hand: resolved, whatever the sides say.
    pub edited: bool,
}

impl ChunkState {
    fn side(&self, ours: bool) -> Side {
        if ours { self.ours } else { self.theirs }
    }

    fn side_mut(&mut self, ours: bool) -> &mut Side {
        if ours { &mut self.ours } else { &mut self.theirs }
    }
}

/// The layout the resolver replaced, put back when it closes.
#[derive(Debug)]
pub(super) struct Saved {
    pub groups: Vec<TabGroup>,
    pub tree: SplitNode,
    pub focused: GroupId,
}

#[derive(Debug)]
pub struct MergeSession {
    pub path: PathBuf,
    /// `path` relative to the work tree, for staging.
    pub rel: String,
    /// The panes' buffers: yours, the result, theirs.
    pub ids: [BufferId; 3],
    pub(super) saved: Saved,
    two_pane: bool,
    chunks: Vec<Chunk>,
    /// Yours and theirs as lines with their terminators.
    ours: Vec<String>,
    theirs: Vec<String>,
    /// Each chunk's text in the result (chars).
    anchors: Vec<Range<usize>>,
    states: Vec<ChunkState>,
    /// The result's `EditLog::end` and version when the anchors were last
    /// brought up to date.
    seq: u64,
    version: u64,
    /// Anchors and states by the result's undo state.
    history: HashMap<u64, (Vec<Range<usize>>, Vec<ChunkState>)>,
    /// The result's undo state once the non-conflicting changes are in;
    /// anything else is the user's work (Cancel asks).
    initial_state: u64,
    /// The work tree file as read (with markers): Apply opens it when the
    /// file has no buffer.
    work: String,
    /// The header row and its buttons as last drawn.
    header_area: Rect,
    header: Vec<(Rect, MergeAction)>,
    /// Each pane's top line as last drawn, to tell which one scrolled.
    last_tops: [Row; 3],
}

/// The lines `r` (chars, normally whole lines) covers: a range that ends
/// inside a line takes that line too.
fn char_lines(rope: &Rope, r: &Range<usize>) -> Range<usize> {
    let len = rope.len_chars();
    let start = rope.char_to_line(r.start.min(len));
    if r.end <= r.start {
        return start..start;
    }
    let l = rope.char_to_line(r.end.min(len));
    start..if rope.line_to_char(l) == r.end.min(len) { l } else { l + 1 }
}

/// Char offset of each line of `lines` and of the end.
fn line_offsets(lines: &[String]) -> Vec<usize> {
    let mut out = Vec::with_capacity(lines.len() + 1);
    let mut at = 0;
    out.push(0);
    for l in lines {
        at += l.chars().count();
        out.push(at);
    }
    out
}

/// A pane's lines per block: the unchanged run before chunk 0, chunk 0,
/// the run after it, …, the run after the last chunk up to `n` (the
/// pane's `len_lines`). `chunks` is `MergeSession::ranges` of the pane.
fn blocks(chunks: &[Range<usize>], n: usize) -> Vec<Range<usize>> {
    let mut out = Vec::with_capacity(2 * chunks.len() + 1);
    let mut at = 0;
    for c in chunks {
        out.push(at..c.start.max(at));
        out.push(c.clone());
        at = c.end.max(at);
    }
    out.push(at..n.max(at));
    out
}

/// Filler rows that line the panes up: in each block, every pane with
/// fewer lines than the longest gets the difference after the block's
/// last line, or after the line before an empty block; an empty block
/// at the top gets its fillers before line 0. `bg` is left `None`.
fn plan_fillers(blocks: &[Vec<Range<usize>>; 3]) -> [Vec<Filler>; 3] {
    let mut out: [Vec<Filler>; 3] = Default::default();
    for b in 0..blocks[0].len() {
        let most = blocks.iter().map(|p| p[b].len()).max().unwrap_or(0);
        for (p, list) in blocks.iter().zip(&mut out) {
            let r = &p[b];
            let top = r.is_empty() && r.start == 0;
            let line = if r.is_empty() { r.start.saturating_sub(1) } else { r.end - 1 };
            let missing = most - r.len();
            if missing > 0 {
                list.push(Filler { line, rows: missing.min(u16::MAX as usize) as u16, block: b, bg: None, top });
            }
        }
    }
    for list in &mut out {
        list.sort_by_key(|f| (f.line, !f.top));
    }
    out
}

/// Where top row `top` of a pane is: its block and the offset into it,
/// counting the block's lines, then its filler rows.
fn block_offset(rows: &Rows, blocks: &[Range<usize>], top: Row) -> (usize, usize) {
    let last = blocks.len() - 1;
    if let Some((f, k)) = rows.filler(top) {
        let b = f.block.min(last);
        return (b, blocks[b].len() + k as usize);
    }
    let b = blocks.iter().position(|r| top.line < r.end).unwrap_or(last);
    (b, top.line.saturating_sub(blocks[b].start))
}

/// The row of a pane at offset `v` into block `b`: one of its lines,
/// else one of its filler rows, else (no fillers there) its last line;
/// clamped to the visible rows.
fn block_row(rows: &Rows, blocks: &[Range<usize>], b: usize, v: usize) -> Row {
    let b = b.min(blocks.len() - 1);
    let r = &blocks[b];
    if v < r.len() {
        return rows.clamp(Row::line(r.start + v));
    }
    if let Some(k) = rows.fillers().iter().position(|f| f.block == b) {
        let i = (v - r.len()).min(u16::MAX as usize) as u16;
        return rows.clamp(rows.filler_row(k, i));
    }
    rows.clamp(Row::line(if r.is_empty() { r.start } else { r.end - 1 }))
}

impl MergeSession {
    pub fn pane_of(&self, id: BufferId) -> Option<Pane> {
        self.ids.iter().position(|x| *x == id).map(|i| Pane::ALL[i])
    }

    fn id(&self, p: Pane) -> BufferId {
        self.ids[p as usize]
    }

    fn changes(&self, k: usize, ours: bool) -> bool {
        self.chunks[k].kind.changes(ours)
    }

    /// Nothing left to decide in chunk `k`: edited by hand, a side taken
    /// (the other one's glyph stays, to add it after), or every side
    /// changing it ignored.
    fn resolved(&self, k: usize) -> bool {
        let s = self.states[k];
        let sides = [true, false].into_iter().filter(|ours| self.changes(k, *ours));
        s.edited || sides.clone().any(|ours| s.side(ours) == Side::Applied) || sides.into_iter().all(|ours| s.side(ours) != Side::Pending)
    }

    /// Chunks not resolved yet.
    pub fn unresolved(&self) -> usize {
        (0..self.chunks.len()).filter(|k| !self.resolved(*k)).count()
    }

    /// Conflicting chunks: (unresolved, all).
    pub fn conflicts(&self) -> (usize, usize) {
        let ks = (0..self.chunks.len()).filter(|k| self.chunks[*k].kind == ChunkKind::Conflict);
        let all = ks.clone().count();
        (ks.filter(|k| !self.resolved(*k)).count(), all)
    }

    /// Lines of each chunk in pane `p` (`result` is the result's text).
    fn ranges(&self, p: Pane, result: &Rope) -> Vec<Range<usize>> {
        match p {
            Pane::Ours => self.chunks.iter().map(|c| c.ours.clone()).collect(),
            Pane::Theirs => self.chunks.iter().map(|c| c.theirs.clone()).collect(),
            Pane::Result => self.anchors.iter().map(|a| char_lines(result, a)).collect(),
        }
    }

    /// Yours or theirs of chunk `k` as text.
    fn side_text(&self, k: usize, ours: bool) -> String {
        let c = &self.chunks[k];
        if ours { self.ours[c.ours.clone()].concat() } else { self.theirs[c.theirs.clone()].concat() }
    }

    fn record(&mut self, state: u64) {
        self.history.insert(state, (self.anchors.clone(), self.states.clone()));
    }

    /// Bring the anchors up to the result's edits since the last call: an
    /// undo state seen before restores its anchors and states exactly;
    /// otherwise every edit maps them (start before inserted text, end
    /// after it only for an empty chunk) and marks the chunks it touches
    /// edited. A whole-text replacement or lost edits leave nothing to
    /// map: every chunk counts as edited.
    pub(super) fn catch_up(&mut self, b: &Buffer) {
        if b.version == self.version {
            self.seq = b.edit_log.end();
            return;
        }
        let state = b.undo.state_id();
        if let Some((a, s)) = self.history.get(&state) {
            (self.anchors, self.states) = (a.clone(), s.clone());
        } else {
            let edits = b.edit_log.since(self.seq).map(|e| e.copied().collect::<Vec<_>>()).filter(|e| !e.contains(&LogEdit::Reset));
            match edits {
                Some(edits) => {
                    for e in edits {
                        let LogEdit::Edit { at, old_len, new_len } = e else { continue };
                        for (a, s) in self.anchors.iter_mut().zip(&mut self.states) {
                            let empty = a.start >= a.end;
                            let touches = if empty {
                                at <= a.start && a.start <= at + old_len
                            } else if old_len == 0 {
                                a.start <= at && at < a.end
                            } else {
                                at < a.end && a.start < at + old_len
                            };
                            s.edited |= touches;
                            let start = super::buffer::map_one(a.start, at, old_len, new_len, true);
                            let end = super::buffer::map_one(a.end, at, old_len, new_len, !empty);
                            *a = start..end.max(start);
                        }
                    }
                }
                None => {
                    let len = b.rope.len_chars();
                    for (a, s) in self.anchors.iter_mut().zip(&mut self.states) {
                        *a = a.start.min(len)..a.end.min(len);
                        s.edited = true;
                    }
                }
            }
            self.record(state);
        }
        self.seq = b.edit_log.end();
        self.version = b.version;
    }

    /// The background of chunk `k`: resolved, conflicting, or the kind of
    /// change a one-sided chunk makes.
    fn chunk_bg(&self, k: usize, theme: &Theme) -> Color {
        if self.resolved(k) {
            return theme.merge_resolved_bg;
        }
        let c = &self.chunks[k];
        let side = match c.kind {
            ChunkKind::Conflict => return theme.merge_conflict_bg,
            ChunkKind::TheirsOnly => &c.theirs,
            ChunkKind::OursOnly | ChunkKind::BothSame => &c.ours,
        };
        if side.is_empty() {
            theme.diff_del_bg
        } else if c.base.is_empty() {
            theme.diff_add_bg
        } else {
            theme.diff_mod_bg
        }
    }

    /// The glyphs pane `p` shows on chunk `k`'s first line: `»` takes
    /// yours, `«` theirs, `×` ignores; none once decided or edited.
    fn glyphs(&self, p: Pane, k: usize) -> String {
        let s = self.states[k];
        let pending = |ours: bool| !s.edited && self.changes(k, ours) && s.side(ours) == Side::Pending;
        let mark = |on: bool, c: char| if on { c } else { ' ' };
        match p {
            Pane::Result => {
                [mark(pending(true), '»'), mark(pending(false), '«'), mark(pending(true) || pending(false), '×')].iter().collect()
            }
            Pane::Ours => [mark(pending(true), '»'), mark(pending(true), '×')].iter().collect(),
            Pane::Theirs => [mark(pending(false), '«'), mark(pending(false), '×')].iter().collect(),
        }
    }

    /// Chunk backgrounds and glyphs of pane `p`; yours and theirs mark only
    /// the chunks they change.
    fn decor(&self, p: Pane, result: &Rope, theme: &Theme) -> LineDecor {
        let ranges = self.ranges(p, result);
        let lines = (0..self.chunks.len())
            .filter(|k| match p {
                Pane::Ours => self.changes(*k, true),
                Pane::Theirs => self.changes(*k, false),
                Pane::Result => true,
            })
            .map(|k| DecorLine { lines: ranges[k].clone(), bg: Some(self.chunk_bg(k, theme)), glyphs: self.glyphs(p, k) })
            .collect();
        LineDecor { action_w: p.action_w(), lines }
    }

    /// The chunk pane `p` shows on `line` (its first line for an empty
    /// one); yours and theirs only know the chunks they change.
    fn chunk_on(&self, p: Pane, result: &Rope, line: usize) -> Option<usize> {
        let ranges = self.ranges(p, result);
        (0..self.chunks.len()).find(|&k| {
            let r = &ranges[k];
            let mine = match p {
                Pane::Ours => self.changes(k, true),
                Pane::Theirs => self.changes(k, false),
                Pane::Result => true,
            };
            mine && (r.contains(&line) || (r.is_empty() && r.start == line))
        })
    }

    /// The header's counter.
    fn status(&self) -> String {
        let (left, all) = self.conflicts();
        let pending = self.unresolved() - left;
        match (left, pending) {
            (0, 0) => "All changes resolved".to_string(),
            (0, n) => format!("{n} change{} left", if n == 1 { "" } else { "s" }),
            (l, _) => format!("{l} of {all} conflict{} left", if all == 1 { "" } else { "s" }),
        }
    }
}

/// The header buttons: full labels, then short ones for narrow screens.
const BUTTONS: [(&str, &str, MergeAction); 5] = [
    ("Accept Yours", "Yours", MergeAction::AcceptAllLeft),
    ("Accept Theirs", "Theirs", MergeAction::AcceptAllRight),
    ("Apply Non-Conflicting", "Non-Conflicting", MergeAction::ApplyNonConflicting),
    ("Apply", "Apply", MergeAction::Apply),
    ("Cancel", "Cancel", MergeAction::Cancel),
];

impl Editor {
    pub fn merge_active(&self) -> bool {
        self.merge.is_some()
    }

    pub fn merge_session(&self) -> Option<&MergeSession> {
        self.merge.as_ref()
    }

    /// One of the resolver's scratch buffers.
    pub fn is_merge_buffer(&self, id: BufferId) -> bool {
        self.merge.as_ref().is_some_and(|m| m.ids.contains(&id))
    }

    /// The layout a session saves: the one the resolver put aside while it
    /// is open (its own is never saved).
    pub fn session_layout(&self) -> (&[TabGroup], &SplitNode, GroupId) {
        match &self.merge {
            Some(m) => (&m.saved.groups, &m.saved.tree, m.saved.focused),
            None => (&self.groups, &self.tree, self.focused),
        }
    }

    /// Open the resolver on `texts` for the file at `path` (`rel` in the
    /// work tree; `labels` name yours and theirs; `work` is the file as
    /// read). The caret goes to the first conflict in the result.
    pub fn open_merge(&mut self, path: PathBuf, rel: String, texts: MergeTexts, labels: [String; 2], work: String, theme: &Theme) {
        if self.merge.is_some() {
            return;
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| rel.clone());
        let chunks = chunks(&texts.base, &texts.ours, &texts.theirs);
        let wrap = self.soft_wrap_for(Some(&path));
        let mut scratch = |title: String, text: &str, read_only: bool, wrap: bool| {
            let id = self.next_id;
            self.next_id += 1;
            let lang = syntax::detect_text(&path, text);
            let settings = indent::FileSettings::for_path(&path);
            let mut b = Buffer::from_text(id, None, text, false);
            b.name = title;
            b.syntax = lang.filter(|_| text.len() <= self.max_highlight_bytes).and_then(|d| syntax::Syntax::new(d, theme));
            b.detected_language = lang;
            b.indent = settings.indent(&b.rope, self.tab_width);
            b.settings = settings;
            b.read_only = read_only;
            b.view.wrap.on = wrap;
            self.buffers.push(b);
            self.mru.insert(0, id);
            id
        };
        let ours_id = scratch(format!("Yours: {}", labels[0]), &texts.ours, true, false);
        let result_id = scratch(format!("Result: {name}"), &texts.base, false, wrap);
        let theirs_id = scratch(format!("Theirs: {}", labels[1]), &texts.theirs, true, false);
        let (ours, theirs): (Vec<String>, Vec<String>) =
            (lines(&texts.ours).into_iter().map(str::to_string).collect(), lines(&texts.theirs).into_iter().map(str::to_string).collect());
        let base: Vec<String> = lines(&texts.base).into_iter().map(str::to_string).collect();
        let base_at = line_offsets(&base);
        let pending = ChunkState { ours: Side::Pending, theirs: Side::Pending, edited: false };
        let mut session = MergeSession {
            path,
            rel,
            ids: [ours_id, result_id, theirs_id],
            saved: Saved {
                groups: std::mem::take(&mut self.groups),
                tree: std::mem::replace(&mut self.tree, SplitNode::Leaf(self.focused)),
                focused: self.focused,
            },
            two_pane: false,
            anchors: chunks.iter().map(|c| base_at[c.base.start]..base_at[c.base.end]).collect(),
            states: vec![pending; chunks.len()],
            chunks,
            ours,
            theirs,
            seq: 0,
            version: 0,
            history: HashMap::new(),
            initial_state: 0,
            work,
            header_area: Rect::default(),
            header: Vec::new(),
            last_tops: [Row::line(0); 3],
        };
        session.record(0);
        self.merge = Some(session);
        self.merge_layout(self.center.width > 0 && self.center.width < TWO_PANE_BELOW);
        // The non-conflicting changes, as one undo step over the base.
        let edits: Vec<(usize, String)> = {
            let m = self.merge.as_ref().expect("merge session");
            (0..m.chunks.len())
                .filter(|k| m.chunks[*k].kind != ChunkKind::Conflict)
                .map(|k| (k, m.side_text(k, m.chunks[k].kind != ChunkKind::TheirsOnly)))
                .collect()
        };
        if !edits.is_empty() {
            let applied: Vec<usize> = edits.iter().map(|(k, _)| *k).collect();
            self.merge_replace(edits, |m| {
                for k in applied {
                    for ours in [true, false] {
                        if m.changes(k, ours) {
                            *m.states[k].side_mut(ours) = Side::Applied;
                        }
                    }
                }
            });
        }
        let Some(ri) = self.index_of_id(result_id) else { return };
        let m = self.merge.as_mut().expect("merge session");
        m.initial_state = self.buffers[ri].undo.state_id();
        // Its tab shows `*` once the user changed something.
        self.buffers[ri].saved_state = m.initial_state;
        let first = (0..m.chunks.len()).find(|k| !m.resolved(*k)).map(|k| m.anchors[k].start).unwrap_or(0);
        self.buffers[ri].view.set_sel(Selection::point(first));
    }

    /// Lay the panes out: three side by side, or (`two_pane`) yours and
    /// theirs as tabs of the left pane beside the result. Focus stays on
    /// the pane it was on.
    fn merge_layout(&mut self, two_pane: bool) {
        let Some(m) = &mut self.merge else { return };
        m.two_pane = two_pane;
        let [ours, result, theirs] = m.ids;
        let focused = self.groups.iter().find(|g| g.id == self.focused).and_then(TabGroup::active_id).unwrap_or(result);
        let mut make = |ids: &[BufferId], shown: BufferId| {
            let mut g = TabGroup::new(self.next_group);
            self.next_group += 1;
            g.views = ids.iter().map(|&id| View { id, parked: None, pinned: false }).collect();
            g.active = Some(ids.iter().position(|x| *x == shown).unwrap_or(0));
            g.touch(shown);
            g
        };
        let split = |ratio, a, b| SplitNode::Split { axis: Axis::Horizontal, ratio, a: Box::new(a), b: Box::new(b) };
        let groups = if two_pane {
            vec![make(&[ours, theirs], if focused == theirs { theirs } else { ours }), make(&[result], result)]
        } else {
            vec![make(&[ours], ours), make(&[result], result), make(&[theirs], theirs)]
        };
        self.tree = match groups.as_slice() {
            [a, b] => split(450, SplitNode::Leaf(a.id), SplitNode::Leaf(b.id)),
            [a, b, c] => split(333, SplitNode::Leaf(a.id), split(500, SplitNode::Leaf(b.id), SplitNode::Leaf(c.id))),
            _ => unreachable!("two or three panes"),
        };
        self.focused = groups.iter().find(|g| g.slot_of(focused).is_some()).map_or(groups[0].id, |g| g.id);
        self.groups = groups;
        if let Some(slot) = self.focused_group().active {
            self.show(slot);
        }
    }

    /// Switch between two and three panes when the editor area crosses
    /// [`TWO_PANE_BELOW`] columns.
    pub(super) fn merge_fit(&mut self, width: u16) {
        let two = width < TWO_PANE_BELOW;
        if self.merge.as_ref().is_some_and(|m| m.two_pane != two) {
            self.merge_layout(two);
        }
    }

    fn merge_result_index(&self) -> Option<usize> {
        self.index_of_id(self.merge.as_ref()?.ids[1])
    }

    /// Bring the chunk anchors up to the result's edits.
    pub(super) fn merge_catch_up(&mut self) {
        let Some(ri) = self.merge_result_index() else { return };
        if let Some(m) = &mut self.merge {
            m.catch_up(&self.buffers[ri]);
        }
    }

    /// The resolver's backgrounds and glyphs for buffer `id`, if it is one
    /// of its panes.
    pub(super) fn merge_decor(&self, id: BufferId, theme: &Theme) -> Option<LineDecor> {
        let m = self.merge.as_ref()?;
        let p = m.pane_of(id)?;
        Some(m.decor(p, &self.buffers[self.merge_result_index()?].rope, theme))
    }

    /// Replace the result text of the chunks in `edits` (chunk, new text)
    /// as one undo step, put the caret at the first, set the anchors
    /// exactly and let `update` change the states.
    fn merge_replace(&mut self, mut edits: Vec<(usize, String)>, update: impl FnOnce(&mut MergeSession)) {
        let Some(ri) = self.merge_result_index() else { return };
        let Some(m) = &mut self.merge else { return };
        if edits.is_empty() {
            return;
        }
        edits.sort_by_key(|(k, _)| *k);
        let b = &mut self.buffers[ri];
        let mut anchors = m.anchors.clone();
        let (mut delta, mut next) = (0isize, edits.iter().peekable());
        for (j, a) in anchors.iter_mut().enumerate() {
            let start = (a.start as isize + delta) as usize;
            match next.next_if(|(k, _)| *k == j) {
                Some((_, text)) => {
                    let n = text.chars().count();
                    delta += n as isize - a.len() as isize;
                    *a = start..start + n;
                }
                None => *a = start..(a.end as isize + delta) as usize,
            }
        }
        let changes = edits
            .iter()
            .rev()
            .map(|(k, text)| {
                let r = m.anchors[*k].clone();
                Change::replace(r.start, b.rope.slice(r).to_string(), text.clone())
            })
            .collect();
        b.apply(changes, Selection::point(anchors[edits[0].0].start), TxKind::Other);
        m.anchors = anchors;
        update(m);
        (m.seq, m.version) = (b.edit_log.end(), b.version);
        m.record(b.undo.state_id());
    }

    /// Yours (`ours`) or theirs for chunk `k`; after the other side was
    /// taken, both (the other first). False when that side does not
    /// change the chunk.
    fn merge_accept(&mut self, k: usize, ours: bool) -> bool {
        let Some(ri) = self.merge_result_index() else { return false };
        let Some(m) = &self.merge else { return false };
        let kind = m.chunks[k].kind;
        let s = m.states[k];
        if !kind.changes(ours) {
            return false;
        }
        if s.side(ours) == Side::Applied && !s.edited {
            return true;
        }
        let append = kind != ChunkKind::BothSame && kind.changes(!ours) && s.side(!ours) == Side::Applied && !s.edited;
        let mut text = m.side_text(k, ours);
        if append {
            text = self.buffers[ri].rope.slice(m.anchors[k].clone()).to_string() + &text;
        }
        self.merge_replace(vec![(k, text)], |m| {
            let s = &mut m.states[k];
            *s.side_mut(ours) = Side::Applied;
            if kind == ChunkKind::BothSame {
                *s.side_mut(!ours) = Side::Applied;
            }
            s.edited = false;
        });
        true
    }

    /// Leave yours and/or theirs of chunk `k` out (the pending ones).
    fn merge_ignore(&mut self, k: usize, ours: bool, theirs: bool) {
        let Some(ri) = self.merge_result_index() else { return };
        let Some(m) = &mut self.merge else { return };
        for (side, on) in [(true, ours), (false, theirs)] {
            if on && m.states[k].side(side) == Side::Pending {
                *m.states[k].side_mut(side) = Side::Ignored;
            }
        }
        m.record(self.buffers[ri].undo.state_id());
    }

    /// Yours or theirs for the whole file.
    fn merge_accept_all(&mut self, ours: bool) {
        let Some(ri) = self.merge_result_index() else { return };
        let Some(m) = &mut self.merge else { return };
        let lines = if ours { &m.ours } else { &m.theirs };
        let at = line_offsets(lines);
        let text = lines.concat();
        let b = &mut self.buffers[ri];
        b.apply(vec![Change::replace(0, b.text(), text)], Selection::point(0), TxKind::Other);
        for (k, c) in m.chunks.iter().enumerate() {
            let r = if ours { &c.ours } else { &c.theirs };
            m.anchors[k] = at[r.start]..at[r.end];
            let s = &mut m.states[k];
            s.edited = false;
            for side in [true, false] {
                if c.kind.changes(side) {
                    *s.side_mut(side) = if side == ours || c.kind == ChunkKind::BothSame { Side::Applied } else { Side::Ignored };
                }
            }
        }
        (m.seq, m.version) = (b.edit_log.end(), b.version);
        m.record(b.undo.state_id());
    }

    /// Apply every one-sided or identical change still pending.
    fn merge_apply_non_conflicting(&mut self, cx: &mut Ctx) {
        let Some(m) = &self.merge else { return };
        let mut edits = Vec::new();
        for (k, c) in m.chunks.iter().enumerate() {
            let ours = c.kind != ChunkKind::TheirsOnly;
            if c.kind != ChunkKind::Conflict && !m.states[k].edited && m.states[k].side(ours) == Side::Pending {
                edits.push((k, m.side_text(k, ours)));
            }
        }
        if edits.is_empty() {
            cx.notify(Level::Info, "No non-conflicting changes left to apply");
            return;
        }
        let ks: Vec<usize> = edits.iter().map(|(k, _)| *k).collect();
        self.merge_replace(edits, |m| {
            for k in ks {
                for side in [true, false] {
                    if m.changes(k, side) {
                        *m.states[k].side_mut(side) = Side::Applied;
                    }
                }
            }
        });
    }

    /// The focused pane, the chunk at its caret and the caret line.
    fn merge_caret(&self) -> Option<(Pane, Option<usize>, usize)> {
        let m = self.merge.as_ref()?;
        let b = self.active_buffer()?;
        let p = m.pane_of(b.id)?;
        let line = b.rope.char_to_line(b.view.sel().head.min(b.rope.len_chars()));
        let result = &self.buffers[self.merge_result_index()?].rope;
        Some((p, m.chunk_on(p, result, line), line))
    }

    /// F7 / Shift+F7: the caret of the focused pane to the next (previous)
    /// unresolved chunk.
    fn merge_step(&mut self, forward: bool, cx: &mut Ctx) {
        let (Some((p, _, line)), Some(ri)) = (self.merge_caret(), self.merge_result_index()) else { return };
        let Some(m) = &self.merge else { return };
        let ranges = m.ranges(p, &self.buffers[ri].rope);
        let open: Vec<usize> = (0..m.chunks.len()).filter(|k| !m.resolved(*k)).collect();
        if open.is_empty() {
            cx.notify(Level::Info, "All changes resolved; Apply (Ctrl+Enter) saves the result");
            return;
        }
        let target =
            if forward { open.iter().find(|k| ranges[**k].start > line) } else { open.iter().rev().find(|k| ranges[**k].start < line) };
        let Some(&k) = target else {
            cx.notify(Level::Info, if forward { "No more unresolved changes below" } else { "No more unresolved changes above" });
            return;
        };
        let to = ranges[k].start;
        if let Some(b) = self.active_buffer_mut() {
            b.goto_line(to, 0);
        }
    }

    /// Run a resolver command.
    pub(super) fn merge_action(&mut self, a: MergeAction, cx: &mut Ctx) {
        if self.merge.is_none() {
            cx.notify(Level::Info, "No merge in progress: Git > Resolve Conflicts opens one");
            return;
        }
        self.merge_catch_up();
        match a {
            MergeAction::NextConflict | MergeAction::PrevConflict => self.merge_step(a == MergeAction::NextConflict, cx),
            MergeAction::AcceptLeft | MergeAction::AcceptRight | MergeAction::Ignore => {
                let Some((p, Some(k), _)) = self.merge_caret() else {
                    cx.notify(Level::Info, "No change at the caret; F7 goes to the next conflict");
                    return;
                };
                match a {
                    MergeAction::Ignore => self.merge_ignore(k, p != Pane::Theirs, p != Pane::Ours),
                    _ => {
                        let ours = a == MergeAction::AcceptLeft;
                        if !self.merge_accept(k, ours) {
                            cx.notify(
                                Level::Info,
                                if ours { "Yours does not change these lines" } else { "Theirs does not change these lines" },
                            );
                        }
                    }
                }
            }
            MergeAction::ApplyNonConflicting => self.merge_apply_non_conflicting(cx),
            MergeAction::AcceptAllLeft | MergeAction::AcceptAllRight => self.merge_accept_all(a == MergeAction::AcceptAllLeft),
            MergeAction::Apply => self.merge_apply(false, cx),
            MergeAction::Cancel => {
                // Esc closes the find bar, extra carets and the selection
                // first.
                let busy =
                    self.active_buffer().is_some_and(|b| b.view.search.is_some() || b.view.sels.is_multi() || !b.view.sel().is_empty());
                if busy {
                    self.handle_action(&EditorAction::Escape, cx);
                } else {
                    self.merge_cancel(false, cx);
                }
            }
        }
    }

    /// A header button: its command, Cancel without the Esc steps.
    pub fn merge_button(&mut self, a: MergeAction, cx: &mut Ctx) {
        match a {
            MergeAction::Cancel => self.merge_cancel(false, cx),
            a => self.merge_action(a, cx),
        }
    }

    /// A click on glyph `glyph` of pane `p` on `line`.
    fn merge_click(&mut self, p: Pane, line: usize, glyph: char) {
        self.merge_catch_up();
        let (Some(m), Some(ri)) = (&self.merge, self.merge_result_index()) else { return };
        let Some(k) = m.chunk_on(p, &self.buffers[ri].rope, line) else { return };
        match glyph {
            '»' => {
                self.merge_accept(k, true);
            }
            '«' => {
                self.merge_accept(k, false);
            }
            '×' => self.merge_ignore(k, p != Pane::Theirs, p != Pane::Ours),
            _ => {}
        }
    }

    /// A press at `x`, `y` in the glyph column of the focused pane: run
    /// the glyph there. False when the press is elsewhere.
    pub(super) fn merge_mouse(&mut self, x: u16, y: u16, theme: &Theme) -> bool {
        let (Some(m), Some(ai)) = (&self.merge, self.active()) else { return false };
        let b = &self.buffers[ai];
        let Some(p) = m.pane_of(b.id) else { return false };
        let text = self.text_area();
        let x0 = text.x.saturating_sub(super::view::gutter_width(b, p.action_w()));
        if x < x0 || x >= x0 + p.action_w() || y < text.y || y >= text.bottom() {
            return false;
        }
        let Some(row) = b.rows().at_offset(b.view.top(), (y - text.y) as usize).filter(|r| r.seg == 0) else { return true };
        let glyph = self.merge_decor(b.id, theme).and_then(|d| d.glyphs_at(row.line).and_then(|g| g.chars().nth((x - x0) as usize)));
        if let Some(g) = glyph {
            self.merge_click(p, row.line, g);
        }
        true
    }

    /// Unresolved changes left: ask, else (or `force`) write the result
    /// into the file's buffer (opening it), save it, stage it and close the
    /// resolver.
    pub fn merge_apply(&mut self, force: bool, cx: &mut Ctx) {
        self.merge_catch_up();
        let (Some(m), Some(ri)) = (&self.merge, self.merge_result_index()) else { return };
        let left = m.unresolved();
        if !force && left > 0 {
            let msg = format!("{left} change{} still unresolved. Apply the result anyway?", if left == 1 { " is" } else { "s are" });
            cx.effects.push(Effect::Dialog(Dialog::confirm(DialogId::MergeApply, "Apply Merge", &msg, &["Apply", "Cancel"])));
            return;
        }
        let text = self.buffers[ri].text();
        let (path, rel, work) = (m.path.clone(), m.rel.clone(), m.work.clone());
        self.merge_close();
        let i = match self.index_of(&path) {
            Some(i) => {
                self.activate_index(i);
                i
            }
            None => self.open_text(path, &work, false, cx.theme),
        };
        let b = &mut self.buffers[i];
        let old = b.text();
        if old != text {
            let pre = old.chars().zip(text.chars()).take_while(|(a, b)| a == b).count();
            let (no, nn) = (old.chars().count(), text.chars().count());
            let suf = old.chars().rev().zip(text.chars().rev()).take(no.min(nn) - pre).take_while(|(a, b)| a == b).count();
            let mid = |s: &str, n: usize| s.chars().skip(pre).take(n - pre - suf).collect::<String>();
            let change = Change::replace(pre, mid(&old, no), mid(&text, nn));
            b.apply(vec![change], Selection::point(b.view.sel().head.min(nn)), TxKind::Other);
        }
        if self.write(i, cx, true) {
            cx.notify(Level::Info, format!("Merged {rel}; staged"));
            cx.effects.push(Effect::Git(crate::git::GitRequest::StageResolved(rel)));
        }
    }

    /// Close the resolver without writing; with work of the user's in the
    /// result, ask first (unless `force`).
    pub fn merge_cancel(&mut self, force: bool, cx: &mut Ctx) {
        if !force && self.merge_edited() {
            cx.effects.push(Effect::Dialog(Dialog::confirm(
                DialogId::MergeCancel,
                "Cancel Merge",
                "Discard the merge result? The file stays as it is.",
                &["Discard", "Keep Merging"],
            )));
            return;
        }
        self.merge_close();
    }

    /// The result has changes of the user's beyond the automatic ones.
    pub fn merge_edited(&self) -> bool {
        let (Some(m), Some(ri)) = (&self.merge, self.merge_result_index()) else { return false };
        self.buffers[ri].undo.state_id() != m.initial_state
    }

    /// Drop the scratch buffers and put the layout back (tabs of buffers
    /// closed meanwhile go; groups left empty collapse).
    pub fn merge_close(&mut self) {
        let Some(m) = self.merge.take() else { return };
        self.buffers.retain(|b| !m.ids.contains(&b.id));
        self.mru.retain(|id| !m.ids.contains(id));
        self.drag = None;
        self.splitters.clear();
        let Saved { mut groups, tree, focused } = m.saved;
        for g in &mut groups {
            while let Some(slot) = g.views.iter().position(|v| !self.buffers.iter().any(|b| b.id == v.id)) {
                super::groups::remove_view(g, slot);
            }
        }
        (self.groups, self.tree, self.focused) = (groups, tree, focused);
        self.collapse_empty();
    }

    /// Commands that would change the layout or open other files are
    /// refused while the resolver is open; closing its tab cancels it.
    /// True when `a` was taken care of here.
    pub(super) fn merge_guard(&mut self, a: &EditorAction, cx: &mut Ctx) -> bool {
        use EditorAction as E;
        if let E::Merge(m) = a {
            self.merge_action(*m, cx);
            return true;
        }
        if self.merge.is_none() {
            return false;
        }
        match a {
            E::CloseTab => self.merge_cancel(false, cx),
            E::SplitRight
            | E::SplitDown
            | E::Unsplit
            | E::UnsplitAll
            | E::MoveTabToOppositeGroup
            | E::MoveTabLeft
            | E::MoveTabRight
            | E::TogglePinTab
            | E::Switcher
            | E::SwitcherBack
            | E::CloseAllTabs
            | E::CloseOtherTabs
            | E::CloseUnpinnedTabs
            | E::NavBack
            | E::NavForward
            | E::ShowBookmarks
            | E::ToggleNumberedBookmark
            | E::GotoNumberedBookmark(_) => cx.notify(Level::Info, BUSY),
            E::Save | E::SaveAs | E::Format => cx.notify(Level::Info, "Apply (Ctrl+Enter) saves the merge result"),
            _ => return false,
        }
        true
    }

    /// Pad the three panes with filler rows so each block takes the same
    /// rows in all of them (also the tab hidden in two-pane mode).
    pub(super) fn merge_fill(&mut self, theme: &Theme) {
        let Some(ri) = self.merge_result_index() else { return };
        let Some(m) = &self.merge else { return };
        let result = &self.buffers[ri].rope;
        let idx: Vec<Option<usize>> = Pane::ALL.iter().map(|p| self.index_of_id(m.id(*p))).collect();
        let Some(lens) = idx.iter().map(|i| i.map(|i| self.buffers[i].rope.len_lines())).collect::<Option<Vec<_>>>() else { return };
        let bl: [Vec<Range<usize>>; 3] = std::array::from_fn(|p| blocks(&m.ranges(Pane::ALL[p], result), lens[p]));
        let mut fills = plan_fillers(&bl);
        for (p, list) in fills.iter_mut().enumerate() {
            for f in list.iter_mut().filter(|f| f.block % 2 == 1) {
                let k = f.block / 2;
                let marks = match Pane::ALL[p] {
                    Pane::Result => true,
                    Pane::Ours => m.changes(k, true),
                    Pane::Theirs => m.changes(k, false),
                };
                f.bg = marks.then(|| m.chunk_bg(k, theme));
            }
        }
        for (i, list) in idx.into_iter().zip(fills) {
            if let Some(i) = i {
                self.buffers[i].view.fillers = list;
            }
        }
    }

    /// The pane that leads the linked scroll this frame: the one that
    /// scrolled since the last frame, else the focused one.
    pub(super) fn merge_master(&self) -> Option<BufferId> {
        let m = self.merge.as_ref()?;
        let shown: Vec<BufferId> = self.groups.iter().filter_map(TabGroup::active_id).collect();
        let top = |id: BufferId| self.index_of_id(id).map(|i| self.buffers[i].view.top());
        let moved = Pane::ALL
            .iter()
            .map(|p| m.id(*p))
            .find(|id| shown.contains(id) && top(*id).is_some_and(|t| t != m.last_tops[m.pane_of(*id).map_or(0, |p| p as usize)]));
        moved.or_else(|| self.focused_group().active_id().filter(|id| m.ids.contains(id))).or(Some(m.ids[1]))
    }

    /// Scroll every other pane to the row matching `master`'s top: the same
    /// offset into the same block.
    pub(super) fn merge_follow(&mut self, master: BufferId) {
        let (Some(m), Some(ri)) = (&self.merge, self.merge_result_index()) else { return };
        let Some(mp) = m.pane_of(master) else { return };
        let Some(mi) = self.index_of_id(master) else { return };
        let result = &self.buffers[ri].rope;
        let blocks_of = |p: Pane, i: usize| blocks(&m.ranges(p, result), self.buffers[i].rope.len_lines());
        let mb = &self.buffers[mi];
        let rows = mb.rows();
        let (block, v) = block_offset(&rows, &blocks_of(mp, mi), rows.clamp(mb.view.top()));
        let targets: Vec<(usize, Row)> = Pane::ALL
            .iter()
            .filter(|p| **p != mp)
            .filter_map(|p| {
                let i = self.index_of_id(m.id(*p))?;
                Some((i, block_row(&self.buffers[i].rows(), &blocks_of(*p, i), block, v)))
            })
            .collect();
        for (i, row) in targets {
            self.buffers[i].view.set_top(row);
        }
    }

    /// Remember each pane's top as drawn.
    pub(super) fn merge_note_tops(&mut self) {
        let tops: Vec<Row> = match &self.merge {
            Some(m) => m.ids.iter().map(|id| self.index_of_id(*id).map_or(Row::line(0), |i| self.buffers[i].view.top())).collect(),
            None => return,
        };
        if let Some(m) = &mut self.merge {
            m.last_tops.copy_from_slice(&tops);
        }
    }

    /// Give the splitter cells beside a chunk row that chunk's background.
    pub(super) fn merge_paint_splitters(&self, frame: &mut Frame, decors: &HashMap<BufferId, LineDecor>) {
        let screen = frame.buffer_mut();
        for bar in self.splitters.iter().filter(|s| s.axis == Axis::Horizontal) {
            let x = bar.rect.x;
            let beside = |right: bool| self.groups.iter().find(|g| if right { g.last_area.x == x + 1 } else { g.last_area.right() == x });
            for y in bar.rect.y..bar.rect.bottom() {
                let bg = [false, true].into_iter().find_map(|right| {
                    let g = beside(right)?;
                    let b = self.buffers.iter().find(|b| Some(b.id) == g.active_id())?;
                    let t = g.text_area;
                    if y < t.y || y >= t.bottom() {
                        return None;
                    }
                    let row = b.rows().at_offset(b.view.top(), (y - t.y) as usize)?;
                    match b.rows().filler(row) {
                        Some((f, _)) => f.bg,
                        None => decors.get(&b.id)?.bg_at(row.line),
                    }
                });
                if let Some(bg) = bg {
                    screen[(x, y)].set_bg(bg);
                }
            }
        }
    }

    /// The header row: the file, the counter and the buttons (short labels
    /// when the full ones do not fit).
    pub(super) fn render_merge_header(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let Some(m) = &mut self.merge else { return };
        m.header_area = area;
        m.header.clear();
        if area.is_empty() {
            return;
        }
        let screen = frame.buffer_mut();
        screen.set_style(area, theme.tab);
        let name = m.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let status = m.status();
        let title = format!(" {name}  {status}");
        let width = |short: bool| BUTTONS.iter().map(|(l, s, _)| if short { s } else { l }.width() as u16 + 3).sum::<u16>();
        let short = title.width() as u16 + width(false) > area.width;
        let mut x = area.right().saturating_sub(width(short)).max(area.x);
        // The counter goes before the file name when both do not fit.
        let room = x.saturating_sub(area.x + 1) as usize;
        let title = [title, format!(" {status}")].into_iter().find(|t| t.width() <= room).unwrap_or_else(|| format!(" {status}"));
        screen.set_stringn(area.x, area.y, &title, room, theme.tab.add_modifier(Modifier::BOLD));
        for (full, brief, a) in BUTTONS {
            let label = format!("[{}]", if short { brief } else { full });
            let w = label.width() as u16;
            if x + w > area.right() {
                break;
            }
            screen.set_string(x, area.y, &label, theme.tab_active);
            m.header.push((Rect::new(x, area.y, w, 1), a));
            x += w + 1;
        }
    }

    /// What the header shows at `x`, `y`: `None` off the header, `Some(None)`
    /// on it but off the buttons.
    pub(super) fn merge_header_hit(&self, x: u16, y: u16) -> Option<Option<MergeAction>> {
        let m = self.merge.as_ref()?;
        let p = Position::new(x, y);
        m.header_area.contains(p).then(|| m.header.iter().find(|(r, _)| r.contains(p)).map(|(_, a)| *a))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ratatui::backend::TestBackend;

    use super::*;
    use crate::config::Config;
    use crate::workspace::Workspace;

    const BASE: &str = "fn main() {\n    one();\n    two();\n    three();\n    four();\n    five();\n}\n";
    const OURS: &str = "fn main() {\n    ONE();\n    two();\n    three();\n    four();\n    five_ours();\n}\n";
    const THEIRS: &str = "fn main() {\n    one();\n    two();\n    THREE();\n    four();\n    five_theirs();\n}\n";

    fn texts() -> MergeTexts {
        MergeTexts { base: BASE.into(), ours: OURS.into(), theirs: THEIRS.into() }
    }

    fn with_cx<T>(f: impl FnOnce(&mut Ctx) -> T) -> (T, Vec<Effect>) {
        let (config, theme, workspace) = (Config::default(), Theme::default(), Workspace::new(PathBuf::from("/tmp")));
        let (bus, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut cx = Ctx { effects: Vec::new(), config: &config, theme: &theme, workspace: &workspace, bus };
        let out = f(&mut cx);
        (out, cx.effects)
    }

    fn act(ed: &mut Editor, a: MergeAction) -> Vec<Effect> {
        let fx = with_cx(|cx| ed.handle_action(&EditorAction::Merge(a), cx)).1;
        ed.check_invariants();
        fx
    }

    fn edit(ed: &mut Editor, a: EditorAction) -> Vec<Effect> {
        let fx = with_cx(|cx| ed.handle_action(&a, cx)).1;
        ed.check_invariants();
        fx
    }

    fn open(ed: &mut Editor) {
        ed.open_merge(
            PathBuf::from("/tmp/main.rs"),
            "main.rs".into(),
            texts(),
            ["main".into(), "dev".into()],
            BASE.into(),
            &Theme::default(),
        );
        ed.check_invariants();
    }

    fn result(ed: &Editor) -> String {
        ed.buffers[ed.merge_result_index().unwrap()].text()
    }

    fn draw(ed: &mut Editor, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut term = ratatui::Terminal::new(TestBackend::new(w, h)).unwrap();
        let frame = term.draw(|f| ed.render(f, Rect::new(0, 0, w, 1), Rect::new(0, 1, w, h - 1), true, &Theme::default())).unwrap();
        ed.check_invariants();
        frame.buffer.clone()
    }

    fn row(screen: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..screen.area.width).map(|x| screen[(x, y)].symbol().to_string()).collect()
    }

    #[test]
    fn merge_buffers_get_no_snapshots_of_unsaved_edits() {
        let mut ed = Editor::new(&Config::default());
        open(&mut ed);
        let i = ed.merge_result_index().unwrap();
        // Even were the result titled and dirty.
        ed.buffers[i].path = Some(PathBuf::from("/tmp/main.rs"));
        edit(&mut ed, EditorAction::InsertChar('x'));
        assert!(ed.buffers[i].dirty());
        assert!(ed.unsaved_captures(1 << 20).is_empty());
    }

    /// The chunks: ONE (yours only), THREE (theirs only), five (conflict).
    #[test]
    fn opening_applies_the_non_conflicting_changes_as_one_undo_step() {
        let mut ed = Editor::new(&Config::default());
        open(&mut ed);
        let m = ed.merge_session().unwrap();
        assert_eq!(m.chunks.iter().map(|c| c.kind).collect::<Vec<_>>(), [ChunkKind::OursOnly, ChunkKind::TheirsOnly, ChunkKind::Conflict]);
        assert_eq!(m.conflicts(), (1, 1));
        assert_eq!(result(&ed), "fn main() {\n    ONE();\n    two();\n    THREE();\n    four();\n    five();\n}\n");
        assert!(!ed.merge_edited());
        assert_eq!(ed.active_buffer().unwrap().name, "Result: main.rs");
        assert_eq!(ed.active_buffer().unwrap().cursor_line_col(), (5, 0), "the caret on the conflict");
        // Undo shows the base: every chunk pending again.
        edit(&mut ed, EditorAction::Undo);
        ed.merge_catch_up();
        assert_eq!(result(&ed), BASE);
        assert_eq!(ed.merge_session().unwrap().unresolved(), 3);
        act(&mut ed, MergeAction::ApplyNonConflicting);
        assert_eq!(ed.merge_session().unwrap().unresolved(), 1);
        assert!(result(&ed).contains("ONE") && result(&ed).contains("THREE"));
    }

    #[test]
    fn accept_left_then_right_takes_both_and_undo_restores_the_states() {
        let mut ed = Editor::new(&Config::default());
        open(&mut ed);
        act(&mut ed, MergeAction::AcceptLeft);
        assert!(result(&ed).contains("    five_ours();\n}"));
        assert_eq!(ed.merge_session().unwrap().unresolved(), 0);
        act(&mut ed, MergeAction::AcceptRight);
        assert!(result(&ed).contains("    five_ours();\n    five_theirs();\n}"), "{}", result(&ed));
        assert!(ed.merge_edited());
        edit(&mut ed, EditorAction::Undo);
        act(&mut ed, MergeAction::NextConflict);
        let m = ed.merge_session().unwrap();
        assert_eq!(m.states[2], ChunkState { ours: Side::Applied, theirs: Side::Pending, edited: false });
        edit(&mut ed, EditorAction::Undo);
        ed.merge_catch_up();
        assert_eq!(ed.merge_session().unwrap().unresolved(), 1);
        edit(&mut ed, EditorAction::Redo);
        edit(&mut ed, EditorAction::Redo);
        ed.merge_catch_up();
        assert!(result(&ed).contains("five_ours();\n    five_theirs();"));
        assert_eq!(ed.merge_session().unwrap().states[2].theirs, Side::Applied);
    }

    #[test]
    fn typing_maps_the_anchors_and_resolves_the_chunk_it_lands_in() {
        let mut ed = Editor::new(&Config::default());
        open(&mut ed);
        // Above every chunk: they move down, none is touched.
        ed.active_buffer_mut().unwrap().view.set_sel(Selection::point(0));
        edit(&mut ed, EditorAction::InsertText("// top\n".into()));
        ed.merge_catch_up();
        let m = ed.merge_session().unwrap();
        let rope = &ed.buffers[ed.merge_result_index().unwrap()].rope;
        assert_eq!(m.ranges(Pane::Result, rope), [2..3, 4..5, 6..7]);
        assert_eq!(m.unresolved(), 1);
        // Inside the conflict: resolved by hand.
        let at = rope.line_to_char(6) + 4;
        ed.active_buffer_mut().unwrap().view.set_sel(Selection::point(at));
        edit(&mut ed, EditorAction::InsertText("x".into()));
        ed.merge_catch_up();
        let m = ed.merge_session().unwrap();
        assert!(m.states[2].edited);
        assert_eq!(m.unresolved(), 0);
        // Accept keeps working on it: the chunk becomes yours.
        act(&mut ed, MergeAction::AcceptLeft);
        assert!(result(&ed).contains("    five_ours();\n}") && !result(&ed).contains("xfive"));
        // An empty chunk takes what is typed at its place.
        let mut m = MergeSession { anchors: std::iter::once(4..4).collect(), ..ed.merge.take().unwrap() };
        m.states.truncate(1);
        m.history.clear();
        let mut b = Buffer::from_text(99, None, "abcd\nefgh\n", false);
        (m.seq, m.version) = (b.edit_log.end(), b.version);
        b.view.set_sel(Selection::point(4));
        b.insert("XY", TxKind::Other);
        m.catch_up(&b);
        assert_eq!(m.anchors.first(), Some(&(4..6)));
        assert!(m.states[0].edited);
    }

    #[test]
    fn whole_file_and_ignore() {
        let mut ed = Editor::new(&Config::default());
        open(&mut ed);
        act(&mut ed, MergeAction::AcceptAllRight);
        assert_eq!(result(&ed), THEIRS);
        let m = ed.merge_session().unwrap();
        assert_eq!(m.unresolved(), 0);
        assert_eq!(m.states[0].ours, Side::Ignored);
        assert_eq!(m.anchors[2], THEIRS.find("    five_theirs").unwrap()..THEIRS.find("}\n").unwrap());
        act(&mut ed, MergeAction::AcceptAllLeft);
        assert_eq!(result(&ed), OURS);
        // Back to the start; ignore the conflict from the result pane.
        edit(&mut ed, EditorAction::Undo);
        edit(&mut ed, EditorAction::Undo);
        act(&mut ed, MergeAction::NextConflict);
        let fx = act(&mut ed, MergeAction::Ignore);
        assert!(fx.is_empty(), "{fx:?}");
        let m = ed.merge_session().unwrap();
        assert_eq!((m.states[2].ours, m.states[2].theirs), (Side::Ignored, Side::Ignored));
        assert!(result(&ed).contains("    five();"), "the base stays");
        let fx = act(&mut ed, MergeAction::NextConflict);
        assert!(matches!(fx.as_slice(), [Effect::Notify(_, t)] if t.starts_with("All changes resolved")));
    }

    #[test]
    fn three_panes_with_colors_counter_and_glyphs_and_cancel_restores_the_split() {
        let mut ed = Editor::new(&Config::default());
        ed.open_text(PathBuf::from("/tmp/a.txt"), "a\n", false, &Theme::default());
        ed.open_text(PathBuf::from("/tmp/b.txt"), "b\n", false, &Theme::default());
        edit(&mut ed, EditorAction::SplitRight);
        let before = (ed.split_tree().clone(), ed.group_ids(), ed.focused, ed.tab_names());
        draw(&mut ed, 120, 20);
        open(&mut ed);
        assert_eq!(ed.session_layout().1, &before.0, "sessions see the saved layout");
        let screen = draw(&mut ed, 120, 20);
        assert_eq!(ed.groups.len(), 3);
        let header = row(&screen, 0);
        assert!(header.starts_with(" main.rs  1 of 1 conflict left"), "{header}");
        assert!(header.contains("[Accept Yours] [Accept Theirs] [Apply Non-Conflicting] [Apply] [Cancel]"), "{header}");
        let tabs = row(&screen, 1);
        assert!(tabs.contains("Yours: main") && tabs.contains("Result: main.rs") && tabs.contains("Theirs: dev"), "{tabs}");
        // Line 6 (five) is the conflict in every pane: glyphs and color.
        let theme = Theme::default();
        let y = 2 + 5;
        let text = row(&screen, y);
        assert!(text.contains("»×") && text.contains("»«×") && text.contains("«×"), "{text}");
        let result_x = ed.groups[1].text_area.x;
        assert_eq!(screen[(result_x, y)].bg, theme.merge_conflict_bg);
        assert_eq!(screen[(result_x, y - 4)].bg, theme.merge_resolved_bg, "ONE was applied");
        assert_eq!(screen[(ed.groups[1].last_area.x - 1, y)].bg, theme.merge_conflict_bg, "the splitter beside it");
        // Typing into yours changes nothing.
        let ours_group = ed.groups[0].id;
        ed.focus_group(ours_group);
        edit(&mut ed, EditorAction::InsertText("zzz".into()));
        assert_eq!(ed.active_buffer().unwrap().text(), OURS);
        // Layout commands are refused; Cancel needs no answer (nothing typed).
        let fx = edit(&mut ed, EditorAction::SplitDown);
        assert!(matches!(fx.as_slice(), [Effect::Notify(_, t)] if t == BUSY));
        let fx = edit(&mut ed, EditorAction::CloseTab);
        assert!(fx.is_empty());
        assert!(!ed.merge_active());
        assert_eq!((ed.split_tree().clone(), ed.group_ids(), ed.focused, ed.tab_names()), before);
        assert_eq!(ed.buffers.len(), 2);
        draw(&mut ed, 120, 20);
    }

    #[test]
    fn two_panes_when_narrow_and_a_click_on_a_glyph_accepts() {
        let mut ed = Editor::new(&Config::default());
        draw(&mut ed, 80, 24);
        open(&mut ed);
        let screen = draw(&mut ed, 80, 24);
        assert_eq!(ed.groups.len(), 2);
        assert_eq!(ed.groups[0].views.len(), 2, "yours and theirs share the left pane");
        let header = row(&screen, 0);
        assert!(
            header.starts_with(" 1 of 1 conflict left ")
                && header.trim_end().ends_with("[Yours] [Theirs] [Non-Conflicting] [Apply] [Cancel]"),
            "{header}"
        );
        // The result pane's glyphs: » at its left edge on the conflict line.
        let g = ed.groups[1].last_area.x;
        let y = 2 + 5;
        assert_eq!(screen[(g, y)].symbol(), "»");
        let m = crate::event::Mouse {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            x: g,
            y,
            mods: crossterm::event::KeyModifiers::NONE,
            clicks: 1,
        };
        with_cx(|cx| ed.handle_mouse(&m, Rect::default(), cx));
        assert!(result(&ed).contains("five_ours"));
        assert_eq!(ed.merge_session().unwrap().unresolved(), 0);
        // Wider again: three panes, focus kept.
        draw(&mut ed, 120, 24);
        assert_eq!(ed.groups.len(), 3);
        assert_eq!(ed.active_buffer().unwrap().name, "Result: main.rs");
        // The user's work makes Cancel ask.
        let fx = act(&mut ed, MergeAction::Cancel);
        assert!(matches!(fx.as_slice(), [Effect::Dialog(d)] if d.id == DialogId::MergeCancel));
        assert!(ed.merge_active());
        let fx = with_cx(|cx| ed.merge_cancel(true, cx)).1;
        assert!(fx.is_empty() && !ed.merge_active());
        assert!(ed.buffers.is_empty());
    }

    #[test]
    fn linked_scroll_follows_the_leading_pane() {
        // In the editor: scrolling the result scrolls the others.
        let mut ed = Editor::new(&Config::default());
        let long = |t: &str| format!("{}{t}", "x\n".repeat(40));
        let texts = MergeTexts { base: long(BASE), ours: long(OURS), theirs: format!("extra\n{}", long(THEIRS)) };
        ed.open_merge(PathBuf::from("/tmp/m.rs"), "m.rs".into(), texts, ["a".into(), "b".into()], String::new(), &Theme::default());
        draw(&mut ed, 120, 20);
        let tops = |ed: &Editor| ed.merge_session().unwrap().ids.map(|id| ed.buffers[ed.index_of_id(id).unwrap()].view.scroll_line);
        let t = tops(&ed);
        assert!(t[1] > 30, "the result scrolled to its conflict: {t:?}");
        assert_eq!((t[0], t[2]), (t[1] - 1, t[1]), "yours lacks the line theirs added at the top");
        // The wheel over yours leads the next frame.
        let ours = ed.groups[0].id;
        let wheel = crate::event::Mouse {
            kind: crossterm::event::MouseEventKind::ScrollUp,
            x: 1,
            y: 3,
            mods: crossterm::event::KeyModifiers::NONE,
            clicks: 1,
        };
        ed.scroll_group(ours, &wheel);
        draw(&mut ed, 120, 20);
        let u = tops(&ed);
        assert_eq!((u[0], u[1], u[2]), (t[0] - 3, t[1] - 3, t[2] - 3));
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn blocks_and_fillers_line_the_panes_up() {
        assert_eq!(blocks(&[2..3, 5..5], 8), [0..2, 2..3, 3..5, 5..5, 5..8]);
        assert_eq!(blocks(&[], 4), [0..4]);
        // One chunk of 1, 3 and 1 lines at line 2.
        let bl = [blocks(&[2..3], 6), blocks(&[2..5], 8), blocks(&[2..3], 6)];
        let fills = plan_fillers(&bl);
        assert_eq!(fills[0], [Filler { line: 2, rows: 2, block: 1, bg: None, top: false }]);
        assert!(fills[1].is_empty());
        assert_eq!(fills[2], fills[0]);
        // An empty chunk at the top pads before line 0; elsewhere it pads after the line before.
        let bl0 = blocks(&[0..0], 3);
        let top = plan_fillers(&[bl0.clone(), blocks(&[0..2], 5), bl0.clone()]);
        assert_eq!(top[0], [Filler { line: 0, rows: 2, block: 1, bg: None, top: true }]);
        assert_eq!(top[2], top[0]);
        let rope0 = Rope::from_str("a\nb\nc");
        let (folds0, wrap0) = (crate::editor::fold::Folds::default(), crate::editor::wrap::Wrap::new(false));
        let r0 = Rows::view(&folds0, &wrap0, &top[0], &rope0, 0);
        assert_eq!(block_row(&r0, &bl0, 1, 0), Row { line: 0, seg: -2 });
        assert_eq!(block_row(&r0, &bl0, 1, 1), Row { line: 0, seg: -1 });
        assert_eq!(block_offset(&r0, &bl0, Row { line: 0, seg: -1 }), (1, 1));
        assert_eq!(block_offset(&r0, &bl0, Row::line(0)), (2, 0));
        let mid = plan_fillers(&[blocks(&[1..1], 3), blocks(&[1..3], 5), blocks(&[1..1], 3)]);
        assert_eq!(mid[0], [Filler { line: 0, rows: 2, block: 1, bg: None, top: false }]);
        // A filler row of one pane maps to the matching text line of another.
        let rope = Rope::from_str(&"l\n".repeat(5));
        let (folds, wrap) = (crate::editor::fold::Folds::default(), crate::editor::wrap::Wrap::new(false));
        let short = Rows::view(&folds, &wrap, &fills[0], &rope, 0);
        let long_rope = Rope::from_str(&"l\n".repeat(7));
        let long = Rows::view(&folds, &wrap, &fills[1], &long_rope, 0);
        let (b0, b1) = (&bl[0], &bl[1]);
        let filler = Row { line: 2, seg: 2 };
        assert_eq!(block_offset(&short, b0, filler), (1, 2), "one line, then the second filler row");
        assert_eq!(block_row(&long, b1, 1, 2), Row::line(4));
        assert_eq!(block_row(&short, b0, 1, 2), filler);
        assert_eq!(block_row(&short, b0, 1, 9), filler, "past the fillers: the last one");
        assert_eq!(block_offset(&long, b1, Row::line(4)), (1, 2));
        assert_eq!(block_offset(&long, b1, Row::line(6)), (2, 1));
        assert_eq!(block_row(&short, b0, 0, 1), Row::line(1));
    }

    #[test]
    fn linked_scroll_keeps_chunks_aligned() {
        let mut ed = Editor::new(&Config::default());
        let doc = |mid: &str| format!("{}{mid}{}", "x\n".repeat(40), "y\n".repeat(40));
        let texts = MergeTexts { base: doc("c\n"), ours: doc("O\n"), theirs: doc("t1\nt2\nt3\nt4\nt5\n") };
        ed.open_merge(PathBuf::from("/tmp/m.rs"), "m.rs".into(), texts, ["a".into(), "b".into()], String::new(), &Theme::default());
        draw(&mut ed, 120, 20);
        let wheel = |kind| crate::event::Mouse { kind, x: 1, y: 3, mods: crossterm::event::KeyModifiers::NONE, clicks: 1 };
        let theirs = ed.groups[2].id;
        let tops = |ed: &Editor| ed.merge_session().unwrap().ids.map(|id| ed.buffers[ed.index_of_id(id).unwrap()].view.top());
        let rows_of = |ed: &Editor, p: usize| {
            let b = &ed.buffers[ed.index_of_id(ed.merge_session().unwrap().ids[p]).unwrap()];
            (b.rows().is_filler(b.view.top()), b.view.top())
        };
        // Theirs leads with the chunk's 3rd line on top.
        let ti = ed.index_of_id(ed.merge_session().unwrap().ids[2]).unwrap();
        ed.buffers[ti].view.set_top(Row::line(39));
        ed.scroll_group(theirs, &wheel(crossterm::event::MouseEventKind::ScrollDown));
        draw(&mut ed, 120, 20);
        let t = tops(&ed);
        assert_eq!(t[2], Row::line(42), "theirs is on the chunk's 3rd line: {t:?}");
        assert!(rows_of(&ed, 0).0 && rows_of(&ed, 1).0, "yours and the result show filler rows: {t:?}");
        assert_eq!((t[0].line, t[1].line, t[0].seg, t[1].seg), (40, 40, 2, 2));
        // Past the chunk all three show the same following line.
        ed.buffers[ti].view.set_top(Row::line(44));
        ed.scroll_group(theirs, &wheel(crossterm::event::MouseEventKind::ScrollDown));
        draw(&mut ed, 120, 20);
        let t = tops(&ed);
        assert_eq!((t[2], t[0], t[1]), (Row::line(47), Row::line(43), Row::line(43)), "{t:?}");
        // Down from yours' chunk line lands on the next text line, not a filler.
        let yi = ed.index_of_id(ed.merge_session().unwrap().ids[0]).unwrap();
        ed.buffers[yi].goto_line(40, 0);
        let yours = ed.groups[0].id;
        ed.focus_group(yours);
        edit(&mut ed, EditorAction::Move(crate::actions::Motion::Down, false));
        let b = &ed.buffers[yi];
        assert_eq!(b.rope.char_to_line(b.view.sel().head), 41);
    }

    #[test]
    fn a_change_at_the_top_pads_the_other_panes_before_line_0() {
        let mut ed = Editor::new(&Config::default());
        let base = "x\n".repeat(40);
        let texts = MergeTexts { base: base.clone(), ours: base.clone(), theirs: format!("t1\nt2\n{base}") };
        ed.open_merge(PathBuf::from("/tmp/m.rs"), "m.rs".into(), texts, ["a".into(), "b".into()], String::new(), &Theme::default());
        draw(&mut ed, 120, 20);
        let wheel = |kind| crate::event::Mouse { kind, x: 1, y: 3, mods: crossterm::event::KeyModifiers::NONE, clicks: 1 };
        let tops = |ed: &Editor| ed.merge_session().unwrap().ids.map(|id| ed.buffers[ed.index_of_id(id).unwrap()].view.top());
        let first = [Row { line: 0, seg: -2 }, Row::line(0), Row::line(0)];
        assert_eq!(tops(&ed), first);
        let yi = ed.index_of_id(ed.merge_session().unwrap().ids[0]).unwrap();
        assert!(ed.buffers[yi].rows().is_filler(ed.buffers[yi].view.top()));
        let theirs = ed.groups[2].id;
        ed.scroll_group(theirs, &wheel(crossterm::event::MouseEventKind::ScrollDown));
        draw(&mut ed, 120, 20);
        let t = tops(&ed);
        assert_eq!((t[2], t[0]), (Row::line(3), Row::line(1)), "{t:?}");
        ed.scroll_group(theirs, &wheel(crossterm::event::MouseEventKind::ScrollUp));
        draw(&mut ed, 120, 20);
        assert_eq!(tops(&ed), first);
        // Up from yours' first line stays on the text.
        ed.buffers[yi].goto_line(0, 0);
        let yours = ed.groups[0].id;
        ed.focus_group(yours);
        edit(&mut ed, EditorAction::Move(crate::actions::Motion::Up, false));
        assert_eq!(ed.buffers[yi].view.sel().head, 0);
    }
}
