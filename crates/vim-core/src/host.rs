//! The host-facing side-effect surface.
//!
//! The engine computes *what* to do; the host decides *how* it shows up:
//! scrolling, the system clipboard, search highlighting and the undo stack.

use std::ops::Range;

/// Where the requested line should land in the viewport (`zz`/`zt`/`zb`).
/// Plain cursor scrolls use [`ScrollAnchor::Cursor`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollAnchor {
    /// Just make the line visible (minimal scroll).
    Cursor,
    /// `zz` — centered.
    Center,
    /// `zt` — top of the viewport.
    Top,
    /// `zb` — bottom of the viewport.
    Bottom,
}

/// Side-effect hooks the host must (or may) implement.
pub trait VimHost {
    /// Inclusive `(first_line, last_line)` of what is currently visible.
    fn viewport(&self) -> (usize, usize);

    /// Make sure `line` is visible.
    fn scroll_to_line(&mut self, line: usize);

    /// Make `line` visible with a specific anchor (`zz`/`zt`/`zb`). The
    /// default forwards to [`VimHost::scroll_to_line`] for hosts that don't
    /// distinguish anchors.
    fn scroll_to_line_anchored(&mut self, line: usize, anchor: ScrollAnchor) {
        let _ = anchor;
        self.scroll_to_line(line);
    }

    /// Scroll the viewport by `lines` (positive = down) without moving the
    /// cursor (`C-e`/`C-y`). Default: no-op for hosts without free scrolling.
    fn scroll_lines(&mut self, lines: i32) {
        let _ = lines;
    }

    fn clipboard_write(&mut self, text: &str);
    fn clipboard_read(&self) -> Option<String>;

    /// Publish search matches for highlighting. `current` is the match the
    /// cursor sits on (rendered differently). An empty slice clears.
    fn set_search_highlights(&mut self, matches: &[Range<usize>], current: Option<Range<usize>>);

    /// Begin an undo group. The engine assigns a fresh monotonically
    /// increasing id per logical undo unit; edits sharing an id (an insert
    /// session) belong to the same group and the host must merge them.
    /// `cursor_offset` is the engine cursor *before* the group's first edit —
    /// the natural place to restore the cursor to on undo.
    fn begin_undo_group(&mut self, id: u64, cursor_offset: usize);

    /// Undo the most recent group. Returns the cursor offset to restore,
    /// or `None` if there is nothing to undo.
    fn undo(&mut self) -> Option<usize>;

    /// Redo. Same contract as [`VimHost::undo`].
    fn redo(&mut self) -> Option<usize>;

    /// The buffer changed: repaint.
    fn changed(&mut self) {}

    /// Feedback for ignored keys (visual bell / flash). Optional.
    fn bell(&mut self) {}

    /// `:w` — persist the buffer. Hosts that don't persist may report the
    /// no-op through their own status channel.
    fn save(&mut self) {}

    /// `:q` — close the editor. The host decides whether that is allowed
    /// (e.g. prompting about unsaved changes is the host's call).
    fn request_close(&mut self) {}

    /// Non-fatal feedback text for the status UI: `E486: Pattern not found`,
    /// substitution counts, unknown Ex commands. Optional (default no-op);
    /// errors that need a decision still go through `bell`.
    fn status_message(&mut self, message: &str) {
        let _ = message;
    }

    /// The buffer's name for status UI (v1; `%` register support comes with
    /// expression registers). Optional.
    fn buffer_name(&self) -> &str {
        ""
    }

    /// `:bnext`/`:bprev`/`:bfirst`/`:blast` — the host owns the buffer list;
    /// it switches and returns false when there is nothing to switch to.
    fn cycle_buffer(&mut self, _forward: bool) -> bool {
        true
    }

    /// Bridge for `:map <Leader>x :action SomeAction<CR>` — dispatch a
    /// HOST application action by id (IdeaVim's `:action` bridge).
    /// `strict` is true for host-specific config layers (a miss is worth
    /// reporting via `status_message`) and false for shared user layers
    /// (mappings aimed at other apps are expected to miss: ignore quietly).
    fn dispatch_host_action_hinted(&mut self, id: &str, strict: bool) {
        let _ = (id, strict);
    }
}
