//! Operators: applying delete/change/yank/indent/case to a span.
//!
//! This module owns vim's operator-range special cases:
//! - `w` as an operator target stops at the end of the last word moved over,
//!   so `dw` never joins lines.
//! - An exclusive motion ending in column 1 becomes inclusive at the end of
//!   the previous line (`d}` keeps the following blank line).
//! - Column 1 + start at/before first non-blank becomes linewise.

use crate::buffer::{clamp_to_line_end, VimBuffer};
use crate::motions::{Motion, MotionKind, MotionResult};
use crate::objects::{self, ObjectRange};
use crate::registers::RegisterKind;
use crate::state::{Ctx, InsertKind, VimState};
use crate::word;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operator {
    Delete,
    Change,
    Yank,
    IndentLeft,
    IndentRight,
    Lowercase,
    Uppercase,
    ToggleCase,
}

/// A concrete span to operate on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpSpan {
    pub start: usize,
    pub end: usize,
    pub linewise: bool,
}

/// Turn a motion result into an operator span, applying the special cases.
pub fn span_from_motion(
    vim: &VimState,
    buf: &dyn VimBuffer,
    motion: Motion,
    result: MotionResult,
) -> OpSpan {
    let start = vim.cursor.offset;
    let start_line = buf.offset_to_line(start);
    // a motion may land past a line's trailing newline (e.g. `w` at the
    // last word of a buffer lands in the phantom final line); anchor the
    // span to the line end so `dw` etc. never swallow the newline
    let target = crate::buffer::clamp_to_line_end(buf, result.offset);
    let target_line = buf.offset_to_line(target);

    if result.kind == MotionKind::Linewise {
        return OpSpan {
            start: buf.line_start(start_line.min(target_line)),
            end: buf.line_range(start_line.max(target_line)).end,
            linewise: true,
        };
    }

    let crossed_lines = target_line != start_line;

    // `w` operator special case: end the span at the end of the last word
    // moved over, so `dw` never joins lines.
    if matches!(motion, Motion::WordStart { .. }) && crossed_lines {
        let mut moved_over_word = word::is_non_blank(buf, start);
        let mut probe = start;
        while let Some(next) = buf.next_char_offset(probe) {
            if next >= buf.line_end(start_line) {
                break;
            }
            if word::is_non_blank(buf, next) {
                moved_over_word = true;
                break;
            }
            probe = next;
        }
        if moved_over_word {
            return OpSpan {
                start,
                end: buf.line_end(start_line),
                linewise: false,
            };
        }
        // no word moved over: fall through to the column-1 rules
    }

    if result.kind == MotionKind::Exclusive && crossed_lines && target == buf.line_start(target_line)
    {
        if start <= buf.first_non_blank(start_line) {
            // exclusive + column 1 + started at/before first non-blank:
            // becomes linewise over the lines fully covered
            return OpSpan {
                start: buf.line_start(start_line),
                end: buf.line_range(target_line - 1).end,
                linewise: true,
            };
        }
        // exclusive + column 1: end moves to the end of the previous line,
        // and the span becomes inclusive (keeps the newline)
        return OpSpan {
            start,
            end: buf.line_end(target_line - 1) + 1,
            linewise: false,
        };
    }

    let (lo, hi) = if target >= start { (start, target) } else { (target, start) };
    let end = match result.kind {
        MotionKind::Inclusive => hi + buf.char_at(hi).map(|c| c.len_utf8()).unwrap_or(0),
        _ => hi,
    };
    OpSpan {
        start: lo,
        end,
        linewise: false,
    }
}

/// Span for a text object.
pub fn span_from_object(_buf: &dyn VimBuffer, object: ObjectRange) -> OpSpan {
    OpSpan {
        start: object.start,
        end: object.end,
        linewise: object.linewise,
    }
}

/// Resolve a text object against the cursor.
pub fn object_span(
    vim: &VimState,
    buf: &dyn VimBuffer,
    object: objects::TextObject,
) -> Option<OpSpan> {
    let range = objects::range(buf, vim.cursor.offset, object)?;
    Some(span_from_object(buf, range))
}

/// Span for the current visual selection.
pub fn span_from_visual(vim: &VimState, buf: &dyn VimBuffer) -> Option<OpSpan> {
    let (anchor, cursor, kind) = vim.visual_selection()?;
    let (lo, hi) = if anchor <= cursor { (anchor, cursor) } else { (cursor, anchor) };
    Some(match kind {
        crate::mode::VisualKind::Line => OpSpan {
            start: buf.line_start(buf.offset_to_line(lo)),
            end: buf.line_range(buf.offset_to_line(hi)).end,
            linewise: true,
        },
        crate::mode::VisualKind::Block | crate::mode::VisualKind::Char => OpSpan {
            start: lo,
            end: hi + buf.char_at(hi).map(|c| c.len_utf8()).unwrap_or(0),
            linewise: false,
        },
    })
}

fn register_kind(span: &OpSpan) -> RegisterKind {
    if span.linewise {
        RegisterKind::Linewise
    } else {
        RegisterKind::Charwise
    }
}

/// Delete the span into the register; updates the cursor.
pub fn delete_span(vim: &mut VimState, ctx: &mut Ctx, span: &OpSpan, register: Option<char>) {
    let text = ctx.buf.slice(span.start..span.end);
    vim.registers
        .store_delete(register, text, register_kind(span));
    vim.edit_delete(ctx, span.start..span.end);

    vim.cursor.offset = if span.linewise {
        let line = ctx
            .buf
            .offset_to_line(span.start)
            .min(ctx.buf.line_count() - 1);
        ctx.buf.first_non_blank(line)
    } else {
        clamp_to_line_end(ctx.buf, span.start)
    };
    vim.cursor.desired_col = None;
}

/// Yank the span into the register.
pub fn yank_span(vim: &mut VimState, ctx: &mut Ctx, span: &OpSpan, register: Option<char>) {
    let text = ctx.buf.slice(span.start..span.end);
    vim.registers.store_yank(register, text, register_kind(span));
}

/// Apply an operator to a span (operator-pending and visual paths).
pub fn apply(
    vim: &mut VimState,
    ctx: &mut Ctx,
    op: Operator,
    span: &OpSpan,
    register: Option<char>,
) {
    match op {
        Operator::Delete => delete_span(vim, ctx, span, register),
        Operator::Yank => yank_span(vim, ctx, span, register),
        Operator::Change => {
            // keep the indent of the first line when changing linewise
            let indent_text = if span.linewise {
                let first = ctx.buf.offset_to_line(span.start);
                let (indent, _) = ctx.buf.line_indent(first);
                ctx.buf.slice(ctx.buf.line_start(first)..ctx.buf.line_start(first) + indent)
            } else {
                String::new()
            };
            let effective = if span.linewise {
                // `cc` clears the lines' contents but the lines themselves
                // survive: never consume the last newline of the span
                let last = ctx.buf.offset_to_line(span.end.saturating_sub(1).max(span.start));
                OpSpan {
                    start: span.start,
                    end: ctx.buf.line_end(last),
                    linewise: false,
                }
            } else {
                *span
            };
            delete_span(vim, ctx, &effective, register);
            if !indent_text.is_empty() {
                let at = vim.cursor.offset;
            vim.edit_insert(ctx, at, &indent_text);
                vim.cursor.offset = at + indent_text.len();
            }
            vim.begin_insert(ctx, InsertKind::Change);
        }
        Operator::IndentLeft | Operator::IndentRight => {
            let first = ctx.buf.offset_to_line(span.start);
            let last = ctx.buf.offset_to_line(span.end.saturating_sub(1).max(span.start));
            for line in first..=last {
                shift_line(vim, ctx, line, matches!(op, Operator::IndentRight));
            }
            vim.cursor.offset = ctx.buf.first_non_blank(first.min(ctx.buf.line_count() - 1));
            vim.cursor.desired_col = None;
        }
        Operator::Lowercase | Operator::Uppercase | Operator::ToggleCase => {
            let text = ctx.buf.slice(span.start..span.end);
            let mapped: String = text
                .chars()
                .map(|c| match op {
                    Operator::Lowercase => c.to_lowercase().next().unwrap_or(c),
                    Operator::Uppercase => c.to_uppercase().next().unwrap_or(c),
                    _ => toggle_case(c),
                })
                .collect();
            vim.edit_replace(ctx, span.start..span.end, &mapped);
            vim.cursor.offset = clamp_to_line_end(ctx.buf, span.start);
            vim.cursor.desired_col = None;
        }
    }
    vim.cursor.offset = clamp_to_line_end(ctx.buf, vim.cursor.offset);
}

pub fn toggle_case(c: char) -> char {
    if c.is_lowercase() {
        c.to_uppercase().next().unwrap_or(c)
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

/// Shift one line by `shiftwidth` left or right.
pub fn shift_line(vim: &mut VimState, ctx: &mut Ctx, line: usize, right: bool) {
    if line >= ctx.buf.line_count() {
        return;
    }
    let start = ctx.buf.line_start(line);
    let (indent, blank) = ctx.buf.line_indent(line);
    if blank {
        return;
    }
    let sw = vim.options.shiftwidth.max(1);
    if right {
        let unit = if vim.options.expandtab {
            " ".repeat(sw)
        } else {
            "\t".to_owned()
        };
        vim.edit_insert(ctx, start, &unit);
    } else if indent > 0 {
        // remove up to `sw` columns of indent; a tab counts as a full unit
        let indent_end = start + indent;
        let mut removed = 0usize;
        let mut o = start;
        let mut cut = start;
        while o < indent_end && removed < sw {
            let Some(c) = ctx.buf.char_at(o) else { break };
            let width = if c == '\t' { sw } else { 1 };
            if removed + width > sw {
                break;
            }
            removed += width;
            o += c.len_utf8();
            cut = o;
        }
        vim.edit_delete(ctx, start..cut);
    }
}

/// `p` / `P`: paste a register.
pub fn put(vim: &mut VimState, ctx: &mut Ctx, register: char, count: usize, after: bool) {
    let Some(data) = vim.registers.get_for_paste(register, ctx.host) else {
        return;
    };
    if data.text.is_empty() {
        return;
    }
    let repeated = data.text.repeat(count.max(1));

    if data.kind == RegisterKind::Linewise {
        let line = ctx.buf.offset_to_line(vim.cursor.offset);
        let line_end = ctx.buf.line_end(line);
        let has_newline = ctx.buf.line_range(line).end > line_end;
        let (insert_at, text) = if has_newline {
            // paste between this line and the next
            let at = ctx.buf.line_range(line).end;
            let text = if repeated.ends_with('\n') {
                repeated
            } else {
                format!("{repeated}\n")
            };
            (at, text)
        } else {
            // last line without trailing newline: open a new line for it
            (ctx.buf.len(), format!("\n{}", repeated.trim_end_matches('\n')))
        };
        vim.edit_insert(ctx, insert_at, &text);
        let pasted_lines = text.trim_end_matches('\n').split('\n').count();
        let cursor_line = (ctx.buf.offset_to_line(insert_at) + pasted_lines - 1)
            .min(ctx.buf.line_count() - 1);
        vim.cursor.offset = ctx.buf.first_non_blank(cursor_line);
    } else {
        let mut at = vim.cursor.offset;
        if after && !ctx.buf.at_line_end(at) {
            at = ctx.buf.next_char_offset(at).unwrap_or(at);
        }
        vim.edit_insert(ctx, at, &repeated);
        // block cursor sits on the last pasted character: step back to the
        // START of the last char — `end - 1` is byte arithmetic and would
        // park the cursor inside a multi-byte character
        let end = at + repeated.len();
        vim.cursor.offset =
            clamp_to_line_end(ctx.buf, ctx.buf.prev_char_offset(end).unwrap_or(at));
    }
    vim.cursor.desired_col = None;
}

/// `J` / `gJ`: join `count` lines (at least one join). `literal` = `gJ`
/// (no separator, keep the next line's indent).
pub fn join_lines(vim: &mut VimState, ctx: &mut Ctx, count: usize, literal: bool) {
    let joins = count.max(2) - 1;
    for _ in 0..joins {
        let line = ctx.buf.offset_to_line(vim.cursor.offset);
        if line + 1 >= ctx.buf.line_count() {
            break;
        }
        let join_at = ctx.buf.line_end(line);
        let next_start = ctx.buf.line_start(line + 1);

        if literal {
            vim.edit_delete(ctx, join_at..next_start);
        } else {
            let next_end = ctx.buf.line_end(line + 1);
            let next_content = ctx.buf.slice(next_start..next_end);
            let next_trimmed = next_content.trim_start();
            let next_indent_len = next_content.len() - next_trimmed.len();

            // no extra space when the line already ends with whitespace or
            // the next line starts with `)`
            let cur_tail = ctx.buf.slice(ctx.buf.line_start(line)..join_at);
            let separator = if cur_tail.ends_with(' ')
                || cur_tail.ends_with('\t')
                || next_trimmed.starts_with(')')
            {
                ""
            } else {
                " "
            };
            ctx.buf
                .replace_range(join_at..next_start + next_indent_len, separator);
        }
        vim.cursor.offset = join_at;
    }
    vim.cursor.desired_col = None;
}

/// `x` / `X`: delete `count` chars under/before the cursor.
pub fn delete_chars(vim: &mut VimState, ctx: &mut Ctx, count: usize, backward: bool) {
    let start = vim.cursor.offset;
    let line = ctx.buf.offset_to_line(start);
    let line_start = ctx.buf.line_start(line);
    let line_end = ctx.buf.line_end(line);
    let (lo, hi) = if backward {
        let mut lo = start;
        for _ in 0..count {
            let Some(prev) = ctx.buf.prev_char_offset(lo) else { break };
            if prev < line_start {
                break;
            }
            lo = prev;
        }
        if lo == start {
            return;
        }
        (lo, start)
    } else {
        if start >= line_end {
            return;
        }
        let mut hi = start;
        for _ in 0..count.saturating_sub(1) {
            let next = ctx.buf.next_char_offset(hi).unwrap_or(hi);
            if next >= line_end {
                break;
            }
            hi = next;
        }
        (start, ctx.buf.next_char_offset(hi).unwrap_or(hi))
    };
    delete_span(
        vim,
        ctx,
        &OpSpan {
            start: lo,
            end: hi,
            linewise: false,
        },
        None,
    );
}

/// `r{char}`: replace `count` chars with `char` (never crosses the line end).
pub fn replace_chars(vim: &mut VimState, ctx: &mut Ctx, ch: char, count: usize) {
    let count = count.max(1);
    let start = vim.cursor.offset;
    let line_end = ctx.buf.line_end(ctx.buf.offset_to_line(start));
    let mut replacements = String::new();
    let mut o = start;
    for _ in 0..count {
        if o >= line_end {
            return;
        }
        let Some(c) = ctx.buf.char_at(o) else { return };
        replacements.push(ch);
        o += c.len_utf8();
    }
    vim.edit_replace(ctx, start..o, &replacements);
    vim.cursor.offset = clamp_to_line_end(
        ctx.buf,
        start + replacements.len() - ch.len_utf8(),
    );
    vim.cursor.desired_col = None;
}

/// `~`: toggle case of `count` chars, cursor ends on the last one.
pub fn toggle_chars(vim: &mut VimState, ctx: &mut Ctx, count: usize) {
    let count = count.max(1);
    let start = vim.cursor.offset;
    let line_end = ctx.buf.line_end(ctx.buf.offset_to_line(start));
    let mut mapped = String::new();
    let mut o = start;
    for _ in 0..count {
        if o >= line_end {
            break;
        }
        let Some(c) = ctx.buf.char_at(o) else { break };
        mapped.push(crate::ops::toggle_case(c));
        o += c.len_utf8();
    }
    if mapped.is_empty() {
        return;
    }
    vim.edit_replace(ctx, start..start + mapped.len(), &mapped);
    // vim's `~` moves right past the last toggled char (staying on it only
    // at line end); plain byte arithmetic was wrong for multi-byte chars
    vim.cursor.offset = clamp_to_line_end(ctx.buf, start + mapped.len());
    vim.cursor.desired_col = None;
}
