//! Motions: pure cursor-target computations.
//!
//! `Motion::target` computes where a motion lands and what *kind* of span it
//! describes (`Exclusive`/`Inclusive`/`Linewise`). Turning a target into an
//! operator range — including vim's special cases (`dw` never joins lines,
//! `cw` acts like `ce`, the column-1 rules) — lives in [`crate::ops`].

use crate::buffer::VimBuffer;
use crate::search;
use crate::state::{Ctx, VimState};
use crate::word;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    LineStart,
    FirstNonBlank,
    LineEnd,
    LastLineNonBlank, // g_
    WordStart { big: bool },
    WordEnd { big: bool },
    WordBack { big: bool },
    WordEndBack { big: bool },
    FindChar { forward: bool, till: bool },
    RepeatFind { reverse: bool }, // ; ,
    MatchBracket,                 // %
    GoToLine { first: bool },     // gg / G
    ParaNext,
    ParaPrev,
    SentenceNext,
    SentencePrev,
    SearchNext { forward: bool }, // n / N
    StarSearch { forward: bool }, // * / #
    MarkJump { linewise: bool },  // '{char} / `{char} as operator target
    Column,                       // | (to count column)
    ScreenTop,
    ScreenMiddle,
    ScreenBottom,
    ScrollHalfDown,
    ScrollHalfUp,
    PageDown,
    PageUp,
    LineDownFirstNonBlank, // enter / +
    LineUpFirstNonBlank,   // -
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MotionKind {
    Exclusive,
    Inclusive,
    Linewise,
}

#[derive(Clone, Copy, Debug)]
pub struct MotionResult {
    pub offset: usize,
    pub kind: MotionKind,
    /// False when the motion could not move (e.g. `h` at buffer start).
    pub moved: bool,
}

impl MotionResult {
    fn new(offset: usize, kind: MotionKind) -> Self {
        MotionResult {
            offset,
            kind,
            moved: true,
        }
    }
    fn stuck(offset: usize) -> Self {
        MotionResult {
            offset,
            kind: MotionKind::Exclusive,
            moved: false,
        }
    }
}

impl Motion {
    pub fn kind(&self) -> MotionKind {
        match self {
            Motion::Up
            | Motion::Down
            | Motion::GoToLine { .. }
            | Motion::ScreenTop
            | Motion::ScreenMiddle
            | Motion::ScreenBottom
            | Motion::PageUp
            | Motion::PageDown
            | Motion::LineDownFirstNonBlank
            | Motion::LineUpFirstNonBlank => MotionKind::Linewise,
            Motion::LineEnd
            | Motion::LastLineNonBlank
            | Motion::WordEnd { .. }
            | Motion::WordEndBack { .. }
            | Motion::MatchBracket
            | Motion::FindChar { till: false, .. }
            | Motion::RepeatFind { .. } => MotionKind::Inclusive,
            _ => MotionKind::Exclusive,
        }
    }

    /// Compute the landing offset. Applies `count` where it makes sense.
    /// Pure with respect to the cursor: callers decide whether to move.
    pub fn target(
        &self,
        vim: &mut VimState,
        ctx: &mut Ctx,
        count: usize,
    ) -> MotionResult {
        let buf = &*ctx.buf;
        let count = count.max(1);
        match *self {
            Motion::Left => {
                let mut o = vim.cursor.offset;
                for _ in 0..count {
                    match buf.prev_char_offset(o) {
                        Some(prev) if prev >= buf.line_start(buf.offset_to_line(o)) => o = prev,
                        _ => break,
                    }
                }
                if o == vim.cursor.offset {
                    MotionResult::stuck(o)
                } else {
                    MotionResult::new(o, MotionKind::Exclusive)
                }
            }
            Motion::Right => {
                let mut o = vim.cursor.offset;
                for _ in 0..count {
                    let line = buf.offset_to_line(o);
                    let end = buf.line_end(line);
                    if end == buf.line_start(line) || o + 1 >= end {
                        break;
                    }
                    o = buf.next_char_offset(o).unwrap_or(o);
                }
                if o == vim.cursor.offset {
                    MotionResult::stuck(o)
                } else {
                    MotionResult::new(o, MotionKind::Exclusive)
                }
            }
            Motion::Up | Motion::Down => {
                let dir = if matches!(self, Motion::Up) { -1i64 } else { 1 };
                let start_line = buf.offset_to_line(vim.cursor.offset) as i64;
                let target_line = (start_line + dir * count as i64).clamp(0, buf.line_count() as i64 - 1) as usize;
                let desired = vim.desired_column(buf);
                let line = buf.line_range(target_line);
                let end = buf.line_end(target_line);
                let o = line.start + desired.min(end - line.start);
                let moved = target_line != start_line as usize;
                if moved {
                    MotionResult::new(o, MotionKind::Linewise)
                } else {
                    MotionResult::stuck(vim.cursor.offset)
                }
            }
            Motion::LineStart => MotionResult::new(buf.line_start(buf.offset_to_line(vim.cursor.offset)), MotionKind::Exclusive),
            Motion::FirstNonBlank => MotionResult::new(buf.first_non_blank(buf.offset_to_line(vim.cursor.offset)), MotionKind::Exclusive),
            Motion::LineEnd => {
                let line = buf.offset_to_line(vim.cursor.offset);
                let end = buf.line_end(line);
                if end == buf.line_start(line) {
                    MotionResult::new(end, MotionKind::Inclusive)
                } else {
                    MotionResult::new(end - 1, MotionKind::Inclusive)
                }
            }
            Motion::LastLineNonBlank => {
                // g_: to the last non-blank of the (count-th) line
                let line = (buf.offset_to_line(vim.cursor.offset) + count - 1).min(buf.line_count() - 1);
                let end = buf.line_end(line);
                if end > buf.line_start(line) {
                    MotionResult::new(end - 1, MotionKind::Inclusive)
                } else {
                    MotionResult::new(end, MotionKind::Inclusive)
                }
            }
            Motion::WordStart { big } => {
                let mut o = vim.cursor.offset;
                for _ in 0..count {
                    o = word::next_word_start(buf, o, big);
                    if o >= buf.len() {
                        break;
                    }
                }
                MotionResult::new(o.min(buf.len()), MotionKind::Exclusive)
            }
            Motion::WordEnd { big } => {
                let mut o = vim.cursor.offset;
                for _ in 0..count {
                    o = word::next_word_end(buf, o, big);
                }
                MotionResult::new(o, MotionKind::Inclusive)
            }
            Motion::WordBack { big } => {
                let mut o = vim.cursor.offset;
                for _ in 0..count {
                    o = word::prev_word_start(buf, o, big);
                }
                MotionResult::new(o, MotionKind::Exclusive)
            }
            Motion::WordEndBack { big } => {
                let mut o = vim.cursor.offset;
                for _ in 0..count {
                    let next = word::prev_word_end(buf, o, big);
                    if next == o {
                        break;
                    }
                    o = next;
                }
                if o == vim.cursor.offset {
                    MotionResult::stuck(o)
                } else {
                    MotionResult::new(o, MotionKind::Inclusive)
                }
            }
            Motion::FindChar { forward, till } => {
                let Some((target_char, _, _)) = vim.last_find else {
                    return MotionResult::stuck(vim.cursor.offset);
                };
                Self::find_from(vim, buf, target_char, forward, till, count)
            }
            Motion::RepeatFind { reverse } => {
                let Some((target_char, forward, till)) = vim.last_find else {
                    return MotionResult::stuck(vim.cursor.offset);
                };
                let (forward, till) = if reverse {
                    (!forward, till)
                } else {
                    (forward, till)
                };
                Self::find_from(vim, buf, target_char, forward, till, count)
            }
            Motion::MatchBracket => {
                match word::match_bracket(buf, vim.cursor.offset) {
                    Some(o) => MotionResult::new(o, MotionKind::Inclusive),
                    None => MotionResult::stuck(vim.cursor.offset),
                }
            }
            Motion::GoToLine { first } => {
                let line = if count > 1 {
                    (count - 1).min(buf.line_count() - 1)
                } else if first {
                    0
                } else {
                    buf.line_count() - 1
                };
                MotionResult::new(buf.line_start(line), MotionKind::Linewise)
            }
            Motion::ParaNext => {
                let mut o = vim.cursor.offset;
                for _ in 0..count {
                    o = word::next_paragraph(buf, o);
                }
                MotionResult::new(o, MotionKind::Exclusive)
            }
            Motion::ParaPrev => {
                let mut o = vim.cursor.offset;
                for _ in 0..count {
                    o = word::prev_paragraph(buf, o);
                }
                MotionResult::new(o, MotionKind::Exclusive)
            }
            Motion::SentenceNext => {
                let mut o = vim.cursor.offset;
                for _ in 0..count {
                    o = word::next_sentence(buf, o);
                }
                MotionResult::new(o, MotionKind::Exclusive)
            }
            Motion::SentencePrev => {
                let mut o = vim.cursor.offset;
                for _ in 0..count {
                    o = word::prev_sentence(buf, o);
                }
                MotionResult::new(o, MotionKind::Exclusive)
            }
            Motion::SearchNext { forward } => {
                match search::jump_to_match(vim, buf, forward, count) {
                    Some(o) => {
                        // re-publish the matches: after Esc dismissed the
                        // highlights (`:noh` semantics) `n`/`N` re-arms them
                        let matches = vim.search.last_matches.clone();
                        let current = matches.iter().find(|m| m.start == o).cloned();
                        ctx.host.set_search_highlights(&matches, current);
                        MotionResult::new(o, MotionKind::Exclusive)
                    }
                    None => MotionResult::stuck(vim.cursor.offset),
                }
            }
            Motion::StarSearch { forward } => {
                search::search_word_under_cursor(vim, buf, ctx.host, forward);
                match search::jump_to_match(vim, buf, forward, 1) {
                    Some(o) => MotionResult::new(o, MotionKind::Exclusive),
                    None => MotionResult::stuck(vim.cursor.offset),
                }
            }
            Motion::MarkJump { linewise } => {
                let Some(name) = vim.char_arg else {
                    return MotionResult::stuck(vim.cursor.offset);
                };
                match vim.marks.resolve(name) {
                    Some(o) => {
                        let o = o.min(buf.len());
                        if linewise {
                            MotionResult::new(buf.line_start(buf.offset_to_line(o)), MotionKind::Linewise)
                        } else {
                            MotionResult::new(o, MotionKind::Exclusive)
                        }
                    }
                    None => MotionResult::stuck(vim.cursor.offset),
                }
            }
            Motion::Column => {
                let line = buf.offset_to_line(vim.cursor.offset);
                let start = buf.line_start(line);
                let end = buf.line_end(line);
                MotionResult::new((start + count - 1).min(end), MotionKind::Exclusive)
            }
            Motion::ScreenTop | Motion::ScreenMiddle | Motion::ScreenBottom => {
                let (first, last) = ctx.host.viewport();
                let line = match self {
                    Motion::ScreenTop => (first + count - 1).min(last),
                    Motion::ScreenMiddle => (first + last) / 2,
                    _ => last.saturating_sub(count - 1).max(first),
                };
                let line = line.min(buf.line_count() - 1);
                MotionResult::new(buf.line_start(line), MotionKind::Linewise)
            }
            Motion::ScrollHalfDown | Motion::ScrollHalfUp => {
                let (first, last) = ctx.host.viewport();
                let visible = last.saturating_sub(first).max(1);
                let half = visible / 2;
                let dir = if matches!(self, Motion::ScrollHalfDown) { 1i64 } else { -1 };
                let line = buf.offset_to_line(vim.cursor.offset) as i64 + dir * half as i64;
                let line = line.clamp(0, buf.line_count() as i64 - 1) as usize;
                let desired = vim.desired_column(buf);
                let end = buf.line_end(line);
                let o = buf.line_start(line) + desired.min(end - buf.line_start(line));
                MotionResult::new(o, MotionKind::Linewise)
            }
            Motion::PageDown | Motion::PageUp => {
                let (first, last) = ctx.host.viewport();
                let visible = last.saturating_sub(first).max(1);
                let dir = if matches!(self, Motion::PageDown) { 1i64 } else { -1 };
                let line = buf.offset_to_line(vim.cursor.offset) as i64 + dir * visible as i64;
                let line = line.clamp(0, buf.line_count() as i64 - 1) as usize;
                let desired = vim.desired_column(buf);
                let end = buf.line_end(line);
                let o = buf.line_start(line) + desired.min(end - buf.line_start(line));
                MotionResult::new(o, MotionKind::Linewise)
            }
            Motion::LineDownFirstNonBlank => {
                let line = (buf.offset_to_line(vim.cursor.offset) + count).min(buf.line_count() - 1);
                MotionResult::new(buf.first_non_blank(line), MotionKind::Linewise)
            }
            Motion::LineUpFirstNonBlank => {
                let line = buf.offset_to_line(vim.cursor.offset).saturating_sub(count);
                MotionResult::new(buf.first_non_blank(line), MotionKind::Linewise)
            }
        }
    }

    fn find_from(
        vim: &VimState,
        buf: &dyn VimBuffer,
        target: char,
        forward: bool,
        till: bool,
        count: usize,
    ) -> MotionResult {
        let mut o = vim.cursor.offset;
        for _ in 0..count {
            let found = if forward {
                word::find_char_forward(buf, o, target, till)
            } else {
                word::find_char_backward(buf, o, target, till)
            };
            match found {
                Some(hit) => o = hit,
                None => return MotionResult::stuck(vim.cursor.offset),
            }
        }
        MotionResult::new(o, if till { MotionKind::Exclusive } else { MotionKind::Inclusive })
    }
}
