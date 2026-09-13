//! Text objects: `iw aw iW aW is as ip ap`, quotes and bracket blocks.
//!
//! A text object yields a span; in Operator-Pending it feeds an operator, in
//! Visual mode it extends the selection.

use crate::buffer::VimBuffer;
use crate::word;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextObject {
    Word { inner: bool, big: bool },
    Sentence { inner: bool },
    Paragraph { inner: bool },
    Quote { inner: bool, quote: char },
    Block { inner: bool, open: char, close: char },
    Tag { inner: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectRange {
    pub start: usize,
    pub end: usize,
    pub linewise: bool,
}

impl ObjectRange {
    fn charwise(start: usize, end: usize) -> Self {
        ObjectRange {
            start,
            end,
            linewise: false,
        }
    }
}

/// Map the single-letter aliases (`b`, `B`) to their bracket pairs.
pub fn resolve_alias(c: char, inner: bool) -> Option<TextObject> {
    match c {
        'b' => Some(TextObject::Block {
            inner,
            open: '(',
            close: ')',
        }),
        'B' => Some(TextObject::Block {
            inner,
            open: '{',
            close: '}',
        }),
        _ => None,
    }
}

pub fn range(buf: &dyn VimBuffer, offset: usize, obj: TextObject) -> Option<ObjectRange> {
    match obj {
        TextObject::Word { inner, big } => word_range(buf, offset, inner, big),
        TextObject::Sentence { inner } => sentence_range(buf, offset, inner),
        TextObject::Paragraph { inner } => paragraph_range(buf, offset, inner),
        TextObject::Quote { inner, quote } => quote_range(buf, offset, inner, quote),
        TextObject::Block { inner, open, close } => block_range(buf, offset, inner, open, close),
        TextObject::Tag { inner } => tag_range(buf, offset, inner),
    }
}

fn word_range(buf: &dyn VimBuffer, offset: usize, inner: bool, big: bool) -> Option<ObjectRange> {
    let line = buf.offset_to_line(offset);
    let line_start = buf.line_start(line);
    let line_end = buf.line_end(line);

    let big_class = |o: usize| match buf.char_at(o)? {
        '\n' => None,
        c if c.is_whitespace() => Some(word::Class::Blank),
        _ => Some(word::Class::Word),
    };
    let small_class = |o: usize| buf.char_at(o).map(word::char_class);
    let class_of = |o: usize| if big { big_class(o) } else { small_class(o) };

    let Some(class) = class_of(offset.min(line_end.saturating_sub(1)).max(line_start)) else {
        return Some(ObjectRange::charwise(line_start, line_end));
    };

    // expand to the run of the same class within this line
    let mut start = offset;
    while start > line_start {
        let Some(prev) = buf.prev_char_offset(start) else { break };
        if class_of(prev) != Some(class) {
            break;
        }
        start = prev;
    }
    let mut end = buf.next_char_offset(offset).unwrap_or(offset);
    while end < line_end {
        if class_of(end) != Some(class) {
            break;
        }
        end += buf.char_at(end).map(|c| c.len_utf8()).unwrap_or(1);
    }

    if inner || class != word::Class::Blank {
        if !inner {
            // aw: prefer trailing whitespace, fall back to leading
            let mut extend = end;
            while extend < line_end {
                match buf.char_at(extend) {
                    Some(c) if c.is_whitespace() => extend += c.len_utf8(),
                    _ => break,
                }
            }
            if extend > end {
                end = extend;
            } else {
                while start > line_start {
                    let Some(prev) = buf.prev_char_offset(start) else { break };
                    match buf.char_at(prev) {
                        Some(c) if c.is_whitespace() => start = prev,
                        _ => break,
                    }
                }
            }
        }
        return Some(ObjectRange::charwise(start, end));
    }

    // cursor is on whitespace and the object is `aw`: the whitespace itself
    Some(ObjectRange::charwise(start, end))
}

fn sentence_range(buf: &dyn VimBuffer, offset: usize, inner: bool) -> Option<ObjectRange> {
    let start = word::prev_sentence(buf, (offset + 1).min(buf.len()));
    let end = word::next_sentence(buf, offset);
    if !inner {
        // outer: include trailing whitespace
        let mut o = end;
        while let Some(c) = buf.char_at(o) {
            if c.is_whitespace() {
                o += c.len_utf8();
            } else {
                break;
            }
        }
        return Some(ObjectRange::charwise(start, o.max(end)));
    }
    // inner: trim leading whitespace
    let mut trimmed_start = start;
    while let Some(c) = buf.char_at(trimmed_start) {
        if c.is_whitespace() {
            trimmed_start += c.len_utf8();
        } else {
            break;
        }
    }
    Some(ObjectRange::charwise(trimmed_start, end.max(trimmed_start)))
}

fn paragraph_range(buf: &dyn VimBuffer, offset: usize, inner: bool) -> Option<ObjectRange> {
    let line = buf.offset_to_line(offset);
    // find the start: first line of this paragraph block
    let mut first = line;
    while first > 0 && buf.line_is_blank(first - 1) == buf.line_is_blank(line) {
        if buf.line_is_blank(line) && !buf.line_is_blank(first - 1) {
            break;
        }
        if !buf.line_is_blank(line) && buf.line_is_blank(first - 1) {
            break;
        }
        first -= 1;
    }
    // find the end: last line of the block
    let mut last = line;
    while last + 1 < buf.line_count() && buf.line_is_blank(last + 1) == buf.line_is_blank(line) {
        last += 1;
    }
    if !inner && last + 1 < buf.line_count() {
        // `ap` swallows one following blank line
        last += 1;
    }
    Some(ObjectRange {
        start: buf.line_start(first),
        end: buf.line_range(last).end,
        linewise: true,
    })
}

fn quote_positions(buf: &dyn VimBuffer, offset: usize, quote: char) -> Vec<usize> {
    let line = buf.offset_to_line(offset);
    let mut positions = Vec::new();
    let mut o = buf.line_start(line);
    let end = buf.line_end(line);
    while o < end {
        if buf.char_at(o) == Some(quote) {
            // an escaped quote \" is text, not a delimiter
            let escaped = o > buf.line_start(line)
                && buf.char_at(o.saturating_sub(1)) == Some('\\');
            if !escaped {
                positions.push(o);
            }
        }
        match buf.next_char_offset(o) {
            Some(next) => o = next,
            None => break,
        }
    }
    positions
}

fn quote_range(
    buf: &dyn VimBuffer,
    offset: usize,
    inner: bool,
    quote: char,
) -> Option<ObjectRange> {
    let positions = quote_positions(buf, offset, quote);
    // pairs in order; find the pair containing the cursor, else the next one
    let mut chosen: Option<(usize, usize)> = None;
    for pair in positions.chunks(2) {
        if pair.len() < 2 {
            break;
        }
        let (open, close) = (pair[0], pair[1]);
        if open <= offset && offset <= close {
            chosen = Some((open, close));
            break;
        }
        if open > offset {
            chosen = Some((open, close));
            break;
        }
    }
    let (open, close) = chosen?;
    let range = if inner {
        ObjectRange::charwise(open + quote.len_utf8(), close)
    } else {
        ObjectRange::charwise(open, close + quote.len_utf8())
    };
    if range.start >= range.end {
        None
    } else {
        Some(range)
    }
}

fn block_range(
    buf: &dyn VimBuffer,
    offset: usize,
    inner: bool,
    open: char,
    close: char,
) -> Option<ObjectRange> {
    // scan backwards for the innermost unmatched `open`
    let mut depth = 0i32;
    let mut open_pos = None;
    let mut o = offset;
    while let Some(prev) = buf.prev_char_offset(o) {
        o = prev;
        match buf.char_at(o) {
            Some(c) if c == close => depth += 1,
            Some(c) if c == open => {
                if depth == 0 {
                    open_pos = Some(o);
                    break;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    let open_pos = open_pos?;

    // scan forwards from the cursor for the innermost unmatched `close`
    let mut depth = 0i32;
    let mut close_pos = None;
    let mut o = offset;
    while let Some(c) = buf.char_at(o) {
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth < 0 {
                close_pos = Some(o);
                break;
            }
        }
        o = buf.next_char_offset(o)?;
    }
    let close_pos = close_pos?;

    if inner {
        let start = open_pos + open.len_utf8();
        if start >= close_pos {
            return None;
        }
        Some(ObjectRange::charwise(start, close_pos))
    } else {
        Some(ObjectRange::charwise(
            open_pos,
            close_pos + close.len_utf8(),
        ))
    }
}

fn tag_range(buf: &dyn VimBuffer, offset: usize, inner: bool) -> Option<ObjectRange> {
    let text = buf.slice(0..buf.len());
    let bytes = text.as_bytes();

    // collect tag spans
    let mut tags: Vec<(usize, usize, &str, bool)> = Vec::new(); // (start, end_after_close, name, is_open)
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            if let Some(close_rel) = text[i..].find('>') {
                let inner_text = &text[i + 1..i + close_rel];
                let (name, is_open) = if let Some(rest) = inner_text.strip_prefix('/') {
                    (
                        rest.trim_end_matches('/').split_whitespace().next().unwrap_or(""),
                        false,
                    )
                } else {
                    (
                        inner_text.split_whitespace().next().unwrap_or(""),
                        !inner_text.ends_with('/'),
                    )
                };
                tags.push((i, i + close_rel + 1, name, is_open));
                i += close_rel + 1;
                continue;
            }
        }
        i += 1;
    }

    // find the innermost pair containing the cursor
    let mut stack: Vec<(usize, &str)> = Vec::new();
    let mut best: Option<(usize, usize)> = None;
    for (start, end, name, is_open) in tags {
        if is_open {
            stack.push((start, name));
        } else if let Some(pos) = stack.iter().rposition(|(_, n)| *n == name) {
            let (open_start, _) = stack[pos];
            stack.truncate(pos);
            if open_start < offset && offset < end {
                let better = match best {
                    Some((bs, _)) => open_start > bs,
                    None => true,
                };
                if better {
                    best = Some((open_start, end));
                }
            }
        }
    }
    let (open_start, close_end) = best?;
    if inner {
        let after_open = text[open_start..].find('>')? + open_start + 1;
        if after_open >= close_end {
            return None;
        }
        Some(ObjectRange::charwise(after_open, close_end))
    } else {
        Some(ObjectRange::charwise(open_start, close_end))
    }
}
