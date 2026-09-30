//! The host side of the demo: viewport, clipboard, search highlights and a
//! snapshot-based undo stack with vim's group semantics.

use std::ops::Range;

use crate::buffer::SharedRope;
use vimcore::host::VimHost;

/// Snapshot entries kept for `u` (oldest dropped beyond this).
const MAX_UNDO_STEPS: usize = 200;

pub struct HostState {
    rope: SharedRope,
    pub viewport: (usize, usize),
    pub highlights: Vec<Range<usize>>,
    pub current_highlight: Option<Range<usize>>,
    /// Text to be flushed to the system clipboard by the view (which owns
    /// the gpui `App`).
    pub pending_clipboard_write: Option<String>,
    /// Mirrored system clipboard (synced on focus / paste).
    pub clipboard: Option<String>,
    pub scrolled_to: Option<usize>,
    /// `:w` — status text for the view to show (demo does not persist).
    pub pending_status: Option<String>,
    /// `:q` — the view should close the window.
    pub pending_close: bool,
    /// `:action <id>` — the view should dispatch this host action.
    pub pending_action: Option<String>,
    /// demo-reserved: tab cycling direction (Some(forward)).
    pub pending_tab_cycle: Option<bool>,
    undo_stack: Vec<(ropey::Rope, usize)>,
    redo_stack: Vec<(ropey::Rope, usize)>,
    open_group: Option<u64>,
}

impl HostState {
    pub fn new(rope: SharedRope) -> Self {
        HostState {
            rope,
            viewport: (0, 24),
            highlights: Vec::new(),
            current_highlight: None,
            pending_clipboard_write: None,
            clipboard: None,
            scrolled_to: None,
            pending_status: None,
            pending_close: false,
            pending_tab_cycle: None,
            pending_action: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            open_group: None,
        }
    }

    #[allow(dead_code)]
    pub fn undo_depth(&self) -> usize {
        self.undo_stack.len()
    }

    /// Shared body of undo/redo: pop a snapshot from the source stack, push
    /// the current rope onto the opposite one, then restore the snapshot.
    /// The popped cursor is returned so the engine can reposition.
    fn travel(&mut self, redo: bool) -> Option<usize> {
        let (rope, cursor) = if redo {
            self.redo_stack.pop()?
        } else {
            self.undo_stack.pop()?
        };
        let current = self.rope.borrow().clone();
        if redo {
            self.undo_stack.push((current, cursor));
        } else {
            self.redo_stack.push((current, cursor));
        }
        *self.rope.borrow_mut() = rope;
        self.open_group = None;
        Some(cursor)
    }
}

impl VimHost for HostState {
    fn viewport(&self) -> (usize, usize) {
        self.viewport
    }

    fn scroll_to_line(&mut self, line: usize) {
        self.scrolled_to = Some(line);
    }

    fn clipboard_write(&mut self, text: &str) {
        self.pending_clipboard_write = Some(text.to_owned());
        self.clipboard = Some(text.to_owned());
    }

    fn clipboard_read(&self) -> Option<String> {
        self.clipboard.clone()
    }

    fn set_search_highlights(&mut self, matches: &[Range<usize>], current: Option<Range<usize>>) {
        self.highlights = matches.to_vec();
        self.current_highlight = current;
    }

    fn begin_undo_group(&mut self, id: u64, cursor: usize) {
        if self.open_group != Some(id) {
            // ropey `Rope` clones in O(1)
            let snapshot = self.rope.borrow().clone();
            // A fresh group is a NEW edit after an undo: vim discards the
            // redo branch there, and keeping it lets a later C-r resurrect
            // the pre-undo text over what was just typed.
            self.redo_stack.clear();
            self.undo_stack.push((snapshot, cursor));
            if self.undo_stack.len() > MAX_UNDO_STEPS {
                self.undo_stack.remove(0);
            }
            self.open_group = Some(id);
        }
    }

    fn undo(&mut self) -> Option<usize> {
        self.travel(false)
    }

    fn redo(&mut self) -> Option<usize> {
        self.travel(true)
    }

    fn changed(&mut self) {}

    fn status_message(&mut self, message: &str) {
        self.pending_status = Some(message.to_owned());
    }

    fn buffer_name(&self) -> &str {
        "untitled"
    }

    fn save(&mut self) {
        let (lines, bytes) = {
            let rope = self.rope.borrow();
            (rope.len_lines().saturating_sub(1), rope.len_bytes())
        };
        self.pending_status = Some(format!(
            "\"{}\" {lines}L, {bytes}B written (demo: not persisted)",
            self.buffer_name()
        ));
    }

    fn request_close(&mut self) {
        self.pending_close = true;
    }

    fn cycle_buffer(&mut self, forward: bool) -> bool {
        self.pending_tab_cycle = Some(forward);
        true
    }

    fn dispatch_host_action_hinted(&mut self, id: &str, strict: bool) {
        // demo-reserved ids: tab switching from mappings (gt/gT analog)
        if id == "demo.tab-next" {
            self.pending_tab_cycle = Some(true);
            return;
        }
        if id == "demo.tab-prev" {
            self.pending_tab_cycle = Some(false);
            return;
        }
        // strict layers report unknown ids; shared user layers ignore them
        // (mappings aimed at other apps are expected to miss)
        let known = matches!(
            id,
            "demo.Save" | "demo.Copy" | "demo.Paste" | "Save" | "Copy" | "Paste"
        );
        if known || strict {
            self.pending_action = Some(id.to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::shared_rope;

    /// The engine's dialog with the host around one edit: open a group,
    /// mutate the shared rope directly (as `VimBufferMut` would).
    fn edit(rope: &SharedRope, host: &mut HostState, group: u64, cursor: usize, text: &str) {
        host.begin_undo_group(group, cursor);
        rope.borrow_mut().insert(0, text);
    }

    #[test]
    fn new_edit_after_undo_discards_the_redo_branch() {
        // undo → type something new → redo must NOT resurrect the pre-undo
        // text over the fresh typing (classic stale-redo corruption).
        let rope = shared_rope("hello");
        let mut host = HostState::new(rope.clone());

        edit(&rope, &mut host, 1, 0, "x"); // "xhello"
        assert_eq!(host.undo().unwrap(), 0);
        assert_eq!(rope.borrow().to_string(), "hello");

        edit(&rope, &mut host, 2, 1, "y"); // "yhello"
        assert_eq!(rope.borrow().to_string(), "yhello");
        assert!(host.redo().is_none(), "fresh edit invalidates redo");
        assert_eq!(rope.borrow().to_string(), "yhello");
    }

    #[test]
    fn insert_session_shares_one_group_and_undoes_in_one_step() {
        // same id across edits = same undo unit (an insert session)
        let rope = shared_rope("ab");
        let mut host = HostState::new(rope.clone());

        host.begin_undo_group(7, 2);
        rope.borrow_mut().insert(2, "c");
        host.begin_undo_group(7, 2);
        rope.borrow_mut().insert(3, "d");
        assert_eq!(host.undo_depth(), 1, "one snapshot per group id");
        host.undo();
        assert_eq!(rope.borrow().to_string(), "ab");
        assert_eq!(host.redo().unwrap(), 2);
        assert_eq!(rope.borrow().to_string(), "abcd");
    }
}
