//! Reusable vim text-editing entity (the officially packaged form of
//! embedding route B, "host-routed keys").
//!
//! - Keys are routed by the host: the host converts gpui [`gpui::Keystroke`]
//!   into engine keys via [`crate::to_core_key`] and feeds
//!   [`VimEdit::process_key`] directly, bypassing [`crate::attach`] (to avoid
//!   double processing); IME-committed text enters the engine through
//!   `EntityInputHandler::replace_text_in_range`. The host only handles
//!   routing and events (drain [`VimEditEvent`]).
//! - Single-line mode: form fields, path input; multi-line: body editors.
//! - Engine side effects are staged in [`EditHost`], landed on flush and
//!   surfaced as [`VimEditEvent`]s.
//! - Visual parameters live in [`VimEditStyle`]: hosts build them from their
//!   own theme (see host-side `theme_style()`-style helpers), and
//!   [`VimEditStyle::default`] provides a neutral dark palette.
//! - vimrc layering: [`VimEdit::new`] loads only the cross-app user layer
//!   `~/.vimcorerc` (lenient actions); the host-specific layer is appended by
//!   the host after construction via [`crate::config::load_layers`]
//!   (loaded later, same-key overrides, strict actions).

use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    canvas, div, px, rgb, rgba, App, Context, ElementInputHandler, EntityInputHandler, FocusHandle,
    Focusable, Point, ScrollHandle, SharedString, Window,
};
use unicode_width::UnicodeWidthChar as _;
use vimcore::buffer::{clamp_to_line_end, VimBuffer, VimBufferMut};
use vimcore::host::{ScrollAnchor, VimHost};
use vimcore::key::KeyKind;
use vimcore::mode::{Mode, VisualKind};
use vimcore::state::{Ctx, KeyResult, VimState};

// ---------- visual style ----------

/// Visual parameters for the edit box (colors/font/metrics). Hosts build them
/// from their own theme; `Default` is a neutral dark palette (0xRRGGBB,
/// feeding straight into gpui `rgb`/`rgba`).
#[derive(Debug, Clone, PartialEq)]
pub struct VimEditStyle {
    /// Monospace font family.
    pub mono_font: String,
    /// Font size (px).
    pub font_size: f32,
    /// Line height (px); scrolling and click-to-position both use it.
    pub line_height: f32,
    /// Monospace character advance (px); cursor/highlight column positions use it.
    pub char_width: f32,
    /// Edit box background.
    pub surface_bg: u32,
    /// Border when unfocused.
    pub border: u32,
    /// Border when focused.
    pub focus_border: u32,
    /// Accent (focus background blend, search highlight, visual selection).
    pub accent: u32,
    /// Highlight of the current search match.
    pub gold: u32,
    /// Cursor color.
    pub cursor: u32,
    /// Placeholder text color (non-content rendering when the buffer is empty).
    pub placeholder: u32,
}

impl Default for VimEditStyle {
    fn default() -> Self {
        Self {
            mono_font: "Menlo".to_string(),
            font_size: 13.0,
            line_height: 20.0,
            char_width: 7.6,
            surface_bg: 0x1f2335,
            border: 0x2a2e42,
            focus_border: 0x7aa2f7,
            accent: 0x7aa2f7,
            gold: 0xe0af68,
            cursor: 0xc0caf5,
            placeholder: 0x565f89,
        }
    }
}

/// Linear blend between a and b (t=0 → a).
fn blend_rgb(a: u32, b: u32, t: f32) -> u32 {
    let (ar, ag, ab) = (a >> 16, (a >> 8) & 0xff, a & 0xff);
    let (br, bg_, bb) = (b >> 16, (b >> 8) & 0xff, b & 0xff);
    let f = |x: u32, y: u32| (x as f32 + (y as f32 - x as f32) * t).round() as u32;
    (f(ar, br) << 16) | (f(ag, bg_) << 8) | f(ab, bb)
}

// ---------- line buffer (Arc COW) ----------

#[derive(Clone, Default)]
pub struct SharedLines(Rc<std::cell::RefCell<Arc<Vec<String>>>>);

impl SharedLines {
    pub fn get(&self) -> Arc<Vec<String>> {
        self.0.borrow().clone()
    }
    pub fn set(&self, lines: Arc<Vec<String>>) {
        *self.0.borrow_mut() = lines;
    }
}

/// offset (UTF-8 bytes) ↔ (line, grapheme column).
///
/// `offset` usually comes from the engine (char boundary), but this path also
/// receives host-side input (IME restore, stale offsets after a host splice):
/// non-boundary offsets floor down to the boundary instead of panicking on the
/// `line[..rem]` slice.
pub fn offset_rc(lines: &[String], offset: usize) -> (usize, usize) {
    let mut rem = offset;
    for (row, line) in lines.iter().enumerate() {
        if rem <= line.len() {
            let mut boundary = rem;
            while boundary > 0 && !line.is_char_boundary(boundary) {
                boundary -= 1;
            }
            let col = line[..boundary].chars().count();
            return (row, col);
        }
        rem -= line.len() + 1;
    }
    (lines.len().saturating_sub(1), 0)
}

pub fn rc_offset(lines: &[String], row: usize, col: usize) -> usize {
    let mut off = 0;
    for (i, line) in lines.iter().enumerate() {
        if i == row {
            return off
                + line
                    .chars()
                    .take(col)
                    .map(|c| c.len_utf8())
                    .sum::<usize>()
                    .min(line.len());
        }
        off += line.len() + 1;
    }
    off.saturating_sub(1)
}

/// VimBuffer implementation over `Arc<Vec<String>>`.
pub struct LinesBuf(pub SharedLines);

impl VimBuffer for LinesBuf {
    fn len(&self) -> usize {
        let lines = self.0.get();
        lines
            .iter()
            .map(|l| l.len() + 1)
            .sum::<usize>()
            .saturating_sub(1)
    }
    fn line_count(&self) -> usize {
        self.0.get().len().max(1)
    }
    fn char_at(&self, offset: usize) -> Option<char> {
        // Contract: a non-char-boundary offset must return None (the engine
        // probes range.end-1 via line_end/line_indent; with a multi-byte line
        // end the offset lands mid-character). Locate per line, no whole-
        // buffer join — this is the engine's per-character motion probe path,
        // so allocation-free O(offset) beats an O(all text) join.
        if offset >= self.len() {
            return None;
        }
        let lines = self.0.get();
        let mut rem = offset;
        for line in lines.iter() {
            if rem < line.len() {
                return line.is_char_boundary(rem).then(|| line[rem..].chars().next())?;
            }
            if rem == line.len() {
                // line-end separator byte: offset < len guarantees a following line
                return Some('\n');
            }
            rem -= line.len() + 1;
        }
        None
    }
    fn prev_char_offset(&self, offset: usize) -> Option<usize> {
        if offset == 0 || offset > self.len() {
            return None;
        }
        let lines = self.0.get();
        let mut rem = offset;
        for line in lines.iter() {
            if rem < line.len() {
                if !line.is_char_boundary(rem) {
                    return None;
                }
                return match line[..rem].chars().next_back() {
                    Some(c) => Some(offset - c.len_utf8()),
                    // no preceding char inside the line = line start: the
                    // previous byte is the inter-line separator '\n' (the
                    // first line's start is offset 0, already caught by the
                    // early return), always length 1
                    None => Some(offset - 1),
                };
            }
            if rem == line.len() {
                // offset is the line end: it points at this line's separator
                // '\n' (when a line follows) or just past the buffer's last
                // byte (last line) — in both cases the previous char is this
                // line's last content char; an empty line steps back one byte
                // (the previous line's '\n')
                return match line.chars().next_back() {
                    Some(c) => Some(offset - c.len_utf8()),
                    None => Some(offset - 1),
                };
            }
            rem -= line.len() + 1;
        }
        None
    }
    fn line_range(&self, line: usize) -> Range<usize> {
        let lines = self.0.get();
        // The contract requires the range to include this line's newline
        // (end = next line's start / last line = buffer end), otherwise the
        // engine's linewise operators (dd/cc/yy) would leave an empty line in place
        let start = rc_offset(&lines, line, 0);
        match lines.get(line) {
            Some(l) if line + 1 < lines.len() => start..start + l.len() + 1,
            Some(l) => start..start + l.len(),
            None => self.len()..self.len(),
        }
    }
    fn offset_to_line(&self, offset: usize) -> usize {
        let lines = self.0.get();
        offset_rc(&lines, offset).0
    }
    fn slice(&self, range: Range<usize>) -> String {
        let lines = self.0.get();
        let text = lines.join("\n");
        text.get(range).unwrap_or("").to_string()
    }
}

impl VimBufferMut for LinesBuf {
    // Newlines are inserted/deleted as real content, never flattened: the
    // single-line semantics are applied by VimEdit.single_line collapsing on
    // flush (judging by content line count would misread a multi-line
    // editor's initial one-line state as single-line mode and replace every
    // enter/o '\n' with a space)
    fn insert_text(&mut self, offset: usize, text: &str) -> bool {
        let lines = self.0.get();
        let text = text.replace('\r', "");
        let mut vec = lines.as_ref().clone();
        let (row, col) = offset_rc(&vec, offset);
        let byte = rc_offset(&vec, row, col);
        let mut joined = vec.join("\n");
        let at = byte.min(joined.len());
        joined.insert_str(at, &text);
        vec = joined.split('\n').map(String::from).collect();
        self.0.set(Arc::new(vec));
        true
    }
    fn delete_range(&mut self, range: Range<usize>) -> bool {
        let lines = self.0.get();
        let mut joined = lines.join("\n");
        let a = range.start.min(joined.len());
        let b = range.end.min(joined.len());
        if a < b {
            joined.replace_range(a..b, "");
        }
        let vec: Vec<String> = joined.split('\n').map(String::from).collect();
        self.0.set(Arc::new(vec));
        true
    }
}

impl LinesBuf {
    pub fn byte_to_utf16(&self, byte: usize) -> usize {
        let lines = self.0.get();
        let text = lines.join("\n");
        // A stale host-side offset may land mid multi-byte char: `get(..mid)`
        // returns None, and the old implementation zeroed it outright (the IME
        // highlight box jumped to the line start). Floor to the nearest boundary.
        let byte = floor_char_boundary(&text, byte.min(text.len()));
        text[..byte].chars().map(|c| c.len_utf16()).sum()
    }
    pub fn utf16_to_byte(&self, u: usize) -> usize {
        let lines = self.0.get();
        let text = lines.join("\n");
        let mut utf16 = 0;
        for (b, c) in text.char_indices() {
            if utf16 >= u {
                return b;
            }
            utf16 += c.len_utf16();
        }
        text.len()
    }
}

// ---------- component-local text undo stack ----------

/// Snapshot-style undo state for `VimEdit`: two stacks of
/// `(text snapshot, (line, col) cursor)`.
///
/// Extracted as a plain struct because `VimEdit::new` needs a gpui `App`
/// (focus handle) and cannot be built headless — semantic tests hit
/// `TextUndo` directly. The rule matches vim: a new snapshot invalidates
/// the redo branch.
#[derive(Default)]
struct TextUndo {
    undo: Vec<(Vec<String>, (usize, usize))>,
    redo: Vec<(Vec<String>, (usize, usize))>,
}

impl TextUndo {
    /// Record the pre-change state as a restore point, dropping the redo
    /// branch — a new edit after an undo must invalidate redo, otherwise one
    /// redo would overwrite the fresh edits with the old text.
    fn record(&mut self, before: Vec<String>, cursor: (usize, usize)) {
        self.undo.push((before, cursor));
        self.redo.clear();
    }

    /// Undo: push the current state onto the redo stack, return the state to restore.
    fn undo(
        &mut self,
        current: Vec<String>,
        cursor: (usize, usize),
    ) -> Option<(Vec<String>, (usize, usize))> {
        let entry = self.undo.pop()?;
        self.redo.push((current, cursor));
        Some(entry)
    }

    /// Redo: the mirror of `undo`.
    fn redo(
        &mut self,
        current: Vec<String>,
        cursor: (usize, usize),
    ) -> Option<(Vec<String>, (usize, usize))> {
        let entry = self.redo.pop()?;
        self.undo.push((current, cursor));
        Some(entry)
    }

    fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }
}

// ---------- host side-effect container ----------

pub struct EditHost {
    pub storage: SharedLines,
    pub viewport: (usize, usize),
    pub scroll_request: Option<(usize, ScrollAnchor)>,
    pub highlights: Vec<Range<usize>>,
    pub current_highlight: Option<Range<usize>>,
    pub pending_clipboard: Option<String>,
    pub clipboard: Option<String>,
    pub group_snapshot: Option<(Arc<Vec<String>>, usize)>,
    pub open_group: Option<u64>,
    pub pending_status: Option<String>,
    pub save_requested: bool,
    pub close_requested: bool,
}

impl EditHost {
    pub fn new(storage: SharedLines) -> Self {
        Self {
            storage,
            viewport: (0, 0),
            scroll_request: None,
            highlights: Vec::new(),
            current_highlight: None,
            pending_clipboard: None,
            clipboard: None,
            group_snapshot: None,
            open_group: None,
            pending_status: None,
            save_requested: false,
            close_requested: false,
        }
    }

    fn take_group_snapshot(&mut self) -> Option<(Arc<Vec<String>>, usize)> {
        self.open_group = None;
        self.group_snapshot.take()
    }

    /// Index (1-based) and total of the current search match, for the status
    /// bar's vim-style "3/17". See [`search_status`].
    pub fn search_status(&self) -> Option<(usize, usize)> {
        search_status(&self.highlights, self.current_highlight.as_ref())
    }
}

impl VimHost for EditHost {
    fn viewport(&self) -> (usize, usize) {
        self.viewport
    }
    fn scroll_to_line(&mut self, line: usize) {
        self.scroll_request = Some((line, ScrollAnchor::Cursor));
    }
    fn scroll_to_line_anchored(&mut self, line: usize, anchor: ScrollAnchor) {
        self.scroll_request = Some((line, anchor));
    }
    fn clipboard_write(&mut self, text: &str) {
        self.pending_clipboard = Some(text.to_string());
    }
    fn clipboard_read(&self) -> Option<String> {
        self.clipboard.clone()
    }
    fn set_search_highlights(&mut self, matches: &[Range<usize>], current: Option<Range<usize>>) {
        self.highlights = matches.to_vec();
        self.current_highlight = current;
    }
    fn begin_undo_group(&mut self, id: u64, cursor_offset: usize) {
        if self.open_group != Some(id) {
            self.group_snapshot = Some((self.storage.get(), cursor_offset));
            self.open_group = Some(id);
        }
    }
    fn undo(&mut self) -> Option<usize> {
        None // handled by the view-local undo stack
    }
    fn redo(&mut self) -> Option<usize> {
        None
    }
    fn save(&mut self) {
        self.save_requested = true;
    }
    fn request_close(&mut self) {
        self.close_requested = true;
    }
    fn status_message(&mut self, message: &str) {
        self.pending_status = Some(message.to_string());
    }
    fn buffer_name(&self) -> &str {
        "edit"
    }
    fn dispatch_host_action_hinted(&mut self, _id: &str, _strict: bool) {}
}

/// Index (1-based) and total of the current search match, for the status
/// bar's vim-style "3/17". Returns None when there is no current match (no
/// search yet / incsearch without a match) or the current match is not in
/// the published list — the UI just hides the counter. Free function because
/// each host's VimHost impl keeps its own highlight list (demo `HostState`,
/// pandamail) and can all delegate here.
pub fn search_status(
    highlights: &[Range<usize>],
    current: Option<&Range<usize>>,
) -> Option<(usize, usize)> {
    let current = current?;
    let index = highlights.iter().position(|r| r == current)? + 1;
    Some((index, highlights.len()))
}

// ---------- VimEdit ----------

/// Editor events observable by the host (pushed to pending_events on flush,
/// drained by the host).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VimEditEvent {
    Changed,
    /// `:w` / `ZZ`
    Save,
    /// `:q`
    Close,
    Status(String),
}

pub struct VimEdit {
    pub focus: FocusHandle,
    pub single_line: bool,
    pub password: bool,
    pub lines: Arc<Vec<String>>,
    pub buf: LinesBuf,
    pub host: EditHost,
    pub vim: VimState,
    /// Visual parameters (hosts inject their theme after construction; see [`VimEditStyle`]).
    pub style: VimEditStyle,
    text_undo: TextUndo,
    pub scroll: ScrollHandle,
    pub marked_range: Option<Range<usize>>,
    /// Visible row count in multi-line mode (estimate, used for viewport reporting)
    pub visible_rows: usize,
    pub modified: bool,
    pub label: &'static str,
    /// Host events produced by flush (the host drains them after process_key)
    pub pending_events: Vec<VimEditEvent>,
    /// Edit-box geometry (captured by the IME canvas, used for viewport math)
    pub area: std::rc::Rc<Cell<gpui::Bounds<gpui::Pixels>>>,
    /// Mouse drag-selecting (set on mousedown, cleared on mouseup)
    pub mouse_selecting: bool,
    /// Placeholder text rendered when the buffer is empty (not buffer
    /// content, display-only; see [`VimEdit::placeholder_text`]).
    pub placeholder: Option<String>,
}

/// Placeholder rendering rule: show only when the whole buffer is empty (a
/// single empty line). Extracted as a free function because `VimEdit`
/// construction needs a gpui `App`; semantic tests hit this directly.
fn placeholder_text<'a>(lines: &[String], placeholder: &'a Option<String>) -> Option<&'a str> {
    let empty = lines.len() == 1 && lines[0].is_empty();
    match (empty, placeholder.as_deref()) {
        (true, Some(p)) if !p.is_empty() => Some(p),
        _ => None,
    }
}

/// Reset the engine's transient state (mode/cursor/visual anchor/pending
/// command/search state/macros…) for "load new content", keeping the
/// configuration surface `options` and `keymaps`.
///
/// Extracted as a free function for the same reason: host tests of
/// `VimEngine` cannot construct a gpui `App`, so semantic tests hit this.
/// The engine has no lightweight reset API and `VimState::new()` zeroes the
/// options/keymaps loaded through rc layering too — in a `set_text` scenario
/// that is configuration loss, not a reset.
fn reset_engine_for_new_text(vim: &mut VimState) {
    let keymaps = std::mem::take(&mut vim.keymaps);
    let options = vim.options.clone();
    *vim = VimState::new();
    vim.keymaps = keymaps;
    vim.options = options;
}

impl Focusable for VimEdit {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl VimEdit {
    pub fn new(single_line: bool, password: bool, label: &'static str, cx: &mut App) -> Self {
        let storage = SharedLines::default();
        storage.set(Arc::new(vec![String::new()]));
        let mut editor_self = Self {
            focus: cx.focus_handle(),
            single_line,
            password,
            lines: Arc::new(vec![String::new()]),
            buf: LinesBuf(storage.clone()),
            host: EditHost::new(storage),
            vim: VimState::new(),
            style: VimEditStyle::default(),
            text_undo: TextUndo::default(),
            scroll: ScrollHandle::new(),
            marked_range: None,
            visible_rows: if single_line { 1 } else { 12 },
            modified: false,
            label,
            pending_events: Vec::new(),
            area: std::rc::Rc::new(Cell::new(gpui::Bounds::default())),
            mouse_selecting: false,
            placeholder: None,
        };
        // vimrc layering: only the cross-app user layer ~/.vimcorerc is
        // loaded here (lenient actions; its :action mappings may target other
        // apps). The host-specific layer is appended by the host after
        // construction:
        // `config::load_layers(&mut edit, &Layers { user: None, host: Some(...) })`
        // (loaded later, same-key overrides, strict actions).
        crate::config::load_layers(
            &mut editor_self,
            &crate::config::Layers::with_default_user(),
        );
        editor_self
    }

    pub fn text(&self) -> String {
        if self.single_line {
            self.lines.join(" ")
        } else {
            self.lines.join("\n")
        }
    }

    /// Replace all content (cursor back to 0, normal mode, undo cleared).
    ///
    /// The configuration surface (options/keymaps) survives: this method
    /// replaces content, not configuration — the old implementation rebuilt
    /// `VimState` wholesale, silently wiping the rc-layered user mappings and
    /// options on every host `set_text`. See [`reset_engine_for_new_text`].
    pub fn set_text(&mut self, text: &str) {
        let mut lines: Vec<String> = if self.single_line {
            vec![text.replace(['\n', '\r'], " ")]
        } else {
            text.split('\n').map(String::from).collect()
        };
        if lines.is_empty() {
            lines.push(String::new());
        }
        self.lines = Arc::new(lines);
        self.buf.0.set(self.lines.clone());
        reset_engine_for_new_text(&mut self.vim);
        self.modified = false;
        self.text_undo.clear();
    }

    pub fn drain_events(&mut self) -> Vec<VimEditEvent> {
        std::mem::take(&mut self.pending_events)
    }

    pub fn is_inserting(&self) -> bool {
        matches!(self.vim.mode, Mode::Insert | Mode::Replace)
    }

    pub fn mode_label(&self) -> &'static str {
        match self.vim.mode {
            Mode::Insert => "INSERT",
            Mode::Replace => "REPLACE",
            Mode::Visual {
                kind: VisualKind::Line,
            } => "V-LINE",
            Mode::Visual { .. } => "VISUAL",
            _ => "",
        }
    }

    pub fn showcmd(&self) -> String {
        self.vim.showcmd()
    }

    /// Key handling for the interceptor path. Returning true = consumed.
    pub fn process_key(&mut self, ks: &gpui::Keystroke, cx: &mut Context<Self>) -> bool {
        if ks.modifiers.platform || ks.modifiers.function {
            return false;
        }
        let key = crate::to_core_key(ks);
        let mode = self.vim.mode;

        // Undo/redo (normal mode + engine idle, local stack). The is_idle
        // gate is essential: the `u` after `gu` and the trailing key of `guu`
        // are part of a pending engine command — intercepting them early
        // swallows them into a local undo and breaks gu/guw/guu/gugu entirely.
        if mode == Mode::Normal && self.vim.is_idle() {
            if let KeyKind::Char('u') = &key.kind {
                if !key.modifiers.control {
                    self.undo();
                    cx.notify();
                    return true;
                }
            }
            if let KeyKind::Char('r') = &key.kind {
                if key.modifiers.control {
                    self.redo();
                    cx.notify();
                    return true;
                }
            }
        }

        let before = self.lines.clone();
        self.host.viewport = (0, self.visible_rows.saturating_sub(1));
        self.buf.0.set(before.clone());
        let result = {
            let mut ctx = Ctx {
                buf: &mut self.buf,
                host: &mut self.host,
            };
            self.vim.handle_key(&mut ctx, key.clone())
        };
        // Printable chars the engine leaves to the host (insert/replace modes)
        // go straight into the buffer
        if result == KeyResult::Unknown && matches!(self.vim.mode, Mode::Insert | Mode::Replace) {
            if let Some(c) = key.printable_char() {
                let mut ctx = Ctx {
                    buf: &mut self.buf,
                    host: &mut self.host,
                };
                self.vim.insert_text_at_cursor(&mut ctx, &c.to_string());
            }
        }
        self.flush(before, cx);
        true
    }

    fn flush(&mut self, before: Arc<Vec<String>>, cx: &mut Context<Self>) {
        let after = self.buf.0.get();
        if !Arc::ptr_eq(&after, &before) {
            self.lines = after.clone();
            self.buf.0.set(after);
            self.modified = true;
            self.pending_events.push(VimEditEvent::Changed);
        }
        if !matches!(self.vim.mode, Mode::Insert | Mode::Replace) {
            if let Some((snap_lines, snap_cursor)) = self.host.take_group_snapshot() {
                let cursor_before = offset_rc(&snap_lines, snap_cursor);
                self.push_undo((*snap_lines).clone(), cursor_before);
            }
        }
        if let Some(text) = self.host.pending_clipboard.take() {
            self.host.clipboard = Some(text.clone());
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        }
        if let Some(msg) = self.host.pending_status.take() {
            self.pending_events.push(VimEditEvent::Status(msg));
        }
        if self.host.save_requested {
            self.host.save_requested = false;
            self.pending_events.push(VimEditEvent::Save);
        }
        if self.host.close_requested {
            self.host.close_requested = false;
            self.pending_events.push(VimEditEvent::Close);
        }
        if let Some((line, anchor)) = self.host.scroll_request.take() {
            self.scroll_line_anchored(line, anchor);
        }
        self.clamp_engine();
        if self.single_line {
            self.collapse_single_line();
        }
        cx.notify();
    }

    fn collapse_single_line(&mut self) {
        if self.lines.len() > 1 {
            let joined: Arc<Vec<String>> = Arc::new(vec![self.lines.join(" ")]);
            self.lines = joined.clone();
            self.buf.0.set(joined);
            self.vim.cursor.offset = self.vim.cursor.offset.min(self.buf.len());
        }
    }

    fn push_undo(&mut self, before: Vec<String>, cursor_before: (usize, usize)) {
        if before == *self.lines {
            return;
        }
        self.text_undo.record(before, cursor_before);
    }

    fn undo(&mut self) {
        let Some((before, cursor_before)) =
            self.text_undo.undo(self.lines.as_ref().clone(), self.cursor_rc())
        else {
            return;
        };
        self.lines = Arc::new(before);
        self.buf.0.set(self.lines.clone());
        self.vim.mode = Mode::Normal;
        self.set_cursor_rc(cursor_before);
    }

    fn redo(&mut self) {
        let Some((after, cursor_after)) =
            self.text_undo.redo(self.lines.as_ref().clone(), self.cursor_rc())
        else {
            return;
        };
        self.lines = Arc::new(after);
        self.buf.0.set(self.lines.clone());
        self.vim.mode = Mode::Normal;
        self.set_cursor_rc(cursor_after);
    }

    fn cursor_rc(&self) -> (usize, usize) {
        offset_rc(&self.lines, self.vim.cursor.offset)
    }

    fn set_cursor_rc(&mut self, (row, col): (usize, usize)) {
        self.vim.cursor.offset = rc_offset(&self.lines, row, col);
        self.vim.cursor.desired_col = Some(col);
    }

    fn clamp_engine(&mut self) {
        let max = self.buf.len();
        self.vim.cursor.offset = clamp_to_line_end(&self.buf, self.vim.cursor.offset.min(max));
    }

    pub fn scroll_line_anchored(&self, line: usize, to: ScrollAnchor) {
        let line_h = self.style.line_height;
        let viewport_h = f32::from(self.scroll.bounds().size.height);
        if viewport_h <= 0.0 {
            return;
        }
        let content = self.lines.len() as f32 * line_h;
        let max = (content - viewport_h).max(0.0);
        let line_top = line as f32 * line_h;
        let target = match to {
            ScrollAnchor::Center | ScrollAnchor::Cursor => {
                line_top + line_h / 2.0 - viewport_h / 2.0
            }
            ScrollAnchor::Top => line_top,
            ScrollAnchor::Bottom => line_top + line_h - viewport_h,
        }
        .clamp(0.0, max);
        self.scroll.set_offset(Point::new(px(0.0), px(-target)));
    }

    /// Replace the flat byte range with `s` and move the cursor after the
    /// inserted text (used for completion insertion). Enters insert mode;
    /// bypasses the engine command pipeline.
    pub fn splice_range(&mut self, range: Range<usize>, s: &str) {
        let before = self.lines.as_ref().clone();
        let cursor_before = self.cursor_rc();
        let mut text = self.lines.join("\n");
        let a = range.start.min(text.len());
        let b = range.end.min(text.len());
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let a = floor_char_boundary(&text, a);
        let b = ceil_char_boundary(&text, b);
        text.replace_range(a..b, s);
        let end = a + s.len();
        let lines: Vec<String> = text.split('\n').map(String::from).collect();
        self.lines = Arc::new(lines);
        self.buf.0.set(self.lines.clone());
        self.vim.mode = Mode::Insert;
        self.vim.cursor.offset = end.min(self.buf.len());
        self.vim.cursor.desired_col = None;
        self.modified = true;
        // Host-bypass edits rank with typing: record the undo point and
        // invalidate the redo branch, otherwise one redo after a completion
        // insert would jump back to the stale pre-splice snapshot
        self.push_undo(before, cursor_before);
        if self.single_line {
            self.collapse_single_line();
        }
    }

    /// Visual selection span for a line (grapheme column range).
    fn selection_span(&self, line: usize) -> Option<(usize, usize)> {
        let (anchor, cursor, kind) = self.vim.visual_selection()?;
        let lines = &self.lines;
        let a = offset_rc(lines, anchor);
        let c = offset_rc(lines, cursor);
        match kind {
            VisualKind::Line => {
                let (lo, hi) = if a.0 <= c.0 { (a.0, c.0) } else { (c.0, a.0) };
                if line >= lo && line <= hi {
                    Some((0, lines.get(line).map(|l| l.chars().count()).unwrap_or(0)))
                } else {
                    None
                }
            }
            VisualKind::Char => {
                let (lo, hi) = if a <= c { (a, c) } else { (c, a) };
                if line < lo.0 || line > hi.0 {
                    return None;
                }
                let len = lines.get(line).map(|l| l.chars().count()).unwrap_or(0);
                let c0 = if line == lo.0 { lo.1 } else { 0 };
                let c1 = if line == hi.0 { hi.1 + 1 } else { len + 1 };
                Some((c0, c1.min(len.max(c0))))
            }
            VisualKind::Block => {
                if line < a.0.min(c.0) || line > a.0.max(c.0) {
                    return None;
                }
                let (c0, c1) = if a.1 <= c.1 { (a.1, c.1) } else { (c.1, a.1) };
                Some((c0, c1 + 1))
            }
        }
    }

    /// Render the edit box (composed by the parent view). `focused` decides
    /// the border and cursor visibility.
    pub fn render_view(&self, focused: bool) -> gpui::Stateful<gpui::Div> {
        let line_h = px(self.style.line_height);
        let line_height_f = self.style.line_height;
        let char_width_f = self.style.char_width;
        let mono = SharedString::from(self.style.mono_font.clone());
        let font_size = px(self.style.font_size);

        // Visible-line window (area captured by the IME canvas; draw
        // everything on the first frame when it is unavailable)
        let area = self.area.get();
        let viewport_h = f32::from(area.size.height);
        let scroll_y = -f32::from(self.scroll.offset().y);
        let total = self.lines.len();
        let first = if self.single_line {
            0
        } else if viewport_h > 0.0 {
            ((scroll_y / line_height_f).floor().max(0.0)) as usize
        } else {
            0
        };
        let visible = if self.single_line {
            1
        } else if viewport_h > 0.0 {
            ((viewport_h / line_height_f).ceil() as usize + 1).min(total.saturating_sub(first))
        } else {
            total
        };
        let last = (first + visible).min(total);

        let (crow, ccol) = self.cursor_rc();

        // Search highlights grouped by line
        let search_by_row: Vec<(usize, usize, usize)> = self
            .host
            .highlights
            .iter()
            .filter_map(|r| {
                let (row, col) = offset_rc(&self.lines, r.start);
                let (row2, col2) = offset_rc(&self.lines, r.end);
                let len = self.lines.get(row).map(|l| l.chars().count()).unwrap_or(0);
                let end = if row2 == row { col2 } else { len };
                (row, col, end.max(col + 1)).into()
            })
            .collect();

        let mut container = div()
            .id(SharedString::from(format!("vim-edit-{}", self.label)))
            .relative()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col();

        container = if self.single_line {
            container.h(px(line_height_f + 6.0)).overflow_hidden()
        } else {
            container
                .h_full()
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
        };

        container = container
            .px_1p5()
            .rounded_sm()
            .text_size(font_size)
            .font_family(mono.clone())
            .line_height(line_h)
            .when(focused, |d| {
                d.border_1()
                    .border_color(rgb(self.style.focus_border))
                    .bg(rgb(blend_rgb(
                        self.style.surface_bg,
                        self.style.accent,
                        0.06,
                    )))
            })
            .when(!focused, |d| {
                d.border_1()
                    .border_color(rgb(self.style.border))
                    .bg(rgb(self.style.surface_bg))
            })
            .children(self.lines[first..last].iter().enumerate().map(|(i, line)| {
                let ix = first + i;
                let display_line: SharedString = if self.password {
                    line.chars().map(|_| '•').collect::<String>().into()
                } else {
                    line.clone().into()
                };
                let x_of =
                    |col: usize| -> f32 { display_width_before(line, col) as f32 * char_width_f };
                // Placeholder: when the whole buffer is empty, draw gray text
                // on the first line (not buffer content, display-only)
                let ph = if ix == 0 { placeholder_text(&self.lines, &self.placeholder) } else { None };
                let mut row = div()
                    .h(line_h)
                    .w_full()
                    .relative()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .when(!display_line.is_empty(), |d| d.child(display_line.clone()))
                    .when(display_line.is_empty() && ph.is_some(), |d| {
                        d.child(
                            div()
                                .text_color(rgb(self.style.placeholder))
                                .child(SharedString::from(ph.unwrap_or_default().to_owned())),
                        )
                    });
                // Search highlights
                for (hit_row, c0, c1) in &search_by_row {
                    if *hit_row == ix {
                        let is_cur = self.host.current_highlight.as_ref().is_some_and(|cur| {
                            let (r0, cc) = offset_rc(&self.lines, cur.start);
                            r0 == ix && cc == *c0
                        });
                        let color = if is_cur {
                            self.style.gold
                        } else {
                            self.style.accent
                        };
                        row = row.child(
                            div()
                                .absolute()
                                .top(px(1.0))
                                .bottom(px(1.0))
                                .left(px(x_of(*c0)))
                                .w(px((x_of(*c1) - x_of(*c0)).max(4.0)))
                                .bg(rgba((color << 8) | 0x3a)),
                        );
                    }
                }
                // Visual selection
                if focused {
                    if let Some((c0, c1)) = self.selection_span(ix) {
                        row = row.child(
                            div()
                                .absolute()
                                .top(px(0.0))
                                .bottom(px(0.0))
                                .left(px(x_of(c0)))
                                .w(px((x_of(c1) - x_of(c0)).max(4.0)))
                                .bg(rgba((self.style.accent << 8) | 0x40)),
                        );
                    }
                }
                // Cursor (block/bar)
                if focused && ix == crow {
                    let block = self.vim.cursor_is_block();
                    let under: Option<char> = line.chars().nth(ccol);
                    let w = under
                        .map(|c| c.width().unwrap_or(1) as f32 * char_width_f)
                        .unwrap_or(char_width_f);
                    let mut cur = div()
                        .absolute()
                        .top(px(1.0))
                        .bottom(px(1.0))
                        .left(px(x_of(ccol)))
                        .w(px(if block { w } else { 2.0 }))
                        .bg(rgb(self.style.cursor));
                    if block {
                        if let Some(ch) = under {
                            cur = cur
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(font_size)
                                .font_family(mono.clone())
                                .text_color(rgb(self.style.surface_bg))
                                .child(SharedString::from(ch.to_string()));
                        }
                    }
                    row = row.child(cur);
                }
                row
            }));

        container
    }

    /// IME attachment canvas (the parent view layers it over the edit box;
    /// also captures geometry for viewport math).
    pub fn render_ime_canvas(&self, entity: &gpui::Entity<Self>) -> impl gpui::IntoElement {
        let entity = entity.clone();
        let focus = self.focus.clone();
        let area_cell = std::rc::Rc::clone(&self.area);
        canvas(
            move |bounds, _window, _cx| {
                area_cell.set(bounds);
                bounds
            },
            move |bounds, _, window, cx| {
                window.handle_input(&focus, ElementInputHandler::new(bounds, entity.clone()), cx);
            },
        )
        .absolute()
        .inset_0()
    }

    // ---- mouse click-to-position / drag-select ----

    /// Press: click positions the cursor; normal mode starts visual
    /// drag-select, insert mode only moves the cursor.
    pub fn mouse_down(&mut self, pos: gpui::Point<gpui::Pixels>) {
        // While the search/command prompt is open, a click through
        // enter_visual_char would feed 'v' into the pattern as prompt input;
        // a pending operator (e.g. `d` already typed) must not be interrupted
        // by the click repositioning the cursor — both cases ignore this
        // press outright.
        if matches!(self.vim.mode, Mode::CommandLine { .. }) || !self.vim.is_idle() {
            return;
        }
        let Some(off) = self.offset_at_point(pos) else {
            return;
        };
        if self.is_inserting() {
            self.place_cursor(off);
            return;
        }
        self.place_cursor(off);
        if self.vim.mode == Mode::Normal {
            // Clicking again while already in visual mode: just move the
            // cursor to extend the existing selection (the anchor stays),
            // without feeding 'v' (inside visual, 'v' toggles/exits visual in
            // the engine)
            self.enter_visual_char();
        }
        self.mouse_selecting = true;
    }

    /// Drag: the cursor follows (the selection extends from the engine's visual anchor).
    pub fn mouse_drag(&mut self, pos: gpui::Point<gpui::Pixels>) {
        if !self.mouse_selecting {
            return;
        }
        if let Some(off) = self.offset_at_point(pos) {
            self.place_cursor(off);
        }
    }

    /// Release: ends the drag-select; an in-place click (empty selection) returns to normal.
    pub fn mouse_up(&mut self) {
        if !self.mouse_selecting {
            return;
        }
        self.mouse_selecting = false;
        let empty = self.vim.visual_selection().is_none_or(|(a, c, _)| a == c);
        if empty {
            self.vim.mode = Mode::Normal;
        }
    }

    fn place_cursor(&mut self, off: usize) {
        let rc = offset_rc(&self.lines, off.min(self.buf.len()));
        self.set_cursor_rc(rc);
    }

    /// Enter charwise visual through the engine key stream (same technique as
    /// the host pager's enter_visual).
    fn enter_visual_char(&mut self) {
        self.host.viewport = (0, self.visible_rows.saturating_sub(1));
        let mut ctx = Ctx {
            buf: &mut self.buf,
            host: &mut self.host,
        };
        self.vim.handle_key(&mut ctx, vimcore::key::Key::char('v'));
    }

    /// Window coordinates → flat offset. Row = viewport row + scroll
    /// compensation; column = monospace advance estimate.
    fn offset_at_point(&self, pos: gpui::Point<gpui::Pixels>) -> Option<usize> {
        let area = self.area.get();
        if area.size.width <= gpui::px(0.0) || area.size.height <= gpui::px(0.0) {
            return None;
        }
        let line_height_f = self.style.line_height;
        let scroll_y = -f32::from(self.scroll.offset().y);
        let first = (scroll_y / line_height_f).floor().max(0.0) as usize;
        let dy = f32::from(pos.y - area.origin.y).max(0.0);
        let vis_row = (dy / line_height_f).floor() as usize;
        let row = (first + vis_row).min(self.lines.len().saturating_sub(1));
        let dx = (f32::from(pos.x - area.origin.x) - 6.0).max(0.0);
        let col = (dx / self.style.char_width).round() as usize;
        let line = self.lines.get(row)?;
        let col = col.min(line.chars().count());
        Some(rc_offset(&self.lines, row, col))
    }
}

fn display_width_before(line: &str, col: usize) -> usize {
    line.chars().take(col).map(|c| c.width().unwrap_or(0)).sum()
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    let max = s.len();
    while i < max && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// For tests/host construction.
impl VimEdit {
    pub fn cursor_offset(&self) -> usize {
        self.vim.cursor.offset
    }
    pub fn mode(&self) -> Mode {
        self.vim.mode
    }
    pub fn set_password(&mut self, on: bool) {
        self.password = on;
    }

    /// Set the placeholder text rendered when the buffer is empty (`None`
    /// disables it). Display-only — it never enters the buffer and takes no
    /// part in cursor/search/selection math.
    pub fn set_placeholder(&mut self, text: Option<String>) {
        self.placeholder = text;
    }

    /// The placeholder text to render now: only set when the whole buffer is empty.
    pub fn placeholder_text(&self) -> Option<&str> {
        placeholder_text(&self.lines, &self.placeholder)
    }
}

// ---- IME / text-input protocol ----
//
// The engine owns the cursor; every mutation is forwarded to VimState here.
// The UTF-16 (gpui) / UTF-8 (engine) boundary converts only at this layer.

impl EntityInputHandler for VimEdit {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let start = self.buf.utf16_to_byte(range_utf16.start);
        let end = self.buf.utf16_to_byte(range_utf16.end);
        Some(self.buf.slice(start..end))
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<gpui::UTF16Selection> {
        let cursor = self.buf.byte_to_utf16(self.vim.cursor.offset);
        let selection = self
            .vim
            .visual_selection()
            .map(|(anchor, cur, _)| {
                let a = self.buf.byte_to_utf16(anchor.min(cur));
                let b = self.buf.byte_to_utf16(anchor.max(cur));
                a..b
            })
            .unwrap_or(cursor..cursor);
        Some(gpui::UTF16Selection {
            range: selection,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let range = self.marked_range.as_ref()?;
        // An empty marked range = the IME ended (setMarkedText with an empty
        // string); report None so gpui's is_composing never wedges true and
        // eats Esc
        if range.is_empty() {
            return None;
        }
        Some(self.buf.byte_to_utf16(range.start)..self.buf.byte_to_utf16(range.end))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        if let Some(range) = self.marked_range.take() {
            if range.start < range.end {
                let mut ctx = Ctx {
                    buf: &mut self.buf,
                    host: &mut self.host,
                };
                self.vim.replace_range(&mut ctx, range, "");
            }
        }
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let explicit_range = range.and_then(|r| {
            let start = self.buf.utf16_to_byte(r.start);
            let end = self.buf.utf16_to_byte(r.end);
            (start < end).then_some(start..end)
        });

        // Committing composed text must replace the marked range and bypass
        // the key pipeline
        let committing = explicit_range.is_none()
            && !text.is_empty()
            && self.marked_range.as_ref().is_some_and(|r| !r.is_empty());
        if committing {
            let marked = self.marked_range.take().unwrap();
            self.vim.record_typed_text(text);
            let mut ctx = Ctx {
                buf: &mut self.buf,
                host: &mut self.host,
            };
            self.vim.replace_range(&mut ctx, marked, text);
        } else if let Some(range) = explicit_range {
            if matches!(self.vim.mode(), Mode::Insert | Mode::Replace) {
                let mut ctx = Ctx {
                    buf: &mut self.buf,
                    host: &mut self.host,
                };
                self.vim.replace_range(&mut ctx, range, text);
            }
        } else {
            crate::dispatch_text(self, text);
        }
        self.clamp_engine();
        if self.single_line {
            self.collapse_single_line();
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range_utf16: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        // Composition preview text lands directly in the buffer and is marked
        // (not through the command pipeline, no `.` recording)
        if !matches!(self.vim.mode(), Mode::Insert | Mode::Replace) {
            return;
        }
        self.vim.set_recording_suppressed(true);
        if let Some(previous) = self.marked_range.take() {
            let mut ctx = Ctx {
                buf: &mut self.buf,
                host: &mut self.host,
            };
            self.vim.replace_range(&mut ctx, previous.clone(), new_text);
            let start = previous.start;
            let end = start + new_text.len();
            self.marked_range = (start < end).then_some(start..end);
        } else {
            let mut ctx = Ctx {
                buf: &mut self.buf,
                host: &mut self.host,
            };
            let start = self.vim.cursor_offset();
            self.vim.insert_text_at_cursor(&mut ctx, new_text);
            self.marked_range = Some(start..start + new_text.len());
        }
        self.vim.set_recording_suppressed(false);
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: gpui::Bounds<gpui::Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<gpui::Bounds<gpui::Pixels>> {
        let byte = self.buf.utf16_to_byte(range_utf16.start);
        let (row, col) = offset_rc(&self.lines, byte);
        let line = self.lines.get(row)?;
        let x = display_width_before(line, col) as f32 * self.style.char_width;
        // The IME canvas overlays the viewport (same assumption as
        // offset_at_point): content coordinates minus the scrolled distance
        // give the screen position, otherwise after multi-line scrolling the
        // candidate box flies out of the visible area
        let scroll_y = -f32::from(self.scroll.offset().y);
        let origin = Point::new(
            element_bounds.origin.x + gpui::px(x + 6.0),
            element_bounds.origin.y
                + gpui::px(row as f32 * self.style.line_height - scroll_y),
        );
        Some(gpui::Bounds::new(
            origin,
            gpui::Size::new(
                gpui::px(self.style.char_width),
                gpui::px(self.style.line_height),
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<gpui::Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let area = self.area.get();
        // Same conversion as offset_at_point: add the scrolled distance back
        // to the viewport y to get the content row
        let scroll_y = -f32::from(self.scroll.offset().y);
        let dy = point.y - area.origin.y + gpui::px(scroll_y);
        let dx = point.x - area.origin.x - gpui::px(6.0);
        let row = (f32::from(dy) / self.style.line_height).max(0.0) as usize;
        let line = self.lines.get(row)?;
        let mut col = 0usize;
        let mut x = 0f32;
        for c in line.chars() {
            let w = c.width().unwrap_or(0) as f32 * self.style.char_width;
            if x + w / 2.0 > f32::from(dx) {
                break;
            }
            x += w;
            col += 1;
        }
        Some(self.buf.byte_to_utf16(rc_offset(&self.lines, row, col)))
    }
}

/// VimEditor trait impl (only to reuse [`crate::dispatch_text`]'s IME text
/// path; keys are not intercepted through attach, the host guard routes them).
impl crate::VimEditor for VimEdit {
    fn vim_parts(&mut self) -> (&mut VimState, &mut dyn VimBufferMut, &mut dyn VimHost) {
        (&mut self.vim, &mut self.buf, &mut self.host)
    }

    fn vim_accepts_keys(&self, window: &Window, _cx: &App) -> bool {
        self.focus.is_focused(window)
    }

    fn vim_did_process_key(
        &mut self,
        _result: KeyResult,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clamp_engine();
        if self.single_line {
            self.collapse_single_line();
        }
        cx.notify();
    }
}

#[cfg(test)]
mod multiline_tests {
    use super::*;
    use vimcore::key::Key;
    use vimcore::state::Ctx;

    // ---- set_text engine-reset semantics (configuration survives) ----

    /// Regression: the old implementation rebuilt `VimState` wholesale, so
    /// after a host `set_text` swapped content, rc-layered options and user
    /// mappings were silently wiped (jk→Esc became literal typing).
    #[test]
    fn engine_reset_for_new_text_keeps_options_and_mappings() {
        let (_, mut buf, mut host, mut vim) = setup();
        vim.keymaps_mut().map_str_noremap(
            vimcore::keymap::ModeClass::Insert,
            "jk",
            "<Esc>",
            true,
        );
        vim.options.number = true; // default false, so the assertion isn't vacuously green
        vim.mode = Mode::Insert;
        vim.cursor.offset = 3;

        reset_engine_for_new_text(&mut vim);
        assert_eq!(vim.mode, Mode::Normal, "transient mode resets with new content");
        assert_eq!(vim.cursor.offset, 0, "cursor resets with new content");
        assert!(vim.options.number, "user options must survive");

        // Mapping survives: in insert mode jk still replaces with Esc instead
        // of typing k into the buffer
        vim.mode = Mode::Insert;
        let mut ctx = Ctx {
            buf: &mut buf,
            host: &mut host,
        };
        vim.handle_key(&mut ctx, Key::char('j'));
        vim.handle_key(&mut ctx, Key::char('k'));
        assert_eq!(vim.mode, Mode::Normal, "the jk mapping must still work");
        assert_eq!(
            buf.0.get().as_ref(),
            &vec!["".to_string()],
            "the mapping's RHS must not land in the buffer"
        );
    }

    /// Assemble the engine plus a buffer with single-line initial content
    /// (the state a body editor opens with).
    fn setup() -> (SharedLines, LinesBuf, EditHost, VimState) {
        let storage = SharedLines::default();
        storage.set(Arc::new(vec![String::new()]));
        let buf = LinesBuf(storage.clone());
        let host = EditHost::new(storage.clone());
        (storage, buf, host, VimState::new())
    }

    // ---- TextUndo (headless semantic tests; VimEdit needs an App to construct) ----

    fn rc(row: usize, col: usize) -> (usize, usize) {
        (row, col)
    }

    #[test]
    fn text_undo_roundtrip() {
        let mut u = TextUndo::default();
        let a = vec!["a".to_string()];
        let b = vec!["ab".to_string()];
        u.record(a.clone(), rc(0, 1));
        assert_eq!(u.undo(b.clone(), rc(0, 2)), Some((a.clone(), rc(0, 1))));
        // redo returns to b, undo back to a, ping-ponging works
        assert_eq!(u.redo(a.clone(), rc(0, 1)), Some((b.clone(), rc(0, 2))));
        assert_eq!(u.undo(b.clone(), rc(0, 2)), Some((a, rc(0, 1))));
    }

    #[test]
    fn text_undo_record_invalidates_redo() {
        // Regression: undo → new edit → redo would overwrite the fresh edit with the old snapshot
        let mut u = TextUndo::default();
        let a = vec!["a".to_string()];
        let b = vec!["b".to_string()];
        let c = vec!["c".to_string()];
        u.record(a, rc(0, 0));
        assert!(u.undo(b.clone(), rc(0, 1)).is_some());
        u.record(c, rc(0, 1)); // a fresh edit after the undo
        assert!(u.redo(b, rc(0, 1)).is_none(), "the redo branch must be invalidated");
    }

    #[test]
    fn text_undo_empty_stacks_are_none() {
        let mut u = TextUndo::default();
        assert!(u.undo(vec![], rc(0, 0)).is_none());
        assert!(u.redo(vec![], rc(0, 0)).is_none());
        u.record(vec!["x".to_string()], rc(0, 0));
        u.clear();
        assert!(u.undo(vec![], rc(0, 0)).is_none());
    }

    // ---- offset_rc boundary robustness ----

    #[test]
    fn offset_rc_floors_mid_char_offsets_instead_of_panicking() {
        // Regression: "中文"[1] is not a char boundary; the old
        // implementation panicked directly on line[..1]
        let lines = vec!["中文".to_string(), "ab".to_string()];
        assert_eq!(offset_rc(&lines, 1), (0, 0), "mid-char floors to boundary");
        assert_eq!(offset_rc(&lines, 4), (0, 1), "mid-char within 文 floors to its start");
        assert_eq!(offset_rc(&lines, 0), (0, 0));
        assert_eq!(offset_rc(&lines, 6), (0, 2), "line end is a valid boundary");
        assert_eq!(offset_rc(&lines, 7), (1, 0), "start of next line");
        assert_eq!(offset_rc(&lines, 9), (1, 2), "end of last line");
        assert_eq!(offset_rc(&lines, 999), (1, 0), "past the end clamps");
    }

    // ---- offset semantics equivalent to the join version (incl. cross-line '\n' probes) ----

    fn buf(text: &str) -> LinesBuf {
        let storage = SharedLines::default();
        storage.set(Arc::new(text.split('\n').map(String::from).collect()));
        LinesBuf(storage)
    }

    #[test]
    fn char_and_prev_char_probes_match_flat_text_semantics() {
        // "ab\n中cd": bytes 0'a' 1'b' 2'\n' 3..5'中' 6'c' 7'd', len=8
        let b = buf("ab\n中cd");
        assert_eq!(b.char_at(0), Some('a'));
        assert_eq!(b.char_at(2), Some('\n'));
        assert_eq!(b.char_at(3), Some('中'));
        assert_eq!(b.char_at(4), None, "mid-char probe is None per contract");
        assert_eq!(b.char_at(7), Some('d'), "last content byte");
        assert_eq!(b.char_at(8), None, "at len is None");

        assert_eq!(b.prev_char_offset(3), Some(2), "line start: the \\n before it");
        assert_eq!(b.prev_char_offset(6), Some(3), "steps back over the wide char");
        assert_eq!(b.prev_char_offset(0), None);
        assert_eq!(b.prev_char_offset(2), Some(1));
        assert_eq!(b.prev_char_offset(8), Some(7), "buffer end steps to last char");
        // Empty-line separator: in "a\n\nb" the byte before a line start is
        // the empty line's own '\n'
        let b = buf("a\n\nb");
        assert_eq!(b.prev_char_offset(3), Some(2));
        assert_eq!(b.char_at(2), Some('\n'));
        // Last line empty (text ends with \n): the byte before the buffer end
        // is that '\n'
        let b = buf("a\n");
        assert_eq!(b.prev_char_offset(2), Some(1));
        assert_eq!(b.char_at(1), Some('\n'));
    }

    /// Regression: non-boundary byte offsets (stale host values) used to zero
    /// out in the UTF-16 conversion, jumping the IME placement box to the
    /// buffer start; they must floor to the containing char's start first.
    #[test]
    fn byte_to_utf16_floors_mid_char_offsets() {
        let b = buf("中文x"); // boundaries 0,3,6,7
        assert_eq!(b.byte_to_utf16(0), 0);
        assert_eq!(b.byte_to_utf16(3), 1, "one char before 文");
        assert_eq!(b.byte_to_utf16(4), 1, "mid-文 floors to its start (was 0)");
        assert_eq!(b.byte_to_utf16(6), 2);
        assert_eq!(b.byte_to_utf16(7), 3);
        assert_eq!(b.byte_to_utf16(99), 3, "past end clamps");
        assert_eq!(b.utf16_to_byte(2), 6, "roundtrip back to a boundary");
    }

    // ---- placeholder text rule ----

    /// The "n/total" counting semantics of EditHost::search_status.
    #[test]
    fn search_status_indexes_current_highlight() {
        let storage = SharedLines::default();
        let mut host = EditHost::new(storage);
        assert_eq!(host.search_status(), None, "no counter without a search");

        host.highlights = vec![0..3, 8..11, 20..23];
        host.current_highlight = Some(8..11);
        assert_eq!(host.search_status(), Some((2, 3)));

        host.current_highlight = None;
        assert_eq!(host.search_status(), None, "incsearch with no current match yet");

        // The current match is not in the published list (a stale value
        // between engine publishes): return None rather than a wrong index
        host.current_highlight = Some(99..102);
        assert_eq!(host.search_status(), None);
    }

    #[test]
    fn placeholder_shows_only_on_empty_buffer() {
        let ph = Some("收件人邮箱".to_string());
        assert_eq!(
            placeholder_text(&[String::new()], &ph),
            Some("收件人邮箱"),
            "empty buffer shows the placeholder"
        );
        assert_eq!(
            placeholder_text(&["x".to_string()], &ph),
            None,
            "any content hides it"
        );
        assert_eq!(
            placeholder_text(&[String::new(), String::new()], &ph),
            None,
            "multi-line buffers never show it"
        );
        assert_eq!(
            placeholder_text(&[String::new()], &None),
            None,
            "unset placeholder"
        );
        assert_eq!(
            placeholder_text(&[String::new()], &Some(String::new())),
            None,
            "an empty placeholder string is treated as unset"
        );
    }

    /// Regression: a multi-line editor starts with one content line, and
    /// insert-mode enter was flattened to a space (the old lines.len()==1
    /// content heuristic misjudged a multi-line editor as single-line).
    #[test]
    fn insert_enter_creates_line() {
        let (_, mut buf, mut host, mut vim) = setup();
        let mut ctx = Ctx {
            buf: &mut buf,
            host: &mut host,
        };
        vim.handle_key(&mut ctx, Key::char('i'));
        vim.insert_text_at_cursor(&mut ctx, "收件人");
        assert_eq!(vim.handle_key(&mut ctx, Key::enter()), KeyResult::Consumed);
        vim.insert_text_at_cursor(&mut ctx, "正文");
        let lines = buf.0.get();
        assert_eq!(*lines, vec!["收件人".to_string(), "正文".to_string()]);
    }

    /// Regression: normal-mode o could not open a line (same content
    /// heuristic flattening the '\n').
    #[test]
    fn normal_o_opens_line_below() {
        let (_, mut buf, mut host, mut vim) = setup();
        buf.0.set(Arc::new(vec!["hello".to_string()]));
        let mut ctx = Ctx {
            buf: &mut buf,
            host: &mut host,
        };
        assert_eq!(
            vim.handle_key(&mut ctx, Key::char('o')),
            KeyResult::Consumed
        );
        let lines = buf.0.get();
        assert_eq!(*lines, vec!["hello".to_string(), String::new()]);
        assert!(matches!(vim.mode, Mode::Insert), "o should enter insert mode");
    }

    /// Regression: with a multi-byte (CJK) char at the line end, enter used
    /// to panic in the char_at→offset_rc non-boundary slice (the char_at
    /// contract requires None at non-boundaries).
    #[test]
    fn cjk_line_tail_enter_no_panic() {
        let (_, mut buf, mut host, mut vim) = setup();
        buf.0.set(Arc::new(vec!["第一行收件人".to_string()]));
        let mut ctx = Ctx {
            buf: &mut buf,
            host: &mut host,
        };
        vim.handle_key(&mut ctx, Key::char('A')); // enter insert at the line end
        assert_eq!(vim.handle_key(&mut ctx, Key::enter()), KeyResult::Consumed);
        assert_eq!(buf.0.get().len(), 2, "enter must create a new line");
        assert_eq!(buf.0.get()[0], "第一行收件人");
    }

    /// Multi-line structural edit: after dd deletes a line the remaining
    /// lines stay separate.
    #[test]
    fn dd_keeps_line_structure() {
        let (_, mut buf, mut host, mut vim) = setup();
        buf.0.set(Arc::new(vec![
            "one".to_string(),
            "two".to_string(),
            "three".to_string(),
        ]));
        let mut ctx = Ctx {
            buf: &mut buf,
            host: &mut host,
        };
        vim.handle_key(&mut ctx, Key::char('d'));
        vim.handle_key(&mut ctx, Key::char('d'));
        let lines = buf.0.get();
        assert_eq!(*lines, vec!["two".to_string(), "three".to_string()]);
    }

    /// Single-line field (form/header field) semantics unchanged: the
    /// newline enter produces exists briefly in the buffer, merged back into
    /// one line by collapse_single_line on flush.
    #[test]
    fn join_semantics_for_single_line_fields() {
        let (_, mut buf, mut host, mut vim) = setup();
        let mut ctx = Ctx {
            buf: &mut buf,
            host: &mut host,
        };
        vim.handle_key(&mut ctx, Key::char('i'));
        vim.insert_text_at_cursor(&mut ctx, "a");
        vim.handle_key(&mut ctx, Key::enter());
        vim.insert_text_at_cursor(&mut ctx, "b");
        // collapse follows the same rule as VimEdit::text(): join(" ")
        let joined = buf.0.get().join(" ");
        assert_eq!(joined, "a b");
    }
}

#[cfg(test)]
mod tck_tests {
    //! Engine TCK (vimcore::tck) acceptance for LinesBuf.
    use super::*;
    use vimcore::tck;

    /// LinesBuf satisfies the edit contract and the engine smoke test (with a
    /// real EditHost on the host side).
    #[test]
    fn lines_buf_meets_engine_contract() {
        let storage = SharedLines::default();
        let mut buf = LinesBuf(storage.clone());
        tck::buffer_edit_contract(&mut buf).unwrap();
        let mut host = EditHost::new(storage);
        tck::engine_smoke_contract(&mut buf, &mut host).unwrap();
    }

    /// The read contract still holds after prefilling multi-byte and
    /// empty-line content.
    #[test]
    fn lines_buf_read_contract_prefilled() {
        let storage = SharedLines::default();
        storage.set(Arc::new(vec![
            "alpha".to_string(),
            "beta 中文".to_string(),
            String::new(),
            "尾行".to_string(),
        ]));
        let buf = LinesBuf(storage);
        tck::buffer_read_contract(&buf).unwrap();
    }
}
