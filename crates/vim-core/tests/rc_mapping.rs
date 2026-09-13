use vim_core::buffer::{VimBuffer, VimBufferMut};
use vim_core::host::VimHost;
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
    actions: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
}
impl VimHost for H {
    fn viewport(&self) -> (usize, usize) { (0, 30) }
    fn scroll_to_line(&mut self, _: usize) {}
    fn clipboard_write(&mut self, _: &str) {}
    fn clipboard_read(&self) -> Option<String> { None }
    fn set_search_highlights(&mut self, _: &[std::ops::Range<usize>], _: Option<std::ops::Range<usize>>) {}
    fn begin_undo_group(&mut self, _: u64, _: usize) {}
    fn undo(&mut self) -> Option<usize> { None }
    fn redo(&mut self) -> Option<usize> { None }
    fn dispatch_host_action_hinted(&mut self, id: &str, _strict: bool) {
        self.actions.borrow_mut().push(id.to_string());
    }
}

#[test]
fn leader_mapping_parses_and_fires() {
    let cfg = vim_core::config::parse("noremap <Leader>d :action Foo<CR>");
    eprintln!("mappings={}", cfg.mappings.len());
    for m in &cfg.mappings {
        eprintln!(
            "class={:?} lhs={:?} rhs={:?} noremap={}",
            m.class, m.lhs, m.rhs, m.noremap
        );
    }
    assert!(!cfg.mappings.is_empty(), "rc 映射应被解析");

    let mut vim = VimState::new();
    vim.apply_config(&cfg);
    let mut buf = B("hello\nworld\n".to_string());
    let mut host = H::default();
    for k in ["\\", "d"] {
        let r = {
            let mut ctx = Ctx { buf: &mut buf, host: &mut host };
            vim.handle_key(&mut ctx, Key::char(k.chars().next().unwrap()))
        };
        eprintln!("key {k} -> {r:?} mode={:?} cmdline={:?}", vim.mode(), vim.cmdline.buffer);
    }
    assert_eq!(
        host.actions.borrow().as_slice(),
        ["Foo"],
        "映射 RHS 的 :action 应派发到宿主"
    );
}
