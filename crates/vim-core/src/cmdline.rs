//! Command-line mode: `/` and `?` search prompts with history.

use crate::key::{Key, KeyKind};
use crate::mode::Mode;
use crate::search;
use crate::state::{Ctx, KeyResult, VimState};

/// Platforms may deliver Enter, Backspace and Tab as bare control characters
/// through the text-input path (`"\n"`, `"\x7f"`, `"\t"`). Normalize them so
/// the prompt treats them like their named keys instead of pattern text.
fn normalize_control_char(key: Key) -> Key {
    if !key.modifiers.is_plain() {
        return key;
    }
    if let KeyKind::Char(c) = key.kind {
        return match c {
            '\n' | '\r' => Key::enter(),
            '\x7f' => Key::backspace(),
            '\t' => Key::tab(),
            _ => key,
        };
    }
    key
}

/// Command-line input buffer + search history.
#[derive(Default)]
pub struct Cmdline {
    pub buffer: String,
    pub history: Vec<String>,
    /// Position while browsing history with Up/Down; None = typing.
    pub history_pos: Option<usize>,
    /// In-progress input stashed while browsing history.
    pub stash: Option<String>,
}

impl VimState {
    pub(crate) fn begin_cmdline(&mut self, prompt: char) {
        self.cmdline.buffer.clear();
        self.cmdline.history_pos = None;
        self.mode = Mode::CommandLine { prompt };
    }

    /// Execute the current pattern and jump to the first match.
    fn execute_search(&mut self, ctx: &mut Ctx, forward: bool) {
        let pattern = self.cmdline.buffer.clone();
        self.mode = Mode::Normal;
        if pattern.is_empty() {
            // empty pattern: re-use the last one, like vim
            if let Some(last) = self.search.pattern.clone() {
                search::set_pattern(self, ctx, last, self.search.forward);
                self.jump_to_current_match(ctx, self.search.forward, 1);
            }
            return;
        }
        if !self.cmdline.history.last().map(|h| h == &pattern).unwrap_or(false) {
            self.cmdline.history.push(pattern.clone());
        }
        self.cmdline.history_pos = None;
        self.cmdline.stash = None;
        self.cmdline.buffer.clear();
        search::set_pattern(self, ctx, pattern, forward);
        self.jump_to_current_match(ctx, forward, 1);
        ctx.host.changed();
    }

    /// Move to the nearest match for the active pattern in `forward`.
    fn jump_to_current_match(&mut self, ctx: &mut Ctx, forward: bool, count: usize) {
        if let Some(offset) = search::jump_to_match(self, ctx.buf, forward, count) {
            self.cursor.offset = offset;
            self.cursor.desired_col = None;
            // publish with the current match marked
            let matches = self.search.last_matches.clone();
            let current = search::all_matches(self, ctx.buf, self.search.pattern.as_deref().unwrap_or(""))
                .into_iter()
                .find(|m| m.start == offset);
            ctx.host.set_search_highlights(&matches, current);
            ctx.host.scroll_to_line(ctx.buf.offset_to_line(offset));
        } else {
            ctx.host.bell();
        }
    }

    fn cancel_cmdline(&mut self, ctx: &mut Ctx) {
        self.mode = Mode::Normal;
        self.cmdline.buffer.clear();
        // restore the previous highlight set
        let matches = self.search.last_matches.clone();
        ctx.host.set_search_highlights(&matches, None);
        ctx.host.changed();
    }

    pub(crate) fn cmdline_key(&mut self, ctx: &mut Ctx, key: Key) -> KeyResult {
        let Mode::CommandLine { prompt } = self.mode else {
            return KeyResult::Consumed;
        };
        let forward = prompt == '/';
        // Some hosts/platforms deliver Enter, Backspace and Tab as bare
        // control characters through the text-input path ("\n", "\x7f", "\t").
        // Normalize them so the prompt treats them like their named keys
        // instead of appending them to the pattern.
        let key = normalize_control_char(key);
        match &key.kind {
            KeyKind::Char(c) if key.modifiers.is_plain() => {
                self.cmdline.buffer.push(*c);
                self.cmdline.history_pos = None;
                if self.options.incsearch {
                    let pattern = self.cmdline.buffer.clone();
                    search::publish_incsearch(self, ctx, &pattern);
                }
                ctx.host.changed();
                KeyResult::Consumed
            }
            KeyKind::Named(name) if key.modifiers.is_plain() => match name.as_str() {
                "enter" => {
                    self.execute_search(ctx, forward);
                    KeyResult::Consumed
                }
                "escape" => {
                    self.cancel_cmdline(ctx);
                    KeyResult::Consumed
                }
                "backspace" => {
                    if self.cmdline.buffer.pop().is_none() {
                        self.cancel_cmdline(ctx);
                    } else if self.options.incsearch {
                        let pattern = self.cmdline.buffer.clone();
                        search::publish_incsearch(self, ctx, &pattern);
                    }
                    ctx.host.changed();
                    KeyResult::Consumed
                }
                "up" | "down" => {
                    let history = &self.cmdline.history;
                    if history.is_empty() {
                        return KeyResult::Consumed;
                    }
                    let pos = match self.cmdline.history_pos {
                        None => {
                            if name == "up" {
                                self.cmdline.stash = Some(self.cmdline.buffer.clone());
                                history.len() - 1
                            } else {
                                return KeyResult::Consumed;
                            }
                        }
                        Some(pos) => {
                            if name == "up" {
                                pos.min(history.len() - 1).saturating_sub(if pos == 0 { 0 } else { 1 })
                            } else {
                                pos + 1
                            }
                        }
                    };
                    if pos >= history.len() {
                        // past the newest entry: back to typing
                        self.cmdline.history_pos = None;
                        self.cmdline.buffer = self.cmdline.stash.take().unwrap_or_default();
                    } else {
                        self.cmdline.history_pos = Some(pos);
                        self.cmdline.buffer = history[pos].clone();
                    }
                    ctx.host.changed();
                    KeyResult::Consumed
                }
                _ => KeyResult::Consumed,
            },
            _ => KeyResult::Consumed,
        }
    }
}
