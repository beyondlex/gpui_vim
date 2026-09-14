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

    /// Whether this motion is a "jump" in vim's sense: it belongs on the
    /// jumplist reachable with `C-o` / `C-i`.
    pub fn is_jump(self) -> bool {
        matches!(
            self,
            Motion::GoToLine { .. }
                | Motion::SearchNext { .. }
                | Motion::StarSearch { .. }
                | Motion::ParaNext
                | Motion::ParaPrev
                | Motion::SentenceNext
                | Motion::SentencePrev
                | Motion::MatchBracket
                | Motion::ScreenTop
                | Motion::ScreenMiddle
                | Motion::ScreenBottom
                | Motion::ScrollHalfDown
                | Motion::ScrollHalfUp
                | Motion::PageUp
                | Motion::PageDown
                | Motion::MarkJump { .. }
        )
    }

    /// Compute the landing offset. Applies `count` where it makes sense
    /// (an absent count arrives as 1). Never moves the cursor — callers
    /// decide — but `SearchNext` may re-publish highlights as a side
    /// effect, so this takes `&mut VimState`.
    pub fn target(&self, vim: &mut VimState, ctx: &mut Ctx, count: usize) -> MotionResult {
        let buf = &*ctx.buf;
        let count = count.max(1);
        match *self {
            // h: count graphemes left, stopping at the line start
            Motion::Left => {
                let mut o = vim.cursor.offset;
                for _ in 0..count {
                    match crate::buffer::prev_grapheme_offset(buf, o) {
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
            // l: count graphemes right, never stepping onto the newline
            Motion::Right => {
                let mut o = vim.cursor.offset;
                for _ in 0..count {
                    let line = buf.offset_to_line(o);
                    let end = buf.line_end(line);
                    if end == buf.line_start(line) || o >= end {
                        break;
                    }
                    // `next < end`: never step onto the newline position
                    match crate::buffer::next_grapheme_offset(buf, o) {
                        Some(next) if next < end => o = next,
                        _ => break,
                    }
                }
                if o == vim.cursor.offset {
                    MotionResult::stuck(o)
                } else {
                    MotionResult::new(o, MotionKind::Exclusive)
                }
            }
            // j/k: keep the desired display column (`desired_col` memoizes
            // it across intermediate lines, like vim's wv_col)
            Motion::Up | Motion::Down => {
                let dir = if matches!(self, Motion::Up) { -1i64 } else { 1 };
                let start_line = buf.offset_to_line(vim.cursor.offset) as i64;
                let target_line = (start_line + dir * count as i64)
                    .clamp(0, buf.line_count() as i64 - 1)
                    as usize;
                let desired = vim.desired_column(buf);
                let o = crate::buffer::offset_for_display_column(buf, target_line, desired);
                let moved = target_line != start_line as usize;
                if moved {
                    MotionResult::new(o, MotionKind::Linewise)
                } else {
                    MotionResult::stuck(vim.cursor.offset)
                }
            }
            // 0: byte offset of the line start
            Motion::LineStart => MotionResult::new(
                buf.line_start(buf.offset_to_line(vim.cursor.offset)),
                MotionKind::Exclusive,
            ),
            // ^: first non-blank char of the current line
            Motion::FirstNonBlank => MotionResult::new(
                buf.first_non_blank(buf.offset_to_line(vim.cursor.offset)),
                MotionKind::Exclusive,
            ),
            Motion::LineEnd => {
                let line = buf.offset_to_line(vim.cursor.offset);
                let end = buf.line_end(line);
                if end == buf.line_start(line) {
                    MotionResult::new(end, MotionKind::Inclusive)
                } else {
                    MotionResult::new(end - 1, MotionKind::Inclusive)
                }
            }
            // g_: last NON-blank char of the count-th line
            Motion::LastLineNonBlank => {
                // g_: to the last non-blank of the (count-th) line
                let line =
                    (buf.offset_to_line(vim.cursor.offset) + count - 1).min(buf.line_count() - 1);
                let end = buf.line_end(line);
                if end > buf.line_start(line) {
                    MotionResult::new(end - 1, MotionKind::Inclusive)
                } else {
                    MotionResult::new(end, MotionKind::Inclusive)
                }
            }
            // w / W: start of the next word run
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
            // e / E: end of the current-or-next word run (inclusive)
            Motion::WordEnd { big } => {
                let mut o = vim.cursor.offset;
                for _ in 0..count {
                    o = word::next_word_end(buf, o, big);
                }
                MotionResult::new(o, MotionKind::Inclusive)
            }
            // b / B: start of the previous word run
            Motion::WordBack { big } => {
                let mut o = vim.cursor.offset;
                for _ in 0..count {
                    o = word::prev_word_start(buf, o, big);
                }
                MotionResult::new(o, MotionKind::Exclusive)
            }
            // ge / gE: end of the previous word run
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
            // f/t/F/T: reuse the last find's target char (for `;`/`,`).
            // The char for a FRESH find arrives as a char argument.
            Motion::FindChar { forward, till } => {
                let Some((target_char, _, _)) = vim.last_find else {
                    return MotionResult::stuck(vim.cursor.offset);
                };
                Self::find_from(vim, buf, target_char, forward, till, count)
            }
            // ; / ,: repeat the last find, `,` mirroring its direction
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
            // %: jump to the bracket matching the one under the cursor
            Motion::MatchBracket => match word::match_bracket(buf, vim.cursor.offset) {
                Some(o) => MotionResult::new(o, MotionKind::Inclusive),
                None => MotionResult::stuck(vim.cursor.offset),
            },
            // gg / G: an explicit count (`1G`, `2gg`) is the line number;
            // the bare forms go to first/last line. execute_command rewrites
            // an explicit `1G` to `first=true` before the count collapses.
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
            // }: next paragraph boundary (blank line)
            Motion::ParaNext => {
                let mut o = vim.cursor.offset;
                for _ in 0..count {
                    o = word::next_paragraph(buf, o);
                }
                MotionResult::new(o, MotionKind::Exclusive)
            }
            // {: previous paragraph boundary
            Motion::ParaPrev => {
                let mut o = vim.cursor.offset;
                for _ in 0..count {
                    o = word::prev_paragraph(buf, o);
                }
                MotionResult::new(o, MotionKind::Exclusive)
            }
            // ): next sentence end. Divergence from vim: only `.!?` single
            // chars end sentences here — no `...` or newline-follow rules.
            Motion::SentenceNext => {
                let mut o = vim.cursor.offset;
                for _ in 0..count {
                    o = word::next_sentence(buf, o);
                }
                MotionResult::new(o, MotionKind::Exclusive)
            }
            // (: previous sentence end (same divergence as `)`)
            Motion::SentencePrev => {
                let mut o = vim.cursor.offset;
                for _ in 0..count {
                    o = word::prev_sentence(buf, o);
                }
                MotionResult::new(o, MotionKind::Exclusive)
            }
            // n / N: jump to the count-th next/previous match of the
            // active pattern
            Motion::SearchNext { forward } => {
                match search::jump_to_match(vim, buf, forward, count) {
                    Some(o) => {
                        // re-publish the matches: after Esc dismissed the
                        // highlights (`:noh` semantics) `n`/`N` re-arms them
                        if vim.options.hlsearch {
                            let current = vim
                                .search
                                .last_matches
                                .iter()
                                .find(|m| m.start == o)
                                .cloned();
                            ctx.host
                                .set_search_highlights(&vim.search.last_matches, current);
                        }
                        MotionResult::new(o, MotionKind::Exclusive)
                    }
                    None => MotionResult::stuck(vim.cursor.offset),
                }
            }
            // * / #: word under cursor becomes the pattern, then jump
            Motion::StarSearch { forward } => {
                search::search_word_under_cursor(vim, buf, ctx.host, forward);
                match search::jump_to_match(vim, buf, forward, 1) {
                    Some(o) => MotionResult::new(o, MotionKind::Exclusive),
                    None => MotionResult::stuck(vim.cursor.offset),
                }
            }
            // '{char} / `{char}: the mark name arrives as a char argument
            Motion::MarkJump { linewise } => {
                let Some(name) = vim.char_arg else {
                    return MotionResult::stuck(vim.cursor.offset);
                };
                match vim.marks.resolve(name) {
                    Some(o) => {
                        let o = o.min(buf.len());
                        if linewise {
                            MotionResult::new(
                                buf.line_start(buf.offset_to_line(o)),
                                MotionKind::Linewise,
                            )
                        } else {
                            MotionResult::new(o, MotionKind::Exclusive)
                        }
                    }
                    None => MotionResult::stuck(vim.cursor.offset),
                }
            }
            // |: display column `count` (wide chars cover two cells)
            Motion::Column => {
                let line = buf.offset_to_line(vim.cursor.offset);
                // `|` counts display columns (wide chars cover two)
                let col = count.max(1) - 1;
                MotionResult::new(
                    crate::buffer::offset_for_display_column(buf, line, col),
                    MotionKind::Exclusive,
                )
            }
            // H / M / L: viewport-relative rows
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
            Motion::ScrollHalfDown | Motion::ScrollHalfUp | Motion::PageDown | Motion::PageUp => {
                let (first, last) = ctx.host.viewport();
                let visible = last.saturating_sub(first).max(1);
                // half-page for <C-d>/<C-u>, full page for <C-f>/<C-b>
                let step = if matches!(self, Motion::PageDown | Motion::PageUp) {
                    visible
                } else {
                    visible / 2
                };
                let dir = if matches!(self, Motion::ScrollHalfDown | Motion::PageDown) {
                    1i64
                } else {
                    -1
                };
                let line = buf.offset_to_line(vim.cursor.offset) as i64 + dir * step as i64;
                let line = line.clamp(0, buf.line_count() as i64 - 1) as usize;
                // keep the DISPLAY column, exactly like j/k: `desired` counts
                // cells, so on CJK/Tab rows it must be mapped back to a byte
                // offset (a plain `line_start + desired` would land mid-char)
                let desired = vim.desired_column(buf);
                let o = crate::buffer::offset_for_display_column(buf, line, desired);
                MotionResult::new(o, MotionKind::Linewise)
            }
            // + / -: adjacent line, first non-blank char
            Motion::LineDownFirstNonBlank => {
                let line =
                    (buf.offset_to_line(vim.cursor.offset) + count).min(buf.line_count() - 1);
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
        MotionResult::new(
            o,
            if till {
                MotionKind::Exclusive
            } else {
                MotionKind::Inclusive
            },
        )
    }
}
