//! Read-only pager: flatten any content into a canonical text, drive the real
//! vim engine over it, and read selections back onto the render pieces.
//!
//! This is embedding **route C (read-only pager), packaged** — see DESIGN.md
//! §6 (phase 2a). Any content UI that can be flattened into text — rendered
//! mail bodies, logs, diffs, help pages — gets visual mode and vim navigation
//! for free:
//!
//! 1. While rendering, feed your pieces into a [`FlatDoc`] through
//!    [`push_group`](FlatDoc::push_group) / [`push_raw`](FlatDoc::push_raw)
//!    (and `open_group`/`push_piece`/`close_group` for pieces joined by raw
//!    separators). The canonical text and the byte-offset bookkeeping are
//!    produced by the same walk, so they can never drift apart.
//! 2. Hand the doc's text to a [`PagerBuf`] (a read-only [`VimBuffer`]; every
//!    engine edit is a no-op) and a [`Pager`] — the visual-mode state machine.
//! 3. Map the engine's byte selection back onto your render units with
//!    [`Pager::group_sel_spans`] / [`selection_spans`].
//!
//! `S` is the host's per-piece annotation (usually a style enum); the engine
//! never inspects it.
//!
//! Extracted from PandaMail's `crates/pm-ui/src/pager.rs`; canonical-text
//! rules, offset accounting, and the selection hi/lo semantics are
//! byte-for-byte identical to the source.

use std::ops::Range;

use vim_core::buffer::{VimBuffer, VimBufferMut};
use vim_core::host::VimHost;
use vim_core::key::Key;
use vim_core::mode::{Mode, VisualKind};
use vim_core::registers::UNNAMED;
use vim_core::state::{Ctx, KeyResult, VimState};

// ---------- flattened document ----------

/// One render unit: a contiguous run of text carrying the host's annotation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Piece<S> {
    /// Piece text, always equal to `canonical_text[flat]`.
    pub text: String,
    /// Host-defined per-piece annotation (style/metadata); opaque to the
    /// engine. Named for the common case.
    pub style: S,
    /// Byte range of this piece within the canonical text.
    pub flat: Range<usize>,
}

/// A text group: the render unit-of-account (one paragraph / heading / list
/// item / table cell / code block). A group's `flat` covers all of its pieces
/// plus any raw separators the host pushed between them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group<S> {
    pub pieces: Vec<Piece<S>>,
    /// Byte range within the canonical text.
    pub flat: Range<usize>,
}

/// A flattened document: the canonical text plus the piece table that maps
/// byte offsets back to render units.
///
/// Build order is render order. Separators pushed between groups via
/// [`FlatDoc::push_raw`] are part of the canonical text (the engine must see
/// the line structure) but belong to no piece, so selection spans never cover
/// them. This is the `Group`/`walk_seq` protocol from PandaMail's pager,
/// abstracted over the content source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlatDoc<S> {
    /// Canonical text — exactly what the engine's buffer sees.
    pub text: String,
    /// Groups in render order; their `flat`s are disjoint ranges into `text`.
    pub groups: Vec<Group<S>>,
    /// Open group (see [`FlatDoc::open_group`]); not part of the document
    /// until [`FlatDoc::close_group`].
    pending: Option<PendingGroup<S>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingGroup<S> {
    start: usize,
    pieces: Vec<Piece<S>>,
}

impl<S> Default for FlatDoc<S> {
    fn default() -> Self {
        Self {
            text: String::new(),
            groups: Vec::new(),
            pending: None,
        }
    }
}

impl<S> FlatDoc<S> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append raw separator text (`"\n\n"` between paragraphs, `"\n"` between
    /// table rows, ...) — canonical text that belongs to no piece. Inside an
    /// open group (see [`FlatDoc::open_group`]) it stays within that group's
    /// range, mirroring code-block lines.
    pub fn push_raw(&mut self, raw: &str) {
        self.text.push_str(raw);
    }

    /// Push one group of contiguous pieces — the pieces' texts are appended
    /// back-to-back with no separator. A group that would be empty (nothing
    /// appended since the current text position) is skipped, matching the
    /// upstream rule that keeps empty render units out of the table.
    pub fn push_group(&mut self, pieces: Vec<(String, S)>) {
        self.open_group();
        for (text, style) in pieces {
            self.push_piece(text, style);
        }
        self.close_group();
    }

    /// Start a group; pieces (and intra-group raw separators) pushed until
    /// [`FlatDoc::close_group`] form one group. Groups must not nest; opening
    /// while one is open implicitly closes it first.
    pub fn open_group(&mut self) {
        self.close_group();
        self.pending = Some(PendingGroup {
            start: self.text.len(),
            pieces: Vec::new(),
        });
    }

    /// Append one piece at the current text position. Outside an open group
    /// this implicitly opens one.
    pub fn push_piece(&mut self, text: String, style: S) {
        if self.pending.is_none() {
            self.open_group();
        }
        let pending = self.pending.as_mut().expect("just opened");
        let start = self.text.len();
        self.text.push_str(&text);
        pending.pieces.push(Piece {
            text,
            style,
            flat: start..self.text.len(),
        });
    }

    /// Finalize the open group; a no-op when nothing was appended (empty
    /// group). Closing with no open group is a no-op.
    pub fn close_group(&mut self) {
        if let Some(pending) = self.pending.take() {
            let end = self.text.len();
            if end > pending.start {
                self.groups.push(Group {
                    pieces: pending.pieces,
                    flat: pending.start..end,
                });
            }
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn groups(&self) -> &[Group<S>] {
        &self.groups
    }

    pub fn into_text(self) -> String {
        self.text
    }

    /// Convenience: a [`PagerBuf`] over a clone of the canonical text.
    pub fn pager_buf(&self) -> PagerBuf {
        PagerBuf::new(self.text.clone())
    }
}

/// Consume piece groups in flatten order while rendering (the host walks its
/// content tree in the same order it built the [`FlatDoc`], so each `next`
/// yields the group for the unit being rendered).
pub struct GroupCursor<'a, S> {
    groups: &'a [Group<S>],
    pos: usize,
}

impl<'a, S> GroupCursor<'a, S> {
    pub fn new(groups: &'a [Group<S>]) -> Self {
        Self { groups, pos: 0 }
    }

    #[allow(clippy::should_implement_trait)] // stream-style cursor, kept for the host API shape
    pub fn next(&mut self) -> Option<&'a Group<S>> {
        let g = self.groups.get(self.pos);
        if g.is_some() {
            self.pos += 1;
        }
        g
    }

    /// Index of the group just consumed.
    pub fn last_index(&self) -> Option<usize> {
        self.pos.checked_sub(1)
    }
}

// ---------- byte/char boundary helpers ----------

/// Largest char boundary `<= i`.
pub fn floor_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Smallest char boundary `>= i`.
pub fn ceil_boundary(s: &str, mut i: usize) -> usize {
    let max = s.len();
    while i < max && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

// ---------- read-only vim buffer ----------

/// A read-only buffer over flattened text (line index precomputed).
///
/// The engine's editing operations are all no-ops: a pager never mutates its
/// content, and the engine tolerates that unchanged (verified upstream).
///
/// Note: the inherent [`PagerBuf::line_start`] / [`PagerBuf::line_end`] take
/// a **byte offset** and return the containing line's start/end — they shadow
/// the `VimBuffer` trait's line-index-based helpers of the same names, and
/// are what line-visual selection expansion uses.
pub struct PagerBuf {
    text: String,
    lines: Vec<(usize, usize)>,
}

impl PagerBuf {
    pub fn new(text: String) -> Self {
        let mut lines = Vec::new();
        let mut start = 0;
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                lines.push((start, i));
                start = i + 1;
            }
        }
        lines.push((start, text.len()));
        Self { text, lines }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Start offset of the line containing byte offset `off`.
    pub fn line_start(&self, offset: usize) -> usize {
        let off = offset.min(self.text.len());
        let idx = self.lines.partition_point(|l| l.0 <= off).saturating_sub(1);
        self.lines[idx].0
    }

    /// End offset (before the newline, if any) of the line containing `off`.
    pub fn line_end(&self, offset: usize) -> usize {
        let off = offset.min(self.text.len());
        let idx = self.lines.partition_point(|l| l.0 <= off).saturating_sub(1);
        self.lines[idx].1.max(self.line_start(off))
    }
}

impl VimBuffer for PagerBuf {
    fn len(&self) -> usize {
        self.text.len()
    }
    fn line_count(&self) -> usize {
        self.lines.len()
    }
    fn char_at(&self, offset: usize) -> Option<char> {
        self.text.get(offset..)?.chars().next()
    }
    fn prev_char_offset(&self, offset: usize) -> Option<usize> {
        if offset == 0 || offset > self.text.len() {
            return None;
        }
        self.text[..offset]
            .chars()
            .next_back()
            .map(|c| offset - c.len_utf8())
    }
    fn line_range(&self, line: usize) -> Range<usize> {
        match self.lines.get(line) {
            Some(l) => {
                // 合同要求非末行 range 含终止 \n（引擎的 linewise 语义依赖
                // 它）；行表存的是内容端点，这里按需补上。经默认 line_end
                // 推导出的「行内容尾」不变，引擎可见语义不受影响。
                if l.1 < self.text.len() && self.text.as_bytes()[l.1] == b'\n' {
                    l.0..l.1 + 1
                } else {
                    l.0..l.1
                }
            }
            None => self.text.len()..self.text.len(),
        }
    }
    fn offset_to_line(&self, offset: usize) -> usize {
        let off = offset.min(self.text.len());
        self.lines.partition_point(|l| l.0 <= off).saturating_sub(1)
    }
    fn slice(&self, range: Range<usize>) -> String {
        let s = floor_boundary(&self.text, range.start);
        let e = ceil_boundary(&self.text, range.end);
        self.text.get(s..e).unwrap_or("").to_string()
    }
}

impl VimBufferMut for PagerBuf {
    fn insert_text(&mut self, _offset: usize, _text: &str) {}
    fn delete_range(&mut self, _range: Range<usize>) {}
}

// ---------- selection read-back ----------

/// Map a byte selection onto a group's pieces: `[(piece index, piece-local
/// byte range)]`, or `None` when the group is not covered.
///
/// Charwise (`line == false`): the selection is clamped into each piece and
/// rounded out to char boundaries, so a multi-byte character straddling the
/// selection edge is covered whole. Linewise: every piece the selection
/// overlaps is covered in full.
pub fn selection_spans<S>(
    group: &Group<S>,
    sel: Range<usize>,
    line: bool,
) -> Option<Vec<(usize, Range<usize>)>> {
    let mut out = Vec::new();
    for (i, p) in group.pieces.iter().enumerate() {
        if line {
            if p.flat.start < sel.end && p.flat.end > sel.start {
                out.push((i, 0..p.text.len()));
            }
        } else {
            let s = sel.start.clamp(p.flat.start, p.flat.end) - p.flat.start;
            let e = sel.end.clamp(p.flat.start, p.flat.end) - p.flat.start;
            let s = floor_boundary(&p.text, s);
            let e = ceil_boundary(&p.text, e);
            if s < e {
                out.push((i, s..e));
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

// ---------- pager state machine ----------

/// Host side effects for the pager: a huge viewport (scrolling belongs to the
/// host UI) and a clipboard sink so `y` can be read back.
#[derive(Default)]
pub struct PagerHost {
    pub clip: Option<String>,
}

impl VimHost for PagerHost {
    fn viewport(&self) -> (usize, usize) {
        (0, 1_000_000)
    }
    fn scroll_to_line(&mut self, _: usize) {}
    fn clipboard_write(&mut self, text: &str) {
        self.clip = Some(text.to_string());
    }
    fn clipboard_read(&self) -> Option<String> {
        None
    }
    fn set_search_highlights(&mut self, _: &[Range<usize>], _: Option<Range<usize>>) {}
    fn begin_undo_group(&mut self, _: u64, _: usize) {}
    fn undo(&mut self) -> Option<usize> {
        None
    }
    fn redo(&mut self) -> Option<usize> {
        None
    }
}

/// The visual-selection state machine over a [`PagerBuf`]: real vim keys in,
/// selection byte ranges out.
///
/// Activation policy follows the upstream host: only visual mode auto-activates
/// (via [`Pager::enter_visual`]); whether normal-mode navigation keys reach the
/// engine is the host's routing decision.
pub struct Pager {
    pub vim: VimState,
    pub buf: PagerBuf,
    pub host: PagerHost,
    pub active: bool,
    yanked: Option<String>,
}

impl Pager {
    pub fn new(text: String) -> Self {
        Self {
            vim: VimState::new(),
            buf: PagerBuf::new(text),
            host: PagerHost::default(),
            active: false,
            yanked: None,
        }
    }

    pub fn set_cursor(&mut self, offset: usize) {
        self.vim.cursor.offset = offset.min(self.buf.len());
        self.vim.cursor.desired_col = None;
    }

    pub fn cursor_offset(&self) -> usize {
        self.vim.cursor.offset
    }

    /// (byte length, line count) for status bars.
    pub fn buf_stats(&self) -> (usize, usize) {
        (self.buf.len(), self.buf.line_count())
    }

    /// Enter visual (charwise) or visual-line mode at the cursor.
    pub fn enter_visual(&mut self, line_mode: bool) {
        let key = if line_mode {
            Key::char('V')
        } else {
            Key::char('v')
        };
        let mut ctx = Ctx {
            buf: &mut self.buf,
            host: &mut self.host,
        };
        self.vim.handle_key(&mut ctx, key);
        self.active = matches!(self.vim.mode, Mode::Visual { .. });
    }

    /// All keys while the pager is active.
    pub fn feed(&mut self, key: Key) -> KeyResult {
        let mut ctx = Ctx {
            buf: &mut self.buf,
            host: &mut self.host,
        };
        let r = self.vim.handle_key(&mut ctx, key);
        if !matches!(self.vim.mode, Mode::Visual { .. }) {
            self.active = false;
        }
        r
    }

    /// Yanked text: the host clipboard sink first (the `"+` path), then the
    /// engine's unnamed register (plain `y` after visual mode exits).
    pub fn take_clip(&mut self) -> Option<String> {
        if let Some(text) = self.host.clip.take() {
            return Some(text);
        }
        if let Some(reg) = self.vim.registers.get(UNNAMED) {
            if !reg.text.is_empty() {
                self.yanked = Some(reg.text.clone());
                return self.yanked.take();
            }
        }
        None
    }

    /// Current selection: `(byte range, is linewise)`.
    pub fn selection(&self) -> Option<(Range<usize>, bool)> {
        if !self.active {
            return None;
        }
        let (a, c, kind) = self.vim.visual_selection()?;
        let lo = floor_boundary(&self.buf.text, a.min(c));
        // Charwise: include the character under the cursor (hi ends at that
        // character's last byte; the engine parks the cursor on the starting
        // byte, and ceil-to-boundary alone would exclude it exactly at the
        // boundary).
        let cur = floor_boundary(&self.buf.text, a.max(c));
        let hi = cur
            + self.buf.text[cur..]
                .chars()
                .next()
                .map(|ch| ch.len_utf8())
                .unwrap_or(0);
        let _ = kind;
        if kind == VisualKind::Line {
            let lo = self.buf.line_start(lo);
            let hi = self.buf.line_end(hi);
            Some((lo..hi, true))
        } else {
            Some((lo..hi, false))
        }
    }

    pub fn selected_text(&self) -> Option<String> {
        let (range, _) = self.selection()?;
        Some(self.buf.slice(range))
    }

    /// Selection coverage of one piece group: `[(piece index, piece-local
    /// byte range)]`, or `None` when inactive or the group is not covered.
    pub fn group_sel_spans<S>(&self, group: &Group<S>) -> Option<Vec<(usize, Range<usize>)>> {
        let (sel, line) = self.selection()?;
        selection_spans(group, sel, line)
    }

    /// Command-line prompt string (`/` search while visual is active).
    pub fn cmdline_prompt(&self) -> Option<String> {
        match &self.vim.mode {
            Mode::CommandLine { prompt } => Some(format!("{prompt}{}", self.vim.cmdline.buffer)),
            _ => None,
        }
    }

    /// Visual mode label for status bars.
    pub fn mode_label(&self) -> Option<&'static str> {
        if !self.active {
            return None;
        }
        match self.vim.mode {
            Mode::Visual {
                kind: VisualKind::Line,
            } => Some("V-LINE"),
            Mode::Visual { .. } => Some("VISUAL"),
            _ => None,
        }
    }
}

/// Selection character count for status bars.
pub fn selected_char_count(pager: &Pager) -> usize {
    pager
        .selected_text()
        .map(|t| t.chars().count())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Simple stand-in for a host's piece-style enum.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Tag {
        A,
        B,
    }

    /// Pieces: [0..7 "你好 " A] · raw "\n" · [8..10 "ab" B] [10..13 " cd" A]
    fn doc() -> FlatDoc<Tag> {
        let mut d = FlatDoc::new();
        d.push_group(vec![("你好 ".into(), Tag::A)]);
        d.push_raw("\n");
        d.push_group(vec![("ab".into(), Tag::B), (" cd".into(), Tag::A)]);
        d
    }

    #[test]
    fn builder_offsets_match_text() {
        let d = doc();
        assert_eq!(d.text, "你好 \nab cd");
        assert_eq!(d.groups.len(), 2);
        for g in &d.groups {
            let joined: String = g.pieces.iter().map(|p| p.text.as_str()).collect();
            assert_eq!(
                &d.text[g.flat.clone()],
                &joined,
                "group text must align with canonical text"
            );
        }
        assert_eq!(d.groups[1].pieces[0].flat, 8..10);
        assert_eq!(d.groups[1].pieces[1].flat, 10..13);
        assert_eq!(d.groups[1].pieces[0].style, Tag::B);
    }

    #[test]
    fn open_close_group_covers_internal_separators() {
        // Code-block shape: one group, per-line pieces joined by raw "\n".
        let mut d = FlatDoc::new();
        d.open_group();
        d.push_piece("l1".into(), Tag::B);
        d.push_raw("\n");
        d.push_piece("l2".into(), Tag::B);
        d.close_group();
        assert_eq!(d.text, "l1\nl2");
        assert_eq!(d.groups.len(), 1);
        assert_eq!(
            d.groups[0].flat,
            0..5,
            "group covers intra-group separators"
        );
        assert_eq!(d.groups[0].pieces[1].flat, 3..5);

        // Empty open/close and empty push_group produce no group.
        let mut d = FlatDoc::<Tag>::new();
        d.open_group();
        d.close_group();
        d.push_group(vec![]);
        d.push_group(vec![("".into(), Tag::A)]);
        assert!(d.groups.is_empty());
        assert_eq!(d.text, "");
    }

    #[test]
    fn empty_document() {
        let buf = PagerBuf::new(String::new());
        assert_eq!(buf.len(), 0);
        assert_eq!(buf.line_count(), 1, "an empty buffer still has one line");
        assert_eq!(buf.line_range(0), 0..0);
        assert_eq!(
            buf.line_range(9),
            0..0,
            "out-of-range line clamps to empty tail"
        );
        assert_eq!(buf.char_at(0), None);
        assert_eq!(buf.prev_char_offset(0), None);
        assert_eq!(buf.offset_to_line(0), 0);
        assert_eq!(buf.slice(0..0), "");

        let d = FlatDoc::<Tag>::default();
        assert!(d.groups.is_empty());
        let g = Group::<Tag> {
            pieces: vec![],
            flat: 0..0,
        };
        assert_eq!(selection_spans(&g, 0..0, false), None);
    }

    #[test]
    fn line_accounting_including_last_line_without_newline() {
        let buf = PagerBuf::new("ab\ncd\n".into());
        assert_eq!(
            buf.line_count(),
            3,
            "trailing newline opens an empty last line"
        );
        // 非末行 range 含终止 \n（引擎 buffer 合同；行内容端点见行表）。
        assert_eq!(buf.line_range(0), 0..3);
        assert_eq!(buf.line_range(1), 3..6);
        assert_eq!(buf.line_range(2), 6..6);
        assert_eq!(buf.char_at(2), Some('\n'));
        assert_eq!(buf.prev_char_offset(3), Some(2));
        assert_eq!(buf.offset_to_line(2), 0);
        assert_eq!(buf.offset_to_line(3), 1);
        assert_eq!(buf.offset_to_line(6), 2);
        assert_eq!(buf.slice(1..4), "b\nc");

        // Inherent offset-based helpers (used by line-visual expansion).
        assert_eq!(buf.line_start(4), 3);
        assert_eq!(buf.line_end(4), 5);
        assert_eq!(buf.line_start(6), 6);
        assert_eq!(buf.line_end(6), 6);

        // Final line without trailing newline ends at text end.
        let buf = PagerBuf::new("l1\nl2".into());
        assert_eq!(buf.line_count(), 2);
        assert_eq!(buf.line_range(1), 3..5);
        assert_eq!(buf.line_end(3), 5);
        assert_eq!(buf.slice(3..5), "l2");
    }

    #[test]
    fn utf8_multibyte_boundaries() {
        let buf = PagerBuf::new("中文x".into()); // boundaries 0, 3, 6, 7
        assert_eq!(buf.char_at(1), None, "no char starts mid-character");
        assert_eq!(buf.char_at(3), Some('文'));
        assert_eq!(
            buf.slice(1..2),
            "中",
            "slice floors/ceils to char boundaries"
        );
        assert_eq!(buf.slice(1..5), "中文");
        assert_eq!(buf.slice(0..7), "中文x");

        // A selection ending mid-character covers that character whole.
        let mut d = FlatDoc::new();
        d.push_group(vec![("中文".into(), Tag::A)]);
        let g = &d.groups[0];
        assert_eq!(selection_spans(g, 1..2, false), Some(vec![(0, 0..3)]));
        assert_eq!(selection_spans(g, 1..4, false), Some(vec![(0, 0..6)]));
        assert_eq!(selection_spans(g, 4..5, false), Some(vec![(0, 3..6)]));
    }

    #[test]
    fn selection_spans_at_piece_boundaries() {
        let d = doc();
        let g = &d.groups[1]; // "ab" B | " cd" A, flat 8..13

        // Exactly one piece.
        assert_eq!(selection_spans(g, 8..10, false), Some(vec![(0, 0..2)]));
        // Across the internal piece boundary.
        assert_eq!(
            selection_spans(g, 8..13, false),
            Some(vec![(0, 0..2), (1, 0..3)]),
            "spans are in piece order"
        );
        // Selection starting in the separator before the group.
        assert_eq!(selection_spans(g, 7..9, false), Some(vec![(0, 0..1)]));
        // Zero-width selection at a piece boundary is empty.
        assert_eq!(selection_spans(g, 8..8, false), None);
        assert_eq!(selection_spans(g, 10..10, false), None);
        // Wholly before / after the group.
        assert_eq!(selection_spans(g, 0..7, false), None);
        assert_eq!(selection_spans(g, 13..15, false), None);

        // Linewise: overlapping pieces are covered in full, separator-only
        // pieces excluded.
        assert_eq!(
            selection_spans(g, 8..11, true),
            Some(vec![(0, 0..2), (1, 0..3)]),
        );
        assert_eq!(selection_spans(g, 0..8, true), None);
    }

    #[test]
    fn visual_selection_end_to_end() {
        let d = doc();
        let mut pager = Pager::new(d.text.clone());
        assert_eq!(pager.selection(), None, "inactive pager has no selection");
        pager.set_cursor(8); // "ab" start
        pager.enter_visual(false);
        assert!(pager.active);
        pager.feed(Key::char('$')); // to end of line "ab cd"
        let (range, is_line) = pager.selection().expect("active visual has a selection");
        assert!(!is_line);
        assert_eq!(range, 8..13);
        assert_eq!(pager.selected_text().as_deref(), Some("ab cd"));
        // Both pieces of group 1 covered, in order, with styles reachable.
        let spans = pager.group_sel_spans(&d.groups[1]).expect("group covered");
        assert_eq!(spans, vec![(0, 0..2), (1, 0..3)]);
        // Separator between groups belongs to no piece.
        assert_eq!(pager.group_sel_spans(&d.groups[0]), None);

        pager.feed(Key::char('y'));
        assert_eq!(pager.take_clip().as_deref(), Some("ab cd"));
        assert!(!pager.active, "y exits visual mode");
        assert_eq!(pager.mode_label(), None);
    }

    #[test]
    fn visual_line_and_last_line_without_newline() {
        // Linewise on the last line (which has no trailing newline): the
        // selection must cover the full line text.
        let mut pager = Pager::new("l1\nl2".into());
        pager.set_cursor(3);
        pager.enter_visual(true);
        assert!(pager.active);
        assert_eq!(pager.mode_label(), Some("V-LINE"));
        let (range, is_line) = pager.selection().expect("line selection");
        assert!(is_line);
        assert_eq!(range, 3..5, "linewise selection covers the full last line");
        assert_eq!(pager.selected_text().as_deref(), Some("l2"));

        // V + j from the first line: selection extends over both lines.
        let mut pager = Pager::new("l1\nl2".into());
        pager.set_cursor(0);
        pager.enter_visual(true);
        pager.feed(Key::char('j'));
        assert_eq!(pager.cursor_offset(), 3);
        let (range, is_line) = pager.selection().unwrap();
        assert!(is_line);
        assert_eq!(range, 0..5);

        // Charwise j lands on the same column of the next line and the
        // selection runs from the anchor char through the cursor char.
        let mut pager = Pager::new("l1\nl2".into());
        pager.set_cursor(0);
        pager.enter_visual(false);
        pager.feed(Key::char('j'));
        let (range, is_line) = pager.selection().unwrap();
        assert!(!is_line);
        assert_eq!(range, 0..4);
    }

    #[test]
    fn empty_pager_is_inert() {
        let mut pager = Pager::new(String::new());
        assert_eq!(pager.buf_stats(), (0, 1));
        pager.set_cursor(99); // clamps to len
        assert_eq!(pager.cursor_offset(), 0);
        assert_eq!(pager.take_clip(), None);
        assert_eq!(pager.mode_label(), None);
        // `/` search works from normal mode even on an empty buffer.
        assert_eq!(pager.feed(Key::char('/')), KeyResult::Consumed);
        assert_eq!(pager.cmdline_prompt().as_deref(), Some("/"));
    }
}

#[cfg(test)]
mod tck_tests {
    //! 引擎 TCK（vim_core::tck）对 PagerBuf 的验收。
    use super::*;
    use vim_core::tck;

    /// 只读契约（编辑/冒烟契约不适用——VimBufferMut 为刻意的空实现）。
    #[test]
    fn pager_buf_read_contract() {
        let buf = PagerBuf::new("alpha\nbeta 中文\n\n尾行\n".into());
        tck::buffer_read_contract(&buf).unwrap();
        let buf = PagerBuf::new("中文 mix 👨‍👩‍👧".into());
        tck::buffer_read_contract(&buf).unwrap();
        let buf = PagerBuf::new(String::new());
        tck::buffer_read_contract(&buf).unwrap();
    }
}
