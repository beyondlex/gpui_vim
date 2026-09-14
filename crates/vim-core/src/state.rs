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
use crate::host::{ScrollAnchor, VimHost};
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
    /// gi: insert where the last insert session ended (`^` mark), or at the
    /// cursor when there is none.
    LastInsertExit, // gi
    Change,             // c / s / S / C
    Replace,            // R: overwrite instead of insert
}

/// Pending multi-row insert of a visual-block `I`/`A`/`c`: on exit the
/// typed text is replicated onto every other row (single undo group).
#[derive(Debug)]
struct BlockInsert {
    /// (adjusted offset, raw row start) for the non-cursor rows. Adjusted
    /// accounts for the block deletion; raw is the position in the original
    /// buffer, used to know which rows sit below the cursor's row.
    rows: Vec<(usize, usize)>,
    /// Raw start of the cursor's row: rows after it shift with the typed
    /// text.
    cursor_raw: usize,
    text: String,
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

    /// Remaining keys of a `:noremap` expansion: the mapping table is not
    /// consulted while these are consumed (no recursive remap).
    no_remap_left: usize,
    /// A user mapping just expanded: the queued expansion belongs to the
    /// mapping, so a cmdline entry inside it must not break the queue early.
    expanding_mapping: bool,
    /// Changelist (`g;`/`g,`): positions of recent changes, newest last;
    /// `change_pos` indexes the current entry.
    changes: Vec<usize>,
    change_pos: usize,
    /// For the `gq`/`gw` spellings of Operator::Format: the trigger letter
    /// (`q` or `w`) to match in the linewise doubling.
    format_trigger: Option<char>,
    /// Visual state to restore when the visual `:` prompt is cancelled:
    /// (kind, anchor). While the prompt is open the selection keeps its
    /// original shape for rendering.
    pub(crate) cmdline_visual: Option<(crate::mode::VisualKind, usize)>,
    /// `:action <unknown-id>` is silently ignored instead of reported.
    /// Hosts sharing one rc file across apps set this while applying the
    /// user layer (mappings aimed at other apps are expected to miss).
    pub(crate) lenient_actions: bool,
    /// Whether every edit re-runs the search highlight scan (default). Huge
    /// file hosts turn this off and refresh on their own schedule.
    hlsearch_live_update: bool,
    /// Visual-block `I`/`A`/`c`: the rows (insert offsets, descending) that
    /// receive the typed text when the session exits, and the text typed on
    /// the cursor row so far.
    block_insert: Option<BlockInsert>,

    /// Jumplist (`C-o`/`C-i`): visited positions, `jump_pos` = index of the
    /// current entry. Jump motions and search execution append the origin
    /// and destination; forward entries are truncated on a new jump.
    jumps: Vec<usize>,
    jump_pos: usize,

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
            no_remap_left: 0,
            expanding_mapping: false,
            lenient_actions: false,
            hlsearch_live_update: true,
            changes: Vec::new(),
            change_pos: 0,
            format_trigger: None,
            cmdline_visual: None,
            block_insert: None,
            jumps: Vec::new(),
            jump_pos: 0,
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
        let kind = match self.mode {
            Mode::Visual { kind } => kind,
            // the visual `:` prompt: keep the selection's original shape
            Mode::CommandLine { .. } => self.cmdline_visual?.0,
            // outside visual there is no selection (a stale anchor must not
            // keep rendering one)
            _ => return None,
        };
        Some((anchor, self.cursor.offset, kind))
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
            None => crate::buffer::display_column(buf, self.cursor.offset),
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

        self.pending_keys.push_back(key.clone());
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
            // :omap there, which we do not support yet). Suppressed while a
            // :noremap expansion is being consumed (no recursive remap).
            // Waiting keeps the key IN the queue: insert-mode printables are
            // declined by the pipeline and delivered by the host later, so
            // popping them here would lose them.
            if self.no_remap_left > 0 {
                self.no_remap_left -= 1;
            } else if self.op.is_none() {
                let contiguous: &[Key] = self.pending_keys.make_contiguous();
                match keymap::lookup(self.keymaps.table(class), contiguous) {
                    MappingMatch::Match { used, expansion, noremap } => {
                        self.pending_keys.drain(..used);
                        if noremap {
                            self.no_remap_left = expansion.len();
                        }
                        self.expanding_mapping = true;
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
                    // a longer mapping may still follow: wait, key queued
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
            // record the key optimistically; keys the engine DECLINES
            // (insert-mode printables on macOS: the host places that text
            // itself) are popped again below — the text placement records
            // the Text step instead, exactly once. Otherwise every typed
            // char would replay twice.
            if !self.replaying {
                self.recording.push(RecordedStep::Key(front.clone()));
                if let Some((_, keys)) = &mut self.macro_capture {
                    keys.push(RecordedStep::Key(front.clone()));
                }
            }
            let outcome = self.process_key(ctx, front);
            if !self.replaying && outcome == ProcessOutcome::Unknown {
                self.recording.pop();
                if let Some((_, keys)) = &mut self.macro_capture {
                    keys.pop();
                }
            }
            match outcome {
                ProcessOutcome::Consumed => {}
                ProcessOutcome::Unknown => any_unknown = true,
                ProcessOutcome::Feed(keys) => {
                    for key in keys.into_iter().rev() {
                        self.pending_keys.push_front(key);
                    }
                }
            }
            // a mapping RHS that enters cmdline mode must keep processing
            // its queued keys (`:action Foo<CR>` as a mapping RHS)
            if matches!(self.mode, Mode::CommandLine { .. })
                && !self.replaying
                && !self.expanding_mapping
            {
                break;
            }
        }
        self.map_depth = 0;
        if self.pending_keys.is_empty() && self.replaying {
            self.replaying = false;
            self.expanding_mapping = false;
            self.replay_texts.clear();
            self.recording.clear();
            self.recording_mutated = false;
        }
        if self.pending_keys.is_empty() {
            self.expanding_mapping = false;
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

    /// `C-a`/`C-x`: find the number at or after the cursor on this line and
    /// add `delta` to it, preserving leading zeros count (roughly) and
    /// cursor on the last digit. Returns false when no number is found.
    fn increment_number_at_cursor(&mut self, ctx: &mut Ctx, delta: i64) -> bool {
        // returns false when no number is found on the line
        let line = ctx.buf.offset_to_line(self.cursor.offset);
        let start = ctx.buf.line_start(line);
        let end = ctx.buf.line_end(line);
        let text = ctx.buf.slice(start..end);
        let bytes = text.as_bytes();

        // cursor column (bytes) within the line
        let cur = self.cursor.offset - start;
        // scan forward from the cursor for a digit; if none ahead, scan the
        // tail of the number the cursor sits inside
        let num_start = (cur..text.len()).find(|&i| bytes[i].is_ascii_digit());
        let num_start = match num_start {
            Some(start) => start,
            None => {
                // maybe the cursor is inside a number's tail: walk back to
                // its first digit
                let mut i = cur.min(text.len().saturating_sub(1));
                while i > 0 && !bytes[i].is_ascii_digit() {
                    i -= 1;
                }
                if !bytes.get(i).is_some_and(|b| b.is_ascii_digit()) {
                    return false;
                }
                i
            }
        };

        // include a preceding minus as sign when it is directly attached
        let signed_start = if num_start > 0 && bytes[num_start - 1] == b'-' {
            num_start - 1
        } else {
            num_start
        };

        // the number ends at the first non-digit
        let mut num_end = num_start;
        while num_end < text.len() && bytes[num_end].is_ascii_digit() {
            num_end += 1;
        }

        let old_text = &text[signed_start..num_end];
        let negative = old_text.starts_with('-');
        let digits = if negative { &old_text[1..] } else { old_text };
        let value: i64 = digits.parse().unwrap_or(i64::MAX);
        let new_value = if negative { -value } else { value }.wrapping_add(delta);
        let new_text = new_value.to_string();

        self.begin_edit(ctx);
        let abs = start + signed_start;
        self.edit_replace(ctx, abs..start + num_end, &new_text);
        self.bump(ctx);
        // cursor on the last digit of the new number
        self.cursor.offset = (abs + new_text.len() - 1).max(abs);
        self.cursor.desired_col = None;
        self.end_edit();
        true
    }

    pub(crate) fn bump(&mut self, ctx: &mut Ctx) {
        self.marks.last_change = Some(self.cursor.offset);
        // changelist: dedupe repeats, cap the list
        if self.changes.last() != Some(&self.cursor.offset) {
            self.changes.truncate(self.change_pos + 1);
            self.changes.push(self.cursor.offset);
            if self.changes.len() > 100 {
                self.changes.remove(0);
            }
            self.change_pos = self.changes.len() - 1;
        }
        self.republish_search(ctx);
        ctx.host.changed();
    }

    /// Buffer edits shift the byte offsets behind any published highlights;
    /// recompute and republish so `hlsearch` visuals follow the text. No-op
    /// while nothing is published (`:noh`, no pattern) — editing must not
    /// revive cleared highlights.
    fn republish_search(&mut self, ctx: &mut Ctx) {
        // hosts embedding the engine in huge-file editors can turn this off
        // (set_hlsearch_live_update(false)) and call refresh_highlights on
        // their own schedule (e.g. 150ms after the last edit)
        if !self.hlsearch_live_update {
            return;
        }
        if !self.options.hlsearch {
            return;
        }
        // NOTE: an empty last_matches cache must NOT early-return here —
        // after a deletion removes the last match, later edits have to
        // re-scan to notice the pattern matching again.
        let Some(pattern) = self.search.pattern.clone() else {
            return;
        };
        let matches = crate::search::all_matches(self, ctx.buf, &pattern);
        if matches.is_empty() {
            self.search.last_matches.clear();
            self.search.last_index = None;
            ctx.host.set_search_highlights(&[], None);
            return;
        }
        let current = matches
            .iter()
            .find(|m| m.start >= self.cursor.offset)
            .or_else(|| matches.last())
            .cloned();
        let index = current.as_ref().and_then(|c| matches.iter().position(|m| m == c));
        self.search.last_matches = matches.clone();
        self.search.last_index = index;
        ctx.host.set_search_highlights(&matches, current);
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
        // the goal column must be read BEFORE the cursor lands: once the
        // cursor sits on a wide char that clamped it, the derived column
        // would be the clamped one, not the original (vim's wv_col)
        let preserves_column = matches!(
            motion,
            Motion::Up
                | Motion::Down
                | Motion::ScrollHalfDown
                | Motion::ScrollHalfUp
                | Motion::PageUp
                | Motion::PageDown
        );
        let desired = if preserves_column {
            Some(self.desired_column(ctx.buf))
        } else {
            None
        };
        self.cursor.offset = clamp_to_line_end(ctx.buf, result.offset);
        if result.kind == crate::motions::MotionKind::Linewise {
            if preserves_column {
                let desired = desired.unwrap();
                self.cursor.desired_col = Some(desired);
                let line = ctx.buf.offset_to_line(self.cursor.offset);
                self.cursor.offset =
                    crate::buffer::offset_for_display_column(ctx.buf, line, desired);
            } else {
                let line = ctx.buf.offset_to_line(self.cursor.offset);
                self.cursor.offset = ctx.buf.first_non_blank(line);
                self.cursor.desired_col = None;
            }
        } else if !preserves_column {
            self.cursor.desired_col = None;
        }
        if matches!(self.mode, Mode::Visual { .. }) {
            if let Some(anchor) = self.visual_anchor {
                let (a, c) = (anchor.min(self.cursor.offset), anchor.max(self.cursor.offset));
                self.marks.active_visual = Some((a, c + 1));
            }
        }
        let line = ctx.buf.offset_to_line(self.cursor.offset);
        ctx.host.scroll_to_line(line);
    }

    pub(crate) fn goto_motion(&mut self, ctx: &mut Ctx, motion: Motion, count: usize) -> bool {
        let origin = self.cursor.offset;
        let result = motion.target(self, ctx, count);
        if !result.moved {
            return false;
        }
        self.apply_motion_result(ctx, motion, result);
        if motion.is_jump() {
            self.record_jump(origin, self.cursor.offset);
        }
        true
    }

    /// Append a jump (origin -> dest) to the jumplist, discarding any
    /// forward entries (like stepping back then jumping anew in a browser).
    pub(crate) fn record_jump(&mut self, origin: usize, dest: usize) {
        if self.jump_pos + 1 < self.jumps.len() {
            self.jumps.truncate(self.jump_pos + 1);
        }
        if self.jumps.last() != Some(&origin) {
            self.jumps.push(origin);
        }
        if self.jumps.last() != Some(&dest) {
            self.jumps.push(dest);
        }
        while self.jumps.len() > 100 {
            self.jumps.remove(0);
        }
        self.jump_pos = self.jumps.len() - 1;
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
        // visual-block I/A/c: replicate the typed text onto the other rows
        // while the session's undo group is still open (one `u` restores
        // all). Rows below the cursor's row shift by the typed byte length.
        if let Some(block) = self.block_insert.take() {
            if !block.text.is_empty() {
                let mut rows: Vec<usize> = block
                    .rows
                    .into_iter()
                    .map(|(adjusted, raw)| {
                        adjusted + if raw > block.cursor_raw { block.text.len() } else { 0 }
                    })
                    .collect();
                rows.sort_unstable_by(|a, b| b.cmp(a)); // bottom-up inserts
                for offset in rows {
                    self.edit_insert(ctx, offset, &block.text);
                }
            }
        }
        self.commit_change_record();
        self.insert_session = None;
        self.republish_search(ctx);
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
        self.republish_search(ctx);
        ctx.host.changed();
    }

    /// Whether each edit re-runs the hlsearch scan (default true). Huge-file
    /// hosts set false and call [`VimState::refresh_highlights`] on their own
    /// schedule (debounce/interval), keeping per-keystroke cost O(edit).
    pub fn set_hlsearch_live_update(&mut self, live: bool) {
        self.hlsearch_live_update = live;
    }

    /// Re-run the highlight scan and publish to the host (for hosts that
    /// disabled [`VimState::set_hlsearch_live_update`]).
    pub fn refresh_highlights(&mut self, ctx: &mut Ctx) {
        self.republish_search(ctx);
    }

    /// While set, `:action <unknown-id>` misses are silently ignored
    /// (host chooses whether the bridge reports; shared rc files contain
    /// mappings aimed at other apps).
    pub fn set_lenient_actions(&mut self, lenient: bool) {
        self.lenient_actions = lenient;
    }

    pub fn lenient_actions(&self) -> bool {
        self.lenient_actions
    }

    /// Apply a parsed user config (`~/.gpui-vimrc` style): options via the
    /// `:set` machinery, mappings into the per-mode mapping tables.
    pub fn apply_config(&mut self, config: &crate::config::Config) -> crate::config::ConfigStats {
        let mut stats = crate::config::ConfigStats::default();
        for setting in &config.settings {
            let ok = match setting {
                crate::config::Setting::On(name) => self.options.set_boolean(name, true),
                crate::config::Setting::Off(name) => self.options.set_boolean(name, false),
                crate::config::Setting::Toggle(name) => match self.options.bool_option(name) {
                    Some(current) => self.options.set_boolean(name, !current),
                    None => false,
                },
                crate::config::Setting::Value(name, value) => self.options.set_value(name, value),
            };
            if ok {
                stats.options += 1;
            } else {
                stats.ignored += 1;
            }
        }
        for mapping in &config.mappings {
            self.keymaps.map(mapping.class, &mapping.lhs, mapping.rhs.clone(), mapping.noremap);
            stats.mappings += 1;
        }
        stats.ignored += config.ignored.len();
        stats
    }

    /// Host-side entry for typed/composed text (the IME placement path).
    /// Records the text as ONE Text step for `.`/macro replay — the key
    /// pipeline declines these chars, so recording them as keys too would
    /// double every typed character on replay.
    pub fn record_typed_text(&mut self, text: &str) {
        if text.is_empty() || self.replaying || self.recording_suppressed {
            return;
        }
        if let Some(block) = &mut self.block_insert {
            block.text.push_str(text);
        }
        if self.insert_session.is_some() {
            match self.recording.last_mut() {
                Some(RecordedStep::Text(existing)) => existing.push_str(text),
                _ => self.recording.push(RecordedStep::Text(text.to_owned())),
            }
            if let Some((_, keys)) = &mut self.macro_capture {
                keys.push(RecordedStep::Text(text.to_owned()));
            }
        }
    }

    fn current_line_indent(&self, ctx: &Ctx) -> usize {
        let line = ctx.buf.offset_to_line(self.cursor.offset);
        let (indent, _) = ctx.buf.line_indent(line);
        indent
    }

    /// Replace an arbitrary range (IME committed composition text).
    pub fn replace_range(&mut self, ctx: &mut Ctx, range: Range<usize>, text: &str) {
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
        self.marks.active_visual = Some((self.cursor.offset, self.cursor.offset));
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
        self.marks.active_visual = None;
        self.mode = Mode::Normal;
        self.discard_change_record();
        ctx.host.changed();
    }

    /// Restore the cursor to a visual range start (used after visual ops).
    /// Row-wise application of an operator over a visual block. Only
    /// Delete / Yank / Change are supported in v1 (other operators bell).
    fn apply_block_operator(&mut self, ctx: &mut Ctx, op: Operator) {
        let Some(block) = ops::span_from_visual_block(self, ctx.buf) else {
            ctx.host.bell();
            return;
        };
        match op {
            Operator::Delete => {
                self.begin_edit(ctx);
                let adjusted = self.delete_block_rows(ctx, &block.rows);
                self.end_edit();
                self.bump(ctx);
                self.reset_pending();
                self.cursor.offset =
                    clamp_to_line_end(ctx.buf, adjusted.first().copied().unwrap_or(0));
                self.cursor.desired_col = None;
                self.finish_visual_op(ctx);
            }
            Operator::Yank => {
                let width = block.col_hi.saturating_sub(block.col_lo);
                let mut texts = Vec::new();
                for range in &block.rows {
                    let mut text = ctx.buf.slice(range.clone());
                    // pad rows to the block width so blockwise put keeps
                    // its rectangle
                    let mut w: usize = text
                        .chars()
                        .map(crate::buffer::char_display_width)
                        .sum();
                    while w < width {
                        text.push(' ');
                        w += 1;
                    }
                    texts.push(text);
                }
                self.registers.store_yank(
                    self.register,
                    texts.join("\n"),
                    crate::registers::RegisterKind::Blockwise,
                );
                self.cursor.offset = clamp_to_line_end(
                    ctx.buf,
                    block.rows.first().map(|r| r.start).unwrap_or(0),
                );
                self.cursor.desired_col = None;
                self.finish_visual_op(ctx);
            }
            Operator::Change => {
                self.begin_edit(ctx);
                let adjusted = self.delete_block_rows(ctx, &block.rows);
                self.bump(ctx);
                self.reset_pending();
                self.cursor.offset = clamp_to_line_end(ctx.buf, adjusted[0]);
                self.block_insert = Some(BlockInsert {
                    rows: adjusted[1..].iter().copied().zip(block.rows[1..].iter().map(|r| r.start)).collect(),
                    cursor_raw: block.rows[0].start,
                    text: String::new(),
                });
                self.begin_insert(ctx, InsertKind::Insert);
                self.discard_change_record();
            }
            _ => ctx.host.bell(),
        }
    }

    /// Delete every block row bottom-up; returns each row's insertion
    /// offset ADJUSTED for the deletions below it (valid after all rows
    /// are gone). Empty rows contribute no shift.
    fn delete_block_rows(&mut self, ctx: &mut Ctx, rows: &[std::ops::Range<usize>]) -> Vec<usize> {
        // a row's final offset shifts by the deletions ABOVE it: prefix-sum
        // top-down first, then delete bottom-up (keeps later offsets valid)
        let mut adjusted = vec![0usize; rows.len()];
        let mut shift = 0usize;
        for i in 0..rows.len() {
            adjusted[i] = rows[i].start.saturating_sub(shift);
            if !rows[i].is_empty() {
                shift += rows[i].len();
            }
        }
        for range in rows.iter().rev() {
            if !range.is_empty() {
                self.edit_delete(ctx, range.clone());
            }
        }
        adjusted
    }

    /// Visual-block `I` (insert at the left edge) and `A` (append at the
    /// right edge): typing lands on the cursor row, the rest replicate on
    /// exit. Short rows insert at their line end, like vim.
    fn begin_block_insert(&mut self, ctx: &mut Ctx, append: bool) {
        let Some(block) = ops::span_from_visual_block(self, ctx.buf) else {
            ctx.host.bell();
            return;
        };
        let cursor_line = ctx.buf.offset_to_line(self.cursor.offset);
        let mut rows = Vec::new();
        let mut typing_offset = None;
        let mut typing_raw = 0usize;
        for (i, range) in block.rows.iter().enumerate() {
            let line = block.first_line + i;
            let (offset, raw) = if append {
                let end = if range.is_empty() { ctx.buf.line_end(line) } else { range.end };
                (end, end)
            } else if range.is_empty() {
                (ctx.buf.line_end(line), range.start)
            } else {
                (range.start, range.start)
            };
            if line == cursor_line {
                typing_offset = Some(offset);
                typing_raw = raw;
            } else {
                rows.push((offset, raw));
            }
        }
        let Some(typing_offset) = typing_offset else {
            ctx.host.bell();
            return;
        };
        self.cursor.offset = typing_offset;
        self.cursor.desired_col = None;
        self.block_insert = Some(BlockInsert {
            rows,
            cursor_raw: typing_raw,
            text: String::new(),
        });
        self.begin_insert(ctx, InsertKind::Insert);
        self.discard_change_record();
    }

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
        Operator::Format => "gq",
    }
}

#[derive(Clone, PartialEq, Eq)]
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

        // 2. operator doubling: dd / yy / >> / guu / g~~ / gqq / gww ...
        // (gq/gw need the remembered spelling: their trigger letters are
        // not derivable from the operator alone)
        if self.op.is_some() && self.cmd_seq.is_empty() {
            let trigger = if self.op == Some(Operator::Format) {
                self.format_trigger
            } else {
                Self::operator_trigger(self.op.unwrap())
            };
            if let (Some(trigger), KeyKind::Char(c), true) =
                (trigger, &key.kind, key.modifiers.is_plain())
            {
                if trigger == *c {
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
                    // remember the gq/gw spelling BEFORE cmd_seq is cleared
                    // (execute_command reads it for the linewise doubling)
                    if *kind == CmdKind::Operator(Operator::Format) {
                        // the spelling letter is the LAST key of [g, q|w]
                        self.format_trigger = match seq.last().map(|k| &k.kind) {
                            Some(KeyKind::Char('w')) => Some('w'),
                            _ => Some('q'),
                        };
                    }
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
                    // in visual mode, `:` seeds the cmdline with the last
                    // selection's line range, like vim
                    if matches!(self.mode, Mode::Visual { .. }) {
                        self.cmdline.buffer.push_str("'<,'>");
                    }
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
        // `:` in visual mode seeds the cmdline with '<,'>; Esc at the
        // prompt returns to the selection, executing it drops to normal
        if key.modifiers.is_plain() && key.kind == KeyKind::Char(':') {
            let kind = match self.mode {
                Mode::Visual { kind } => kind,
                _ => crate::mode::VisualKind::Char,
            };
            self.cmdline_visual = Some((kind, self.visual_anchor.unwrap_or(self.cursor.offset)));
            self.begin_cmdline(':');
            self.cmdline.buffer.push_str("'<,'>");
            return ProcessOutcome::Consumed;
        }
        // visual-block I/A: insert at the block edge on every row
        if matches!(self.mode, Mode::Visual { kind: crate::mode::VisualKind::Block }) {
            if let (KeyKind::Char(c), true) = (&key.kind, key.modifiers.is_plain()) {
                if *c == 'I' || *c == 'A' {
                    self.begin_block_insert(ctx, *c == 'A');
                    return ProcessOutcome::Consumed;
                }
            }
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
        if matches!(self.mode, Mode::Visual { kind: crate::mode::VisualKind::Block }) {
            self.apply_block_operator(ctx, op);
            return;
        }
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
            Operator::Format => None,
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
                self.republish_search(ctx);
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
                self.republish_search(ctx);
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
                // and the anchor (zz/zt/zb semantics)
                let anchor = match cmd {
                    NormalCmd::ScrollCenter => ScrollAnchor::Center,
                    NormalCmd::ScrollTop => ScrollAnchor::Top,
                    _ => ScrollAnchor::Bottom,
                };
                ctx.host.scroll_to_line_anchored(line, anchor);
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
            NormalCmd::InsertAtLastChange => {
                // `gi`: insert where the last insert session ended ('^)
                if let Some(offset) = self.marks.get('^') {
                    self.cursor.offset = offset.min(ctx.buf.len());
                    self.cursor.desired_col = None;
                }
                self.start_insert(ctx, InsertKind::Insert);
            }
            NormalCmd::OlderChange | NormalCmd::NewerChange => {
                let older = cmd == NormalCmd::OlderChange;
                let count = self.take_total_count().max(1);
                if self.changes.is_empty() {
                    ctx.host
                        .status_message("E664: changelist is empty");
                    ctx.host.bell();
                    return;
                }
                for _ in 0..count {
                    let moved = if older {
                        if self.change_pos == 0 {
                            false
                        } else {
                            self.change_pos -= 1;
                            true
                        }
                    } else if self.change_pos + 1 < self.changes.len() {
                        self.change_pos += 1;
                        true
                    } else {
                        false
                    };
                    if !moved {
                        ctx.host
                            .status_message("E662: At start of changelist");
                        ctx.host.bell();
                        break;
                    }
                }
                let offset = self.changes[self.change_pos].min(ctx.buf.len());
                self.cursor.offset = clamp_to_line_end(ctx.buf, offset);
                self.cursor.desired_col = None;
                ctx.host
                    .scroll_to_line(ctx.buf.offset_to_line(self.cursor.offset));
                ctx.host.changed();
            }
            NormalCmd::IncrementNumber | NormalCmd::DecrementNumber => {
                let delta: i64 = if cmd == NormalCmd::IncrementNumber {
                    self.take_total_count().max(1) as i64
                } else {
                    -(self.take_total_count().max(1) as i64)
                };
                if !self.increment_number_at_cursor(ctx, delta) {
                    ctx.host.status_message("E18: Unexpected end of file");
                    ctx.host.bell();
                }
            }
            NormalCmd::JumpBackward | NormalCmd::JumpForward => {
                let backward = cmd == NormalCmd::JumpBackward;
                let count = self.take_total_count().max(1);
                for _ in 0..count {
                    let moved = if backward {
                        if self.jump_pos == 0 {
                            false
                        } else {
                            self.jump_pos -= 1;
                            true
                        }
                    } else if self.jump_pos + 1 < self.jumps.len() {
                        self.jump_pos += 1;
                        true
                    } else {
                        false
                    };
                    if !moved {
                        ctx.host.bell();
                        break;
                    }
                    self.cursor.offset =
                        clamp_to_line_end(ctx.buf, self.jumps[self.jump_pos]);
                    self.cursor.desired_col = None;
                    ctx.host
                        .scroll_to_line(ctx.buf.offset_to_line(self.cursor.offset));
                }
                ctx.host.changed();
            }
            NormalCmd::RestoreVisual => {
                if let Some((lo, hi, kind)) = self.last_visual {
                    self.visual_anchor = Some(lo);
                    self.cursor.offset = hi.saturating_sub(1).min(ctx.buf.len());
                    self.mode = Mode::Visual { kind };
                }
            }
            NormalCmd::WriteQuit => {
                ctx.host.save();
                ctx.host.request_close();
            }
            NormalCmd::QuitNoSave => {
                ctx.host.request_close();
            }
        }
    }

    /// Visual-block `p`/`P`: a blockwise register replaces the block row by
    /// row; any other register's text is replicated onto every row.
    fn block_put_replace(&mut self, ctx: &mut Ctx) {
        let register = self.register.unwrap_or(crate::registers::UNNAMED);
        let Some(block) = ops::span_from_visual_block(self, ctx.buf) else {
            ctx.host.bell();
            return;
        };
        let Some(data) = self.registers.get_for_paste(register, ctx.host) else {
            ctx.host.bell();
            return;
        };
        self.recording_blocked = true;
        self.begin_edit(ctx);
        let cursor_to = block.rows.first().map(|r| r.start).unwrap_or(0);
        match data.kind {
            crate::registers::RegisterKind::Blockwise => {
                let rows: Vec<&str> = data.text.trim_end_matches('\n').split('\n').collect();
                for (i, range) in block.rows.iter().enumerate().rev() {
                    if range.is_empty() {
                        continue;
                    }
                    let text = rows.get(i).or_else(|| rows.last()).copied().unwrap_or("");
                    self.edit_delete(ctx, range.clone());
                    if !text.is_empty() {
                        self.edit_insert(ctx, range.start, text);
                    }
                }
            }
            _ => {
                let adjusted = self.delete_block_rows(ctx, &block.rows);
                let text = data.text.trim_end_matches('\n');
                if !text.is_empty() {
                    for &offset in adjusted.iter().rev() {
                        self.edit_insert(ctx, offset, text);
                    }
                }
            }
        }
        self.cursor.offset = clamp_to_line_end(ctx.buf, cursor_to);
        self.cursor.desired_col = None;
        self.end_edit();
        self.bump(ctx);
        self.finish_visual_op(ctx);
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
                    'V' => VisualKind::Line,
                    _ => VisualKind::Block,
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
                if matches!(self.mode, Mode::Visual { kind: crate::mode::VisualKind::Block }) {
                    self.block_put_replace(ctx);
                    return;
                }
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
                    self.cursor.offset = crate::buffer::next_grapheme_offset(
                        ctx.buf,
                        self.cursor.offset,
                    )
                    .unwrap_or(self.cursor.offset);
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
            InsertKind::LastInsertExit => {
                if let Some(off) = self.marks.last_insert_exit {
                    self.cursor.offset = off.min(ctx.buf.len());
                }
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
                match reg.and_then(|r| self.macros.get(&r).map(|keys| (r, keys.clone()))) {
                    Some((r, keys)) => {
                        // remember the RESOLVED register: for `@@` the pressed
                        // key is '@' itself, which is not a stored macro
                        self.last_macro_played = Some(r);
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
                        let origin = self.cursor.offset;
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
                        self.record_jump(origin, self.cursor.offset);
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
