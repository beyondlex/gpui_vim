// 临时性能探针：量引擎关键路径的真实耗时
use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::time::Instant;
use vim_core::buffer::{VimBuffer, VimBufferMut};
use vim_core::host::VimHost;
use vim_core::key::Key;
use vim_core::state::{Ctx, VimState};

#[derive(Clone)]
struct B(Rc<RefCell<String>>);
impl VimBuffer for B {
    fn len(&self) -> usize {
        self.0.borrow().len()
    }
    fn line_count(&self) -> usize {
        self.0.borrow().split('\n').count()
    }
    fn char_at(&self, o: usize) -> Option<char> {
        self.0.borrow()[o..].chars().next()
    }
    fn prev_char_offset(&self, o: usize) -> Option<usize> {
        if o == 0 {
            return None;
        }
        self.0.borrow()[..o]
            .chars()
            .next_back()
            .map(|c| o - c.len_utf8())
    }
    fn line_range(&self, line: usize) -> Range<usize> {
        let t = self.0.borrow();
        let mut s = 0;
        for (i, p) in t.split('\n').enumerate() {
            if i == line {
                return s..s + p.len() + usize::from(i + 1 < t.split('\n').count());
            }
            s += p.len() + 1;
        }
        t.len()..t.len()
    }
    fn offset_to_line(&self, o: usize) -> usize {
        self.0.borrow()[..o.min(self.0.borrow().len())]
            .split('\n')
            .count()
            - 1
    }
    fn slice(&self, r: Range<usize>) -> String {
        self.0.borrow()[r].to_owned()
    }
}
impl VimBufferMut for B {
    fn insert_text(&mut self, o: usize, t: &str) {
        self.0.borrow_mut().insert_str(o, t)
    }
    fn delete_range(&mut self, r: Range<usize>) {
        self.0.borrow_mut().replace_range(r, "")
    }
}

/// ropey-backed buffer: the "good host" (O(log n) line lookups), mirroring
/// the demo's `RopeBuffer`
#[derive(Clone)]
struct R(Rc<RefCell<ropey::Rope>>);
impl VimBuffer for R {
    fn len(&self) -> usize {
        self.0.borrow().len_bytes()
    }
    fn line_count(&self) -> usize {
        self.0.borrow().len_lines()
    }
    fn char_at(&self, o: usize) -> Option<char> {
        let rope = self.0.borrow();
        let ci = rope.try_byte_to_char(o).ok()?;
        rope.get_char(ci)
    }
    fn prev_char_offset(&self, o: usize) -> Option<usize> {
        let rope = self.0.borrow();
        if o == 0 || o > rope.len_bytes() {
            return None;
        }
        let ci = rope.try_byte_to_char(o).ok()?;
        if ci == 0 {
            return None;
        }
        let prev = rope.get_char(ci - 1)?;
        Some(o - prev.len_utf8())
    }
    fn line_range(&self, line: usize) -> Range<usize> {
        let rope = self.0.borrow();
        if line >= rope.len_lines() {
            return rope.len_bytes()..rope.len_bytes();
        }
        let start = rope.line_to_byte(line);
        let end = if line + 1 < rope.len_lines() {
            rope.line_to_byte(line + 1)
        } else {
            rope.len_bytes()
        };
        start..end
    }
    fn offset_to_line(&self, o: usize) -> usize {
        let rope = self.0.borrow();
        rope.try_byte_to_line(o.min(rope.len_bytes())).unwrap_or(0)
    }
    fn slice(&self, r: Range<usize>) -> String {
        let rope = self.0.borrow();
        let (s, e) = (
            rope.try_byte_to_char(r.start).unwrap_or(0),
            rope.try_byte_to_char(r.end).unwrap_or(0),
        );
        rope.slice(s..e).to_string()
    }
}
impl VimBufferMut for R {
    fn insert_text(&mut self, o: usize, t: &str) {
        let mut rope = self.0.borrow_mut();
        let ci = rope.byte_to_char(o);
        rope.insert(ci, t);
    }
    fn delete_range(&mut self, r: Range<usize>) {
        let mut rope = self.0.borrow_mut();
        let (s, e) = (rope.byte_to_char(r.start), rope.byte_to_char(r.end));
        rope.remove(s..e);
    }
}
#[derive(Default)]
struct H {
    hl: Vec<Range<usize>>,
}
impl VimHost for H {
    fn viewport(&self) -> (usize, usize) {
        (0, 40)
    }
    fn scroll_to_line(&mut self, _: usize) {}
    fn clipboard_write(&mut self, _: &str) {}
    fn clipboard_read(&self) -> Option<String> {
        None
    }
    fn set_search_highlights(&mut self, m: &[Range<usize>], _: Option<Range<usize>>) {
        self.hl = m.to_vec()
    }
    fn begin_undo_group(&mut self, _: u64, _: usize) {}
    fn undo(&mut self) -> Option<usize> {
        None
    }
    fn redo(&mut self) -> Option<usize> {
        None
    }
    fn changed(&mut self) {}
    fn status_message(&mut self, _: &str) {}
}

fn feed<B: VimBufferMut>(vim: &mut VimState, buf: &mut B, h: &mut H, keys: &[Key]) {
    for k in keys {
        let mut ctx = Ctx { buf, host: h };
        vim.handle_key(&mut ctx, k.clone());
    }
}

fn main() {
    // ---- 1. 普通 motion：10k 次按键 ----
    let body = "lorem ipsum dolor sit amet ".repeat(400); // ~11KB 单段长行 + 短行
    let mut buf = B(Rc::new(RefCell::new(format!("{body}\nsecond line here\n"))));
    let mut vim = VimState::new();
    let mut h = H::default();
    let keys: Vec<Key> = std::iter::repeat_n(Key::parse("w"), 2000).collect();
    let t = Instant::now();
    feed(&mut vim, &mut buf, &mut h, &keys);
    println!(
        "2000x 'w' motion (11KB buffer): {:>8.2?}  ({:.1} us/key)",
        t.elapsed(),
        t.elapsed().as_micros() as f64 / 2000.0
    );

    // ---- 2. 大 buffer（1MB）上的 w 移动 ----
    let big = "word ".repeat(200_000); // 1MB 单行
    let mut buf = B(Rc::new(RefCell::new(big)));
    let mut vim = VimState::new();
    let mut h = H::default();
    let keys: Vec<Key> = std::iter::repeat_n(Key::parse("w"), 200).collect();
    let t = Instant::now();
    feed(&mut vim, &mut buf, &mut h, &keys);
    println!(
        "200x 'w' motion (1MB line):     {:>8.2?}  ({:.1} us/key)",
        t.elapsed(),
        t.elapsed().as_micros() as f64 / 200.0
    );

    // ---- 3. 搜索 + hlsearch 常驻后，每次编辑的 republish 成本 ----
    let lines: String = "the quick brown fox jumps over the lazy dog\n".repeat(20_000); // ~900KB
    let mut buf = B(Rc::new(RefCell::new(lines)));
    let mut vim = VimState::new();
    let mut h = H::default();
    feed(
        &mut vim,
        &mut buf,
        &mut h,
        &[
            Key::parse("/"),
            Key::char('f'),
            Key::char('o'),
            Key::char('x'),
            Key::named("enter"),
        ],
    );
    let matches = h.hl.len();
    let t = Instant::now();
    // republish_search 的核心成本 = all_matches 全文件正则扫描；
    // bump 是 crate 内部，这里直接量 20 次等价的搜索扫描
    for _ in 0..20 {
        let _ = vim_core::search::all_matches(&vim, &buf, "fox");
    }
    println!(
        "20x search re-scan (900KB, {} matches): {:>8.2?}  ({:.2} ms/scan)",
        matches,
        t.elapsed(),
        t.elapsed().as_millis() as f64 / 20.0
    );

    // ---- 4. :%s 全文件替换 ----
    let lines: String = "foo bar baz\n".repeat(50_000); // ~600KB
    let mut buf = B(Rc::new(RefCell::new(lines)));
    let mut vim = VimState::new();
    let mut h = H::default();
    let cmd: Vec<Key> = ":s/foo/bar/g"
        .chars()
        .map(Key::char)
        .chain(std::iter::once(Key::named("enter")))
        .collect();
    let t = Instant::now();
    feed(&mut vim, &mut buf, &mut h, &cmd);
    println!(":%s/foo/bar/g (600KB, 50k lines): {:>8.2?}", t.elapsed());

    // ---- 5. gq 重排 200 行段落 ----
    let para =
        "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod\n".repeat(200);
    let mut buf = B(Rc::new(RefCell::new(para)));
    let mut vim = VimState::new();
    vim.options_mut().textwidth = 80;
    let mut h = H::default();
    let cmd: Vec<Key> = ["g", "q", "G"].iter().map(|k| Key::parse(k)).collect();
    let t = Instant::now();
    feed(&mut vim, &mut buf, &mut h, &cmd);
    println!("gqG reflow (200 lines):          {:>8.2?}", t.elapsed());

    // ---- 6. x 删除（10k 行 buffer）----
    let mut buf = B(Rc::new(RefCell::new("line of text\n".repeat(10_000))));
    let mut vim = VimState::new();
    let mut h = H::default();
    let keys: Vec<Key> = std::iter::repeat_n(Key::parse("x"), 100).collect();
    let t = Instant::now();
    feed(&mut vim, &mut buf, &mut h, &keys);
    println!(
        "100x 'x' delete (10k lines):     {:>8.2?}  ({:.1} us/key)",
        t.elapsed(),
        t.elapsed().as_micros() as f64 / 100.0
    );

    // ---- 7. n 连跳（900KB、1 万匹配，ropey buffer）----
    // hlsearch 常驻后连按 n：未命中缓存时每次都是全文件 slice + 正则重扫；
    // ropey buffer 隔离掉朴素 line-lookup 的宿主成本，反映引擎自身开销
    let lines: String = "the quick brown fox jumps over the lazy dog\n".repeat(20_000); // ~900KB
    let mut buf = R(Rc::new(RefCell::new(ropey::Rope::from(lines))));
    let mut vim = VimState::new();
    let mut h = H::default();
    feed(
        &mut vim,
        &mut buf,
        &mut h,
        &[
            Key::parse("/"),
            Key::char('f'),
            Key::char('o'),
            Key::char('x'),
            Key::named("enter"),
        ],
    );
    let keys: Vec<Key> = std::iter::repeat_n(Key::parse("n"), 200).collect();
    let t = Instant::now();
    feed(&mut vim, &mut buf, &mut h, &keys);
    println!(
        "200x 'n' jump (900KB ropey):    {:>8.2?}  ({:.1} us/key)",
        t.elapsed(),
        t.elapsed().as_micros() as f64 / 200.0
    );
}
