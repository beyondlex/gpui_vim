//! Command-line mode: `/` `?` search prompts and `:` Ex commands, with
//! per-prompt history.

use std::collections::HashMap;

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

/// Command-line input buffer + per-prompt history.
#[derive(Default)]
pub struct Cmdline {
    pub buffer: String,
    /// History per prompt: `/` and `?` share search history in vim, `:` has
    /// its own command history.
    pub history: HashMap<char, Vec<String>>,
    /// Position while browsing history with Up/Down; None = typing.
    pub history_pos: Option<usize>,
    /// In-progress input stashed while browsing history.
    pub stash: Option<String>,
}

impl Cmdline {
    fn history_for(&mut self, prompt: char) -> &mut Vec<String> {
        self.history.entry(prompt).or_default()
    }
}

impl VimState {
    pub(crate) fn begin_cmdline(&mut self, prompt: char) {
        self.cmdline.buffer.clear();
        self.cmdline.history_pos = None;
        self.mode = Mode::CommandLine { prompt };
    }

    /// Execute the current pattern and jump to the first match.
    fn execute_search(&mut self, ctx: &mut Ctx, pattern: String, forward: bool) {
        self.mode = Mode::Normal;
        if pattern.is_empty() {
            // empty pattern: re-use the last one, like vim
            if let Some(last) = self.search.pattern.clone() {
                search::set_pattern(self, ctx, last, self.search.forward);
                self.jump_to_current_match(ctx, self.search.forward, 1);
            }
            return;
        }
        self.cmdline.history_pos = None;
        self.cmdline.stash = None;
        search::set_pattern(self, ctx, pattern, forward);
        self.jump_to_current_match(ctx, forward, 1);
        ctx.host.changed();
    }

    /// Move to the nearest match for the active pattern in `forward`.
    fn jump_to_current_match(&mut self, ctx: &mut Ctx, forward: bool, count: usize) {
        if let Some(offset) = search::jump_to_match(self, ctx.buf, forward, count) {
            self.cursor.offset = offset;
            self.cursor.desired_col = None;
            // publish with the current match marked (respecting `hlsearch`)
            if self.options.hlsearch {
                let matches = self.search.last_matches.clone();
                let current = search::all_matches(self, ctx.buf, self.search.pattern.as_deref().unwrap_or(""))
                    .into_iter()
                    .find(|m| m.start == offset);
                ctx.host.set_search_highlights(&matches, current);
            } else {
                ctx.host.set_search_highlights(&[], None);
            }
            ctx.host.scroll_to_line(ctx.buf.offset_to_line(offset));
        } else {
            ctx.host.bell();
        }
    }

    fn cancel_cmdline(&mut self, ctx: &mut Ctx) {
        self.mode = Mode::Normal;
        self.discard_change_record();
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
        let key = normalize_control_char(key);
        match &key.kind {
            KeyKind::Char(c) if key.modifiers.is_plain() => {
                self.cmdline.buffer.push(*c);
                self.cmdline.history_pos = None;
                // incremental search applies to the search prompts only —
                // a half-typed `:set` line is not a pattern
                if self.options.incsearch && prompt != ':' {
                    let pattern = self.cmdline.buffer.clone();
                    search::publish_incsearch(self, ctx, &pattern);
                }
                ctx.host.changed();
                KeyResult::Consumed
            }
            KeyKind::Named(name) if key.modifiers.is_plain() => match name.as_str() {
                "enter" => {
                    // record into this prompt's history (dedup consecutive
                    // repeats; an empty entry reuses the previous value)
                    let entry = std::mem::take(&mut self.cmdline.buffer);
                    if !entry.is_empty() {
                        let history = self.cmdline.history_for(prompt);
                        if history.last() != Some(&entry) {
                            history.push(entry.clone());
                        }
                    }
                    self.cmdline.history_pos = None;
                    self.cmdline.stash = None;
                    if prompt == ':' {
                        self.mode = Mode::Normal;
                        self.execute_ex(ctx, &entry);
                    } else {
                        self.execute_search(ctx, entry, prompt == '/');
                    }
                    KeyResult::Consumed
                }
                "escape" => {
                    self.cancel_cmdline(ctx);
                    KeyResult::Consumed
                }
                "backspace" => {
                    if self.cmdline.buffer.pop().is_none() {
                        self.cancel_cmdline(ctx);
                    } else if self.options.incsearch && prompt != ':' {
                        let pattern = self.cmdline.buffer.clone();
                        search::publish_incsearch(self, ctx, &pattern);
                    }
                    ctx.host.changed();
                    KeyResult::Consumed
                }
                "up" | "down" => {
                    let Some(history) = self.cmdline.history.get(&prompt) else {
                        return KeyResult::Consumed;
                    };
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

    // ---- `:` Ex commands ---------------------------------------------------

    /// Execute a `:` command line. Supported in v1: `:noh[lsearch]`,
    /// `:set` (booleans, `no`/`!` forms, `name=value` numerics),
    /// `:[%]s/pat/rep/[g]`, `:w`, `:q`/`:q!`, `:wq`. Unknown commands ring
    /// the bell and return to normal mode (mode is already Normal here).
    fn execute_ex(&mut self, ctx: &mut Ctx, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        match line {
            "noh" | "nohl" | "nohlsearch" => {
                search::clear_highlights(self, ctx);
                return;
            }
            "w" | "write" => {
                ctx.host.save();
                return;
            }
            "q" | "quit" | "q!" | "quit!" => {
                ctx.host.request_close();
                return;
            }
            "wq" | "x" | "xit" => {
                ctx.host.save();
                ctx.host.request_close();
                return;
            }
            _ => {}
        }
        if let Some(rest) = line
            .strip_prefix("set")
            .filter(|rest| rest.is_empty() || rest.starts_with(' '))
        {
            self.ex_set(ctx, rest.trim_start());
            return;
        }
        if self.ex_substitute(ctx, line) {
            return;
        }
        ctx.host.bell();
    }

    /// `:set` with space-separated items: `name`, `noname`, `name!`,
    /// `name=value`. Stops at the first unknown item (bell).
    fn ex_set(&mut self, ctx: &mut Ctx, args: &str) {
        if args.is_empty() {
            // vim lists all options here; we have no message channel yet
            ctx.host.bell();
            return;
        }
        for arg in args.split_whitespace() {
            let ok = if let Some(name) = arg.strip_suffix('!') {
                match self.options.bool_option(name) {
                    Some(current) => self.options.set_boolean(name, !current),
                    None => false,
                }
            } else if let Some(name) = arg.strip_prefix("no") {
                self.options.set_boolean(name, false)
            } else if let Some((name, value)) = arg.split_once('=') {
                self.options.set_value(name, value)
            } else {
                self.options.set_boolean(arg, true)
            };
            if !ok {
                ctx.host.bell();
                return;
            }
        }
    }

    /// `:[%]s{sep}pattern{sep}replacement{sep}[flags]` — over the current
    /// line, or every line with `%`. `g` replaces all matches per line
    /// (default: first match per line). Replacement follows Rust regex
    /// expansion (`$1`, documented divergence from vim's `\1`). Returns
    /// false when `line` is not a substitute command at all.
    fn ex_substitute(&mut self, ctx: &mut Ctx, line: &str) -> bool {
        let (whole_file, rest) = match line.strip_prefix('%') {
            Some(rest) => (true, rest),
            None => (false, line),
        };
        let Some(after_s) = rest.strip_prefix('s') else {
            return false;
        };
        let Some(sep) = after_s.chars().next() else {
            ctx.host.bell();
            return true;
        };
        if sep.is_alphanumeric() {
            // `:sort` & friends are not supported; don't mangle them
            ctx.host.bell();
            return true;
        }
        let mut parts = after_s[sep.len_utf8()..].split(sep);
        let (Some(pattern), Some(replacement)) = (parts.next(), parts.next()) else {
            ctx.host.bell();
            return true;
        };
        let flags = parts.next().unwrap_or("");
        if parts.next().is_some() {
            ctx.host.bell();
            return true;
        }

        // an empty pattern reuses the last search, like vim
        let pattern = if pattern.is_empty() {
            match self.search.pattern.clone() {
                Some(p) => p,
                None => {
                    ctx.host.bell();
                    return true;
                }
            }
        } else {
            pattern.to_owned()
        };
        let Some(builder) = search::compile(self, &pattern) else {
            ctx.host.bell();
            return true;
        };
        let Ok(re) = builder.build() else {
            ctx.host.bell();
            return true;
        };
        let global = flags.contains('g');

        let first_line = if whole_file {
            0
        } else {
            ctx.buf.offset_to_line(self.cursor.offset)
        };
        let last_line = if whole_file {
            ctx.buf.line_count().saturating_sub(1)
        } else {
            first_line
        };

        let range_start = ctx.buf.line_start(first_line);
        let range_end = ctx.buf.line_end(last_line);
        let mut joined: Vec<String> = Vec::new();
        let mut total = 0usize;
        let mut last_match: Option<usize> = None;
        for line_no in first_line..=last_line {
            let ls = ctx.buf.line_start(line_no);
            let le = ctx.buf.line_end(line_no);
            let text = ctx.buf.slice(ls..le);
            let mut hits = 0usize;
            let mut last_hit: Option<usize> = None;
            let replaced = if global {
                re.replace_all(&text, |caps: &regex::Captures| {
                    let m = caps.get(0).unwrap();
                    if m.is_empty() {
                        return m.as_str().to_owned();
                    }
                    hits += 1;
                    last_hit = Some(m.start());
                    let mut out = String::new();
                    caps.expand(replacement, &mut out);
                    out
                })
            } else {
                re.replace(&text, |caps: &regex::Captures| {
                    let m = caps.get(0).unwrap();
                    if m.is_empty() {
                        return m.as_str().to_owned();
                    }
                    hits += 1;
                    last_hit = Some(m.start());
                    let mut out = String::new();
                    caps.expand(replacement, &mut out);
                    out
                })
            }
            .to_string();
            if hits > 0 {
                total += hits;
                if let Some(off) = last_hit {
                    last_match = Some(ls + off);
                }
            }
            joined.push(replaced);
        }

        if total == 0 {
            ctx.host.bell();
            return true;
        }

        self.begin_edit(ctx);
        let new_text = joined.join("\n");
        self.edit_replace(ctx, range_start..range_end, &new_text);
        self.end_edit();
        self.bump(ctx);
        if let Some(offset) = last_match {
            self.cursor.offset = crate::buffer::clamp_to_line_end(ctx.buf, offset);
            self.cursor.desired_col = None;
        }
        // `.` repeats the substitution at the cursor's line
        self.commit_change_record();
        true
    }
}
