//! The reference editor view: renders buffer + vim state, wires IME, mouse
//! and the clipboard. This is the file to copy when integrating the engine
//! into your own gpui app.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::Range;

use gpui::prelude::*;
use gpui::{
    actions, canvas, div, px, rgba, Bounds, ClipboardItem, Context, ElementInputHandler,
    FocusHandle, Font, FontStyle, FontWeight, MouseDownEvent, MouseMoveEvent, Pixels, Point,
    Render, ScrollStrategy, SharedString, UniformListScrollHandle, Window,
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
fn editor_bg() -> gpui::Hsla {
    gpui::rgba(0xfffdf6ff).into()
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
    /// Shaped geometry per line number, captured at paint time: the line's
    /// text origin X and the shaped line. Gives the mouse and the IME rect
    /// exact positions for wide/clustered glyphs (a uniform cell width is
    /// wrong for CJK and emoji).
    shaped_lines: RefCell<HashMap<usize, (Pixels, gpui::ShapedLine)>>,
    dragging: Cell<bool>,
    /// Caret blink state (library helper; the engine owns no timers).
    caret_blinker: std::rc::Rc<gpui_vim::render::CaretBlinker>,
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
        let editor = Editor {
            buffer,
            host,
            vim: VimState::new(),
            focus_handle: cx.focus_handle(),
            scroll_handle: UniformListScrollHandle::new(),
            marked_range: None,
            visible_lines: Cell::new((0, 24)),
            text_area_bounds: Cell::new(Bounds::default()),
            char_width: Cell::new(8.4),
            shaped_lines: RefCell::new(HashMap::new()),
            dragging: Cell::new(false),
            caret_blinker: gpui_vim::render::CaretBlinker::new(),
            status_message: None,
            _vim_subscription: None,
        };
        editor.spawn_blink_loop(cx);
        editor
    }

    /// Toggle the caret every 500ms; input keeps it solid (see `mark_caret_activity`).
    fn spawn_blink_loop(&self, cx: &mut Context<Self>) {
        self.caret_blinker.spawn_loop(cx);
    }

    /// Keep the caret visible and restart the blink phase (on any user input).
    fn mark_caret_activity(&self) {
        self.caret_blinker.note_activity();
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
            Mode::CommandLine { prompt, .. } => match prompt {
                ':' => format!(":{}", self.vim.cmdline.buffer),
                other => format!("SEARCH {}{}", other, self.vim.cmdline.buffer),
            },
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

        // Prefer the shaped geometry captured at paint time: exact hit
        // testing for CJK/emoji (a uniform cell width is off by 2x there).
        if let Some((_, shaped)) = self.shaped_lines.borrow().get(&line) {
            // dx is already relative to the line's text start (the line
            // canvas origin is area.origin.x + gutter)
            let index = shaped.closest_index_for_x(px(dx));
            let offset = line_start + index.min(line_end - line_start);
            return offset.min(line_end);
        }

        // fallback for lines that were not painted yet
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
        self.mark_caret_activity();
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

    /// `:w` status text and `:q` close requests arrive through the host.
    fn flush_host_effects(&mut self, cx: &mut Context<Self>) {
        if let Some(status) = self.host.pending_status.take() {
            self.status_message = Some(status);
        }
        if self.host.pending_close {
            self.host.pending_close = false;
            cx.quit();
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
        self.mark_caret_activity();
        self.flush_clipboard(cx);
        self.flush_scroll();
        self.flush_host_effects(cx);
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
                        visible
                            .clone()
                            .map(|line| {
                                view.read(cx).render_line(line, view.clone())
                            })
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
                // absolute + auto insets would sit at the element's
                // STATIC position — after the uniform_list — and every
                // mouse row would map back to the first line
                .absolute()
                .inset_0()
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

    /// Per-line style from the demo palette.
    fn overlay_style(&self) -> gpui_vim::render::OverlayStyle {
        gpui_vim::render::OverlayStyle {
            font: font(),
            font_size: px(FONT_SIZE),
            text: text_color(),
            background: editor_bg(),
            cursor: cursor_color(),
            selection: selection_color(),
            search: search_color(),
            search_current: current_search_color(),
            mark: mark_color(),
            caret_fallback_width: 8.4,
        }
    }

    /// Build the paint-time overlay set for `line` (delegates to the library).
    fn overlays_for_line(&self, line: usize) -> gpui_vim::render::LineOverlays {
        if line >= self.buffer.line_count() {
            return gpui_vim::render::LineOverlays::default();
        }
        gpui_vim::render::compute_line_overlays(&gpui_vim::render::LineOverlayInputs {
            vim: &self.vim,
            buf: &self.buffer,
            line,
            search_highlights: &self.host.highlights,
            search_current: self.host.current_highlight.clone(),
            ime_marked: self.marked_range.clone(),
            caret_visible: self.caret_blinker.is_visible(),
            style: &self.overlay_style(),
        })
    }

    fn render_line(&self, line: usize, view: SharedView) -> impl IntoElement {
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
        // out-of-range lines (`~` placeholders) carry no overlays; the
        // library computation handles the caret blink phase itself
        let style = self.overlay_style();
        let overlays = if in_range {
            self.overlays_for_line(line)
        } else {
            gpui_vim::render::LineOverlays::default()
        };

        div()
            .id(("line", line as u64))
            .flex()
            .flex_row()
            // uniform_list lays each item out with a definite width, but an
            // auto-width flex row shrinks to its content (just the gutter),
            // which would collapse the flex_1 text area to 0px — full-width
            // overlays (V-line selection) would never paint.
            .w_full()
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
                        {
                            let view = view.clone();
                            move |bounds, _, window, cx| {
                                let shaped = gpui_vim::render::paint_vim_line(
                                    window,
                                    cx,
                                    text.clone(),
                                    bounds,
                                    px(LINE_HEIGHT),
                                    &overlays,
                                    &style,
                                );
                                view.update(cx, |editor, _| {
                                    editor.shaped_lines.borrow_mut().insert(
                                        line,
                                        (bounds.origin.x, shaped.clone()),
                                    );
                                });
                            }
                        },
                    )
                    // canvas has no intrinsic size; without this its bounds
                    // are 0px wide and full-width overlays (V-line selection)
                    // never paint — x_for_index-based ones are unaffected
                    .absolute()
                    .size_full()),
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
            .when_some(self.vim.macro_recording(), |bar, register| {
                bar.child(
                    div()
                        .text_color(gpui::red())
                        .child(format!("recording @{}", register)),
                )
            })
            .when_some(self.status_message.clone(), |bar, message| {
                bar.child(div().text_color(gpui::yellow()).child(message))
            })
            .child(div().child(format!("{line}:{col}")))
    }
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
        let range = self.marked_range.as_ref()?;
        // An empty marked range means the composition has ended (the IME
        // cancels with an empty setMarkedText). Reporting it as Some would
        // keep gpui's `is_composing` true forever, routing every keystroke —
        // including Esc — to the IME instead of the engine.
        if range.is_empty() {
            return None;
        }
        Some(self.buffer.byte_to_utf16(range.start)..self.buffer.byte_to_utf16(range.end))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        // A direct unmark (commit tail, or a cancel path that skips the
        // empty setMarkedText) leaves the composition text in the buffer —
        // remove it so a discarded composition can't leak into the document.
        // After a commit the range is already gone (the commit replaced it),
        // so this is a no-op there.
        if let Some(range) = self.marked_range.take() {
            if range.start < range.end {
                let (vim, buf, host) = gpui_vim::VimEditor::vim_parts(self);
                let mut ctx = Ctx { buf, host };
                vim.replace_range(&mut ctx, range, "");
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

        // Committing a composition (`insertText:` while marked pinyin is on
        // screen) must REPLACE the marked range: the raw pinyin sitting there
        // is a preview, not document text. The committed text also bypasses
        // the key pipeline — dispatch_text here could fire insert-mode
        // mappings (e.g. `jk`) from composed characters.
        let committing = explicit_range.is_none()
            && !text.is_empty()
            && self.marked_range.as_ref().is_some_and(|r| !r.is_empty());
        if committing {
            let marked = self.marked_range.take().unwrap();
            let (vim, buf, host) = gpui_vim::VimEditor::vim_parts(self);
            vim.record_typed_text(text);
            let mut ctx = Ctx { buf, host };
            vim.replace_range(&mut ctx, marked, text);
        } else if let Some(range) = explicit_range {
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
        self.mark_caret_activity();
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
        // The raw pinyin is a preview: suppress `.` recording so only the
        // committed text (replace_text_in_range) becomes repeatable.
        if !matches!(self.vim.mode(), Mode::Insert | Mode::Replace) {
            return;
        }
        let (vim, _buf, _host) = gpui_vim::VimEditor::vim_parts(self);
        vim.set_recording_suppressed(true);
        if let Some(previous) = self.marked_range.take() {
            let (vim, buf, host) = gpui_vim::VimEditor::vim_parts(self);
            let mut ctx = Ctx { buf, host };
            vim.replace_range(&mut ctx, previous.clone(), new_text);
            // An empty new_text is the IME cancelling the composition: drop
            // the marker entirely (an empty Some would wedge gpui's
            // is_composing high and steal every later keystroke).
            let start = previous.start;
            let end = start + new_text.len();
            self.marked_range = (start < end).then_some(start..end);
        } else {
            let (vim, buf, host) = gpui_vim::VimEditor::vim_parts(self);
            let mut ctx = Ctx { buf, host };
            let start = vim.cursor_offset();
            vim.insert_text_at_cursor(&mut ctx, new_text);
            self.marked_range = Some(start..start + new_text.len());
        }
        let (vim, _buf, _host) = gpui_vim::VimEditor::vim_parts(self);
        vim.set_recording_suppressed(false);
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
        // shaped geometry gives the exact x for wide/clustered glyphs;
        // x_for_index is relative to the line text start (after the gutter)
        let within_line = self
            .shaped_lines
            .borrow()
            .get(&line)
            .map(|(_, shaped)| f32::from(shaped.x_for_index(col)))
            .unwrap_or(col as f32 * self.char_width.get());
        let x = f32::from(self.gutter_width()) + within_line;
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
