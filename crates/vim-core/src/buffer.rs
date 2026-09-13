//! The host-facing text model.
//!
//! The engine never owns the buffer: the host implements [`VimBuffer`] (and
//! [`VimBufferMut`] if it wants the engine to edit). All offsets are **UTF-8
//! byte offsets**; conversion to/from UTF-16 (the gpui `InputHandler`
//! coordinate space) happens once, at the integration boundary.

use std::ops::Range;

/// Read-only text access with line semantics.
///
/// Lines are 0-based. A line's range extends over its terminating `\n` when
/// present. The final line of a buffer typically has no trailing newline.
/// An empty buffer still has exactly one line.
pub trait VimBuffer {
    /// Total length in bytes.
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Number of lines (>= 1).
    fn line_count(&self) -> usize;
    /// The character that starts at byte `offset`, if `offset` is a char
    /// boundary inside the buffer.
    fn char_at(&self, offset: usize) -> Option<char>;
    /// The byte offset of the character that *ends* at `offset`
    /// (i.e. the previous char boundary), if any.
    fn prev_char_offset(&self, offset: usize) -> Option<usize>;
    /// `[start, end)` covering `line` including its newline, if any.
    fn line_range(&self, line: usize) -> Range<usize>;
    /// The 0-based line containing byte `offset`.
    fn offset_to_line(&self, offset: usize) -> usize;
    /// Copy a byte range out of the buffer.
    fn slice(&self, range: Range<usize>) -> String;

    // ---- provided helpers -------------------------------------------------

    fn line_start(&self, line: usize) -> usize {
        self.line_range(line).start
    }

    /// Offset of the line's end, *before* the newline.
    fn line_end(&self, line: usize) -> usize {
        let range = self.line_range(line);
        if range.end > range.start && self.char_at(range.end - 1) == Some('\n') {
            range.end - '\n'.len_utf8()
        } else {
            range.end
        }
    }

    fn line_content(&self, line: usize) -> String {
        let range = self.line_range(line);
        let end = self.line_end(line);
        self.slice(range.start..end)
    }

    fn next_char_offset(&self, offset: usize) -> Option<usize> {
        let c = self.char_at(offset)?;
        Some(offset + c.len_utf8())
    }

    /// Byte length of the leading indentation of `line`, and whether the line
    /// is blank.
    fn line_indent(&self, line: usize) -> (usize, bool) {
        let start = self.line_start(line);
        let end = self.line_end(line);
        let mut offset = start;
        while offset < end {
            match self.char_at(offset) {
                Some(c @ (' ' | '\t')) => offset += c.len_utf8(),
                _ => return (offset - start, false),
            }
        }
        (offset - start, true)
    }

    /// Offset of the first non-blank character of `line` (line end if blank).
    fn first_non_blank(&self, line: usize) -> usize {
        let start = self.line_start(line);
        let (indent, _) = self.line_indent(line);
        let end = self.line_end(line);
        (start + indent).min(end)
    }

    /// Is `offset` at (or past) the end of its line, before the newline?
    fn at_line_end(&self, offset: usize) -> bool {
        let line = self.offset_to_line(offset);
        offset >= self.line_end(line)
    }

    fn line_is_blank(&self, line: usize) -> bool {
        self.line_indent(line).1
    }
}

/// Mutating text access. Every engine edit funnels through the two required
/// methods, which keeps the undo transaction boundary visible to the host.
pub trait VimBufferMut: VimBuffer {
    fn insert_text(&mut self, offset: usize, text: &str);
    fn delete_range(&mut self, range: Range<usize>);

    fn replace_range(&mut self, range: Range<usize>, text: &str) {
        self.delete_range(range.clone());
        self.insert_text(range.start, text);
    }
}

/// Clamp helper: never park the cursor inside the `\n`.
pub fn clamp_to_line_end(buf: &dyn VimBuffer, offset: usize) -> usize {
    let line = buf.offset_to_line(offset);
    offset.min(buf.line_end(line))
}

#[cfg(test)]
pub(crate) mod testbuf {
    //! A `String`-backed buffer used by the engine's own tests.

    use super::*;

    #[derive(Default)]
    pub struct TestBuffer(pub String);

    impl TestBuffer {
        pub fn new(s: &str) -> Self {
            TestBuffer(s.to_owned())
        }

        /// Assert-style helper for tests: apply `keys` starting in normal mode.
        pub fn text(&self) -> &str {
            &self.0
        }
    }

    impl VimBuffer for TestBuffer {
        fn len(&self) -> usize {
            self.0.len()
        }
        fn line_count(&self) -> usize {
            if self.0.is_empty() {
                1
            } else {
                self.0.split('\n').count()
            }
        }
        fn char_at(&self, offset: usize) -> Option<char> {
            if offset >= self.0.len() || !self.0.is_char_boundary(offset) {
                return None;
            }
            self.0[offset..].chars().next()
        }
        fn prev_char_offset(&self, offset: usize) -> Option<usize> {
            if offset == 0 || offset > self.0.len() || !self.0.is_char_boundary(offset) {
                return None;
            }
            self.0[..offset].chars().next_back().map(|c| offset - c.len_utf8())
        }
        fn line_range(&self, line: usize) -> Range<usize> {
            let mut start = 0usize;
            for (i, part) in self.0.split('\n').enumerate() {
                let part_len = part.len();
                if i == line {
                    let end = if i + 1 == self.line_count() {
                        start + part_len
                    } else {
                        start + part_len + 1
                    };
                    return start..end;
                }
                start += part_len + 1;
            }
            start..start
        }
        fn offset_to_line(&self, offset: usize) -> usize {
            let offset = offset.min(self.0.len());
            self.0[..offset].split('\n').count() - 1
        }
        fn slice(&self, range: Range<usize>) -> String {
            self.0[range].to_owned()
        }
    }

    impl VimBufferMut for TestBuffer {
        fn insert_text(&mut self, offset: usize, text: &str) {
            self.0.insert_str(offset, text);
        }
        fn delete_range(&mut self, range: Range<usize>) {
            self.0.replace_range(range, "");
        }
    }
}
