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
use vim_core::buffer::VimBuffer;
use vim_core::mode::VisualKind;
use vim_core::ops as core_ops;
use vim_core::state::VimState;

/// Width of the thin (insert-mode) caret bar, in logical pixels.
const CARET_BAR_WIDTH: f32 = 2.0;
/// Half-pixel threshold below which a computed caret width is treated as
/// "no advance available" and the fallback width is used instead
/// (`x_for_index` rounds, so a zero-advance char can yield ±fractional px).
const HALF_PX: f32 = 0.5;
/// Caret blink period.
const BLINK_INTERVAL: Duration = Duration::from_millis(500);

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

/// One overlay rectangle over the line.
#[derive(Debug)]
pub struct OverlayQuad {
    /// Byte range within the line.
    pub range: std::ops::Range<usize>,
    pub color: Hsla,
    /// True stretches the quad over the full line width (linewise
    /// selection), ignoring `range`'s shape.
    pub full_width: bool,
}

/// Quads and caret geometry for one rendered line, in line-local byte
/// coordinates (subtract the line's byte start before painting).
#[derive(Debug, Default)]
pub struct LineOverlays {
    pub quads: Vec<OverlayQuad>,
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

/// Clamp a buffer range to the line and translate it to line-local
/// coordinates. `None` when the clamped range is empty (nothing of the
/// range lies within this line).
fn to_line_local(
    range: std::ops::Range<usize>,
    line_start: usize,
    line_end: usize,
) -> Option<std::ops::Range<usize>> {
    let start = range.start.clamp(line_start, line_end) - line_start;
    let end = range.end.clamp(line_start, line_end) - line_start;
    (start < end).then_some(start..end)
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
    let mut quads: Vec<OverlayQuad> = Vec::new();

    // search highlights
    if vim.options.hlsearch || !search_highlights.is_empty() {
        for highlight in *search_highlights {
            let is_current = search_current.as_ref() == Some(highlight);
            let color = if is_current {
                style.search_current
            } else {
                style.search
            };
            if let Some(range) = to_line_local(highlight.clone(), line_start, line_end) {
                quads.push(OverlayQuad {
                    range,
                    color,
                    full_width: false,
                });
            }
        }
    }

    // IME marked text
    if let Some(marked) = ime_marked {
        if let Some(range) = to_line_local(marked, line_start, line_end) {
            quads.push(OverlayQuad {
                range,
                color: style.mark,
                full_width: false,
            });
        }
    }

    // visual selection (char / line / block) — the span math lives in
    // vim-core so the demo's selection ops and this renderer share it
    if let Some(span) = core_ops::span_from_visual(vim, buf) {
        match span_kind(vim) {
            VisualKind::Block => {
                if let Some(block) = core_ops::span_from_visual_block(vim, buf) {
                    if line >= block.first_line {
                        if let Some(row) = block.rows.get(line - block.first_line) {
                            if let Some(range) = to_line_local(row.clone(), line_start, line_end) {
                                quads.push(OverlayQuad {
                                    range,
                                    color: style.selection,
                                    full_width: false,
                                });
                            }
                        }
                    }
                }
            }
            // linewise selection: one full-width quad per line it covers
            VisualKind::Line => {
                let first = buf.offset_to_line(span.start);
                let last = buf.offset_to_line(span.end.saturating_sub(1).max(span.start));
                if line >= first && line <= last {
                    quads.push(OverlayQuad {
                        range: 0..0,
                        color: style.selection,
                        full_width: true,
                    });
                }
            }
            VisualKind::Char => {
                if let Some(range) = to_line_local(span.start..span.end, line_start, line_end) {
                    quads.push(OverlayQuad {
                        range,
                        color: style.selection,
                        full_width: false,
                    });
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

/// The visual kind of the live selection, if any (`span_from_visual` alone
/// folds Block into its Char arm, but block rendering needs per-row spans).
fn span_kind(vim: &VimState) -> VisualKind {
    match vim.visual_selection() {
        Some((_, _, kind)) => kind,
        None => VisualKind::Char,
    }
}

/// The byte range of the char starting at `at` within `text` (`None` past
/// the end or on a non-boundary).
fn char_range_at(text: &str, at: usize) -> Option<std::ops::Range<usize>> {
    text.get(at..)
        .and_then(|rest| rest.chars().next())
        .map(|c| at..at + c.len_utf8())
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
        .and_then(|at| char_range_at(&text, at));
    // block-caret width needs the char under the cursor; compute before
    // `text` moves into the shaper (per visible line, per frame — a
    // SharedString clone is a heap copy, so keep it single-ownership)
    let caret_end_byte = overlays
        .cursor
        .filter(|_| overlays.cursor_block)
        .and_then(|at| char_range_at(&text, at))
        .map(|range| range.end.min(text.len()));
    let runs = build_runs(&text, inverted, style);
    let shaped = window
        .text_system()
        .shape_line(text, style.font_size, &runs, None);

    paint_overlays(window, &shaped, bounds, line_height, overlays);
    paint_caret(
        window,
        &shaped,
        bounds,
        line_height,
        overlays,
        style,
        caret_end_byte,
    );

    let _ = shaped.paint(
        point(bounds.origin.x, bounds.origin.y),
        line_height,
        gpui::TextAlign::Left,
        None,
        window,
        cx,
    );
    shaped
}

/// Split the line into shaped runs: plain text, or text / inverted cursor
/// char / text when the block cursor sits on this line.
fn build_runs(
    text: &str,
    inverted: Option<std::ops::Range<usize>>,
    style: &OverlayStyle,
) -> Vec<TextRun> {
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
    runs
}

/// Paint the selection / highlight / IME quads under the text.
fn paint_overlays(
    window: &mut Window,
    shaped: &gpui::ShapedLine,
    bounds: Bounds<Pixels>,
    line_height: Pixels,
    overlays: &LineOverlays,
) {
    for quad in &overlays.quads {
        let x0 = if quad.full_width {
            0.0
        } else {
            f32::from(shaped.x_for_index(quad.range.start))
        };
        let x1 = if quad.full_width {
            f32::from(bounds.size.width)
        } else {
            f32::from(shaped.x_for_index(quad.range.end))
        };
        if x1 > x0 {
            let area = Bounds::from_corners(
                point(bounds.origin.x + px(x0), bounds.origin.y),
                point(bounds.origin.x + px(x1), bounds.origin.y + line_height),
            );
            window.paint_quad(gpui::fill(area, quad.color));
        }
    }
}

/// Paint the caret: a char-wide block (normal/visual) or a thin bar
/// (insert), positioned via the shaped line's advance widths.
fn paint_caret(
    window: &mut Window,
    shaped: &gpui::ShapedLine,
    bounds: Bounds<Pixels>,
    line_height: Pixels,
    overlays: &LineOverlays,
    style: &OverlayStyle,
    caret_end_byte: Option<usize>,
) {
    let Some(cursor_byte) = overlays.cursor else {
        return;
    };
    let x = f32::from(shaped.x_for_index(cursor_byte));
    let width = if overlays.cursor_block {
        // advance to the next char boundary — byte+1 is mid-char for
        // multi-byte characters (x_for_index rounds up, which would
        // give a zero-width quad)
        let end_byte = caret_end_byte.unwrap_or(cursor_byte);
        let next = f32::from(shaped.x_for_index(end_byte)) - x;
        if next > HALF_PX {
            next
        } else {
            style.caret_fallback_width
        }
    } else {
        CARET_BAR_WIDTH
    };
    let quad = Bounds::from_corners(
        point(bounds.origin.x + px(x), bounds.origin.y),
        point(
            bounds.origin.x + px(x + width),
            bounds.origin.y + line_height,
        ),
    );
    window.paint_quad(gpui::fill(quad, style.cursor));
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
        cx.spawn(async move |entity, cx| loop {
            cx.background_executor().timer(BLINK_INTERVAL).await;
            let alive = entity
                .update(cx, |_, cx| {
                    blinker.tick();
                    cx.notify();
                })
                .is_ok();
            if !alive {
                break;
            }
        })
        .detach();
    }
}
