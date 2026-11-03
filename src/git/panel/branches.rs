//! Log tab: the branch tree left of the commit list (HEAD, Local, Remote › <remote>).

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::buffer::Buffer as Screen;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::{Area, GitPanel, put};
use crate::app::Ctx;
use crate::config::theme::Theme;
use crate::event::{Effect, Mouse};
use crate::git::model::LogRev;
use crate::ui::menu::ContextMenu;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum BranchRow {
    Head,
    /// Header "Local".
    Local,
    /// Header "Remote".
    Remote,
    /// Header of one remote, such as "origin".
    RemoteName(String),
    /// Index into `GitPanel::branches`.
    Branch(usize),
}

/// The pane shows only when the Log body is at least this wide.
pub(super) const BRANCHES_MIN_BODY: u16 = 100;

/// Columns of the pane for a Log body `body_w` wide; 0 hides it.
pub(super) fn pane_width(body_w: u16, has_branches: bool) -> u16 {
    if !has_branches || body_w < BRANCHES_MIN_BODY { 0 } else { (body_w / 5).clamp(20, 32) }
}

impl GitPanel {
    /// The tree rows, rebuilt from `branches` and the collapsed headers.
    pub(super) fn branch_rows(&self) -> Vec<BranchRow> {
        let mut rows = vec![BranchRow::Head, BranchRow::Local];
        if !self.branch_collapsed.contains(&BranchRow::Local) {
            rows.extend(self.branches.iter().enumerate().filter(|(_, b)| !b.remote).map(|(i, _)| BranchRow::Branch(i)));
        }
        let mut remotes: Vec<&str> = Vec::new();
        for b in self.branches.iter().filter(|b| b.remote) {
            let r = b.name.split('/').next().unwrap_or(&b.name);
            if !remotes.contains(&r) {
                remotes.push(r);
            }
        }
        if !remotes.is_empty() {
            rows.push(BranchRow::Remote);
            if !self.branch_collapsed.contains(&BranchRow::Remote) {
                for r in remotes {
                    let header = BranchRow::RemoteName(r.to_string());
                    let open = !self.branch_collapsed.contains(&header);
                    rows.push(header);
                    if open {
                        let own = |b: &&crate::git::model::BranchInfo| b.remote && b.name.split('/').next() == Some(r);
                        rows.extend(self.branches.iter().enumerate().filter(|(_, b)| own(b)).map(|(i, _)| BranchRow::Branch(i)));
                    }
                }
            }
        }
        rows
    }

    pub(super) fn toggle_branch_header(&mut self, row: &BranchRow) {
        if matches!(row, BranchRow::Local | BranchRow::Remote | BranchRow::RemoteName(_)) && !self.branch_collapsed.remove(row) {
            self.branch_collapsed.insert(row.clone());
        }
    }

    fn selected_branch_row(&self) -> Option<BranchRow> {
        self.branch_rows().get(self.branch_list.selected).cloned()
    }

    /// Space, Enter on a header, or a click: filter the Log, or toggle a header.
    pub(super) fn activate_branch_row(&mut self, cx: &mut Ctx) {
        match self.selected_branch_row() {
            Some(BranchRow::Head) => self.edit_log_filter(|f| f.rev = LogRev::Head, cx),
            Some(BranchRow::Branch(i)) => {
                if let Some(name) = self.branches.get(i).map(|b| b.name.clone()) {
                    self.edit_log_filter(|f| f.rev = LogRev::Rev(name), cx);
                }
            }
            Some(h) => self.toggle_branch_header(&h),
            None => {}
        }
    }

    /// Enter on a row: toggle a header, else the branch menu.
    pub(super) fn open_branch_row(&mut self, cx: &mut Ctx) {
        match self.selected_branch_row() {
            Some(h @ (BranchRow::Local | BranchRow::Remote | BranchRow::RemoteName(_))) => self.toggle_branch_header(&h),
            _ => self.branch_menu(None, cx),
        }
    }

    /// The operations of the selected branch (HEAD: the checked-out local branch).
    pub(super) fn branch_menu(&mut self, at: Option<(u16, u16)>, cx: &mut Ctx) {
        let target = match self.selected_branch_row() {
            Some(BranchRow::Branch(i)) => self.branches.get(i),
            Some(BranchRow::Head) => self.branches.iter().find(|b| b.current && !b.remote),
            _ => None,
        };
        let Some(b) = target.cloned() else { return };
        let items = self.branch_op_items(&b);
        if items.is_empty() {
            return;
        }
        let (x, y) = at.unwrap_or_else(|| {
            let row = self.branch_list.selected.saturating_sub(self.branch_list.offset) as u16;
            (self.branches_rect.x + 2, self.branches_rect.y + row)
        });
        cx.effects.push(Effect::ContextMenu(ContextMenu::new(items, x, y)));
    }

    /// A click in the pane; false when it is outside.
    pub(super) fn click_branches(&mut self, m: &Mouse, cx: &mut Ctx) -> bool {
        if !self.branches_rect.contains(ratatui::layout::Position::new(m.x, m.y)) {
            return false;
        }
        self.area = Area::Branches;
        self.branch_list.fit(self.branch_rows().len(), self.branches_rect.height as usize);
        if let Some(i) = self.branch_list.hit(self.branches_rect, m.x, m.y) {
            self.branch_list.select(i);
            if m.kind == MouseEventKind::Down(MouseButton::Right) {
                self.branch_menu(Some((m.x, m.y)), cx);
            } else {
                self.activate_branch_row(cx);
            }
        }
        true
    }

    pub(super) fn render_branches(&mut self, buf: &mut Screen, r: Rect, active: bool, theme: &Theme) {
        self.branches_rect = r;
        let rows = self.branch_rows();
        self.branch_list.fit(rows.len(), r.height as usize);
        for (vi, i) in self.branch_list.visible().enumerate() {
            let y = r.y + vi as u16;
            let selected = i == self.branch_list.selected;
            let base = if selected {
                if active { theme.list_selected } else { theme.list_selected_unfocused }
            } else {
                Style::default().fg(theme.fg)
            };
            if selected {
                buf.set_style(Rect::new(r.x, y, r.width, 1), base);
            }
            let bold = base.add_modifier(Modifier::BOLD);
            let dim = base.add_modifier(Modifier::DIM);
            let side = if selected { base } else { theme.dim };
            let mut x = r.x;
            let header = |buf: &mut Screen, x: &mut u16, depth: u16, row: &BranchRow, label: &str| {
                *x += depth * 2;
                let arrow = if self.branch_collapsed.contains(row) { "▸ " } else { "▾ " };
                put(buf, x, y, r.right(), arrow, dim);
                put(buf, x, y, r.right(), label, bold);
            };
            match &rows[i] {
                BranchRow::Head => {
                    put(buf, &mut x, y, r.right(), "  HEAD", base);
                    if let Some(l) = self.branch_label() {
                        put(buf, &mut x, y, r.right(), &format!(" ({l})"), side);
                    }
                    if self.log_filter.rev == LogRev::Head {
                        buf.set_style(Rect::new(r.x + 2, y, 4.min(r.width.saturating_sub(2)), 1), bold);
                    }
                }
                row @ BranchRow::Local => header(buf, &mut x, 0, row, "Local"),
                row @ BranchRow::Remote => header(buf, &mut x, 0, row, "Remote"),
                row @ BranchRow::RemoteName(n) => header(buf, &mut x, 1, row, n),
                BranchRow::Branch(bi) => {
                    let Some(b) = self.branches.get(*bi) else { continue };
                    x += if b.remote { 4 } else { 2 };
                    put(buf, &mut x, y, r.right(), if b.current { "* " } else { "  " }, base);
                    let label = if b.remote { b.name.split_once('/').map_or(b.name.as_str(), |(_, n)| n) } else { &b.name };
                    let filtered = matches!(&self.log_filter.rev, LogRev::Rev(n) if *n == b.name);
                    let text = crate::util::text::fit(label, r.right().saturating_sub(x) as usize);
                    put(buf, &mut x, y, r.right(), &text, if b.current || filtered { bold } else { base });
                    if b.ahead > 0 {
                        put(buf, &mut x, y, r.right(), &format!(" ↑{}", b.ahead), side);
                    }
                    if b.behind > 0 {
                        put(buf, &mut x, y, r.right(), &format!(" ↓{}", b.behind), side);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::model::BranchInfo;

    fn b(name: &str, remote: bool) -> BranchInfo {
        BranchInfo { name: name.into(), upstream: None, ahead: 0, behind: 0, current: false, remote }
    }

    fn panel() -> GitPanel {
        let mut p = GitPanel::new();
        p.branches = vec![b("main", false), b("dev", false), b("origin/main", true), b("origin/feature/x", true), b("upstream/main", true)];
        p
    }

    #[test]
    fn branch_rows_group_local_and_remotes() {
        use BranchRow::*;
        assert_eq!(
            panel().branch_rows(),
            [
                Head,
                Local,
                Branch(0),
                Branch(1),
                Remote,
                RemoteName("origin".into()),
                Branch(2),
                Branch(3),
                RemoteName("upstream".into()),
                Branch(4)
            ]
        );
    }

    #[test]
    fn collapsed_headers_hide_their_rows() {
        use BranchRow::*;
        let mut p = panel();
        p.toggle_branch_header(&Local);
        p.toggle_branch_header(&RemoteName("origin".into()));
        assert_eq!(p.branch_rows(), [Head, Local, Remote, RemoteName("origin".into()), RemoteName("upstream".into()), Branch(4)]);
        p.toggle_branch_header(&Local);
        p.toggle_branch_header(&RemoteName("origin".into()));
        assert_eq!(p.branch_rows().len(), 10);
        p.toggle_branch_header(&Head);
        assert!(p.branch_collapsed.is_empty());
    }

    #[test]
    fn no_remotes_no_remote_header() {
        let mut p = GitPanel::new();
        p.branches = vec![b("main", false)];
        assert_eq!(p.branch_rows(), [BranchRow::Head, BranchRow::Local, BranchRow::Branch(0)]);
    }

    #[test]
    fn pane_width_hides_below_the_minimum() {
        assert_eq!(pane_width(99, true), 0);
        assert_eq!(pane_width(100, true), 20);
        assert_eq!(pane_width(115, true), 23);
        assert_eq!(pane_width(200, true), 32);
        assert_eq!(pane_width(150, false), 0);
    }
}
