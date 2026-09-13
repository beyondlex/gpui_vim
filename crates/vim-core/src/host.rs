//! The host-facing side-effect surface.
//!
//! The engine computes *what* to do; the host decides *how* it shows up:
//! scrolling, the system clipboard, search highlighting and the undo stack.

use std::ops::Range;

/// Side-effect hooks the host must (or may) implement.
pub trait VimHost {
    /// Inclusive `(first_line, last_line)` of what is currently visible.
    fn viewport(&self) -> (usize, usize);

    /// Make sure `line` is visible.
    fn scroll_to_line(&mut self, line: usize);

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
    fn changed(&mut self);

    /// Feedback for ignored keys (visual bell / flash). Optional.
    fn bell(&mut self) {}
}
