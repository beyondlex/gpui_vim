//! Shared test harness.
//!
//! One `Rc<RefCell<String>>` backs two views — a [`BufferView`] implementing
//! `VimBuffer(Mut)` and a [`HostView`] implementing `VimHost` — so a `Ctx`
//! can hold disjoint `&mut`s, exactly like a real host integration.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use vim_core::buffer::{clamp_to_line_end, VimBuffer, VimBufferMut};
use vim_core::host::VimHost;
use vim_core::key::Key;
use vim_core::state::{Ctx, KeyResult, VimState};

type Shared = Rc<RefCell<String>>;

#[derive(Clone)]
pub struct BufferView(pub Shared);

impl VimBuffer for BufferView {
    fn len(&self) -> usize {
        self.0.borrow().len()
    }
    fn line_count(&self) -> usize {
        let text = self.0.borrow();
        if text.is_empty() {
            1
        } else if text.ends_with('\n') {
            text.split('\n').count() - 1
        } else {
            text.split('\n').count()
        }
    }
    fn char_at(&self, offset: usize) -> Option<char> {
        let text = self.0.borrow();
        if offset >= text.len() || !text.is_char_boundary(offset) {
            return None;
        }
        text[offset..].chars().next()
    }
    fn prev_char_offset(&self, offset: usize) -> Option<usize> {
        let text = self.0.borrow();
        if offset == 0 || offset > text.len() || !text.is_char_boundary(offset) {
            return None;
        }
        text[..offset].chars().next_back().map(|c| offset - c.len_utf8())
    }
    fn line_range(&self, line: usize) -> Range<usize> {
        let text = self.0.borrow();
        let mut start = 0usize;
        let total = if text.is_empty() {
            1
        } else if text.ends_with('\n') {
            text.split('\n').count() - 1
        } else {
            text.split('\n').count()
        };
        if line >= total {
            return text.len()..text.len();
        }
        for (i, part) in text.split('\n').enumerate() {
            if i == line {
                let end = if i + 1 == text.split('\n').count() {
                    start + part.len()
                } else {
                    start + part.len() + 1
                };
                return start..end;
            }
            start += part.len() + 1;
        }
        start..start
    }
    fn offset_to_line(&self, offset: usize) -> usize {
        let text = self.0.borrow();
        let offset = offset.min(text.len());
        let line = text[..offset].split('\n').count() - 1;
        line.min(self.line_count() - 1)
    }
    fn slice(&self, range: Range<usize>) -> String {
        self.0.borrow()[range].to_owned()
    }
}

impl VimBufferMut for BufferView {
    fn insert_text(&mut self, offset: usize, text: &str) {
        self.0.borrow_mut().insert_str(offset, text);
    }
    fn delete_range(&mut self, range: Range<usize>) {
        self.0.borrow_mut().replace_range(range, "");
    }
}

pub struct HostView {
    text: Shared,
    pub viewport: (usize, usize),
    pub clipboard: Option<String>,
    pub highlights: Vec<Range<usize>>,
    pub current_highlight: Option<Range<usize>>,
    pub scrolled_to: Vec<usize>,
    pub group_count: usize,
    /// `:w` counter (Ex command tests).
    pub saved: usize,
    /// `:q` flag (Ex command tests).
    pub close_requested: bool,
    /// status_message texts (Ex feedback tests).
    pub statuses: Vec<String>,
    undo_stack: Vec<(String, usize)>,
    redo_stack: Vec<(String, usize)>,
    open_group: Option<u64>,
}

impl VimHost for HostView {
    fn viewport(&self) -> (usize, usize) {
        self.viewport
    }
    fn scroll_to_line(&mut self, line: usize) {
        self.scrolled_to.push(line);
    }
    fn clipboard_write(&mut self, text: &str) {
        self.clipboard = Some(text.to_owned());
    }
    fn clipboard_read(&self) -> Option<String> {
        self.clipboard.clone()
    }
    fn set_search_highlights(&mut self, matches: &[Range<usize>], current: Option<Range<usize>>) {
        self.highlights = matches.to_vec();
        self.current_highlight = current;
    }
    fn changed(&mut self) {}

    fn save(&mut self) {
        self.saved += 1;
    }

    fn request_close(&mut self) {
        self.close_requested = true;
    }

    fn status_message(&mut self, message: &str) {
        self.statuses.push(message.to_owned());
    }

    fn buffer_name(&self) -> &str {
        "test-buffer"
    }

    fn begin_undo_group(&mut self, id: u64, cursor: usize) {
        if self.open_group != Some(id) {
            self.undo_stack.push((self.text.borrow().clone(), cursor));
            self.open_group = Some(id);
            self.group_count += 1;
        }
    }
    fn undo(&mut self) -> Option<usize> {
        let (text, cursor) = self.undo_stack.pop()?;
        self.redo_stack.push((self.text.borrow().clone(), cursor));
        *self.text.borrow_mut() = text;
        self.open_group = None;
        Some(cursor)
    }
    fn redo(&mut self) -> Option<usize> {
        let (text, cursor) = self.redo_stack.pop()?;
        self.undo_stack.push((self.text.borrow().clone(), cursor));
        *self.text.borrow_mut() = text;
        self.open_group = None;
        Some(cursor)
    }
}

/// One engine session over one buffer.
pub struct Fixture {
    pub buf: BufferView,
    pub host: HostView,
    pub vim: VimState,
    _keep: Shared,
}

impl Fixture {
    pub fn new(initial: &str) -> Self {
        let shared: Shared = Rc::new(RefCell::new(initial.to_owned()));
        Fixture {
            buf: BufferView(shared.clone()),
            host: HostView {
                text: shared.clone(),
                viewport: (0, 9),
                clipboard: None,
                highlights: Vec::new(),
                current_highlight: None,
                scrolled_to: Vec::new(),
                group_count: 0,
                saved: 0,
                close_requested: false,
                statuses: Vec::new(),
                undo_stack: Vec::new(),
                redo_stack: Vec::new(),
                open_group: None,
            },
            vim: VimState::new(),
            _keep: shared,
        }
    }

    pub fn at(initial: &str, line: usize, col: usize) -> Self {
        let mut f = Fixture::new(initial);
        let line_start = f.buf.line_start(line);
        f.vim.cursor.offset = line_start + col;
        f
    }

    pub fn feed<I: AsRef<str>>(&mut self, keys: impl IntoIterator<Item = I>) -> &mut Self {
        for k in keys {
            let key = Key::parse(k.as_ref());
            let mut ctx = Ctx {
                buf: &mut self.buf,
                host: &mut self.host,
            };
            let _ = self.vim.handle_key(&mut ctx, key);
        }
        self
    }

    pub fn feed_raw(&mut self, key: Key) -> KeyResult {
        let mut ctx = Ctx {
            buf: &mut self.buf,
            host: &mut self.host,
        };
        self.vim.handle_key(&mut ctx, key)
    }

    /// Simulate the IME text-input path (what the host does with typed text).
    pub fn type_text(&mut self, s: &str) {
        let mut ctx = Ctx {
            buf: &mut self.buf,
            host: &mut self.host,
        };
        self.vim.insert_text_at_cursor(&mut ctx, s);
    }

    pub fn text(&self) -> String {
        self.buf.0.borrow().clone()
    }

    pub fn cursor(&self) -> usize {
        self.vim.cursor.offset
    }

    pub fn line(&self) -> usize {
        self.buf.offset_to_line(self.vim.cursor.offset)
    }

    pub fn clamp(&self, offset: usize) -> usize {
        clamp_to_line_end(&self.buf, offset)
    }
}

/// Convenience: run `keys` from `(line, col)` and return the fixture.
pub fn edit(initial: &str, line: usize, col: usize, keys: &[&str]) -> Fixture {
    let mut f = Fixture::at(initial, line, col);
    f.feed(keys);
    f
}
