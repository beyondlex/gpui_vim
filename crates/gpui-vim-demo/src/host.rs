//! The host side of the demo: viewport, clipboard, search highlights and a
//! snapshot-based undo stack with vim's group semantics.

use std::ops::Range;

use crate::buffer::SharedRope;
use vim_core::host::VimHost;

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
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            open_group: None,
        }
    }

    #[allow(dead_code)]
    pub fn undo_depth(&self) -> usize {
        self.undo_stack.len()
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
            self.undo_stack.push((snapshot, cursor));
            if self.undo_stack.len() > 200 {
                self.undo_stack.remove(0);
            }
            self.open_group = Some(id);
        }
    }

    fn undo(&mut self) -> Option<usize> {
        let (rope, cursor) = self.undo_stack.pop()?;
        let current = self.rope.borrow().clone();
        self.redo_stack.push((current, cursor));
        *self.rope.borrow_mut() = rope;
        self.open_group = None;
        Some(cursor)
    }

    fn redo(&mut self) -> Option<usize> {
        let (rope, cursor) = self.redo_stack.pop()?;
        let current = self.rope.borrow().clone();
        self.undo_stack.push((current, cursor));
        *self.rope.borrow_mut() = rope;
        self.open_group = None;
        Some(cursor)
    }

    fn changed(&mut self) {}
}
