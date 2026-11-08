//! The model of the Debug window's Disassembly tab.
//!
//! gdb 17.2 disassembles only forward from an address: its `disassemble` ignores a negative `instructionOffset`
//! (it runs `arch.disassemble(pc, count=offset + count)[offset:]`), so a listing "around" the pc cannot be asked.
//! The listing is therefore always `DISASM_COUNT` instructions forward from an *anchor*. The first listing starts at
//! the pc. At a later stop the anchor stays at the old first address while the new pc is one of the listed rows above
//! the last `KEEP_TAIL`, so a run of instruction steps moves the `▶` through a stable listing; otherwise the listing
//! starts at the new pc. This is the same on every adapter and needs no byte offsets on variable-length code.

use crate::ui::widgets::list::ListState;

/// Instructions asked per listing.
pub const DISASM_COUNT: usize = 32;
/// A pc within this many rows of the end of the listing moves the anchor to the pc.
pub const KEEP_TAIL: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisasmRow {
    pub address: String,
    pub symbol: Option<String>,
    pub instruction: String,
    /// `demo.c:10`, set only where the source line changes.
    pub place: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DisasmLoad {
    /// Nothing asked for the current selection yet.
    #[default]
    Idle,
    Asked,
    Shown,
    Failed(String),
}

#[derive(Debug, Default)]
pub struct DisasmView {
    pub load: DisasmLoad,
    pub rows: Vec<DisasmRow>,
    pub list: ListState,
    /// The pc the current ask was made for.
    pc: Option<String>,
    pub pc_row: Option<usize>,
}

/// `0x`/`0X` hex or plain decimal.
pub fn parse_address(s: &str) -> Option<u64> {
    let s = s.trim();
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}

/// Compares the numbers when both parse (so `0x00ab` is `0xab`), else the strings.
fn same_address(a: &str, b: &str) -> bool {
    match (parse_address(a), parse_address(b)) {
        (Some(a), Some(b)) => a == b,
        _ => a == b,
    }
}

impl DisasmView {
    /// A new selection: ask again. The rows stay, for the anchor.
    pub fn reset(&mut self) {
        self.load = DisasmLoad::Idle;
        self.pc = None;
        self.pc_row = None;
    }

    /// A new session, or a new root: nothing is kept.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Where the listing for `pc` starts.
    pub fn anchor_for(&self, pc: &str) -> String {
        if self.load == DisasmLoad::Idle
            && let Some(i) = self.rows.iter().position(|r| same_address(&r.address, pc))
            && i + KEEP_TAIL < self.rows.len()
        {
            return self.rows[0].address.clone();
        }
        pc.to_string()
    }

    /// Marks the listing as asked for `pc` and returns the `memoryReference` to send.
    pub fn ask(&mut self, pc: &str) -> String {
        let anchor = self.anchor_for(pc);
        self.load = DisasmLoad::Asked;
        self.pc = Some(pc.to_string());
        anchor
    }

    /// The answer. False (and nothing changes) unless a listing is being asked for.
    pub fn show(&mut self, rows: Vec<DisasmRow>) -> bool {
        if self.load != DisasmLoad::Asked {
            return false;
        }
        self.pc_row = self.pc.as_deref().and_then(|pc| rows.iter().position(|r| same_address(&r.address, pc)));
        self.list = ListState::default();
        self.list.set_len(rows.len());
        self.list.select(self.pc_row.unwrap_or(0));
        self.rows = rows;
        self.load = DisasmLoad::Shown;
        true
    }

    /// The adapter refused. False (and nothing changes) unless a listing is being asked for.
    pub fn failed(&mut self, message: String) -> bool {
        if self.load != DisasmLoad::Asked {
            return false;
        }
        self.rows.clear();
        self.pc_row = None;
        self.list = ListState::default();
        self.load = DisasmLoad::Failed(message);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(first: u64, n: usize) -> Vec<DisasmRow> {
        (0..n)
            .map(|i| DisasmRow { address: format!("{:#x}", first + 4 * i as u64), symbol: None, instruction: "nop".into(), place: None })
            .collect()
    }

    fn shown(first: u64, n: usize, pc: &str) -> DisasmView {
        let mut v = DisasmView::default();
        v.ask(pc);
        assert!(v.show(rows(first, n)));
        v.reset();
        v
    }

    #[test]
    fn parse_address_reads_hex_and_decimal() {
        assert_eq!(parse_address("0x1f"), Some(31));
        assert_eq!(parse_address("0X1F"), Some(31));
        assert_eq!(parse_address("31"), Some(31));
        assert_eq!(parse_address("zz"), None);
    }

    #[test]
    fn first_listing_starts_at_the_pc() {
        let mut v = DisasmView::default();
        assert_eq!(v.anchor_for("0x1000"), "0x1000");
        assert_eq!(v.ask("0x1000"), "0x1000");
        assert_eq!(v.load, DisasmLoad::Asked);
    }

    #[test]
    fn the_start_stays_while_the_pc_is_above_the_tail() {
        let v = shown(0x1000, DISASM_COUNT, "0x1000");
        // Row 23: 23 + 8 < 32.
        assert_eq!(v.anchor_for(&format!("{:#x}", 0x1000 + 4 * 23)), "0x1000");
        assert_eq!(v.anchor_for("0x1000"), "0x1000");
    }

    #[test]
    fn the_start_moves_to_the_pc_near_the_tail_or_outside() {
        let v = shown(0x1000, DISASM_COUNT, "0x1000");
        let near = format!("{:#x}", 0x1000 + 4 * 24);
        assert_eq!(v.anchor_for(&near), near, "row 24: 24 + 8 is not below 32");
        assert_eq!(v.anchor_for("0x9000"), "0x9000", "not listed");
    }

    #[test]
    fn the_anchor_is_the_pc_unless_idle() {
        let mut v = shown(0x1000, DISASM_COUNT, "0x1000");
        v.ask("0x1004");
        assert_eq!(v.anchor_for("0x1008"), "0x1008", "while asked, the rule does not apply");
    }

    #[test]
    fn same_address_ignores_zero_padding() {
        assert!(same_address("0x00ab", "0xab"));
        assert!(!same_address("0xab", "0xac"));
        assert!(same_address("rip", "rip"));
        assert!(!same_address("rip", "0x1"));
    }

    #[test]
    fn show_marks_and_selects_the_pc_row() {
        let mut v = DisasmView::default();
        v.ask("0x100c");
        assert!(v.show(rows(0x1000, 10)));
        assert_eq!((v.load.clone(), v.pc_row, v.list.selected), (DisasmLoad::Shown, Some(3), 3));
        assert_eq!(v.list.len, 10);
        // A pc outside the listing marks nothing and selects the first row.
        let mut v = DisasmView::default();
        v.ask("0x9000");
        assert!(v.show(rows(0x1000, 10)));
        assert_eq!((v.pc_row, v.list.selected), (None, 0));
    }

    #[test]
    fn show_and_failed_apply_only_when_asked() {
        let mut v = DisasmView::default();
        assert!(!v.show(rows(0x1000, 3)));
        assert!(!v.failed("x".into()));
        assert_eq!((v.load.clone(), v.rows.len()), (DisasmLoad::Idle, 0));
        v.ask("0x1000");
        assert!(v.failed("Cannot access memory at address 0x0".into()));
        assert_eq!(v.load, DisasmLoad::Failed("Cannot access memory at address 0x0".into()));
        assert!(v.rows.is_empty() && v.pc_row.is_none());
        assert!(!v.show(rows(0x1000, 3)), "a failed listing takes no late answer");
    }

    #[test]
    fn reset_keeps_rows_for_the_anchor() {
        let mut v = shown(0x1000, 10, "0x1000");
        assert_eq!((v.load.clone(), v.rows.len(), v.pc_row), (DisasmLoad::Idle, 10, None));
        v.clear();
        assert_eq!((v.load.clone(), v.rows.len()), (DisasmLoad::Idle, 0));
    }
}
