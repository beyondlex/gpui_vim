//! ropey-backed buffer implementing the engine's `VimBuffer(Mut)` traits.
//!
//! The rope is shared with the host (for undo snapshots) through an
//! `Rc<RefCell<Rope>>`, mirroring how a real host would wire the two views.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use vim_core::buffer::VimBuffer;
use vim_core::buffer::VimBufferMut;

pub type SharedRope = Rc<RefCell<ropey::Rope>>;

pub fn shared_rope(text: &str) -> SharedRope {
    Rc::new(RefCell::new(ropey::Rope::from(text)))
}

/// Read-only view used as `Ctx::buf`.
#[derive(Clone)]
pub struct RopeBuffer(pub SharedRope);

impl RopeBuffer {
    pub fn new(text: &str) -> Self {
        RopeBuffer(shared_rope(text))
    }

    pub fn shared(&self) -> &SharedRope {
        &self.0
    }

    pub fn text(&self) -> String {
        self.0.borrow().to_string()
    }

    /// Line count with the engine's semantics: a trailing newline does not
    /// open a phantom final line.
    pub fn line_count_real(&self) -> usize {
        let rope = self.0.borrow();
        let lines = rope.len_lines();
        if lines > 1 && rope.get_char(rope.len_chars() - 1) == Some('\n') {
            lines - 1
        } else {
            lines
        }
    }

    // ---- UTF-16 helpers (the gpui InputHandler coordinate space) ----------

    pub fn byte_to_utf16(&self, byte: usize) -> usize {
        let rope = self.0.borrow();
        let byte = byte.min(rope.len_bytes());
        match rope.try_byte_to_char(byte) {
            Ok(ci) => rope.char_to_utf16_cu(ci),
            Err(_) => rope.len_utf16_cu(),
        }
    }

    pub fn utf16_to_byte(&self, utf16: usize) -> usize {
        let rope = self.0.borrow();
        let utf16 = utf16.min(rope.len_utf16_cu());
        match rope.try_utf16_cu_to_char(utf16) {
            Ok(ci) => rope.char_to_byte(ci),
            Err(_) => rope.len_bytes(),
        }
    }

    /// Clamp a byte offset onto a char boundary (and into the buffer).
    #[allow(dead_code)]
    pub fn clamp(&self, byte: usize) -> usize {
        let rope = self.0.borrow();
        let byte = byte.min(rope.len_bytes());
        match rope.try_byte_to_char(byte) {
            Ok(_) => byte,
            Err(_) => {
                // walk back to the nearest boundary
                for b in (0..byte).rev() {
                    if rope.try_byte_to_char(b).is_ok() {
                        return b;
                    }
                }
                0
            }
        }
    }
}

impl VimBuffer for RopeBuffer {
    fn len(&self) -> usize {
        self.0.borrow().len_bytes()
    }

    fn line_count(&self) -> usize {
        self.line_count_real()
    }

    fn char_at(&self, offset: usize) -> Option<char> {
        let rope = self.0.borrow();
        let ci = rope.try_byte_to_char(offset).ok()?;
        rope.get_char(ci)
    }

    fn prev_char_offset(&self, offset: usize) -> Option<usize> {
        let rope = self.0.borrow();
        if offset == 0 || offset > rope.len_bytes() {
            return None;
        }
        let ci = rope.try_byte_to_char(offset).ok()?;
        if ci == 0 {
            return None;
        }
        let prev = rope.get_char(ci - 1)?;
        Some(offset - prev.len_utf8())
    }

    fn line_range(&self, line: usize) -> Range<usize> {
        let rope = self.0.borrow();
        if line >= rope.len_lines() {
            return rope.len_bytes()..rope.len_bytes();
        }
        let start = rope.line_to_byte(line);
        let end = if line + 1 < rope.len_lines() {
            rope.line_to_byte(line + 1)
        } else {
            rope.len_bytes()
        };
        start..end
    }

    fn offset_to_line(&self, offset: usize) -> usize {
        let rope = self.0.borrow();
        let offset = offset.min(rope.len_bytes());
        let line = rope.try_byte_to_line(offset).unwrap_or(0);
        // clamp into the engine's line space (no phantom trailing line)
        line.min(self.line_count_real().saturating_sub(1))
    }

    fn slice(&self, range: Range<usize>) -> String {
        let rope = self.0.borrow();
        let end = range.end.min(rope.len_bytes());
        let start = range.start.min(end);
        // ropey's `slice` takes *char* indices; convert from bytes
        let start_char = rope.try_byte_to_char(start).unwrap_or(0);
        let end_char = rope.try_byte_to_char(end).unwrap_or(start_char);
        rope.slice(start_char..end_char).to_string()
    }
}

impl VimBufferMut for RopeBuffer {
    fn insert_text(&mut self, offset: usize, text: &str) {
        let mut rope = self.0.borrow_mut();
        let offset = offset.min(rope.len_bytes());
        let char_idx = rope.try_byte_to_char(offset).unwrap_or(rope.len_chars());
        rope.insert(char_idx, text);
    }

    fn delete_range(&mut self, range: Range<usize>) {
        let mut rope = self.0.borrow_mut();
        let end = range.end.min(rope.len_bytes());
        let start = range.start.min(end);
        let start_char = rope.try_byte_to_char(start).unwrap_or(0);
        let end_char = rope.try_byte_to_char(end).unwrap_or(start_char);
        rope.remove(start_char..end_char);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vim_core::buffer::VimBuffer as _;

    fn buf(text: &str) -> RopeBuffer {
        RopeBuffer(shared_rope(text))
    }

    #[test]
    fn line_semantics_match_engine() {
        let b = buf("alpha\nbeta\ngamma\ndelta\n");
        assert_eq!(b.line_count(), 4);
        assert_eq!(b.line_range(3), 17..23);
        assert_eq!(b.line_end(3), 22);

        let b = buf("abc");
        assert_eq!(b.line_count(), 1);
        assert_eq!(b.line_range(0), 0..3);
    }

    #[test]
    fn utf16_roundtrip_multibyte() {
        let b = buf("héllo 你好 world");
        let byte = b.line_range(0).end;
        let utf16 = b.byte_to_utf16(byte);
        assert_eq!(b.utf16_to_byte(utf16), byte);
    }
}
