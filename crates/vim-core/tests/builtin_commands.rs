//! Builtin-command regressions: `gi` / `ZZ` / `ZQ` and count+`gg` with the
//! builtin `g` chord table (a user mapping with the same prefix shadows
//! builtin chords — mappings go through the Waiting path; builtins share one
//! trie and never conflict).

use vim_core::buffer::{VimBuffer, VimBufferMut};
use vim_core::host::{ScrollAnchor, VimHost};
use vim_core::key::Key;
use vim_core::state::{Ctx, VimState};

struct B(String);
impl VimBuffer for B {
    fn len(&self) -> usize { self.0.len() }
    fn line_count(&self) -> usize { self.0.lines().count() }
    fn char_at(&self, o: usize) -> Option<char> { self.0[o..].chars().next() }
    fn prev_char_offset(&self, o: usize) -> Option<usize> { Some(o.saturating_sub(1)).filter(|_| o > 0) }
    fn line_range(&self, l: usize) -> std::ops::Range<usize> {
        let start: usize = self.0.lines().take(l).map(|s| s.len() + 1).sum();
        let len = self.0.lines().nth(l).map(|s| s.len()).unwrap_or(0);
        start..start + len + 1
    }
    fn offset_to_line(&self, o: usize) -> usize { self.0[..o].matches('\n').count() }
    fn slice(&self, r: std::ops::Range<usize>) -> String { self.0[r].to_string() }
}
impl VimBufferMut for B {
    fn insert_text(&mut self, o: usize, t: &str) { self.0.insert_str(o, t) }
    fn delete_range(&mut self, r: std::ops::Range<usize>) { let _ = self.0.drain(r); }
}

#[derive(Default)]
struct H {
    saved: bool,
    closed: bool,
    anchored: Option<(usize, ScrollAnchor)>,
}
impl VimHost for H {
    fn viewport(&self) -> (usize, usize) { (0, 30) }
    fn scroll_to_line(&mut self, _: usize) {}
    fn scroll_to_line_anchored(&mut self, line: usize, anchor: ScrollAnchor) {
        self.anchored = Some((line, anchor));
    }
    fn clipboard_write(&mut self, _: &str) {}
    fn clipboard_read(&self) -> Option<String> { None }
    fn set_search_highlights(&mut self, _: &[std::ops::Range<usize>], _: Option<std::ops::Range<usize>>) {}
    fn begin_undo_group(&mut self, _: u64, _: usize) {}
    fn undo(&mut self) -> Option<usize> { None }
    fn redo(&mut self) -> Option<usize> { None }
    fn save(&mut self) { self.saved = true; }
    fn request_close(&mut self) { self.closed = true; }
}

fn feed(vim: &mut VimState, buf: &mut B, host: &mut H, keys: &[&str]) {
    for k in keys {
        let key = Key::parse(k);
        let result = {
            let mut ctx = Ctx { buf, host };
            vim.handle_key(&mut ctx, key.clone())
        };
        // host-side placement for insert-mode printables (dispatch_text's job)
        if result == vim_core::state::KeyResult::Unknown {
            if matches!(vim.mode(), vim_core::mode::Mode::Insert) {
                if let Some(c) = key.printable_char() {
                    let mut ctx = Ctx { buf, host };
                    vim.insert_text_at_cursor(&mut ctx, &c.to_string());
                }
            }
        }
    }
}

fn buffer() -> B {
    B((1..=120).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n"))
}

#[test]
fn count_gg_moves_through_builtin_g_chord_table() {
    let mut vim = VimState::new();
    let mut buf = buffer();
    let mut host = H::default();
    feed(&mut vim, &mut buf, &mut host, &["5", "0", "g", "g"]);
    assert_ne!(vim.cursor.offset, 0, "50gg should move the cursor");
}

#[test]
fn gi_inserts_at_last_insert_exit() {
    let mut vim = VimState::new();
    let mut buf = B("hello\nworld\n".to_string());
    let mut host = H::default();
    // A append at line end → esc → gg; gi should return to line 0 end in insert
    feed(&mut vim, &mut buf, &mut host, &["A", "X", "escape", "g", "g"]);
    assert_eq!(vim.cursor.offset, 0);
    feed(&mut vim, &mut buf, &mut host, &["g", "i"]);
    assert!(matches!(vim.mode(), vim_core::mode::Mode::Insert), "gi enters insert");
    // vim `^` semantics: leaving insert backs onto the last typed char
    assert_eq!(vim.cursor.offset, 5, "gi at last insert exit");
}

#[test]
fn zz_saves_and_quits_zq_quits() {
    let mut vim = VimState::new();
    let mut buf = buffer();
    let mut host = H::default();
    feed(&mut vim, &mut buf, &mut host, &["Z", "Z"]);
    assert!(host.saved && host.closed, "ZZ = save + close");

    let mut vim = VimState::new();
    let mut buf = buffer();
    let mut host = H::default();
    feed(&mut vim, &mut buf, &mut host, &["Z", "Q"]);
    assert!(!host.saved && host.closed, "ZQ = close without saving");
}
