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
    /// History per prompt: `/` and `?` SHARE the search history (vim), `:`
    /// has its own. Keys are canonicalized through [`history_key`].
    pub history: HashMap<char, Vec<String>>,
    /// Position while browsing history with Up/Down; None = typing.
    pub history_pos: Option<usize>,
    /// In-progress input stashed while browsing history.
    pub stash: Option<String>,
    /// Last executed Ex command line (`@:` replays it).
    pub last_command: Option<String>,
}

impl Cmdline {
    /// `/` and `?` share one search history, like vim.
    fn history_key(prompt: char) -> char {
        if prompt == ':' { ':' } else { '/' }
    }

    fn history_for(&mut self, prompt: char) -> &mut Vec<String> {
        self.history.entry(Self::history_key(prompt)).or_default()
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
            // empty pattern: re-use the last one, like vim — but in the
            // direction of the CURRENT prompt (`?` + Enter repeats BACKWARD,
            // `/` + Enter forward; verified against vim 9.1)
            if let Some(last) = self.search.pattern.clone() {
                search::set_pattern(self, ctx, last, forward);
                self.jump_to_current_match(ctx, forward, 1);
                ctx.host.changed();
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
            let origin = self.cursor.offset;
            self.cursor.offset = offset;
            self.cursor.desired_col = None;
            self.record_jump(origin, offset);
            // publish with the current match marked (respecting `hlsearch`).
            // `jump_to_match` returns a `.start` from `last_matches` (it may
            // have re-scanned into it), so no second scan is needed here.
            if self.options.hlsearch {
                let matches = self.search.last_matches.clone();
                let current = matches.iter().find(|m| m.start == offset).cloned();
                ctx.host.set_search_highlights(&matches, current);
            } else {
                ctx.host.set_search_highlights(&[], None);
            }
            ctx.host.scroll_to_line(ctx.buf.offset_to_line(offset));
        } else {
            ctx.host.bell();
        }
    }

    /// Esc at the prompt: a visual `:` returns to the intact selection
    /// (vim semantics) and a second Esc from there exits visual; a plain
    /// search/ex prompt aborts back to normal mode. Highlights are restored
    /// to the pre-prompt set — incremental highlighting is preview-only and
    /// must not leak as an accepted pattern.
    fn cancel_cmdline(&mut self, ctx: &mut Ctx) {
        if let Some((kind, anchor)) = self.cmdline_visual.take() {
            self.mode = Mode::Visual { kind };
            self.visual_anchor = Some(anchor);
        } else {
            self.mode = Mode::Normal;
        }
        self.discard_change_record();
        self.cmdline.buffer.clear();
        // restore the previous highlight set
        let matches = self.search.last_matches.clone();
        ctx.host.set_search_highlights(&matches, None);
        ctx.host.changed();
    }

    /// One keystroke at the prompt. Printable chars append to the buffer
    /// (and drive incremental search on `/`/`?`); Enter executes and pushes
    /// to this prompt's history (consecutive duplicates deduped); Up/Down
    /// browse history with an in-progress stash; Esc cancels. Everything
    /// else is swallowed — a cmdline is never `Unknown` to the host.
    pub(crate) fn cmdline_key(&mut self, ctx: &mut Ctx, key: Key) -> KeyResult {
        let Mode::CommandLine { prompt } = self.mode else {
            return KeyResult::Consumed;
        };
        let key = normalize_control_char(key);
        // <C-c> cancels the prompt (vim's interrupt), not just <Esc>
        if key == Key::ctrl_char('c') {
            self.cancel_cmdline(ctx);
            return KeyResult::Consumed;
        }
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
                        if !entry.is_empty() {
                            self.cmdline.last_command = Some(entry.clone());
                        }
                        self.execute_ex(ctx, &entry);
                        // executing a visual `:` command ends visual mode
                        // (marks written, anchor cleared), like vim
                        if let Some((_kind, anchor)) = self.cmdline_visual.take() {
                            self.close_visual_after_cmdline(anchor);
                        }
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
                    let Some(history) = self.cmdline.history.get(&Cmdline::history_key(prompt))
                    else {
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
                        // stored positions are always in range and history
                        // only grows, so browsing up is just "one earlier"
                        Some(pos) if name == "up" => pos.saturating_sub(1),
                        Some(pos) => pos + 1,
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

    /// Close out a visual selection after its `:'<,'>` command ran: write
    /// the `<`/`>` marks from the final cursor position, drop the anchor and
    /// return to normal mode. The cursor is NOT moved here (contrast
    /// `cancel_cmdline`, which restores the selection untouched — vim lets
    /// the executed command decide where the cursor ends up).
    fn close_visual_after_cmdline(&mut self, anchor: usize) {
        let cursor = self.cursor.offset;
        let (lo, hi) = if anchor <= cursor {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        };
        self.marks.set('<', lo);
        self.marks.set('>', hi);
        self.marks.last_visual = Some((lo, hi + 1));
        self.visual_anchor = None;
        self.marks.active_visual = None;
        self.mode = Mode::Normal;
    }

    // ---- `:` Ex commands ---------------------------------------------------

    /// Execute a `:` command line. Supported in v1: `:noh[lsearch]`,
    /// `:set` (booleans, `no`/`!` forms, `name=value` numerics),
    /// `:[%]s/pat/rep/[g]`, `:[range]d[elete]`, `:w`, `:q`/`:q!`, `:wq`/`:x`,
    /// `:bn[ext]`/`:bp[revious]`, and IdeaVim's `:action <id>` bridge.
    /// Unknown commands get E492 and return to normal mode (mode is already
    /// Normal here).
    pub(crate) fn execute_ex(&mut self, ctx: &mut Ctx, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        // the range prefix is parsed off before command dispatch; `None`
        // means "no range typed" — commands then apply their own default
        let (range, line) = match Self::parse_range(self, ctx, line) {
            Some(parsed) => parsed,
            None => {
                ctx.host.status_message("E16: Invalid range");
                ctx.host.bell();
                return;
            }
        };
        let line = line.trim();
        if line.is_empty() {
            // a bare `:5` moves to line 5 (first non-blank); a plain `:`
            // (no range) is a no-op
            if let Some((first, _)) = range {
                let line_no = first.min(ctx.buf.line_count().saturating_sub(1));
                self.cursor.offset = ctx.buf.first_non_blank(line_no);
                self.cursor.desired_col = None;
                ctx.host.scroll_to_line(line_no);
            }
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
            "q" | "quit" => {
                // not forced: the host may refuse (vim E37 on modified)
                ctx.host.request_close_forced(false);
                return;
            }
            "q!" | "quit!" => {
                ctx.host.request_close_forced(true);
                return;
            }
            "wq" | "x" | "xit" => {
                ctx.host.save();
                ctx.host.request_close_forced(true);
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
        if line == "bnext" || line == "bn" {
            if !ctx.host.cycle_buffer(true) {
                ctx.host.bell();
            }
            return;
        }
        if line == "bprev" || line == "bprevious" || line == "bp" {
            if !ctx.host.cycle_buffer(false) {
                ctx.host.bell();
            }
            return;
        }
        // IdeaVim's host-action bridge: :action SomeId dispatches the host
        // application action by id (typically the RHS of a :map)
        if let Some(id) = line.strip_prefix("action").filter(|r| r.starts_with(' ')) {
            let id = id.trim();
            if id.is_empty() {
                ctx.host.bell();
            } else {
                // strict = the layer wants misses reported (host-specific
                // rc); lenient = shared-rc misses should pass silently
                ctx.host
                    .dispatch_host_action_hinted(id, !self.lenient_actions);
            }
            return;
        }
        // `:s` without a range defaults to the current line
        let range = range.unwrap_or_else(|| {
            let current = ctx.buf.offset_to_line(self.cursor.offset);
            (current, current)
        });
        if self.ex_substitute(ctx, line, range) {
            return;
        }
        // :{range}d[elete] — delete the range's lines
        if let Some(rest) =
            Self::boundary_cmd(line, "d").or_else(|| Self::boundary_cmd(line, "delete"))
        {
            let _register = rest.trim(); // named registers not supported
            self.ex_delete_lines(ctx, range);
            return;
        }
        ctx.host
            .status_message(&format!("E492: Not an editor command: {line}"));
        ctx.host.bell();
    }

    /// `cmd` at the line start with a command boundary (end, space, `!`).
    fn boundary_cmd<'a>(line: &'a str, cmd: &str) -> Option<&'a str> {
        line.strip_prefix(cmd)
            .filter(|rest| rest.is_empty() || rest.starts_with(' ') || rest.starts_with('!'))
    }

    /// Parse an Ex range prefix: `%`, `.`, `$`, `'`, numbers, each with an
    /// optional `+n`/`-n` offset, joined by `,` or `;`. Returns the resolved
    /// inclusive line range (`None` when no prefix was typed — the command
    /// then decides its own default) and the remainder of the line (the
    /// command). Note: vim's `;` sets the cursor to each intermediate
    /// address; here `,` and `;` are treated alike (documented divergence).
    fn parse_range<'a>(
        vim: &VimState,
        ctx: &Ctx,
        line: &'a str,
    ) -> Option<(Option<(usize, usize)>, &'a str)> {
        fn base_line(spec: &str, vim: &VimState, ctx: &Ctx) -> Option<usize> {
            match spec {
                "." | "" => Some(ctx.buf.offset_to_line(vim.cursor.offset)),
                // "%" is handled by the caller before per-address parsing;
                // treat it as unusable here
                "%" => None,
                "$" => Some(ctx.buf.line_count().saturating_sub(1)),
                "'<" => vim
                    .marks
                    .active_visual()
                    .map(|(a, _)| ctx.buf.offset_to_line(a))
                    .or_else(|| {
                        vim.marks
                            .resolve('<')
                            .map(|off| ctx.buf.offset_to_line(off))
                    }),
                "'>" => vim
                    .marks
                    .active_visual()
                    .map(|(_, b)| ctx.buf.offset_to_line(b.saturating_sub(1)))
                    .or_else(|| {
                        vim.marks
                            .resolve('>')
                            .map(|off| ctx.buf.offset_to_line(off))
                    }),
                // `'a`-style marks: resolve through the mark table (the range
                // scanner accepts any `'x`; dropping them here made
                // `:'a,'b d` fail with E16 even though they parsed)
                other if other.len() == 2 && other.starts_with('\'') => other[1..]
                    .chars()
                    .next()
                    .and_then(|name| vim.marks.resolve(name))
                    .map(|off| ctx.buf.offset_to_line(off.min(ctx.buf.len()))),
                other => other.parse::<usize>().ok().map(|n| n.saturating_sub(1)),
            }
        }
        fn with_offset(base: usize, spec: &str) -> usize {
            match spec.strip_prefix('-') {
                Some(n) => base.saturating_sub(n.parse::<usize>().unwrap_or(0)),
                None => match spec.strip_prefix('+') {
                    Some(n) => base + n.parse::<usize>().unwrap_or(0),
                    _ => base,
                },
            }
        }
        // split off the range part: a command starts at the first letter
        // that is not part of a `'<` / `'>` mark spec. Scan the allowed
        // range alphabet manually.
        let bytes = line.as_bytes();
        let mut range_end = 0usize;
        let mut i = 0usize;
        while i < bytes.len() {
            let c = bytes[i] as char;
            if c == '\'' {
                // mark spec: ' + one char
                i += 2;
                range_end = i.min(bytes.len());
                continue;
            }
            if c.is_ascii_digit()
                || matches!(c, '.' | '$' | '%' | ',' | ';' | '+' | '-' | '>' | ' ')
            {
                i += 1;
                range_end = i;
                continue;
            }
            break;
        }
        let (range_part, rest) = line.split_at(range_end);
        if range_part.is_empty() {
            // no prefix: the command applies its own default
            return Some((None, line));
        }
        if range_part.trim_end() == "%" {
            let last = ctx.buf.line_count().saturating_sub(1);
            return Some((Some((0, last)), rest.trim_start()));
        }
        let last = ctx.buf.line_count().saturating_sub(1);
        let mut first: Option<usize> = None;
        let mut last_line: Option<usize> = None;
        let mut previous: Option<usize> = None;
        for part in range_part.split([',', ';']) {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let (base_str, off_str) = part
                .find(['+', '-'])
                .map(|i| part.split_at(i))
                .unwrap_or((part, ""));
            // a bare `+n` / `-n` offsets the PREVIOUS address (vim: `.,+1`
            // is two addresses); an absent previous defaults to the cursor
            let value = match base_line(base_str, vim, ctx) {
                Some(base) => with_offset(base, off_str),
                None if base_str.is_empty() => {
                    let base =
                        previous.unwrap_or_else(|| ctx.buf.offset_to_line(vim.cursor.offset));
                    with_offset(base, off_str)
                }
                None => return None,
            };
            let value = value.min(last);
            previous = Some(value);
            if first.is_none() {
                first = Some(value);
            }
            last_line = Some(value);
        }
        match (first, last_line) {
            (Some(first), Some(last)) => {
                Some((Some((first.min(last), first.max(last))), rest.trim_start()))
            }
            _ => None,
        }
    }

    /// `:{range}d` — delete the lines of the range (single undo step),
    /// cursor to the first non-blank of the line that took their place.
    fn ex_delete_lines(&mut self, ctx: &mut Ctx, (first, last): (usize, usize)) {
        let last = last.min(ctx.buf.line_count().saturating_sub(1));
        let start = ctx.buf.line_start(first);
        let end = ctx.buf.line_range(last).end;
        if start >= end {
            ctx.host.bell();
            return;
        }
        self.begin_edit(ctx);
        self.edit_delete(ctx, start..end);
        self.bump(ctx);
        let below = ctx.buf.line_count().saturating_sub(1);
        let line = first.min(below);
        self.cursor.offset = ctx.buf.first_non_blank(line);
        self.cursor.desired_col = None;
        ctx.host.changed();
        self.commit_change_record();
    }

    /// `:set` with space-separated items: `name`, `noname`, `name!`,
    /// `name=value`. Stops at the first unknown item (bell).
    fn ex_set(&mut self, ctx: &mut Ctx, args: &str) {
        if args.is_empty() {
            // vim lists all options here; we have no message channel yet
            ctx.host.bell();
            return;
        }
        let mut applied = false;
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
            applied = true;
        }
        // search options (ic/isd/…) change how the next scan must run: drop
        // the cached match list so `n` re-scans under the new options
        if applied {
            self.search.matches_generation = None;
        }
    }

    /// `:[%]s{sep}pattern{sep}replacement{sep}[flags]` — over the current
    /// line, or every line with `%`. `g` replaces all matches per line
    /// (default: first match per line). Replacement follows Rust regex
    /// expansion (`$1`, documented divergence from vim's `\1`). Returns
    /// false when `line` is not a substitute command at all.
    fn ex_substitute(&mut self, ctx: &mut Ctx, line: &str, range: (usize, usize)) -> bool {
        let Some(after_s) = line.strip_prefix('s') else {
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
        let Some(mut builder) = search::compile(self, &pattern) else {
            ctx.host.bell();
            return true;
        };
        // `i` forces case-insensitive for this substitution, `I` forces
        // case-sensitive (overriding ignorecase/smartcase, like vim)
        if flags.contains('i') {
            builder.case_insensitive(true);
        } else if flags.contains('I') {
            builder.case_insensitive(false);
        }
        let Ok(re) = builder.build() else {
            ctx.host.bell();
            return true;
        };
        let global = flags.contains('g');

        let (first_line, last_line) = range;

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
            // one counting replacer serves both modes: `replace` stops after
            // the first match, `replace_all` runs to the end of the line
            let count_replacements = |caps: &regex::Captures| -> String {
                let m = caps.get(0).unwrap();
                if m.is_empty() {
                    // empty matches would be counted once per position
                    return m.as_str().to_owned();
                }
                hits += 1;
                last_hit = Some(m.start());
                let mut out = String::new();
                caps.expand(replacement, &mut out);
                out
            };
            let replaced = if global {
                re.replace_all(&text, count_replacements)
            } else {
                re.replace(&text, count_replacements)
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
            ctx.host
                .status_message(&format!("E486: Pattern not found: {pattern}"));
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
        ctx.host.status_message(&format!("{total} substitutions"));
        // `.` repeats the substitution at the cursor's line
        self.commit_change_record();
        true
    }
}
