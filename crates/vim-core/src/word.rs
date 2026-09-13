//! Character classes and word/sentence/paragraph scanning.
//!
//! Shared by motions (`w`, `b`, `e`, ...) and text objects (`iw`, `aw`, ...).
//! A "class run" never spans a newline; blank lines act as boundaries for
//! `w`-family motions, exactly as they do in vim.

use crate::buffer::VimBuffer;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Blank,
    Word,
    Punct,
}

pub fn char_class(c: char) -> Class {
    if c.is_whitespace() {
        Class::Blank
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

pub fn is_word_char(c: char) -> bool {
    char_class(c) == Class::Word
}

/// Class used by the w/b/e family. With `big` (W/B/E), only blanks separate
/// runs. A newline always ends a run.
pub fn class_at(buf: &dyn VimBuffer, offset: usize, big: bool) -> Option<Class> {
    let c = buf.char_at(offset)?;
    if c == '\n' {
        return None; // run boundary
    }
    Some(match char_class(c) {
        Class::Blank => Class::Blank,
        _ if big => Class::Word,
        other => other,
    })
}

/// `true` when the character at `offset` is not a blank and not a newline.
pub fn is_non_blank(buf: &dyn VimBuffer, offset: usize) -> bool {
    matches!(buf.char_at(offset), Some(c) if !c.is_whitespace())
}

/// The next class run's start at or after `offset`, crossing lines freely.
fn next_non_blank(buf: &dyn VimBuffer, offset: usize) -> Option<usize> {
    let mut o = offset;
    loop {
        match buf.char_at(o) {
            None => return None,
            Some('\n') => o += 1,
            Some(c) if c.is_whitespace() => o += c.len_utf8(),
            Some(_) => return Some(o),
        }
    }
}

/// First non-blank at/after `offset` but staying on the same line.
#[allow(dead_code)]
fn next_non_blank_in_line(buf: &dyn VimBuffer, offset: usize) -> Option<usize> {
    let line = buf.offset_to_line(offset);
    let mut o = offset;
    let end = buf.line_end(line);
    while o < end {
        if is_non_blank(buf, o) {
            return Some(o);
        }
        o = buf.next_char_offset(o)?;
    }
    None
}

/// `w` / `W`: start of the next word. Stops on blank lines, never lands on a
/// newline.
pub fn next_word_start(buf: &dyn VimBuffer, offset: usize, big: bool) -> usize {
    let mut o = offset;
    // consume the remainder of the current run (if we are on one)
    if let Some(class) = class_at(buf, o, big) {
        if class != Class::Blank {
            while let Some(next) = buf.next_char_offset(o) {
                match class_at(buf, next, big) {
                    Some(c) if c == class => o = next,
                    _ => {
                        o = next;
                        break;
                    }
                }
            }
        }
    }
    // now step over blanks: spaces first, then whole lines
    loop {
        // skip spaces/tabs on this line
        while let Some(c) = buf.char_at(o) {
            if c == ' ' || c == '\t' {
                o += c.len_utf8();
            } else {
                break;
            }
        }
        match buf.char_at(o) {
            Some('\n') => {
                o += 1;
                let line = buf.offset_to_line(o);
                // stop on blank lines, otherwise continue skipping indent
                if buf.line_is_blank(line) {
                    return buf.line_start(line);
                }
            }
            None => return o,
            Some(_) => return o,
        }
    }
}

/// `e` / `E`: end of the current word, or of the next one when already at a
/// run end. Returns the offset *of the last character* (inclusive motion).
pub fn next_word_end(buf: &dyn VimBuffer, offset: usize, big: bool) -> usize {
    let mut o = offset;
    // finish the current run first
    if let Some(class) = class_at(buf, o, big) {
        if class != Class::Blank {
            while let Some(next) = buf.next_char_offset(o) {
                match class_at(buf, next, big) {
                    Some(c) if c == class => o = next,
                    _ => break,
                }
            }
            if o != offset {
                return o;
            }
        }
    }
    // on a run end / blank: jump to the end of the next run
    let Some(start) = next_non_blank(buf, buf.next_char_offset(offset).unwrap_or(offset)) else {
        return buf.len();
    };
    let class = class_at(buf, start, big).unwrap_or(Class::Word);
    let mut o = start;
    while let Some(next) = buf.next_char_offset(o) {
        match class_at(buf, next, big) {
            Some(c) if c == class => o = next,
            _ => break,
        }
    }
    o
}

/// `b` / `B`: start of the current or previous word.
pub fn prev_word_start(buf: &dyn VimBuffer, offset: usize, big: bool) -> usize {
    let mut o = offset;
    // if mid-run, walk back to its start
    if let Some(class) = class_at(buf, o, big) {
        if class != Class::Blank {
            while let Some(prev) = buf.prev_char_offset(o) {
                match class_at(buf, prev, big) {
                    Some(c) if c == class => o = prev,
                    _ => break,
                }
            }
            if o != offset {
                return o;
            }
        }
    }
    // on a run start / blank: walk back over blanks, then to the previous
    // run's start
    loop {
        let Some(prev) = buf.prev_char_offset(o) else {
            return o;
        };
        match buf.char_at(prev) {
            Some('\n') => {
                // crossing a line upwards: blank lines are skipped freely
                let line = buf.offset_to_line(prev);
                o = prev;
                if buf.line_is_blank(line) {
                    return buf.line_start(line);
                }
            }
            Some(c) if c.is_whitespace() => o = prev,
            _ => {
                o = prev;
                // walk to the start of this run
                if let Some(class) = class_at(buf, o, big) {
                    while let Some(prev) = buf.prev_char_offset(o) {
                        match class_at(buf, prev, big) {
                            Some(c) if c == class => o = prev,
                            _ => break,
                        }
                    }
                }
                return o;
            }
        }
    }
}

/// `ge` / `gE`: end of the previous word (inclusive).
///
/// A word-end position is a non-blank char whose following char starts a
/// different run (or the buffer end). We search backwards for the largest
/// such position before `offset`; when none exists the offset is unchanged.
pub fn prev_word_end(buf: &dyn VimBuffer, offset: usize, big: bool) -> usize {
    let mut o = offset;
    while let Some(prev) = buf.prev_char_offset(o) {
        o = prev;
        let Some(c) = buf.char_at(o) else { return o };
        if c.is_whitespace() {
            continue;
        }
        let at_run_end = match buf.next_char_offset(o) {
            Some(next) => class_at(buf, next, big) != class_at(buf, o, big),
            None => true,
        };
        if at_run_end {
            return o;
        }
    }
    o
}

/// `f`/`t`/`F`/`T` helpers: find `target` on the current line.
pub fn find_char_forward(
    buf: &dyn VimBuffer,
    offset: usize,
    target: char,
    till: bool,
) -> Option<usize> {
    let line = buf.offset_to_line(offset);
    let mut o = buf.next_char_offset(offset).unwrap_or(offset);
    let end = buf.line_end(line);
    while o < end {
        if buf.char_at(o) == Some(target) {
            return Some(if till {
                buf.prev_char_offset(o).unwrap_or(offset)
            } else {
                o
            });
        }
        o = buf.next_char_offset(o)?;
    }
    None
}

pub fn find_char_backward(
    buf: &dyn VimBuffer,
    offset: usize,
    target: char,
    till: bool,
) -> Option<usize> {
    let line = buf.offset_to_line(offset);
    let mut o = offset;
    let start = buf.line_start(line);
    while o > start {
        let Some(prev) = buf.prev_char_offset(o) else { break };
        o = prev;
        if buf.char_at(o) == Some(target) {
            return Some(if till {
                buf.next_char_offset(o).unwrap_or(offset)
            } else {
                o
            });
        }
    }
    None
}

const OPENERS: &[(char, char)] = &[('(', ')'), ('[', ']'), ('{', '}')];

fn bracket_pair(c: char) -> Option<(char, char, bool)> {
    // (char, other, is_opener)
    for (open, close) in OPENERS {
        if c == *open {
            return Some((c, *close, true));
        }
        if c == *close {
            return Some((c, *open, false));
        }
    }
    None
}

/// `%`: find the match of the bracket under the cursor, scanning the whole
/// buffer with nesting. Falls back to the next bracket on the line.
pub fn match_bracket(buf: &dyn VimBuffer, offset: usize) -> Option<usize> {
    // locate a bracket at or after the cursor (same line first, like vim)
    let line = buf.offset_to_line(offset);
    let mut start = None;
    let mut o = offset;
    let line_end = buf.line_end(line);
    while o <= line_end {
        if let Some((_, _, _)) = bracket_pair(buf.char_at(o)?) {
            start = Some(o);
            break;
        }
        o = buf.next_char_offset(o)?;
    }
    let start = start?;

    let (ch, other, is_opener) = bracket_pair(buf.char_at(start)?)?;
    let mut depth = 0i32;
    let mut o = start;
    if is_opener {
        while let Some(c) = buf.char_at(o) {
            if c == ch {
                depth += 1;
            } else if c == other {
                depth -= 1;
                if depth == 0 {
                    return Some(o);
                }
            }
            o = buf.next_char_offset(o)?;
        }
    } else {
        while let Some(prev) = buf.prev_char_offset(o) {
            o = prev;
            let c = buf.char_at(o)?;
            if c == ch {
                depth += 1;
            } else if c == other {
                depth -= 1;
                if depth < 0 {
                    return Some(o);
                }
            }
        }
    }
    None
}

/// `}`: start of the next paragraph boundary (a blank line), buffer end.
pub fn next_paragraph(buf: &dyn VimBuffer, offset: usize) -> usize {
    let line = buf.offset_to_line(offset);
    let mut line = line + 1;
    while line < buf.line_count() {
        if buf.line_is_blank(line) {
            return buf.line_start(line);
        }
        line += 1;
    }
    buf.len()
}

/// `{`: start of the previous paragraph boundary.
pub fn prev_paragraph(buf: &dyn VimBuffer, offset: usize) -> usize {
    let line = buf.offset_to_line(offset);
    let mut line = line.saturating_sub(1);
    while line > 0 {
        if buf.line_is_blank(line) {
            return buf.line_start(line);
        }
        line -= 1;
    }
    buf.line_start(0)
}

/// `(`: beginning of the current or previous sentence.
pub fn prev_sentence(buf: &dyn VimBuffer, offset: usize) -> usize {
    let mut o = offset;
    while let Some(prev) = buf.prev_char_offset(o) {
        o = prev;
        if matches!(buf.char_at(o), Some('.' | '!' | '?')) {
            // skip the whitespace run after the terminator
            let mut after = buf.next_char_offset(o).unwrap_or(o);
            while let Some(c) = buf.char_at(after) {
                if c.is_whitespace() {
                    after += c.len_utf8();
                } else {
                    break;
                }
            }
            if after < offset {
                return after;
            }
        }
    }
    0
}

/// `)`: beginning of the next sentence.
pub fn next_sentence(buf: &dyn VimBuffer, offset: usize) -> usize {
    let mut o = offset;
    while let Some(c) = buf.char_at(o) {
        if matches!(c, '.' | '!' | '?') {
            // skip whitespace run; sentence starts at next non-blank
            let mut after = buf.next_char_offset(o).unwrap_or(o);
            while let Some(w) = buf.char_at(after) {
                if w.is_whitespace() {
                    after += w.len_utf8();
                } else {
                    break;
                }
            }
            if buf.char_at(after).is_some() {
                return after;
            }
        }
        o += c.len_utf8();
    }
    buf.len()
}
