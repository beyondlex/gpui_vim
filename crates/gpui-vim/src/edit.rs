//! 可复用的 vim 文本编辑实体（嵌入路线 B「宿主路由」的官方打包形态）。
//!
//! - 按键由宿主路由：宿主把 gpui [`gpui::Keystroke`] 经 [`crate::to_core_key`]
//!   转成引擎按键后喂 [`VimEdit::process_key`]，不走 [`crate::attach`]（避免
//!   双路处理）；IME 提交文本经 `EntityInputHandler::replace_text_in_range`
//!   进引擎。宿主只管路由与事件（drain [`VimEditEvent`]）。
//! - 单行模式：表单字段、路径输入；多行：正文编辑器。
//! - 引擎副作用统一暂存 [`EditHost`]，flush 落位并产生 [`VimEditEvent`]。
//! - 视觉参数集中在 [`VimEditStyle`]：宿主从各自主题构造（见宿主侧
//!   `theme_style()` 一类 helper），[`VimEditStyle::default`] 给一套中性深色
//!   默认值。
//! - vimrc 分层：[`VimEdit::new`] 只加载跨应用的用户层 `~/.gpui-vimrc`
//!   （action 宽松）；宿主专属层由宿主构造后用 [`crate::config::load_layers`]
//!   追加（后加载、同键覆盖、action 严格）。

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
use gpui_vim_core::buffer::{clamp_to_line_end, VimBuffer, VimBufferMut};
use gpui_vim_core::host::{ScrollAnchor, VimHost};
use gpui_vim_core::key::KeyKind;
use gpui_vim_core::mode::{Mode, VisualKind};
use gpui_vim_core::state::{Ctx, KeyResult, VimState};

// ---------- 视觉样式 ----------

/// 编辑框的视觉参数（颜色/字体/度量）。宿主从自己的主题构造；`Default`
/// 是一套中性深色值（0xRRGGBB，与 gpui `rgb`/`rgba` 直接配套）。
#[derive(Debug, Clone, PartialEq)]
pub struct VimEditStyle {
    /// 等宽字体 family。
    pub mono_font: String,
    /// 字号（px）。
    pub font_size: f32,
    /// 行高（px）。滚动与点击定位都按它换算。
    pub line_height: f32,
    /// 等宽单字符 advance（px）；光标/高亮的列定位按此换算。
    pub char_width: f32,
    /// 编辑框底色。
    pub surface_bg: u32,
    /// 未聚焦描边。
    pub border: u32,
    /// 聚焦描边。
    pub focus_border: u32,
    /// accent（聚焦底色混合、搜索高亮、可视选区）。
    pub accent: u32,
    /// 当前搜索命中高亮。
    pub gold: u32,
    /// 光标颜色。
    pub cursor: u32,
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
        }
    }
}

/// 在 a、b 间线性混合（t=0 → a）。
fn blend_rgb(a: u32, b: u32, t: f32) -> u32 {
    let (ar, ag, ab) = (a >> 16, (a >> 8) & 0xff, a & 0xff);
    let (br, bg_, bb) = (b >> 16, (b >> 8) & 0xff, b & 0xff);
    let f = |x: u32, y: u32| (x as f32 + (y as f32 - x as f32) * t).round() as u32;
    (f(ar, br) << 16) | (f(ag, bg_) << 8) | f(ab, bb)
}

// ---------- 行缓冲（Arc COW） ----------

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

/// offset（UTF-8 字节）↔（行, grapheme 列）。
pub fn offset_rc(lines: &[String], offset: usize) -> (usize, usize) {
    let mut rem = offset;
    for (row, line) in lines.iter().enumerate() {
        if rem <= line.len() {
            let col = line[..rem].chars().count();
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

/// `Arc<Vec<String>>` 上的 VimBuffer 实现。
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
        // 合同：非字符边界必须返回 None（引擎 line_end/line_indent 会探测
        // range.end-1；多字节行尾时 offset 落在字符中间，不能经 offset_rc 切片）
        let lines = self.0.get();
        let text = lines.join("\n");
        text.get(offset..)?.chars().next()
    }
    fn prev_char_offset(&self, offset: usize) -> Option<usize> {
        if offset == 0 || offset > self.len() {
            return None;
        }
        let lines = self.0.get();
        let text = lines.join("\n");
        text.get(..offset)?
            .chars()
            .next_back()
            .map(|c| offset - c.len_utf8())
    }
    fn line_range(&self, line: usize) -> Range<usize> {
        let lines = self.0.get();
        // 合同要求 range 包含本行换行符（end = 下一行起点 / 末行 = 缓冲区尾），
        // 否则引擎的 linewise 操作（dd/cc/yy）会在原位留下空行
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
    // 换行按真实内容插入/删除，不做压平：单行语义由 VimEdit.single_line
    // 在 flush 时 collapse（按内容行数判断会把多行编辑器初始的单行状态
    // 误判为单行模式，enter/o 的 '\n' 全被替换成空格）
    fn insert_text(&mut self, offset: usize, text: &str) {
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
    }
    fn delete_range(&mut self, range: Range<usize>) {
        let lines = self.0.get();
        let mut joined = lines.join("\n");
        let a = range.start.min(joined.len());
        let b = range.end.min(joined.len());
        if a < b {
            joined.replace_range(a..b, "");
        }
        let vec: Vec<String> = joined.split('\n').map(String::from).collect();
        self.0.set(Arc::new(vec));
    }
}

impl LinesBuf {
    pub fn byte_to_utf16(&self, byte: usize) -> usize {
        let lines = self.0.get();
        let text = lines.join("\n");
        text.get(..byte.min(text.len()))
            .map(|s| s.chars().map(|c| c.len_utf16()).sum())
            .unwrap_or(0)
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

// ---------- 宿主副作用容器 ----------

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
        None // 视图本地撤销栈处理
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

// ---------- VimEdit ----------

/// 宿主可观察的编辑事件（flush 后进入 pending_events，宿主 drain）。
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
    /// 视觉参数（宿主构造后注入自己的主题；见 [`VimEditStyle`]）。
    pub style: VimEditStyle,
    text_undo: Vec<(Vec<String>, (usize, usize))>,
    text_redo: Vec<(Vec<String>, (usize, usize))>,
    pub scroll: ScrollHandle,
    pub marked_range: Option<Range<usize>>,
    /// 多行模式可见行数（估算，用于 viewport 汇报）
    pub visible_rows: usize,
    pub modified: bool,
    pub label: &'static str,
    /// flush 产生的宿主事件（宿主在 process_key 后 drain）
    pub pending_events: Vec<VimEditEvent>,
    /// 编辑框几何（IME canvas 捕获，用于视口换算）
    pub area: std::rc::Rc<Cell<gpui::Bounds<gpui::Pixels>>>,
    /// 鼠标拖选中（mousedown 置位，mouseup 复位）
    pub mouse_selecting: bool,
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
            text_undo: Vec::new(),
            text_redo: Vec::new(),
            scroll: ScrollHandle::new(),
            marked_range: None,
            visible_rows: if single_line { 1 } else { 12 },
            modified: false,
            label,
            pending_events: Vec::new(),
            area: std::rc::Rc::new(Cell::new(gpui::Bounds::default())),
            mouse_selecting: false,
        };
        // vimrc 分层：这里只加载跨应用的用户层 ~/.gpui-vimrc（action 宽松，
        // 其 :action 映射可能面向其他应用）。宿主专属层由宿主构造后追加：
        // `config::load_layers(&mut edit, &Layers { user: None, host: Some(...) })`
        // （后加载、同键覆盖、action 严格）。
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

    /// 替换全部内容（光标回 0、normal 模式、清撤销）。
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
        self.vim = VimState::new();
        self.vim.cursor.offset = 0;
        self.modified = false;
        self.text_undo.clear();
        self.text_redo.clear();
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

    /// 拦截器路径按键处理。返回 true = 已消费。
    pub fn process_key(&mut self, ks: &gpui::Keystroke, cx: &mut Context<Self>) -> bool {
        if ks.modifiers.platform || ks.modifiers.function {
            return false;
        }
        let key = crate::to_core_key(ks);
        let mode = self.vim.mode;

        // 撤销/重做（normal 态 + 引擎空闲，本地栈）。is_idle 门槛必不可少：
        // `gu` 之后的 `u`、`guu` 的尾键都是引擎待完成命令的一部分，抢先拦截
        // 会把它们吞成本地 undo，导致 gu/guw/guu/gugu 全部失效。
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
        // 引擎留给宿主的可打印字符（insert/replace 态）直接落缓冲
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
        self.text_undo.push((before, cursor_before));
        self.text_redo.clear();
    }

    fn undo(&mut self) {
        let Some((before, cursor_before)) = self.text_undo.pop() else {
            return;
        };
        self.text_redo
            .push((self.lines.as_ref().clone(), self.cursor_rc()));
        self.lines = Arc::new(before);
        self.buf.0.set(self.lines.clone());
        self.vim.mode = Mode::Normal;
        self.set_cursor_rc(cursor_before);
    }

    fn redo(&mut self) {
        let Some((after, cursor_after)) = self.text_redo.pop() else {
            return;
        };
        self.text_undo
            .push((self.lines.as_ref().clone(), self.cursor_rc()));
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

    /// 用 `s` 替换扁平字节区间并把光标移到插入内容之后（补全插入用）。
    /// 进入 insert 态；不经引擎命令流水线。
    pub fn splice_range(&mut self, range: Range<usize>, s: &str) {
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
        if self.single_line {
            self.collapse_single_line();
        }
    }

    /// 指定行的可视选区（grapheme 列区间）。
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

    /// 渲染编辑框（父视图组合调用）。`focused` 决定描边与光标可见。
    pub fn render_view(&self, focused: bool) -> gpui::Stateful<gpui::Div> {
        let line_h = px(self.style.line_height);
        let line_height_f = self.style.line_height;
        let char_width_f = self.style.char_width;
        let mono = SharedString::from(self.style.mono_font.clone());
        let font_size = px(self.style.font_size);

        // 可见行窗口（area 由 IME canvas 捕获；首帧取不到就全画）
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

        // 搜索高亮按行分组
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
                let mut row = div()
                    .h(line_h)
                    .w_full()
                    .relative()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .when(!display_line.is_empty(), |d| d.child(display_line.clone()));
                // 搜索高亮
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
                // 可视选区
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
                // 光标（块/竖线）
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

    /// IME 挂接画布（父视图叠在编辑框上；顺带捕获几何供视口换算）。
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

    // ---- 鼠标点击定位 / 拖选 ----

    /// 按下：点击定位光标；normal 态进入可视拖选，insert 态仅移动光标。
    pub fn mouse_down(&mut self, pos: gpui::Point<gpui::Pixels>) {
        let Some(off) = self.offset_at_point(pos) else {
            return;
        };
        if self.is_inserting() {
            self.place_cursor(off);
            return;
        }
        self.place_cursor(off);
        self.enter_visual_char();
        self.mouse_selecting = true;
    }

    /// 拖动：光标跟随（选区由引擎可视锚点延伸）。
    pub fn mouse_drag(&mut self, pos: gpui::Point<gpui::Pixels>) {
        if !self.mouse_selecting {
            return;
        }
        if let Some(off) = self.offset_at_point(pos) {
            self.place_cursor(off);
        }
    }

    /// 抬起：结束拖选；原地点击（选区为空）退回 normal。
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

    /// 引擎按键流进入字符可视（与宿主 pager 的 enter_visual 同法）。
    fn enter_visual_char(&mut self) {
        self.host.viewport = (0, self.visible_rows.saturating_sub(1));
        let mut ctx = Ctx {
            buf: &mut self.buf,
            host: &mut self.host,
        };
        self.vim.handle_key(&mut ctx, gpui_vim_core::key::Key::char('v'));
    }

    /// 窗口坐标 → 扁平偏移。行 = 视口行 + 滚动补偿；列 = 等宽字宽估算。
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

/// 供测试/宿主构造。
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
}

// ---- IME / text-input 协议 ----
//
// 引擎持有光标；所有变更在此转发给 VimState。UTF-16(gpui)/UTF-8(引擎)
// 边界只在这一层转换。

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
        // 空 marked range = IME 结束（setMarkedText 空串），报 None 避免
        // gpui 的 is_composing 永远为 true 把 Esc 都吞掉
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

        // 提交中的组合文本要替换 marked 区间，且不走按键流水线
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
        // 组合预览文本直接落盘并标记（不进命令流水线，不做 . 记录）
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
        let origin = Point::new(
            element_bounds.origin.x + gpui::px(x + 6.0),
            element_bounds.origin.y + gpui::px(row as f32 * self.style.line_height),
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
        let dy = point.y - area.origin.y;
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

/// VimEditor trait 实现（仅为复用 [`crate::dispatch_text`] 的 IME 文本路径；
/// 按键不通过 attach 拦截，由宿主 guard 路由）。
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
    use gpui_vim_core::key::Key;
    use gpui_vim_core::state::Ctx;

    /// 组装引擎 + 单行初始内容的 buffer（正文编辑器打开时的状态）。
    fn setup() -> (SharedLines, LinesBuf, EditHost, VimState) {
        let storage = SharedLines::default();
        storage.set(Arc::new(vec![String::new()]));
        let buf = LinesBuf(storage.clone());
        let host = EditHost::new(storage.clone());
        (storage, buf, host, VimState::new())
    }

    /// 回归：多行编辑器初始只有一行内容，insert 态 enter 被压成空格
    /// （旧的 lines.len()==1 内容启发式把多行编辑器误判为单行）。
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

    /// 回归：normal 态 o 无法新建一行（同样是内容启发式压掉 '\n'）。
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
        assert!(matches!(vim.mode, Mode::Insert), "o 应进入 insert 模式");
    }

    /// 回归：行尾是多字节字符（中文）时 enter 曾在 char_at→offset_rc
    /// 非边界切片处 panic（char_at 合同要求非边界返回 None）。
    #[test]
    fn cjk_line_tail_enter_no_panic() {
        let (_, mut buf, mut host, mut vim) = setup();
        buf.0.set(Arc::new(vec!["第一行收件人".to_string()]));
        let mut ctx = Ctx {
            buf: &mut buf,
            host: &mut host,
        };
        vim.handle_key(&mut ctx, Key::char('A')); // 行尾进入 insert
        assert_eq!(vim.handle_key(&mut ctx, Key::enter()), KeyResult::Consumed);
        assert_eq!(buf.0.get().len(), 2, "enter 应新建一行");
        assert_eq!(buf.0.get()[0], "第一行收件人");
    }

    /// 多行结构操作：dd 删除一行后剩余行保持独立。
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

    /// 单行字段（表单/头部字段）语义不变：enter 产生的换行在 buffer 里
    /// 短暂存在，flush 时由 collapse_single_line 并回一行。
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
        // collapse 与 VimEdit::text() 同规则：join(" ")
        let joined = buf.0.get().join(" ");
        assert_eq!(joined, "a b");
    }
}

#[cfg(test)]
mod tck_tests {
    //! 引擎 TCK（gpui_vim_core::tck）对 LinesBuf 的验收。
    use super::*;
    use gpui_vim_core::tck;

    /// LinesBuf 满足编辑契约与引擎冒烟（宿主侧用真实 EditHost）。
    #[test]
    fn lines_buf_meets_engine_contract() {
        let storage = SharedLines::default();
        let mut buf = LinesBuf(storage.clone());
        tck::buffer_edit_contract(&mut buf).unwrap();
        let mut host = EditHost::new(storage);
        tck::engine_smoke_contract(&mut buf, &mut host).unwrap();
    }

    /// 预填多字节与空行内容后只读契约仍成立。
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
