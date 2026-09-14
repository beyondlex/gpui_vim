//! Insert-mode key handling.
//!
//! Printable characters intentionally return [`KeyResult::Unknown`] (unless an
//! insert-mode mapping wants them): the host's IME path owns text input, so
//! composition (Chinese/Japanese input, accents, ...) keeps working. Text
//! arrives back through [`VimState::insert_text_at_cursor`].

use crate::key::{Key, KeyKind};
use crate::mode::Mode;
use crate::motions::Motion;
use crate::state::{Ctx, ProcessOutcome, VimState};
use crate::word;

impl VimState {
    pub(crate) fn insert_key(&mut self, ctx: &mut Ctx, key: Key) -> ProcessOutcome {
        // finish a pending <C-r>{register}
        if self.insert_register_pending {
            self.insert_register_pending = false;
            if let Some(name) = key.printable_char() {
                if let Some(data) = self.registers.get_for_paste(name, ctx.host) {
                    self.insert_text_at_cursor(ctx, &data.text);
                }
            }
            return ProcessOutcome::Consumed;
        }

        // exit insert: <Esc>, <C-[>, <C-c>
        if key == Key::escape() || key == Key::ctrl_char('[') || key == Key::ctrl_char('c') {
            self.exit_insert(ctx);
            return ProcessOutcome::Consumed;
        }

        if key.modifiers.is_plain() {
            if let KeyKind::Named(name) = &key.kind {
                match name.as_str() {
                    "enter" => {
                        self.insert_text_at_cursor(ctx, "\n");
                        return ProcessOutcome::Consumed;
                    }
                    "backspace" => {
                        self.insert_backspace(ctx);
                        return ProcessOutcome::Consumed;
                    }
                    "tab" => {
                        self.insert_tab(ctx);
                        return ProcessOutcome::Consumed;
                    }
                    "delete" => {
                        let at = self.cursor.offset;
                        if let Some(c) = ctx.buf.char_at(at) {
                            self.begin_edit(ctx);
                            self.edit_delete(ctx, at..at + c.len_utf8());
                            ctx.host.changed();
                        }
                        return ProcessOutcome::Consumed;
                    }
                    "up" | "down" => {
                        let motion = if name == "up" { Motion::Up } else { Motion::Down };
                        self.goto_motion(ctx, motion, 1);
                        return ProcessOutcome::Consumed;
                    }
                    "left" | "right" => {
                        let motion = if name == "left" { Motion::Left } else { Motion::Right };
                        self.goto_motion(ctx, motion, 1);
                        return ProcessOutcome::Consumed;
                    }
                    "home" => {
                        self.cursor.offset =
                            ctx.buf.line_start(ctx.buf.offset_to_line(self.cursor.offset));
                        return ProcessOutcome::Consumed;
                    }
                    "end" => {
                        let line = ctx.buf.offset_to_line(self.cursor.offset);
                        self.cursor.offset = ctx.buf.line_end(line);
                        return ProcessOutcome::Consumed;
                    }
                    "pageup" => {
                        self.goto_motion(ctx, Motion::PageUp, 1);
                        return ProcessOutcome::Consumed;
                    }
                    "pagedown" => {
                        self.goto_motion(ctx, Motion::PageDown, 1);
                        return ProcessOutcome::Consumed;
                    }
                    _ => {}
                }
            }
        }

        // Ctrl chords inside insert. Only alt/platform (cmd) disqualify a
        // chord — `is_plain()` additionally excludes control itself, which
        // made this whole block unreachable once.
        if key.modifiers.control && !key.modifiers.alt && !key.modifiers.platform {
            match &key.kind {
                KeyKind::Char('w') | KeyKind::Char('W') => {
                    self.insert_delete_word_before(ctx);
                    return ProcessOutcome::Consumed;
                }
                KeyKind::Char('u') => {
                    self.insert_delete_to_line_start(ctx);
                    return ProcessOutcome::Consumed;
                }
                KeyKind::Char('r') => {
                    self.insert_register_pending = true;
                    return ProcessOutcome::Consumed;
                }
                _ => return ProcessOutcome::Unknown,
            }
        }

        // printable text: let the host/IME decide *unless* an insert mapping
        // is interested (e.g. `jk` -> Esc)
        if let Some(c) = key.printable_char() {
            self.pending_unknown_char = Some(c);
            return ProcessOutcome::Unknown;
        }

        ProcessOutcome::Unknown
    }

    fn insert_backspace(&mut self, ctx: &mut Ctx) {
        let at = self.cursor.offset;
        let line_start = ctx.buf.line_start(ctx.buf.offset_to_line(at));
        if at > line_start {
            if let Some(prev) = ctx.buf.prev_char_offset(at) {
                self.begin_edit(ctx);
                self.edit_delete(ctx, prev..at);
                self.cursor.offset = prev;
                ctx.host.changed();
            }
        } else if at > 0 {
            // join with the previous line
            self.begin_edit(ctx);
            self.edit_delete(ctx, at - 1..at);
            self.cursor.offset = at - 1;
            ctx.host.changed();
        }
    }

    fn insert_tab(&mut self, ctx: &mut Ctx) {
        if self.options.expandtab {
            let sw = self.options.tabstop.max(1);
            let line = ctx.buf.offset_to_line(self.cursor.offset);
            let col = self.cursor.offset - ctx.buf.line_start(line);
            let spaces = sw - (col % sw);
            self.insert_text_at_cursor(ctx, &" ".repeat(spaces));
        } else {
            self.insert_text_at_cursor(ctx, "\t");
        }
    }

    /// `<C-w>`: delete the word before the cursor, like typing `b` then
    /// deleting. Word classes follow normal mode (`(`, `bar`, `)` are three
    /// separate words), and the deletion never crosses back over the line
    /// start.
    fn insert_delete_word_before(&mut self, ctx: &mut Ctx) {
        let at = self.cursor.offset;
        let line_start = ctx.buf.line_start(ctx.buf.offset_to_line(at));
        let target = word::prev_word_start(ctx.buf, at, false).max(line_start);
        if target < at {
            self.begin_edit(ctx);
            self.edit_delete(ctx, target..at);
            self.cursor.offset = target;
            ctx.host.changed();
        }
    }

    fn insert_delete_to_line_start(&mut self, ctx: &mut Ctx) {
        let at = self.cursor.offset;
        let line_start = ctx.buf.line_start(ctx.buf.offset_to_line(at));
        if at > line_start {
            self.begin_edit(ctx);
            self.edit_delete(ctx, line_start..at);
            self.cursor.offset = line_start;
            ctx.host.changed();
        }
    }

    /// Called when insert mode is left implicitly (host navigation etc.).
    pub fn ensure_normal_mode(&mut self, ctx: &mut Ctx) {
        if matches!(self.mode, Mode::Insert) {
            self.exit_insert(ctx);
        }
    }
}
