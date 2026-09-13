//! Engine state and the key-handling pipeline.
//!
//! The pipeline mirrors IdeaVim's `KeyHandler` consumer chain, simplified for
//! a synchronous engine:
//!
//! ```text
//! mapping queue → char-argument → count → register → operator-pending
//!   → command trie (per phase) → fall back to the host (Unknown)
//! ```
//!
//! `KeyResult::Unknown` is the contract with the host: an unknown key is fed
//! back to gpui's normal key handling (keymap bindings, IME, ...).

use crate::buffer::{clamp_to_line_end, VimBuffer, VimBufferMut};
use crate::cmdline::Cmdline;
use crate::host::VimHost;
use crate::key::{Key, KeyKind, Modifiers};
use crate::keymap::{self, Keymaps, MappingMatch};
use crate::marks::Marks;
use crate::mode::{Mode, VisualKind};
use crate::motions::Motion;
use crate::ops::{self, Operator};
use crate::options::Options;
use crate::registers::Registers;
use crate::search::SearchState;
use crate::tables::{mapping_class_for, CmdKind, CommandTables, NormalCmd, Phase, VisualCmd};
use std::collections::VecDeque;
use std::collections::HashMap;
use std::ops::Range;

/// Buffer + host pair threaded through all engine calls.
pub struct Ctx<'a> {
    pub buf: &'a mut dyn VimBufferMut,
    pub host: &'a mut dyn VimHost,
}

/// What happened to a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyResult {
    /// The engine used (or deliberately swallowed) the key.
    Consumed,
    /// The engine does not handle this key: the host should process it.
    Unknown,
}

/// How an insert session was started (drives cursor placement + `Esc`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertKind {
    Insert,             // i
    Append,             // a
    InsertFirstNonBlank, // I
    AppendLineEnd,      // A
    OpenLine { below: bool }, // o / O
    InsertAtColumnZero, // gI
    Change,             // c / s / S / C
    Replace,            // R: overwrite instead of insert
}

/// Synthetic pending-key marker for a recorded [`RecordedStep::Text`]: when
/// the `.` replay reaches it, the stashed text is applied through
/// `insert_text_at_cursor` instead of the key pipeline. Not producible by
/// `Key::parse`, so it can never collide with real keys or mappings.
pub(crate) const DOT_TEXT_MARKER: &str = "\u{0}dot-text";

/// One recorded step of the last change, for `.` repeat. Typed text is
/// recorded as [`RecordedStep::Text`] (it never goes through the key
/// pipeline — on macOS it arrives via the IME — so replay must not feed it
/// back as keys either, or the host would insert it a second time).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RecordedStep {
    Key(Key),
    Text(String),
}

/// State collected while a char-argument command waits for its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CharArgCmd {
    Find { forward: bool, till: bool },
    Replace,
    MarkSet,
    JumpMark { linewise: bool },
    /// `q{reg}` / the trailing `q` that stops recording.
    MacroRecord,
    /// `@{reg}` / `@@`.
    MacroPlay,
}

#[derive(Clone, Copy, Debug)]
pub struct Cursor {
    pub offset: usize,
    /// Visual column (bytes from line start) kept across vertical moves,
    /// like vim's `wv_col`. `None` = derive from offset.
    pub desired_col: Option<usize>,
}

/// An in-progress insert session: one undo group + `'^` bookkeeping.
#[derive(Clone, Copy, Debug)]
pub(crate) struct InsertSession {
    /// Kept for future per-kind behaviors (e.g. `{count}R` repeating the
    /// entered text); exit behavior is currently uniform across kinds.
    #[allow(dead_code)]
    kind: InsertKind,
    #[allow(dead_code)]
    start_offset: usize,
    /// The undo group id (the host merges all session edits into one group).
    #[allow(dead_code)]
    group_id: u64,
}

/// The vim engine. Hosts embed one per buffer/editor.
pub struct VimState {
    pub mode: Mode,
    pub cursor: Cursor,
    pub(crate) visual_anchor: Option<usize>,
    pub(crate) last_visual: Option<(usize, usize, VisualKind)>,

    // pending command assembly
    count: Option<usize>,
    register: Option<char>,
    register_pending: bool,
    op: Option<Operator>,
    op_count: Option<usize>,
    cmd_seq: Vec<Key>,
    char_arg_cmd: Option<CharArgCmd>,

    pub(crate) char_arg: Option<char>,
    pub(crate) last_find: Option<(char, bool, bool)>,

    pending_keys: VecDeque<Key>,
    map_depth: usize,

    /// `.` repeat: the last change as replayable steps, plus the in-progress
    /// recording. Text typed during an insert session is recorded as
    /// [`RecordedStep::Text`]. Visual-mode changes are not repeatable (v1).
    last_change: Vec<RecordedStep>,
    recording: Vec<RecordedStep>,
    recording_mutated: bool,
    recording_blocked: bool,
    /// Set while `.` replays: keys flow through the pipeline but recording
    /// and committing are suppressed so `last_change` stays put.
    replaying: bool,
    replay_texts: VecDeque<String>,
    /// Hosts suppress recording while IME composition previews mutate the
    /// buffer; only the committed text becomes part of a `.` repeat.
    recording_suppressed: bool,

    /// Macro registers (`q`/`@`). Stored as recorded steps (not register
    /// text): the notation round-trip through `Key::parse` is lossy for
    /// named keys, and typed text never flows through the key pipeline.
    macros: HashMap<char, Vec<RecordedStep>>,
    /// Active `q` recording: the register and the steps captured so far.
    macro_capture: Option<(char, Vec<RecordedStep>)>,
    /// Last register executed with `@` (for `@@`).
    last_macro_played: Option<char>,

    pub(crate) insert_session: Option<InsertSession>,
    pub(crate) insert_register_pending: bool,

    pub options: Options,
    pub registers: Registers,
    pub marks: Marks,
    pub search: SearchState,
    pub cmdline: Cmdline,
    pub keymaps: Keymaps,
    tables: CommandTables,

    undo_seq: u64,
    open_undo: Option<u64>,
    /// Platform plumbing (gpui-vim): the last printable char the key
    /// interceptor declined in insert mode. On Linux/Windows the platform
    /// delivers that same char again through the text-input path, where it
    /// must be placed without re-running the pipeline.
    pub(crate) pending_unknown_char: Option<char>,
}

impl Default for VimState {
    fn default() -> Self {
        Self::new()
    }
}

impl VimState {
    pub fn new() -> Self {
        VimState {
            mode: Mode::Normal,
            cursor: Cursor {
                offset: 0,
                desired_col: None,
            },
            visual_anchor: None,
            last_visual: None,
            count: None,
            register: None,
            register_pending: false,
            op: None,
            op_count: None,
            cmd_seq: Vec::new(),
            char_arg_cmd: None,
            char_arg: None,
            last_find: None,
            pending_keys: VecDeque::new(),
            map_depth: 0,
            last_change: Vec::new(),
            recording: Vec::new(),
            recording_mutated: false,
            recording_blocked: false,
            replaying: false,
            replay_texts: VecDeque::new(),
            recording_suppressed: false,
            macros: HashMap::new(),
            macro_capture: None,
            last_macro_played: None,
            insert_session: None,
            insert_register_pending: false,
            options: Options::default(),
            registers: Registers::default(),
            marks: Marks::default(),
            search: SearchState::default(),
            cmdline: Cmdline::default(),
            keymaps: Keymaps::default(),
            tables: CommandTables::build(),
            undo_seq: 0,
            open_undo: None,
            pending_unknown_char: None,
        }
    }

    // ---- read API for hosts ------------------------------------------------

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn cursor_offset(&self) -> usize {
        self.cursor.offset
    }

    /// `(anchor, cursor, kind)` while in visual mode (raw, unnormalized).
    pub fn visual_selection(&self) -> Option<(usize, usize, VisualKind)> {
        let anchor = self.visual_anchor?;
        Some((anchor, self.cursor.offset, match self.mode {
            Mode::Visual { kind } => kind,
            _ => VisualKind::Char,
        }))
    }

    /// Status-bar mode text (`-- INSERT --` etc.).
    pub fn mode_indicator(&self) -> &'static str {
        self.mode.indicator()
    }

    /// Pending-key display for `showcmd` (e.g. `"3d"` while typing `3dd`).
    pub fn showcmd(&self) -> String {
        if !self.options.showcmd {
            return String::new();
        }
        let mut s = String::new();
        if let Some(r) = self.register {
            s.push('"');
            s.push(r);
        }
        if let Some(c) = self.count.or(self.op_count) {
            s.push_str(&c.to_string());
        }
        if let Some(op) = self.op {
            s.push_str(op_keys(op));
        }
        for key in &self.cmd_seq {
            s.push_str(&key.notation());
        }
        if self.char_arg_cmd.is_some() {
            s.push_str(self.char_arg.map(|c| c.to_string()).as_deref().unwrap_or(""));
        }
        if matches!(self.mode, Mode::Visual { .. }) {
            if let Some((_, _, kind)) = self.visual_selection() {
                s.push_str(kind.indicator());
            }
        }
        if s.is_empty() {
            return String::new();
        }
        s
    }

    /// Cursor width hint: block in normal/visual modes, bar in insert.
    pub fn cursor_is_block(&self) -> bool {
        !matches!(self.mode, Mode::Insert | Mode::Replace)
    }

    pub fn options_mut(&mut self) -> &mut Options {
        &mut self.options
    }

    pub fn keymaps_mut(&mut self) -> &mut Keymaps {
        &mut self.keymaps
    }

    /// The register currently recording a macro (`q`), for host status UI.
    pub fn macro_recording(&self) -> Option<char> {
        self.macro_capture.as_ref().map(|(reg, _)| *reg)
    }

    /// Host-side recording suppression: while set, text placed into the
    /// buffer is NOT recorded for `.` repeat. Wrap IME composition preview
    /// mutations (the raw pinyin) with this; only the committed text should
    /// be repeatable.
    pub fn set_recording_suppressed(&mut self, suppressed: bool) {
        self.recording_suppressed = suppressed;
    }

    /// Platform plumbing (see [`VimState::take_pending_unknown_char`]).
    pub fn set_pending_unknown_char(&mut self, c: Option<char>) {
        self.pending_unknown_char = c;
    }

    /// Platform plumbing: the printable char the key interceptor declined in
    /// insert mode and which the platform will deliver a second time through
    /// the text-input path.
    pub fn take_pending_unknown_char(&mut self) -> Option<char> {
        self.pending_unknown_char.take()
    }

    /// Column helper for vertical motions.
    pub(crate) fn desired_column(&self, buf: &dyn VimBuffer) -> usize {
        match self.cursor.desired_col {
            Some(col) => col,
            None => {
                let line = buf.offset_to_line(self.cursor.offset);
                self.cursor.offset - buf.line_start(line)
            }
        }
    }

    // ---- top-level entry ---------------------------------------------------

    /// Feed one keystroke into the engine.
    pub fn handle_key(&mut self, ctx: &mut Ctx, mut key: Key) -> KeyResult {
        // an escape is an escape no matter which modifiers the platform
        // layered on top of it (hyper-key taps like caps-lock→Esc mappings
        // can release their modifiers in the same event). This runs *before*
        // the Cmd passthrough so `<D-Esc>` still exits insert mode.
        if matches!(&key.kind, KeyKind::Named(name) if name == "escape") {
            key.modifiers = Modifiers::NONE;
        }

        // A printable key's shift flag is redundant: the character itself
        // already encodes it (macOS hands over `I` as shift+i with key_char
        // "I", `$` as shift+4 with key_char "$"), while commands and mappings
        // are declared as plain chars (`Key::parse("I")`, `Key::parse("$")`).
        // Drop the flag so shifted keys hit the same command-table entries —
        // otherwise every uppercase letter and shifted punctuation misses.
        if key.modifiers.shift && key.modifiers.is_plain() && matches!(key.kind, KeyKind::Char(_)) {
            key.modifiers.shift = false;
        }

        // other host command chords (Cmd-…) always pass through
        if key.modifiers.platform {
            return KeyResult::Unknown;
        }

        if matches!(self.mode, Mode::CommandLine { .. }) {
            // the pipeline loop never runs in cmdline mode — record here so
            // `.` can replay Ex commands typed into the prompt
            if !self.replaying {
                self.recording.push(RecordedStep::Key(key.clone()));
                if let Some((_, keys)) = &mut self.macro_capture {
                    keys.push(RecordedStep::Key(key.clone()));
                }
            }
            return self.cmdline_key(ctx, key);
        }

        self.pending_keys.push_back(key);
        let class = mapping_class_for(self.mode);
        let mut guard = 0usize;
        let mut any_unknown = false;

        while let Some(front) = self.pending_keys.front().cloned() {
            guard += 1;
            if guard > 500 {
                self.pending_keys.clear();
                self.reset_pending();
                self.discard_change_record();
                self.replaying = false;
                self.replay_texts.clear();
                return KeyResult::Consumed;
            }

            // user mappings (not while an operator is pending: vim uses
            // :omap there, which we do not support yet)
            if self.op.is_none() {
                let contiguous: &[Key] = self.pending_keys.make_contiguous();
                match keymap::lookup(self.keymaps.table(class), contiguous) {
                    MappingMatch::Match { used, expansion } => {
                        self.pending_keys.drain(..used);
                        for key in expansion.into_iter().rev() {
                            self.pending_keys.push_front(key);
                        }
                        self.map_depth += 1;
                        if self.map_depth > 100 {
                            self.pending_keys.clear();
                            self.reset_pending();
                            ctx.host.bell();
                            return KeyResult::Consumed;
                        }
                        continue;
                    }
                    MappingMatch::Waiting => return KeyResult::Consumed,
                    MappingMatch::None => {}
                }
            }

            self.pending_keys.pop_front();
            // replayed text is applied inline, not through the key pipeline
            if front.kind == KeyKind::Named(DOT_TEXT_MARKER.to_owned()) {
                if let Some(text) = self.replay_texts.pop_front() {
                    self.insert_text_at_cursor(ctx, &text);
                }
                continue;
            }
            if !self.replaying {
                self.recording.push(RecordedStep::Key(front.clone()));
                if let Some((_, keys)) = &mut self.macro_capture {
                    keys.push(RecordedStep::Key(front.clone()));
                }
            }
            match self.process_key(ctx, front) {
                ProcessOutcome::Consumed => {}
                ProcessOutcome::Unknown => any_unknown = true,
                ProcessOutcome::Feed(keys) => {
                    for key in keys.into_iter().rev() {
                        self.pending_keys.push_front(key);
                    }
                }
            }
            if matches!(self.mode, Mode::CommandLine { .. }) && !self.replaying {
                break;
            }
        }
        self.map_depth = 0;
        if self.pending_keys.is_empty() && self.replaying {
            self.replaying = false;
            self.recording.clear();
            self.recording_mutated = false;
            self.replay_texts.clear();
        }
        if any_unknown {
            KeyResult::Unknown
        } else {
            KeyResult::Consumed
        }
    }

    fn process_key(&mut self, ctx: &mut Ctx, key: Key) -> ProcessOutcome {
        match self.mode {
            Mode::Normal => self.normal_key(ctx, key),
            Mode::Visual { .. } => self.visual_key(ctx, key),
            Mode::Insert | Mode::Replace => self.insert_key(ctx, key),
            Mode::CommandLine { .. } => {
                self.cmdline_key(ctx, key);
                ProcessOutcome::Consumed
            }
        }
    }

    fn reset_pending(&mut self) {
        self.count = None;
        self.register = None;
        self.register_pending = false;
        self.op = None;
        self.op_count = None;
        self.cmd_seq.clear();
        self.char_arg_cmd = None;
        self.char_arg = None;
    }

    // ---- undo grouping -----------------------------------------------------

    /// Open (or reuse) the undo group for the current logical command.
    pub(crate) fn begin_edit(&mut self, ctx: &mut Ctx) {
        if !self.replaying {
            self.recording_mutated = true;
        }
        if self.open_undo.is_none() {
            self.undo_seq += 1;
            let id = self.undo_seq;
            self.open_undo = Some(id);
            ctx.host.begin_undo_group(id, self.cursor.offset);
        }
    }

    /// Close any open undo group (end of a logical command or insert session).
    pub(crate) fn end_edit(&mut self) {
        self.open_undo = None;
    }

    pub(crate) fn bump(&mut self, ctx: &mut Ctx) {
        self.marks.last_change = Some(self.cursor.offset);
        ctx.host.changed();
    }

    // ---- buffer edits (the ONLY mutation paths; keep marks in sync) -------

    /// All engine buffer mutations go through these three wrappers so marks
    /// (`a-z`, `^ . < >`) and the last-visual span shift with the text. Never
    /// call `ctx.buf.insert_text/delete_range/replace_range` directly.
    pub(crate) fn edit_insert(&mut self, ctx: &mut Ctx, at: usize, text: &str) {
        if text.is_empty() {
            return;
        }
        ctx.buf.insert_text(at, text);
        let len = text.len();
        self.marks.adjust_insert(at, len);
        if let Some((a, b, _)) = &mut self.last_visual {
            if *a > at {
                *a += len;
            }
            if *b > at {
                *b += len;
            }
        }
    }

    pub(crate) fn edit_delete(&mut self, ctx: &mut Ctx, range: Range<usize>) {
        if range.start >= range.end {
            return;
        }
        ctx.buf.delete_range(range.clone());
        self.marks.adjust_delete(range.clone());
        if let Some((a, b, _)) = &mut self.last_visual {
            if *a >= range.end {
                *a -= range.len();
            } else if *a > range.start {
                *a = range.start;
            }
            if *b >= range.end {
                *b -= range.len();
            } else if *b > range.start {
                *b = range.start;
            }
        }
    }

    pub(crate) fn edit_replace(&mut self, ctx: &mut Ctx, range: Range<usize>, text: &str) {
        if range.start >= range.end {
            return self.edit_insert(ctx, range.start, text);
        }
        ctx.buf.replace_range(range.clone(), text);
        let new_len = text.len();
        self.marks.adjust_replace(range.clone(), new_len);
        let delta = new_len as isize - range.len() as isize;
        if let Some((a, b, _)) = &mut self.last_visual {
            let apply = |pos: &mut usize| {
                if *pos >= range.end {
                    *pos = (*pos as isize + delta).max(0) as usize;
                } else if *pos > range.start {
                    *pos = range.start;
                }
            };
            apply(a);
            apply(b);
        }
    }

    // ---- movement ------------------------------------------------------------

    pub(crate) fn apply_motion_result(
        &mut self,
        ctx: &mut Ctx,
        motion: Motion,
        result: crate::motions::MotionResult,
    ) {
        self.cursor.offset = clamp_to_line_end(ctx.buf, result.offset);
        let preserves_column = matches!(
            motion,
            Motion::Up
                | Motion::Down
                | Motion::ScrollHalfDown
                | Motion::ScrollHalfUp
                | Motion::PageUp
                | Motion::PageDown
        );
        if result.kind == crate::motions::MotionKind::Linewise {
            if preserves_column {
                // keep desired column, clamp offset into the line
                let line = ctx.buf.offset_to_line(self.cursor.offset);
                let desired = self.desired_column(ctx.buf);
                let end = ctx.buf.line_end(line);
                self.cursor.offset = ctx.buf.line_start(line) + desired.min(end - ctx.buf.line_start(line));
            } else {
                let line = ctx.buf.offset_to_line(self.cursor.offset);
                self.cursor.offset = ctx.buf.first_non_blank(line);
                self.cursor.desired_col = None;
            }
        } else if !preserves_column {
            self.cursor.desired_col = None;
        }
        let line = ctx.buf.offset_to_line(self.cursor.offset);
        ctx.host.scroll_to_line(line);
    }

    pub(crate) fn goto_motion(&mut self, ctx: &mut Ctx, motion: Motion, count: usize) -> bool {
        let result = motion.target(self, ctx, count);
        if !result.moved {
            return false;
        }
        self.apply_motion_result(ctx, motion, result);
        true
    }

    // ---- insert sessions -------------------------------------------------------

    pub(crate) fn begin_insert(&mut self, ctx: &mut Ctx, kind: InsertKind) {
        // Reuse an open undo group when one exists: the change family (c/s/S/C)
        // deletes the span through a group that is already open, and the
        // deletion + subsequent typing must undo as ONE step. Without this the
        // host would snapshot between deletion and typing, so the first `u`
        // only undid the typing and a second one was needed for the deletion.
        let group_id = match self.open_undo {
            Some(id) => id,
            None => {
                self.undo_seq += 1;
                let id = self.undo_seq;
                self.open_undo = Some(id);
                ctx.host.begin_undo_group(id, self.cursor.offset);
                id
            }
        };
        self.insert_session = Some(InsertSession {
            kind,
            start_offset: self.cursor.offset,
            group_id,
        });
        self.mode = if kind == InsertKind::Replace {
            Mode::Replace
        } else {
            Mode::Insert
        };
        self.cursor.desired_col = None;
    }

    pub(crate) fn exit_insert(&mut self, ctx: &mut Ctx) {
        // back one char unless at the line start (Replace mode too: vim
        // leaves the cursor on the last replaced character)
        let line_start = ctx.buf.line_start(ctx.buf.offset_to_line(self.cursor.offset));
        if self.cursor.offset > line_start {
            if let Some(prev) = ctx.buf.prev_char_offset(self.cursor.offset) {
                self.cursor.offset = prev.max(line_start);
            }
        }
        self.marks.last_insert_exit = Some(self.cursor.offset);
        self.marks.set('^', self.cursor.offset);
        self.commit_change_record();
        self.insert_session = None;
        self.end_edit();
        self.mode = Mode::Normal;
        self.insert_register_pending = false;
        ctx.host.changed();
    }

    /// Insert `text` at the cursor (the IME path, and internal insert helpers).
    ///
    /// Only meaningful in insert/replace mode. Newlines get autoindent.
    pub fn insert_text_at_cursor(&mut self, ctx: &mut Ctx, text: &str) {
        if !matches!(self.mode, Mode::Insert | Mode::Replace) || text.is_empty() {
            return;
        }
        self.begin_edit(ctx);
        let indent_chars = ctx
            .buf
            .slice(
                ctx.buf.line_start(ctx.buf.offset_to_line(self.cursor.offset))
                    ..ctx.buf.line_start(ctx.buf.offset_to_line(self.cursor.offset))
                        + self.current_line_indent(ctx),
            );
        let expanded = if self.options.autoindent && !indent_chars.is_empty() {
            text.replace('\n', &format!("\n{indent_chars}"))
        } else {
            text.to_owned()
        };
        let at = self.cursor.offset;
        if self.mode == Mode::Replace {
            // overwrite up to the text length, then insert the remainder
            let mut end = at;
            let line_end = ctx.buf.line_end(ctx.buf.offset_to_line(at));
            for _ in 0..expanded.chars().count() {
                let Some(next) = ctx.buf.next_char_offset(end) else { break };
                if next > line_end {
                    break;
                }
                end = next;
            }
            self.edit_replace(ctx, at..end.min(line_end.max(at)), &expanded);
        } else {
            self.edit_insert(ctx, at, &expanded);
        }
        self.cursor.offset = at + expanded.len();
        if self.insert_session.is_some()
            && !self.replaying
            && !self.recording_suppressed
            && !text.is_empty()
        {
            match self.recording.last_mut() {
                Some(RecordedStep::Text(existing)) => existing.push_str(text),
                _ => self.recording.push(RecordedStep::Text(text.to_owned())),
            }
            if let Some((_, keys)) = &mut self.macro_capture {
                keys.push(RecordedStep::Text(text.to_owned()));
            }
        }
        ctx.host.changed();
    }

    fn current_line_indent(&self, ctx: &Ctx) -> usize {
        let line = ctx.buf.offset_to_line(self.cursor.offset);
        let (indent, _) = ctx.buf.line_indent(line);
        indent
    }

    /// Replace an arbitrary range (IME committed composition text).
    pub fn replace_range(&mut self, ctx: &mut Ctx, range: Range<usize>, text: &str) {
        // an IME commit during an insert session is recorded as typed text
        if self.insert_session.is_some()
            && !self.replaying
            && !self.recording_suppressed
            && !text.is_empty()
        {
            match self.recording.last_mut() {
                Some(RecordedStep::Text(existing)) => existing.push_str(text),
                _ => self.recording.push(RecordedStep::Text(text.to_owned())),
            }
            if let Some((_, keys)) = &mut self.macro_capture {
                keys.push(RecordedStep::Text(text.to_owned()));
            }
        }
        self.begin_edit(ctx);
        self.edit_replace(ctx, range.clone(), text);
        // place the cursor at the end of the replacement when it touches it
        if range.contains(&self.cursor.offset) || self.cursor.offset == range.end {
            self.cursor.offset = range.start + text.len();
        } else if self.cursor.offset > range.end {
            self.cursor.offset += text.len().saturating_sub(range.len());
        }
        ctx.host.changed();
    }

    /// Host-initiated cursor move (e.g. a mouse click).
    pub fn set_cursor_offset(&mut self, buf: &dyn VimBuffer, offset: usize) {
        let offset = clamp_to_line_end(buf, offset.min(buf.len()));
        self.cursor.offset = offset;
        self.cursor.desired_col = None;
        if matches!(self.mode, Mode::Visual { .. }) {
            self.visual_anchor = Some(offset);
        }
    }

    /// Host-initiated visual selection (e.g. a mouse drag).
    pub fn set_visual_range(&mut self, buf: &dyn VimBuffer, anchor: usize, cursor: usize) {
        self.visual_anchor = Some(clamp_to_line_end(buf, anchor.min(buf.len())));
        self.cursor.offset = clamp_to_line_end(buf, cursor.min(buf.len()));
        if !matches!(self.mode, Mode::Visual { .. }) {
            self.mode = Mode::Visual { kind: VisualKind::Char };
        }
    }

    // ---- visual helpers ---------------------------------------------------------

    #[allow(dead_code)]
    pub(crate) fn enter_visual(&mut self, kind: VisualKind) {
        self.visual_anchor = Some(self.cursor.offset);
        self.mode = Mode::Visual { kind };
    }

    pub(crate) fn exit_visual(&mut self, ctx: &mut Ctx) {
        if let Some((anchor, cursor, kind)) = self.visual_selection() {
            let (lo, hi) = if anchor <= cursor { (anchor, cursor) } else { (cursor, anchor) };
            self.marks.last_visual = Some((lo, hi + 1));
            self.marks.set('<', lo);
            self.marks.set('>', hi);
            let _ = kind;
            self.cursor.offset = lo;
        }
        self.last_visual = self
            .visual_selection()
            .map(|(a, c, k)| (a.min(c), c.max(a), k));
        self.visual_anchor = None;
        self.mode = Mode::Normal;
        self.discard_change_record();
        ctx.host.changed();
    }

    /// Restore the cursor to a visual range start (used after visual ops).
    pub(crate) fn finish_visual_op(&mut self, ctx: &mut Ctx) {
        self.discard_change_record();
        if let Some((anchor, cursor, kind)) = self.visual_selection() {
            let (lo, hi) = if anchor <= cursor { (anchor, cursor) } else { (cursor, anchor) };
            self.marks.last_visual = Some((lo, hi + 1));
            self.last_visual = Some((lo, hi + 1, kind));
        }
        self.visual_anchor = None;
        if !matches!(self.mode, Mode::Insert | Mode::Replace) {
            self.mode = Mode::Normal;
        }
        self.cursor.offset = clamp_to_line_end(ctx.buf, self.cursor.offset);
        ctx.host.changed();
    }
}

fn op_keys(op: Operator) -> &'static str {
    match op {
        Operator::Delete => "d",
        Operator::Change => "c",
        Operator::Yank => "y",
        Operator::IndentLeft => "<",
        Operator::IndentRight => ">",
        Operator::Lowercase => "gu",
        Operator::Uppercase => "gU",
        Operator::ToggleCase => "g~",
    }
}

pub(crate) enum ProcessOutcome {
    Consumed,
    Unknown,
    Feed(Vec<Key>),
}

impl VimState {
    // ---- normal mode -----------------------------------------------------

    fn normal_key(&mut self, ctx: &mut Ctx, key: Key) -> ProcessOutcome {
        // 1. complete a pending char-argument
        if self.char_arg_cmd.is_some() {
            return self.complete_char_arg(ctx, key);
        }
        if self.register_pending {
            self.register_pending = false;
            if let Some(c) = key.printable_char() {
                self.register = Some(c);
                return ProcessOutcome::Consumed;
            }
            self.register = None;
            ctx.host.bell();
            return ProcessOutcome::Consumed;
        }

        // 2. operator doubling: dd / yy / >> / guu / g~~ ...
        if self.op.is_some() && self.cmd_seq.is_empty() {
            if let (KeyKind::Char(c), true) = (&key.kind, key.modifiers.is_plain()) {
                if Self::operator_trigger(self.op.unwrap()) == Some(*c) {
                    let count = self.take_total_count();
                    let line = ctx.buf.offset_to_line(self.cursor.offset);
                    let last = (line + count - 1).min(ctx.buf.line_count() - 1);
                    let span = ops::OpSpan {
                        start: ctx.buf.line_start(line),
                        end: ctx.buf.line_range(last).end,
                        linewise: true,
                    };
                    self.complete_operator_with_span(ctx, span);
                    return ProcessOutcome::Consumed;
                }
            }
        }

        // 3. a pending trie walk
        if !self.cmd_seq.is_empty() {
            let phase = if self.op.is_some() { Phase::Pending } else { Phase::Normal };
            let mut seq = self.cmd_seq.clone();
            seq.push(key.clone());
            match self.tables.trie(phase).get(&seq) {
                keymap::Walk::Hit(kind) => {
                    self.cmd_seq.clear();
                    return self.execute_command(ctx, *kind);
                }
                keymap::Walk::Pending => {
                    self.cmd_seq = seq;
                    return ProcessOutcome::Consumed;
                }
                keymap::Walk::Miss => {
                    // execute the longest terminal prefix, re-feed the rest
                    if let Some((len, kind)) = self.tables.longest_terminal(phase, &seq) {
                        self.cmd_seq.clear();
                        let rest = seq[len..].to_vec();
                        let outcome = self.execute_command(ctx, kind);
                        if rest.is_empty() {
                            return outcome;
                        }
                        return ProcessOutcome::Feed(rest);
                    }
                    // nothing matched: drop the first key, retry the rest
                    let first = self.cmd_seq.remove(0);
                    let mut rest = self.cmd_seq.clone();
                    self.cmd_seq.clear();
                    rest.push(key);
                    ctx.host.bell();
                    let _ = first;
                    if rest.is_empty() {
                        return ProcessOutcome::Consumed;
                    }
                    return ProcessOutcome::Feed(rest);
                }
            }
        }

        // 4. count digits
        if let KeyKind::Char(c) = &key.kind {
            if key.modifiers.is_plain() && c.is_ascii_digit() {
                let d = c.to_digit(10).unwrap() as usize;
                if !(d == 0 && self.count.is_none()) {
                    self.count = Some(self.count.unwrap_or(0) * 10 + d);
                    return ProcessOutcome::Consumed;
                }
                // 0 falls through to the trie (line-start motion)
            }
        }

        // 5. register prefix
        if key.kind == KeyKind::Char('"') && key.modifiers.is_plain() {
            self.register_pending = true;
            return ProcessOutcome::Consumed;
        }

        // 6. escape clears pending state; with search highlights showing it
        //    also dismisses them (`:noh` semantics) — the next search or
        //    `n`/`N` re-publishes them
        if key == Key::escape() || key == Key::ctrl_char('[') {
            self.reset_pending();
            self.discard_change_record();
            if !self.search.last_matches.is_empty() {
                crate::search::clear_highlights(self, ctx);
            }
            return ProcessOutcome::Consumed;
        }

        // 7. search prompts & the Ex command line
        if key.modifiers.is_plain() {
            match &key.kind {
                KeyKind::Char('/') => {
                    self.begin_cmdline('/');
                    return ProcessOutcome::Consumed;
                }
                KeyKind::Char('?') => {
                    self.begin_cmdline('?');
                    return ProcessOutcome::Consumed;
                }
                KeyKind::Char(':') => {
                    self.begin_cmdline(':');
                    return ProcessOutcome::Consumed;
                }
                _ => {}
            }
        }

        // 8. arrow / navigation keys
        if let Some(outcome) = self.navigation_key(ctx, &key) {
            return outcome;
        }

        // 9. the command trie
        let phase = if self.op.is_some() { Phase::Pending } else { Phase::Normal };
        let single = [key.clone()];
        match self.tables.trie(phase).get(&single) {
            keymap::Walk::Hit(kind) => self.execute_command(ctx, *kind),
            keymap::Walk::Pending => {
                self.cmd_seq.push(key);
                ProcessOutcome::Consumed
            }
            keymap::Walk::Miss => {
                if key.modifiers.control || key.modifiers.alt {
                    return ProcessOutcome::Unknown;
                }
                self.reset_pending();
                ctx.host.bell();
                ProcessOutcome::Consumed
            }
        }
    }

    /// Arrow keys etc. behave like their vim equivalents in normal/visual.
    fn navigation_key(&mut self, ctx: &mut Ctx, key: &Key) -> Option<ProcessOutcome> {
        if !key.modifiers.is_plain() {
            return None;
        }
        let name = match &key.kind {
            KeyKind::Named(name) => name.as_str(),
            _ => return None,
        };
        let motion = match name {
            "left" => Motion::Left,
            "right" => Motion::Right,
            "up" => Motion::Up,
            "down" => Motion::Down,
            "home" => Motion::LineStart,
            "end" => Motion::LineEnd,
            "pageup" => Motion::PageUp,
            "pagedown" => Motion::PageDown,
            "delete" => {
                let count = self.count.take().unwrap_or(1);
                self.begin_edit(ctx);
                ops::delete_chars(self, ctx, count, false);
                self.end_edit();
                self.bump(ctx);
                return Some(ProcessOutcome::Consumed);
            }
            _ => return None,
        };
        let count = self.count.take().unwrap_or(1);
        if !self.goto_motion(ctx, motion, count) {
            ctx.host.bell();
        }
        Some(ProcessOutcome::Consumed)
    }

    // ---- visual mode ---------------------------------------------------------

    fn visual_key(&mut self, ctx: &mut Ctx, key: Key) -> ProcessOutcome {
        if self.char_arg_cmd.is_some() {
            return self.complete_char_arg(ctx, key);
        }
        if self.register_pending {
            self.register_pending = false;
            if let Some(c) = key.printable_char() {
                self.register = Some(c);
                return ProcessOutcome::Consumed;
            }
            self.register = None;
            ctx.host.bell();
            return ProcessOutcome::Consumed;
        }

        if key == Key::escape() || key == Key::ctrl_char('[') {
            self.reset_pending();
            self.exit_visual(ctx);
            return ProcessOutcome::Consumed;
        }

        if !self.cmd_seq.is_empty() {
            let mut seq = self.cmd_seq.clone();
            seq.push(key.clone());
            match self.tables.trie(Phase::Visual).get(&seq) {
                keymap::Walk::Hit(kind) => {
                    self.cmd_seq.clear();
                    return self.execute_command(ctx, *kind);
                }
                keymap::Walk::Pending => {
                    self.cmd_seq = seq;
                    return ProcessOutcome::Consumed;
                }
                keymap::Walk::Miss => {
                    if let Some((len, kind)) = self.tables.longest_terminal(Phase::Visual, &seq) {
                        self.cmd_seq.clear();
                        let rest = seq[len..].to_vec();
                        let outcome = self.execute_command(ctx, kind);
                        return if rest.is_empty() {
                            outcome
                        } else {
                            ProcessOutcome::Feed(rest)
                        };
                    }
                    self.cmd_seq.clear();
                    ctx.host.bell();
                    return ProcessOutcome::Consumed;
                }
            }
        }

        if let KeyKind::Char(c) = &key.kind {
            if key.modifiers.is_plain() && c.is_ascii_digit() {
                let d = c.to_digit(10).unwrap() as usize;
                if !(d == 0 && self.count.is_none()) {
                    self.count = Some(self.count.unwrap_or(0) * 10 + d);
                    return ProcessOutcome::Consumed;
                }
            }
        }
        if key.kind == KeyKind::Char('"') && key.modifiers.is_plain() {
            self.register_pending = true;
            return ProcessOutcome::Consumed;
        }
        if let Some(outcome) = self.navigation_key(ctx, &key) {
            return outcome;
        }

        let single = [key.clone()];
        match self.tables.trie(Phase::Visual).get(&single) {
            keymap::Walk::Hit(kind) => self.execute_command(ctx, *kind),
            keymap::Walk::Pending => {
                self.cmd_seq.push(key);
                ProcessOutcome::Consumed
            }
            keymap::Walk::Miss => {
                if key.modifiers.control || key.modifiers.alt {
                    return ProcessOutcome::Unknown;
                }
                ctx.host.bell();
                ProcessOutcome::Consumed
            }
        }
    }

    // ---- operator completion & command execution -----------------------------

    /// Execute a resolved command. `count`/`register` come from pending state.
    pub(crate) fn execute_command(&mut self, ctx: &mut Ctx, kind: CmdKind) -> ProcessOutcome {
        // `q` while recording stops immediately (vim semantics) — it must not
        // wait for a char argument, or the NEXT key would be eaten as the
        // "stop key" and the real trailing `q` would stay in the macro
        if kind == CmdKind::Normal(NormalCmd::RecordMacro) && self.macro_capture.is_some() {
            if let Some((reg, mut keys)) = self.macro_capture.take() {
                keys.pop(); // drop the stopping `q` (captured by the hook)
                self.macros.insert(reg, keys);
                // vim: the recording register counts as "used", so `@@`
                // replays it right after recording
                self.last_macro_played = Some(reg);
            }
            return ProcessOutcome::Consumed;
        }
        // char-argument commands wait for their argument first
        if kind.takes_char() {
            self.char_arg_cmd = Some(match kind {
                CmdKind::Motion(Motion::FindChar { forward, till }) => {
                    CharArgCmd::Find { forward, till }
                }
                CmdKind::Motion(Motion::MarkJump { linewise })
                | CmdKind::Normal(NormalCmd::JumpMark { linewise }) => {
                    CharArgCmd::JumpMark { linewise }
                }
                CmdKind::Normal(NormalCmd::ReplaceChar) => CharArgCmd::Replace,
                CmdKind::Normal(NormalCmd::MarkSet) => CharArgCmd::MarkSet,
                CmdKind::Normal(NormalCmd::RecordMacro) => CharArgCmd::MacroRecord,
                CmdKind::Normal(NormalCmd::PlayMacro) => CharArgCmd::MacroPlay,
                _ => unreachable!("takes_char out of sync"),
            });
            return ProcessOutcome::Consumed;
        }
        match kind {
            CmdKind::Motion(mut motion) => {
                let count = self.take_total_count();
                // `cw` on a word char acts like `ce` (keeps trailing space)
                if self.op == Some(Operator::Change) {
                    if let Motion::WordStart { big } = motion {
                        if !big {
                            if let Some(c) = ctx.buf.char_at(self.cursor.offset) {
                                if !c.is_whitespace() {
                                    motion = Motion::WordEnd { big };
                                }
                            }
                        }
                    }
                }
                if self.op.is_some() {
                    let result = motion.target(self, ctx, count);
                    if !result.moved {
                        ctx.host.bell();
                        self.reset_pending();
                        return ProcessOutcome::Consumed;
                    }
                    let span = ops::span_from_motion(self, ctx.buf, motion, result);
                    self.complete_operator_with_span(ctx, span);
                } else {
                    if !self.goto_motion(ctx, motion, count) {
                        ctx.host.bell();
                    }
                }
                self.end_command();
                ProcessOutcome::Consumed
            }
            CmdKind::Object(object) => {
                match self.mode {
                    Mode::Visual { .. } => {
                        // extend the selection to the object
                        let Some(span) = ops::object_span(self, ctx.buf, object) else {
                            ctx.host.bell();
                            return ProcessOutcome::Consumed;
                        };
                        self.visual_anchor = Some(span.start);
                        let end = span.end.min(ctx.buf.len());
                        self.cursor.offset = ctx
                            .buf
                            .prev_char_offset(end)
                            .unwrap_or(span.start);
                        self.cursor.desired_col = None;
                    }
                    _ if self.op.is_some() => {
                        let Some(span) = ops::object_span(self, ctx.buf, object) else {
                            ctx.host.bell();
                            self.reset_pending();
                            return ProcessOutcome::Consumed;
                        };
                        self.complete_operator_with_span(ctx, span);
                    }
                    _ => ctx.host.bell(),
                }
                ProcessOutcome::Consumed
            }
            CmdKind::Operator(op) => {
                if matches!(self.mode, Mode::Visual { .. }) {
                    self.apply_visual_operator(ctx, op);
                } else {
                    self.op = Some(op);
                    self.op_count = None;
                }
                ProcessOutcome::Consumed
            }
            CmdKind::EnterInsert(insert) => {
                self.start_insert(ctx, insert);
                self.end_command();
                ProcessOutcome::Consumed
            }
            CmdKind::EnterVisual(kind) => {
                self.enter_visual(kind);
                self.end_command();
                ProcessOutcome::Consumed
            }
            CmdKind::Normal(cmd) => {
                self.execute_normal_cmd(ctx, cmd);
                self.end_command();
                ProcessOutcome::Consumed
            }
            CmdKind::Visual(cmd) => {
                self.execute_visual_cmd(ctx, cmd);
                self.end_command();
                ProcessOutcome::Consumed
            }
        }
    }

    /// Apply an operator to the current visual selection and leave visual mode
    /// (unless the operator opened insert, e.g. `c`).
    fn apply_visual_operator(&mut self, ctx: &mut Ctx, op: Operator) {
        // visual-mode changes are not `.`-repeatable in v1
        self.recording_blocked = true;
        let Some(span) = ops::span_from_visual(self, ctx.buf) else {
            ctx.host.bell();
            return;
        };
        self.begin_edit(ctx);
        ops::apply(self, ctx, op, &span, self.register);
        // an operator that entered insert mode (visual `c`) keeps its group
        // open so deletion + typing undo as one step
        if self.insert_session.is_none() {
            self.end_edit();
        }
        self.bump(ctx);
        self.reset_pending();
        if matches!(self.mode, Mode::Visual { .. }) {
            self.finish_visual_op(ctx);
        }
    }

    /// An operator got its motion/object (from the trie or a doubled key).
    pub(crate) fn complete_operator_with_span(&mut self, ctx: &mut Ctx, span: ops::OpSpan) {
        let Some(op) = self.op.take() else { return };
        self.op_count = None;
        self.begin_edit(ctx);
        ops::apply(self, ctx, op, &span, self.register);
        // an operator that entered insert mode (cw/ciw/cc) keeps its group
        // open so deletion + typing undo as one step
        if self.insert_session.is_none() {
            self.end_edit();
        }
        self.bump(ctx);
        self.reset_pending();
        if self.insert_session.is_none() {
            self.commit_change_record();
        }
    }

    fn take_total_count(&mut self) -> usize {
        let pre = self.count.take().unwrap_or(1);
        let post = self.op_count.take().unwrap_or(1);
        pre * post
    }

    /// End of a complete top-level command: commit the recording if the
    /// command mutated the buffer. An active insert session commits later,
    /// in `exit_insert` (the session is part of the same change).
    pub(crate) fn commit_change_record(&mut self) {
        if self.replaying {
            return;
        }
        if self.recording_blocked {
            self.discard_change_record();
        } else if self.recording_mutated && !self.recording.is_empty() {
            self.last_change = std::mem::take(&mut self.recording);
            self.recording_mutated = false;
        } else {
            self.recording.clear();
            self.recording_mutated = false;
        }
    }

    /// Visual-mode changes are not repeatable in v1: discard the recording.
    pub(crate) fn discard_change_record(&mut self) {
        self.recording.clear();
        self.recording_mutated = false;
        self.recording_blocked = false;
    }

    /// End of a complete top-level command.
    fn end_command(&mut self) {
        if self.insert_session.is_none() {
            self.commit_change_record();
        }
        self.count = None;
        self.register = None;
        self.register_pending = false;
        self.cmd_seq.clear();
        if self.insert_session.is_none() {
            self.end_edit();
        }
    }

    /// The operator's own key, for `dd`/`yy`/`>>` and `guu`/`g~~` doubling.
    pub(crate) fn operator_trigger(op: Operator) -> Option<char> {
        match op {
            Operator::Delete => Some('d'),
            Operator::Change => Some('c'),
            Operator::Yank => Some('y'),
            Operator::IndentLeft => Some('<'),
            Operator::IndentRight => Some('>'),
            Operator::Lowercase => Some('u'),
            Operator::Uppercase => Some('U'),
            Operator::ToggleCase => Some('~'),
        }
    }

    pub(crate) fn execute_normal_cmd(&mut self, ctx: &mut Ctx, cmd: NormalCmd) {
        match cmd {
            NormalCmd::DeleteCharForward => {
                let count = self.take_total_count();
                self.begin_edit(ctx);
                ops::delete_chars(self, ctx, count, false);
                self.end_edit();
                self.bump(ctx);
            }
            NormalCmd::DeleteCharBackward => {
                let count = self.take_total_count();
                self.begin_edit(ctx);
                ops::delete_chars(self, ctx, count, true);
                self.end_edit();
                self.bump(ctx);
            }
            NormalCmd::SubstituteChar => {
                let count = self.take_total_count();
                self.begin_edit(ctx);
                ops::delete_chars(self, ctx, count, false);
                self.start_insert(ctx, InsertKind::Change);
                self.bump(ctx);
            }
            NormalCmd::SubstituteLine => {
                let line = ctx.buf.offset_to_line(self.cursor.offset);
                let span = ops::OpSpan {
                    start: ctx.buf.line_start(line),
                    end: ctx.buf.line_range(line).end,
                    linewise: true,
                };
                self.begin_edit(ctx);
                ops::apply(self, ctx, Operator::Change, &span, self.register);
                self.bump(ctx);
            }
            NormalCmd::ChangeToEnd => {
                let line = ctx.buf.offset_to_line(self.cursor.offset);
                let span = ops::OpSpan {
                    start: self.cursor.offset,
                    end: ctx.buf.line_end(line),
                    linewise: false,
                };
                if span.end > span.start {
                    self.begin_edit(ctx);
                    ops::apply(self, ctx, Operator::Change, &span, self.register);
                    self.bump(ctx);
                } else {
                    self.start_insert(ctx, InsertKind::AppendLineEnd);
                }
            }
            NormalCmd::DeleteToEnd => {
                let line = ctx.buf.offset_to_line(self.cursor.offset);
                let span = ops::OpSpan {
                    start: self.cursor.offset,
                    end: ctx.buf.line_end(line),
                    linewise: false,
                };
                if span.end > span.start {
                    self.begin_edit(ctx);
                    ops::apply(self, ctx, Operator::Delete, &span, self.register);
                    self.bump(ctx);
                }
            }
            NormalCmd::YankLine => {
                let count = self.take_total_count();
                let line = ctx.buf.offset_to_line(self.cursor.offset);
                let last = (line + count - 1).min(ctx.buf.line_count() - 1);
                let span = ops::OpSpan {
                    start: ctx.buf.line_start(line),
                    end: ctx.buf.line_range(last).end,
                    linewise: true,
                };
                ops::yank_span(self, ctx, &span, self.register);
            }
            NormalCmd::ReplaceChar => {
                self.char_arg_cmd = Some(CharArgCmd::Replace);
            }
            NormalCmd::ToggleChar => {
                let count = self.take_total_count();
                self.begin_edit(ctx);
                ops::toggle_chars(self, ctx, count);
                self.end_edit();
                self.bump(ctx);
            }
            NormalCmd::PutAfter => {
                let count = self.take_total_count();
                let register = self.register.unwrap_or(crate::registers::UNNAMED);
                self.begin_edit(ctx);
                ops::put(self, ctx, register, count, true);
                self.end_edit();
                self.bump(ctx);
            }
            NormalCmd::PutBefore => {
                let count = self.take_total_count();
                let register = self.register.unwrap_or(crate::registers::UNNAMED);
                self.begin_edit(ctx);
                ops::put(self, ctx, register, count, false);
                self.end_edit();
                self.bump(ctx);
            }
            NormalCmd::Join => {
                let count = self.take_total_count();
                self.begin_edit(ctx);
                ops::join_lines(self, ctx, count, false);
                self.end_edit();
                self.bump(ctx);
            }
            NormalCmd::JoinLiteral => {
                let count = self.take_total_count();
                self.begin_edit(ctx);
                ops::join_lines(self, ctx, count, true);
                self.end_edit();
                self.bump(ctx);
            }
            NormalCmd::Undo => {
                let count = self.take_total_count();
                for _ in 0..count {
                    if let Some(offset) = ctx.host.undo() {
                        self.cursor.offset = clamp_to_line_end(ctx.buf, offset.min(ctx.buf.len()));
                    } else {
                        ctx.host.bell();
                        break;
                    }
                }
                ctx.host.changed();
            }
            NormalCmd::Redo => {
                let count = self.take_total_count();
                for _ in 0..count {
                    if let Some(offset) = ctx.host.redo() {
                        self.cursor.offset = clamp_to_line_end(ctx.buf, offset.min(ctx.buf.len()));
                    } else {
                        ctx.host.bell();
                        break;
                    }
                }
                ctx.host.changed();
            }
            NormalCmd::MarkSet => {
                self.char_arg_cmd = Some(CharArgCmd::MarkSet);
            }
            NormalCmd::RecordMacro => {
                self.char_arg_cmd = Some(CharArgCmd::MacroRecord);
            }
            NormalCmd::PlayMacro => {
                self.char_arg_cmd = Some(CharArgCmd::MacroPlay);
            }
            NormalCmd::JumpMark { linewise } => {
                self.char_arg_cmd = Some(CharArgCmd::JumpMark { linewise });
            }
            NormalCmd::LinewiseOp(op) => {
                let count = self.take_total_count();
                let line = ctx.buf.offset_to_line(self.cursor.offset);
                let last = (line + count - 1).min(ctx.buf.line_count() - 1);
                let span = ops::OpSpan {
                    start: ctx.buf.line_start(line),
                    end: ctx.buf.line_range(last).end,
                    linewise: true,
                };
                self.begin_edit(ctx);
                ops::apply(self, ctx, op, &span, self.register);
                self.end_edit();
                self.bump(ctx);
            }
            NormalCmd::ScrollCenter | NormalCmd::ScrollTop | NormalCmd::ScrollBottom => {
                let line = ctx.buf.offset_to_line(self.cursor.offset);
                // hosts implement the actual scroll; notify with the line
                ctx.host.scroll_to_line(line);
            }
            NormalCmd::RepeatChange => {
                let count = self.take_total_count().max(1);
                if self.last_change.is_empty() {
                    ctx.host.bell();
                    return;
                }
                let steps = self.last_change.clone();
                self.replaying = true;
                // the `.` key itself is already in the recording — drop it
                // so the next change doesn't start with a stale `.` step
                self.recording.clear();
                self.recording_mutated = false;
                for _ in 0..count {
                    for step in &steps {
                        match step {
                            RecordedStep::Key(key) => self.pending_keys.push_back(key.clone()),
                            RecordedStep::Text(text) => {
                                self.replay_texts.push_back(text.clone());
                                self.pending_keys
                                    .push_back(Key::named(DOT_TEXT_MARKER));
                            }
                        }
                    }
                }
            }
            NormalCmd::RestoreVisual => {
                if let Some((lo, hi, kind)) = self.last_visual {
                    self.visual_anchor = Some(lo);
                    self.cursor.offset = hi.saturating_sub(1).min(ctx.buf.len());
                    self.mode = Mode::Visual { kind };
                }
            }
        }
    }

    pub(crate) fn execute_visual_cmd(&mut self, ctx: &mut Ctx, cmd: VisualCmd) {
        match cmd {
            VisualCmd::Exit => {
                self.exit_visual(ctx);
            }
            VisualCmd::ToggleKind { to } => {
                let current = match self.mode {
                    Mode::Visual { kind } => kind,
                    _ => return,
                };
                let target = match to {
                    'v' => VisualKind::Char,
                    _ => VisualKind::Line,
                };
                if current == target {
                    self.exit_visual(ctx);
                } else {
                    self.mode = Mode::Visual { kind: target };
                }
            }
            VisualCmd::SwapEnds => {
                if let Some((anchor, cursor, kind)) = self.visual_selection() {
                    self.visual_anchor = Some(cursor);
                    self.cursor.offset = anchor;
                    let _ = kind;
                }
            }
            VisualCmd::PutReplace => {
                let register = self.register.unwrap_or(crate::registers::UNNAMED);
                let Some(span) = ops::span_from_visual(self, ctx.buf) else {
                    return;
                };
                // stash the paste content before the deletion rewrites registers
                let stashed = self.registers.get_for_paste(register, ctx.host);
                self.begin_edit(ctx);
                ops::delete_span(self, ctx, &span, self.register);
                if let Some(data) = stashed {
                    let repeated = data.text.repeat(self.take_total_count().max(1));
                    if data.kind == crate::registers::RegisterKind::Linewise {
                        let text = if repeated.ends_with('\n') {
                            repeated
                        } else {
                            format!("{repeated}\n")
                        };
                        let at = self.cursor.offset;
                        self.edit_insert(ctx, at, &text);
                        self.cursor.offset = ctx.buf.first_non_blank(ctx.buf.offset_to_line(at));
                    } else {
                        let at = self.cursor.offset.min(ctx.buf.len());
                        self.edit_insert(ctx, at, &repeated);
                        // cursor on the last pasted char's START — byte - 1
                        // would sit inside a multi-byte character
                        let end = at + repeated.len();
                        self.cursor.offset = ctx
                            .buf
                            .prev_char_offset(end)
                            .unwrap_or(at);
                    }
                }
                self.end_edit();
                self.bump(ctx);
                self.finish_visual_op(ctx);
            }
            VisualCmd::Join { literal } => {
                let Some(span) = ops::span_from_visual(self, ctx.buf) else {
                    return;
                };
                let count = self.take_total_count();
                let first = ctx.buf.offset_to_line(span.start);
                let last = ctx.buf.offset_to_line(span.end.saturating_sub(1).max(span.start));
                self.begin_edit(ctx);
                self.cursor.offset = span.start;
                ops::join_lines(self, ctx, (last - first + 1).max(count), literal);
                self.end_edit();
                self.bump(ctx);
                self.finish_visual_op(ctx);
            }
        }
    }

    /// `start_insert` places the cursor per `InsertKind` then begins a session.
    pub(crate) fn start_insert(&mut self, ctx: &mut Ctx, kind: InsertKind) {
        match kind {
            InsertKind::Insert | InsertKind::Change | InsertKind::Replace => {}
            InsertKind::Append => {
                if !ctx.buf.at_line_end(self.cursor.offset) {
                    self.cursor.offset = ctx.buf.next_char_offset(self.cursor.offset).unwrap_or(self.cursor.offset);
                }
            }
            InsertKind::InsertFirstNonBlank => {
                let line = ctx.buf.offset_to_line(self.cursor.offset);
                self.cursor.offset = ctx.buf.first_non_blank(line);
            }
            InsertKind::AppendLineEnd => {
                let line = ctx.buf.offset_to_line(self.cursor.offset);
                self.cursor.offset = ctx.buf.line_end(line);
            }
            InsertKind::InsertAtColumnZero => {
                let line = ctx.buf.offset_to_line(self.cursor.offset);
                self.cursor.offset = ctx.buf.line_start(line);
            }
            InsertKind::OpenLine { below } => {
                // open the undo group BEFORE mutating, so the snapshot the
                // host takes can actually undo the inserted line
                self.begin_edit(ctx);
                let line = ctx.buf.offset_to_line(self.cursor.offset);
                let (indent, _) = ctx.buf.line_indent(line);
                let indent_str = " ".repeat(indent);
                if below {
                    let at = ctx.buf.line_end(line);
                    self.edit_insert(ctx, at, &format!("\n{indent_str}"));
                    self.cursor.offset = at + 1 + indent_str.len();
                } else {
                    let at = ctx.buf.line_start(line);
                    self.edit_insert(ctx, at, &format!("{indent_str}\n"));
                    self.cursor.offset = at + indent_str.len();
                }
                ctx.host.changed();
            }
        }
        self.begin_insert(ctx, kind);
    }

    // ---- char-argument commands -------------------------------------------------

    fn complete_char_arg(&mut self, ctx: &mut Ctx, key: Key) -> ProcessOutcome {
        let Some(cmd) = self.char_arg_cmd.take() else {
            return ProcessOutcome::Consumed;
        };
        if key == Key::escape() || key == Key::ctrl_char('[') {
            self.reset_pending();
            return ProcessOutcome::Consumed;
        }
        let Some(c) = key.printable_char() else {
            self.reset_pending();
            ctx.host.bell();
            return ProcessOutcome::Consumed;
        };
        self.char_arg = Some(c);
        match cmd {
            CharArgCmd::Find { forward, till } => {
                self.last_find = Some((c, forward, till));
                let motion = Motion::FindChar { forward, till };
                let count = self.take_total_count();
                if self.op.is_some() {
                    let result = motion.target(self, ctx, count);
                    if !result.moved {
                        ctx.host.bell();
                        self.reset_pending();
                        return ProcessOutcome::Consumed;
                    }
                    let span = ops::span_from_motion(self, ctx.buf, motion, result);
                    self.complete_operator_with_span(ctx, span);
                } else if !self.goto_motion(ctx, motion, count) {
                    ctx.host.bell();
                }
            }
            CharArgCmd::Replace => {
                let count = self.take_total_count();
                self.begin_edit(ctx);
                ops::replace_chars(self, ctx, c, count);
                self.end_edit();
                self.bump(ctx);
            }
            CharArgCmd::MarkSet => {
                self.marks.set(c, self.cursor.offset);
            }
            CharArgCmd::MacroRecord => {
                // starting `q{reg}`; the stop is handled in execute_command
                self.macro_capture = Some((c, Vec::new()));
            }
            CharArgCmd::MacroPlay => {
                let reg = if c == '@' { self.last_macro_played } else { Some(c) };
                match reg.and_then(|r| self.macros.get(&r).cloned()) {
                    Some(keys) => {
                        self.last_macro_played = Some(c);
                        let count = self.take_total_count().max(1);
                        // replay through the pipeline with `.` recording
                        // suppressed; the key guard handles recursive macros
                        self.replaying = true;
                        self.recording.clear();
                        self.recording_mutated = false;
                        for _ in 0..count {
                            for step in &keys {
                                match step {
                                    RecordedStep::Key(key) => {
                                        self.pending_keys.push_back(key.clone())
                                    }
                                    RecordedStep::Text(text) => {
                                        self.replay_texts.push_back(text.clone());
                                        self.pending_keys.push_back(Key::named(DOT_TEXT_MARKER));
                                    }
                                }
                            }
                        }
                    }
                    None => ctx.host.bell(),
                }
            }
            CharArgCmd::JumpMark { linewise } => {
                match self.marks.resolve(c) {
                    Some(offset) => {
                        let offset = offset.min(ctx.buf.len());
                        if linewise {
                            let line = ctx.buf.offset_to_line(offset);
                            let target = ctx.buf.first_non_blank(line);
                            self.cursor.offset = target;
                            self.cursor.desired_col = None;
                            ctx.host.scroll_to_line(line);
                        } else {
                            self.cursor.offset = offset;
                            self.cursor.desired_col = None;
                            ctx.host.scroll_to_line(ctx.buf.offset_to_line(offset));
                        }
                    }
                    None => ctx.host.bell(),
                }
            }
        }
        self.char_arg = None;
        self.end_command();
        ProcessOutcome::Consumed
    }
}
