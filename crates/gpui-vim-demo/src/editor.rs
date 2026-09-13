//! The reference editor view: renders buffer + vim state, wires IME, mouse
//! and the clipboard. This is the file to copy when integrating the engine
//! into your own gpui app.

use std::cell::Cell;
use std::ops::Range;

use gpui::prelude::*;
use gpui::{
    actions, canvas, div, fill, px, rgba, Bounds, ClipboardItem, Context,
    ElementInputHandler, FocusHandle, Font, FontStyle, FontWeight, MouseDownEvent,
    MouseMoveEvent, Pixels, Point, Render, ScrollStrategy, SharedString, TextRun,
    UniformListScrollHandle, Window,
};
use vim_core::buffer::VimBuffer;
use vim_core::state::{Ctx, KeyResult, VimState};
use vim_core::{Mode, VimHost};

use crate::buffer::RopeBuffer;
use crate::host::HostState;

pub const FONT_FAMILY: &str = "Menlo";
pub const FONT_SIZE: f32 = 14.0;
pub const LINE_HEIGHT: f32 = 20.0;

fn cursor_color() -> gpui::Hsla {
    gpui::black()
}
fn selection_color() -> gpui::Hsla {
    gpui::rgba(0x3b82f655).into()
}
fn search_color() -> gpui::Hsla {
    gpui::rgba(0xf59e0b3d).into()
}
fn current_search_color() -> gpui::Hsla {
    gpui::rgba(0xef44444f).into()
}
fn mark_color() -> gpui::Hsla {
    gpui::rgba(0x10b981aa).into()
}
fn gutter_color() -> gpui::Hsla {
    gpui::rgba(0x9ca3afff).into()
}
fn gutter_active_color() -> gpui::Hsla {
    gpui::rgba(0x111827ff).into()
}
fn tilde_color() -> gpui::Hsla {
    gpui::rgba(0xd1d5dbff).into()
}
fn text_color() -> gpui::Hsla {
    gpui::rgba(0x1f2937ff).into()
}
fn bar_bg() -> gpui::Hsla {
    gpui::rgba(0x1f2937ff).into()
}
fn bar_text() -> gpui::Hsla {
    gpui::rgba(0xe5e7ebff).into()
}
fn accent() -> gpui::Hsla {
    gpui::rgba(0x3b82f6ff).into()
}

type SharedView = gpui::Entity<Editor>;

actions!(demo, [Save, Copy, Paste]);

/// Overlay geometry for one line, computed at render time (byte columns) and
/// painted at paint time (via the shaped line).
#[derive(Clone)]
struct LineOverlays {
    /// (byte range within line, color, full_width)
    quads: Vec<(Range<usize>, gpui::Hsla, bool)>,
    /// Cursor column in bytes, if this is the cursor line.
    cursor: Option<usize>,
    cursor_block: bool,
}

pub struct Editor {
    buffer: RopeBuffer,
    host: HostState,
    vim: VimState,
    pub focus_handle: FocusHandle,
    scroll_handle: UniformListScrollHandle,
    /// UTF-8 byte range of IME marked (composing) text.
    marked_range: Option<Range<usize>>,
    /// Visible line range, updated by the uniform list (without notify).
    visible_lines: Cell<(usize, usize)>,
    /// Bounds of the text area in window coordinates (mouse + IME mapping).
    text_area_bounds: Cell<Bounds<Pixels>>,
    /// Width of one monospace character, measured each frame.
    char_width: Cell<f32>,
    dragging: Cell<bool>,
    status_message: Option<String>,
    /// Keeps the keystroke interceptor alive. `gpui::Subscription` detaches
    /// on drop, so it must outlive the engine's use — storing it in the view
    /// (instead of a local in `main`) is what makes the engine keep working
    /// after window setup.
    _vim_subscription: Option<gpui::Subscription>,
}

pub fn font() -> Font {
    Font {
        family: SharedString::from(FONT_FAMILY),
        features: Default::default(),
        fallbacks: None,
        weight: FontWeight::NORMAL,
        style: FontStyle::Normal,
    }
}

impl Editor {
    /// Full buffer text (for debugging/tests).
    pub fn text(&self) -> String {
        self.buffer.text()
    }

    pub fn new(initial_text: &str, cx: &mut Context<Self>) -> Self {
        let buffer = RopeBuffer::new(initial_text);
        let host = HostState::new(buffer.shared().clone());
        Editor {
            buffer,
            host,
            vim: VimState::new(),
            focus_handle: cx.focus_handle(),
            scroll_handle: UniformListScrollHandle::new(),
            marked_range: None,
            visible_lines: Cell::new((0, 24)),
            text_area_bounds: Cell::new(Bounds::default()),
            char_width: Cell::new(8.4),
            dragging: Cell::new(false),
            status_message: None,
            _vim_subscription: None,
        }
    }

    /// Store the `attach()` subscription on the view (see field docs).
    pub fn set_vim_subscription(&mut self, subscription: gpui::Subscription) {
        self._vim_subscription = Some(subscription);
    }

    // ---- read helpers --------------------------------------------------------

    fn mode_label(&self) -> String {
        match self.vim.mode() {
            Mode::Normal => "NORMAL".to_owned(),
            Mode::Insert => "-- INSERT --".to_owned(),
            Mode::Replace => "-- REPLACE --".to_owned(),
            Mode::Visual { kind } => format!("-- {} --", kind.indicator()),
            Mode::CommandLine { prompt, .. } => {
                format!("SEARCH {}{}", prompt, self.vim.cmdline.buffer)
            }
        }
    }

    fn line_col(&self) -> (usize, usize) {
        let line = self.buffer.offset_to_line(self.vim.cursor_offset());
        let col = self.vim.cursor_offset() - self.buffer.line_start(line);
        (line + 1, col)
    }

    /// Normalized visual-selection span (bytes) for rendering, if any.
    fn selection_span(&self) -> Option<(Range<usize>, bool)> {
        let (anchor, cursor, kind) = self.vim.visual_selection()?;
        let (lo, hi) = if anchor <= cursor { (anchor, cursor) } else { (cursor, anchor) };
        let linewise = kind == vim_core::VisualKind::Line;
        let range = if linewise {
            let start = self.buffer.line_start(self.buffer.offset_to_line(lo));
            let end = self.buffer.line_range(self.buffer.offset_to_line(hi)).end;
            start..end
        } else {
            let end = hi + self.buffer.char_at(hi).map(|c| c.len_utf8()).unwrap_or(0);
            lo..end
        };
        Some((range, linewise))
    }

    fn gutter_cols(&self) -> usize {
        5
    }

    fn gutter_width(&self) -> Pixels {
        px(self.gutter_cols() as f32 * self.char_width.get())
    }

    // ---- mouse ---------------------------------------------------------------

    fn byte_at_point(&self, position: Point<Pixels>) -> usize {
        let area = self.text_area_bounds.get();
        let dy = f32::from(position.y - area.origin.y);
        let dx = f32::from(position.x - area.origin.x - self.gutter_width());
        let row = (dy / LINE_HEIGHT).floor().max(0.0) as usize;
        let line = (self.visible_lines.get().0 + row).min(self.buffer.line_count() - 1);
        let line_start = self.buffer.line_start(line);
        let line_end = self.buffer.line_end(line);
        let col = (dx / self.char_width.get()).round().max(0.0) as usize;
        let mut offset = line_start;
        let mut visual = 0usize;
        while offset < line_end && visual < col {
            match self.buffer.next_char_offset(offset) {
                Some(next) if next <= line_end => offset = next,
                _ => break,
            }
            visual += 1;
        }
        offset.min(line_end)
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
        let offset = self.byte_at_point(event.position);
        self.vim.set_cursor_offset(&self.buffer, offset);
        self.dragging.set(true);
        self.host.scrolled_to = None;
        cx.notify();
    }

    fn on_mouse_drag(&mut self, event: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.dragging.get() {
            return;
        }
        let offset = self.byte_at_point(event.position);
        let anchor = self.vim.cursor_offset();
        self.vim.set_visual_range(&self.buffer, anchor, offset);
        cx.notify();
    }

    fn on_mouse_up(&mut self, _event: &gpui::MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.dragging.set(false);
        cx.notify();
    }

    // ---- post-key flushes ------------------------------------------------------

    fn flush_clipboard(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.host.pending_clipboard_write.take() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn flush_scroll(&mut self) {
        if let Some(line) = self.host.scrolled_to.take() {
            let (first, last) = self.visible_lines.get();
            if line < first || line > last {
                self.scroll_handle.scroll_to_item(line, ScrollStrategy::Center);
            }
        }
    }

    // ---- actions -----------------------------------------------------------------

    pub fn save(&mut self, _action: &Save, _window: &mut Window, cx: &mut Context<Self>) {
        self.status_message = Some(format!(
            "\"untitled\" {}L, {}B written (demo: not persisted)",
            self.buffer.line_count(),
            self.buffer.len()
        ));
        cx.notify();
    }

    pub fn copy(&mut self, _action: &Copy, _window: &mut Window, cx: &mut Context<Self>) {
        let text = self
            .selection_span()
            .map(|(range, _)| self.buffer.slice(range))
            .unwrap_or_default();
        if !text.is_empty() {
            let len = text.len();
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            self.status_message = Some(format!("copied {len} bytes"));
        }
        cx.notify();
    }

    pub fn paste(&mut self, _action: &Paste, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let Some(text) = item.text() else { return };
        let (vim, buf, host) = gpui_vim::VimEditor::vim_parts(self);
        let mut ctx = Ctx { buf, host };
        vim.insert_text_at_cursor(&mut ctx, &text);
        cx.notify();
    }
}

impl gpui_vim::VimEditor for Editor {
    fn vim_parts(&mut self) -> (&mut VimState, &mut dyn vim_core::buffer::VimBufferMut, &mut dyn VimHost) {
        (&mut self.vim, &mut self.buffer, &mut self.host)
    }

    fn vim_accepts_keys(&self, window: &Window, cx: &gpui::App) -> bool {
        self.focus_handle.contains_focused(window, cx)
    }

    fn vim_did_process_key(
        &mut self,
        _result: KeyResult,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.flush_clipboard(cx);
        self.flush_scroll();
        cx.notify();
    }
}

// ---- rendering -------------------------------------------------------------------

pub const SAMPLE: &str = "// gpui-vim demo — modal editing for gpui apps\n\
fn main() {\n    let hello = \"vim in gpui\";\n    println!(\"{hello}\");\n}\n\
\n\
Try:  hjkl  w b e  0 ^ $  gg G  dd  dw  cw  ciw  3dw\n\
      yy p  v e d  V j d  viw y  guu  g~~  >>  <<  zz\n\
      /print <CR>  n N  *  ma `a  u <C-r>  x r J p P ~\n\
\n";

impl Render for Editor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // focus recovery: claim focus whenever the window is active but the
        // editor handle is not focused (e.g. after app activation)
        if window.is_window_active() && !self.focus_handle.contains_focused(window, cx) {
            window.focus(&self.focus_handle);
        }
        let view = cx.entity();
        div()
            .id("editor-root")
            .size_full()
            .flex()
            .flex_col()
            .font_family(FONT_FAMILY)
            .text_size(px(FONT_SIZE))
            .line_height(px(LINE_HEIGHT))
            .bg::<gpui::Hsla>(gpui::Hsla::from(rgba(0xfffdf6ff)))
            .text_color(text_color())
            .track_focus(&self.focus_handle)
            .key_context(gpui_vim::key_context(self.vim.mode()))
            .on_action(cx.listener(Editor::save))
            .on_action(cx.listener(Editor::copy))
            .on_action(cx.listener(Editor::paste))
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(Editor::on_mouse_down))
            .on_mouse_move(cx.listener(Editor::on_mouse_drag))
            .on_mouse_up(gpui::MouseButton::Left, cx.listener(Editor::on_mouse_up))
            .child(self.render_text_area(view))
            .child(self.render_status_bar())
    }
}

impl Editor {
    fn render_text_area(&self, view: SharedView) -> impl IntoElement {
        let line_count = self.buffer.line_count().max(24);
        let focus = self.focus_handle.clone();
        let view2 = view.clone();
        div()
            .id("text-area")
            .flex_1()
            .relative()
            .overflow_hidden()
            .child(
                gpui::uniform_list("lines", line_count, {
                    let view = view.clone();
                    move |visible, _window, cx| {
                        view.update(cx, |editor, _| {
                            editor
                                .visible_lines
                                .set((visible.start, visible.end.saturating_sub(1)));
                        });
                        let editor = view.read(cx);
                        visible
                            .clone()
                            .map(|line| editor.render_line(line))
                            .collect::<Vec<_>>()
                    }
                })
                .flex_1()
                .size_full()
                .track_scroll(self.scroll_handle.clone()),
            )
            .child(
                // paint-only surface: IME plumbing + geometry capture
                canvas(
                    move |bounds, _window, _cx| bounds,
                    move |bounds, _, window, cx| {
                        let view = view2.clone();
                        // capture geometry for mouse/IME mapping (no notify)
                        view.update(cx, |editor, _| {
                            editor.text_area_bounds.set(bounds);
                        });
                        window.handle_input(
                            &focus,
                            ElementInputHandler::new(bounds, view.clone()),
                            cx,
                        );
                    },
                )
                .absolute()
                .size_full(),
            )
    }

    fn gutter_text(&self, line: usize) -> String {
        let current = self.buffer.offset_to_line(self.vim.cursor_offset());
        let label = if self.vim.options.relativenumber && line != current {
            line.abs_diff(current).to_string()
        } else {
            (line + 1).to_string()
        };
        format!("{label:>width$} ", width = self.gutter_cols() - 1)
    }

    /// Build the paint-time overlay set for `line` from the engine state.
    fn overlays_for_line(&self, line: usize) -> LineOverlays {
        let line_start = self.buffer.line_start(line);
        let line_end = self.buffer.line_end(line);
        let mut quads: Vec<(Range<usize>, gpui::Hsla, bool)> = Vec::new();

        // search highlights
        if self.vim.options.hlsearch || !self.host.highlights.is_empty() {
            for highlight in &self.host.highlights {
                let is_current = self.host.current_highlight.as_ref() == Some(highlight);
                let color = if is_current {
                    current_search_color()
                } else {
                    search_color()
                };
                let start = highlight.start.clamp(line_start, line_end) - line_start;
                let end = highlight.end.clamp(line_start, line_end) - line_start;
                if start < end {
                    quads.push((start..end, color, false));
                }
            }
        }

        // IME marked text
        if let Some(marked) = &self.marked_range {
            let start = marked.start.clamp(line_start, line_end) - line_start;
            let end = marked.end.clamp(line_start, line_end) - line_start;
            if start < end {
                quads.push((start..end, mark_color(), false));
            }
        }

        // visual selection
        if let Some((selection, linewise)) = self.selection_span() {
            if linewise {
                quads.push((0..0, selection_color(), true));
            } else {
                let start = selection.start.clamp(line_start, line_end) - line_start;
                let end = selection.end.clamp(line_start, line_end) - line_start;
                if start < end {
                    quads.push((start..end, selection_color(), false));
                }
            }
        }

        // cursor
        let cursor_line = self.buffer.offset_to_line(self.vim.cursor_offset());
        let cursor = if line == cursor_line {
            let block = self.vim.cursor_is_block();
            Some(if block {
                self.vim.cursor_offset() - line_start
            } else {
                // bar cursor sits between characters
                self.vim.cursor_offset() - line_start
            })
        } else {
            None
        };

        LineOverlays {
            quads,
            cursor,
            cursor_block: self.vim.cursor_is_block(),
        }
    }

    fn render_line(&self, line: usize) -> impl IntoElement {
        let in_range = line < self.buffer.line_count();
        let text = if in_range {
            SharedString::from(self.buffer.line_content(line))
        } else {
            SharedString::from("~".to_owned())
        };
        let gutter = if in_range {
            self.gutter_text(line)
        } else {
            format!("{:>width$} ", "~", width = self.gutter_cols() - 1)
        };
        let gutter_active = in_range && line == self.buffer.offset_to_line(self.vim.cursor_offset());
        let overlays = if in_range {
            self.overlays_for_line(line)
        } else {
            LineOverlays {
                quads: Vec::new(),
                cursor: None,
                cursor_block: true,
            }
        };

        div()
            .id(("line", line as u64))
            .flex()
            .flex_row()
            .h(px(LINE_HEIGHT))
            .child(
                div()
                    .w(self.gutter_width())
                    .text_color(if gutter_active { gutter_active_color() } else { gutter_color() })
                    .child(gutter),
            )
            .child(
                div()
                    .flex_1()
                    .relative()
                    .text_color(if in_range { text_color() } else { tilde_color() })
                    .child(canvas(
                        move |bounds, _window, _cx| bounds,
                        move |bounds, _, window, cx| {
                            let runs = [TextRun {
                                len: text.len(),
                                font: font(),
                                color: text_color(),
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            }];
                            let shaped = window
                                .text_system()
                                .shape_line(text.clone(), px(FONT_SIZE), &runs, None);

                            // overlays under the text
                            for (byte_range, color, full_width) in &overlays.quads {
                                let x0 = if *full_width {
                                    0.0
                                } else {
                                    f32::from(shaped.x_for_index(byte_range.start))
                                };
                                let x1 = if *full_width {
                                    f32::from(bounds.size.width)
                                } else {
                                    f32::from(shaped.x_for_index(byte_range.end))
                                };
                                if x1 > x0 {
                                    let quad_bounds = Bounds::from_corners(
                                        point(bounds.origin.x + px(x0), bounds.origin.y),
                                        point(
                                            bounds.origin.x + px(x1),
                                            bounds.origin.y + px(LINE_HEIGHT),
                                        ),
                                    );
                                    window.paint_quad(fill(quad_bounds, *color));
                                }
                            }

                            // cursor
                            if let Some(cursor_byte) = overlays.cursor {
                                let x = f32::from(shaped.x_for_index(cursor_byte));
                                let width = if overlays.cursor_block {
                                    let next = f32::from(
                                        shaped.x_for_index((cursor_byte + 1).min(text.len())),
                                    ) - x;
                                    if next > 0.5 { next } else { 8.4 }
                                } else {
                                    2.0
                                };
                                let quad_bounds = Bounds::from_corners(
                                    point(bounds.origin.x + px(x), bounds.origin.y),
                                    point(
                                        bounds.origin.x + px(x + width),
                                        bounds.origin.y + px(LINE_HEIGHT),
                                    ),
                                );
                                window.paint_quad(fill(quad_bounds, cursor_color()));
                            }

                            let _ = shaped.paint(
                                point(bounds.origin.x, bounds.origin.y),
                                px(LINE_HEIGHT),
                                window,
                                cx,
                            );
                        },
                    )),
            )
    }

    fn render_status_bar(&self) -> impl IntoElement {
        let (line, col) = self.line_col();
        let mode = self.mode_label();
        let showcmd = self.vim.showcmd();
        div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(LINE_HEIGHT * 1.5))
            .px_2()
            .gap_3()
            .bg(bar_bg())
            .text_color(bar_text())
            .text_size(px(12.0))
            .child(
                div()
                    .px_2()
                    .rounded_sm()
                    .bg(accent())
                    .text_color(gpui::Hsla::from(rgba(0xffffffff)))
                    .child(mode),
            )
            .when(!showcmd.is_empty(), |bar| {
                bar.child(div().child(format!("pending: {showcmd}")))
            })
            .child(div().flex_1())
            .when_some(self.status_message.clone(), |bar, message| {
                bar.child(div().text_color(gpui::yellow()).child(message))
            })
            .child(div().child(format!("{line}:{col}")))
    }
}

fn point(x: Pixels, y: Pixels) -> Point<Pixels> {
    Point::new(x, y)
}


// ---- IME / text-input protocol -------------------------------------------------
//
// The engine owns the cursor, so every mutation arrives here first and is
// forwarded to `VimState`. Offsets cross the UTF-16 (gpui) / UTF-8 (engine)
// boundary exactly here.

impl gpui::EntityInputHandler for Editor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let start = self.buffer.utf16_to_byte(range_utf16.start);
        let end = self.buffer.utf16_to_byte(range_utf16.end);
        let text = self.buffer.slice(start..end);
        adjusted_range.replace(start..end);
        Some(text)
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<gpui::UTF16Selection> {
        let cursor = self.buffer.byte_to_utf16(self.vim.cursor_offset());
        let selection = self
            .selection_span()
            .map(|(range, _)| {
                let start = self.buffer.byte_to_utf16(range.start);
                let end = self.buffer.byte_to_utf16(range.end);
                start..end
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
        self.marked_range
            .as_ref()
            .map(|range| {
                let start = self.buffer.byte_to_utf16(range.start);
                let end = self.buffer.byte_to_utf16(range.end);
                start..end
            })
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // An explicit non-empty range = the IME editing its own composition;
        // route it as a direct replacement. Otherwise the text is ordinary
        // typing: feed it through the engine pipeline (normal-mode commands,
        // `jk` mappings, search prompts all arrive here on macOS).
        let concrete = self.buffer.clone();
        let explicit_range = range.and_then(|r| {
            let start = concrete.utf16_to_byte(r.start);
            let end = concrete.utf16_to_byte(r.end);
            (start < end).then_some(start..end)
        });

        if let Some(range) = explicit_range {
            let (vim, buf, host) = gpui_vim::VimEditor::vim_parts(self);
            if matches!(vim.mode(), Mode::Insert | Mode::Replace) {
                let mut ctx = Ctx { buf, host };
                vim.replace_range(&mut ctx, range, text);
            }
        } else {
            gpui_vim::dispatch_text(self, text);
        }

        let (vim, _buf, _host) = gpui_vim::VimEditor::vim_parts(self);
        let cursor_line = _buf.offset_to_line(vim.cursor_offset());
        self.host.scrolled_to = Some(cursor_line);
        self.flush_scroll();
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
        // IME composition (e.g. pinyin): place the text directly and mark it,
        // bypassing the command pipeline — composing text is not commands.
        if !matches!(self.vim.mode(), Mode::Insert | Mode::Replace) {
            return;
        }
        if let Some(previous) = self.marked_range.take() {
            let (vim, buf, host) = gpui_vim::VimEditor::vim_parts(self);
            let mut ctx = Ctx { buf, host };
            vim.replace_range(&mut ctx, previous.clone(), new_text);
            let end = previous.start + new_text.len();
            self.marked_range = Some(previous.start..end);
        } else {
            let (vim, buf, host) = gpui_vim::VimEditor::vim_parts(self);
            let mut ctx = Ctx { buf, host };
            let start = vim.cursor_offset();
            vim.insert_text_at_cursor(&mut ctx, new_text);
            self.marked_range = Some(start..start + new_text.len());
        }
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let byte = self.buffer.utf16_to_byte(range_utf16.start);
        let line = self.buffer.offset_to_line(byte);
        let row = line.saturating_sub(self.visible_lines.get().0);
        let col = byte - self.buffer.line_start(line);
        let x = f32::from(self.gutter_width()) + col as f32 * self.char_width.get();
        let origin = Point::new(
            element_bounds.origin.x + px(x),
            element_bounds.origin.y + px(row as f32 * LINE_HEIGHT),
        );
        Some(Bounds::new(
            origin,
            gpui::Size::new(px(self.char_width.get()), px(LINE_HEIGHT)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let byte = self.byte_at_point(point);
        Some(self.buffer.byte_to_utf16(byte))
    }
}
