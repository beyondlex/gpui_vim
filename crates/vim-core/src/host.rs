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

    /// Bridge for `:map <Leader>x :action SomeAction<CR>` — dispatch a
    /// HOST application action by id (IdeaVim's `:action` bridge). Unknown
    /// ids are the host's problem: report through `status_message`.
    fn dispatch_host_action(&mut self, id: &str) {
        let _ = id;
    }
}
