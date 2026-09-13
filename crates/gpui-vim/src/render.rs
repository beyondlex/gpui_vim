//! Reusable vim-mode line rendering for gpui hosts: overlay computation,
//! line painting (with vim-style cursor inversion) and caret blinking.
//!
//! Hosts embed a [`VimState`], render their own buffer lines, and call
//! [`compute_line_overlays`] + [`paint_vim_line`] per visible line. The
//! shaped line comes back for host-side hit testing (mouse, IME rects).

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui::{px, App, Bounds, Context, Hsla, Pixels, Point, SharedString, TextRun, Window};
use vim_core::buffer::{display_column, VimBuffer};
use vim_core::ops::block_row_range;
use vim_core::mode::VisualKind;
use vim_core::state::VimState;

/// Colors and typography for line rendering.
#[derive(Clone, Debug)]
pub struct OverlayStyle {
    pub font: gpui::Font,
    pub font_size: Pixels,
    pub text: Hsla,
    /// Background behind the text: the block cursor inverts its char.
    pub background: Hsla,
    pub cursor: Hsla,
    pub selection: Hsla,
    pub search: Hsla,
    pub search_current: Hsla,
    pub mark: Hsla,
    /// Fallback caret width when the char under the cursor has no advance
    /// (end of line). ASCII cell width is a good value.
    pub caret_fallback_width: f32,
}

/// Quads and caret geometry for one rendered line, in line-local byte
/// coordinates (subtract the line's byte start before painting).
#[derive(Debug, Default)]
pub struct LineOverlays {
    /// (byte range within the line, color, spans the full line width)
    pub quads: Vec<(std::ops::Range<usize>, Hsla, bool)>,
    /// Byte column of the caret within the line, if this is the caret line.
    pub cursor: Option<usize>,
    pub cursor_block: bool,
}

/// Everything [`compute_line_overlays`] needs for one line.
pub struct LineOverlayInputs<'a> {
    pub vim: &'a VimState,
    pub buf: &'a dyn VimBuffer,
    pub line: usize,
    /// Active search highlights (host-published), buffer byte ranges.
    pub search_highlights: &'a [std::ops::Range<usize>],
    /// The match the cursor sits on, rendered with [`OverlayStyle::search_current`].
    pub search_current: Option<std::ops::Range<usize>>,
    /// IME composition range, if composing.
    pub ime_marked: Option<std::ops::Range<usize>>,
    /// Blink phase: false hides the caret without touching the engine.
    pub caret_visible: bool,
    pub style: &'a OverlayStyle,
}

/// Compute the overlay quads and caret geometry for one line. The line must
/// exist in the buffer; hosts render `~` placeholders themselves.
pub fn compute_line_overlays(inputs: &LineOverlayInputs) -> LineOverlays {
    let LineOverlayInputs {
        vim,
        buf,
        line,
        search_highlights,
        search_current,
        ime_marked,
        caret_visible,
        style,
    } = inputs;
    let buf: &dyn VimBuffer = *buf;
    let line = *line;
    let caret_visible = *caret_visible;
    let search_current = search_current.clone();
    let ime_marked = ime_marked.clone();
    let line_start = buf.line_start(line);
    let line_end = buf.line_end(line);
    let mut quads: Vec<(std::ops::Range<usize>, Hsla, bool)> = Vec::new();

    // search highlights
    if vim.options.hlsearch || !search_highlights.is_empty() {
        for highlight in *search_highlights {
            let is_current = search_current.as_ref() == Some(highlight);
            let color = if is_current {
                style.search_current
            } else {
                style.search
            };
            let start = highlight.start.clamp(line_start, line_end) - line_start;
            let end = highlight.end.clamp(line_start, line_end) - line_start;
            if start < end {
                quads.push((start..end, color, false));
            }
        }
    }

    // IME marked text
    if let Some(marked) = ime_marked {
        let start = marked.start.clamp(line_start, line_end) - line_start;
        let end = marked.end.clamp(line_start, line_end) - line_start;
        if start < end {
            quads.push((start..end, style.mark, false));
        }
    }

    // visual selection (char / line / block)
    if let Some((anchor, cursor, kind)) = vim.visual_selection() {
        match kind {
            VisualKind::Block => {
                let first_line = buf.offset_to_line(anchor.min(cursor));
                let last_line = buf.offset_to_line(anchor.max(cursor));
                if line >= first_line && line <= last_line {
                    let a_col = display_column(buf, anchor);
                    let c_col = display_column(buf, cursor);
                    let (col_lo, col_hi) = if a_col <= c_col { (a_col, c_col) } else { (c_col, a_col) };
                    // the cursor char is part of the block
                    let range = block_row_range(buf, line, col_lo, col_hi + 1);
                    if !range.is_empty() {
                        quads.push((
                            range.start - line_start..range.end - line_start,
                            style.selection,
                            false,
                        ));
                    }
                }
            }
            VisualKind::Line => {
                let selection = {
                    let lo = anchor.min(cursor);
                    let hi = anchor.max(cursor);
                    let start = buf.line_start(buf.offset_to_line(lo));
                    let end = buf.line_range(buf.offset_to_line(hi)).end;
                    start..end
                };
                let first = buf.offset_to_line(selection.start);
                let last = buf
                    .offset_to_line(selection.end.saturating_sub(1).max(selection.start));
                if line >= first && line <= last {
                    quads.push((0..0, style.selection, true));
                }
            }
            VisualKind::Char => {
                let lo = anchor.min(cursor);
                let hi = anchor.max(cursor);
                let end = hi + buf.char_at(hi).map(|c| c.len_utf8()).unwrap_or(0);
                let start = lo.clamp(line_start, line_end) - line_start;
                let end = end.clamp(line_start, line_end) - line_start;
                if start < end {
                    quads.push((start..end, style.selection, false));
                }
            }
        }
    }

    // cursor
    let cursor_line = buf.offset_to_line(vim.cursor_offset());
    let cursor = if line == cursor_line && caret_visible {
        Some(vim.cursor_offset() - line_start)
    } else {
        None
    };

    LineOverlays {
        quads,
        cursor,
        cursor_block: vim.cursor_is_block(),
    }
}

/// Shape the line (splitting runs so the block cursor inverts its char) and
/// paint overlay quads under the text. Returns the shaped line for host-side
/// hit testing (mouse, IME rects).
pub fn paint_vim_line(
    window: &mut Window,
    cx: &mut App,
    text: SharedString,
    bounds: Bounds<Pixels>,
    line_height: Pixels,
    overlays: &LineOverlays,
    style: &OverlayStyle,
) -> gpui::ShapedLine {
    // vim-style invert: the character under the block cursor is painted in
    // the background color so it reads through the solid cursor block
    let inverted = overlays
        .cursor
        .filter(|_| overlays.cursor_block)
        .and_then(|at| {
            text.get(at..)
                .and_then(|rest| rest.chars().next())
                .map(|c| at..at + c.len_utf8())
        });
    let mut runs: Vec<TextRun> = Vec::new();
    let mut push_run = |len: usize, color: Hsla| {
        if len > 0 {
            runs.push(TextRun {
                len,
                font: style.font.clone(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            });
        }
    };
    match inverted {
        Some(range) => {
            push_run(range.start, style.text);
            push_run(range.len(), style.background);
            push_run(text.len() - range.end, style.text);
        }
        None => push_run(text.len(), style.text),
    }
    let shaped = window
        .text_system()
        .shape_line(text.clone(), style.font_size, &runs, None);

    // overlay quads under the text
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
            let quad = Bounds::from_corners(
                point(bounds.origin.x + px(x0), bounds.origin.y),
                point(bounds.origin.x + px(x1), bounds.origin.y + line_height),
            );
            window.paint_quad(gpui::fill(quad, *color));
        }
    }

    // caret
    if let Some(cursor_byte) = overlays.cursor {
        let x = f32::from(shaped.x_for_index(cursor_byte));
        let width = if overlays.cursor_block {
            // advance to the next char boundary — byte+1 is mid-char for
            // multi-byte characters (x_for_index rounds up, which would
            // give a zero-width quad)
            let end_byte = text
                .get(cursor_byte..)
                .and_then(|rest| rest.chars().next())
                .map(|c| (cursor_byte + c.len_utf8()).min(text.len()))
                .unwrap_or(cursor_byte);
            let next = f32::from(shaped.x_for_index(end_byte)) - x;
            if next > 0.5 {
                next
            } else {
                style.caret_fallback_width
            }
        } else {
            2.0
        };
        let quad = Bounds::from_corners(
            point(bounds.origin.x + px(x), bounds.origin.y),
            point(bounds.origin.x + px(x + width), bounds.origin.y + line_height),
        );
        window.paint_quad(gpui::fill(quad, style.cursor));
    }

    let _ = shaped.paint(point(bounds.origin.x, bounds.origin.y), line_height, window, cx);
    shaped
}

fn point(x: Pixels, y: Pixels) -> Point<Pixels> {
    Point::new(x, y)
}

/// Caret blink state with a 500ms toggle loop. Input keeps the caret solid
/// and restarts the phase (`note_activity` on every user interaction).
pub struct CaretBlinker {
    visible: Cell<bool>,
    phase_reset: Cell<bool>,
}

impl CaretBlinker {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            visible: Cell::new(true),
            phase_reset: Cell::new(false),
        })
    }

    /// Keep the caret visible and restart the blink phase (on user input).
    pub fn note_activity(&self) {
        self.phase_reset.set(true);
        self.visible.set(true);
    }

    pub fn is_visible(&self) -> bool {
        self.visible.get()
    }

    fn tick(&self) {
        if self.phase_reset.take() {
            self.visible.set(true);
        } else {
            self.visible.set(!self.visible.get());
        }
    }

    /// Spawn the blink loop on the host entity: toggles every 500ms and
    /// notifies the entity so it repaints. Ends when the entity is dropped.
    pub fn spawn_loop<E: 'static>(self: &Rc<Self>, cx: &mut Context<E>) {
        let blinker = Rc::clone(self);
        cx.spawn(async move |entity, cx| {
            loop {
                gpui::Timer::after(Duration::from_millis(500)).await;
                let alive = entity
                    .update(cx, |_, cx| {
                        blinker.tick();
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }
}
