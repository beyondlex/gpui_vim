//! The data-driven command table.
//!
//! Every built-in command is one row: a key sequence, the phase it applies
//! in, and what it does. Tries per phase are built once in `VimState::new`.
//! Adding a command = one row + (if new) one handler match arm.

use crate::key::Key;
use crate::keymap::{ModeClass, Trie};
use crate::motions::Motion;
use crate::objects::TextObject;
use crate::ops::Operator;
use crate::state::InsertKind;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Phase {
    Normal,
    /// Operator-pending (between an operator and its motion/object).
    Pending,
    Visual,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NormalCmd {
    DeleteCharForward,   // x
    DeleteCharBackward,  // X
    SubstituteChar,      // s
    SubstituteLine,      // S
    ChangeToEnd,         // C
    DeleteToEnd,         // D
    YankLine,            // Y
    ReplaceChar,         // r{char}
    ToggleChar,          // ~
    PutAfter,            // p
    PutBefore,           // P
    Join,                // J
    JoinLiteral,         // gJ
    Undo,                // u
    Redo,                // <C-r>
    MarkSet,             // m{char}
    RecordMacro,         // q{reg}
    PlayMacro,           // @{reg} / @@
    JumpMark { linewise: bool }, // '{char} / `{char
    LinewiseOp(Operator), // guu / gUU / g~~ / gugu ...
    ScrollCenter,        // zz
    ScrollTop,           // zt
    ScrollBottom,        // zb
    RestoreVisual,       // gv
    RepeatChange,        // .
    JumpBackward,        // C-o
    JumpForward,         // C-i
    OlderChange,         // g;
    NewerChange,         // g,
    IncrementNumber,     // C-a
    DecrementNumber,     // C-x
    WriteQuit,           // ZZ
    QuitNoSave,          // ZQ
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisualCmd {
    Exit,                     // same-kind v / V / Esc handled elsewhere
    ToggleKind { to: char },  // v / V
    SwapEnds,                 // o
    PutReplace,               // p / P replace selection
    Join { literal: bool },   // J / gJ
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmdKind {
    Motion(Motion),
    Object(TextObject),
    Operator(Operator),
    EnterInsert(InsertKind),
    EnterVisual(crate::mode::VisualKind),
    Normal(NormalCmd),
    Visual(VisualCmd),
}

impl CmdKind {
    /// Commands that consume the next key as an argument.
    pub fn takes_char(self) -> bool {
        matches!(
            self,
            CmdKind::Motion(Motion::FindChar { .. })
                | CmdKind::Motion(Motion::MarkJump { .. })
                | CmdKind::Normal(NormalCmd::ReplaceChar)
                | CmdKind::Normal(NormalCmd::MarkSet)
                | CmdKind::Normal(NormalCmd::JumpMark { .. })
                | CmdKind::Normal(NormalCmd::RecordMacro)
                | CmdKind::Normal(NormalCmd::PlayMacro)
        )
    }

    /// Does executing this command mutate the buffer? Used for undo groups.
    ///
    /// The `Normal` exclusion list is "pure navigation / bookkeeping":
    /// marks, macros, scrolls and list walks only move the cursor or
    /// engine state. `Undo`/`Redo` count as NON-mutating even though they
    /// change the text — the HOST begins their undo accounting itself
    /// (`begin_undo_group` was already consumed when the change being
    /// undone was made), and a fresh group here would nest confusingly.
    /// `RepeatChange` is excluded because its replayed steps mutate on
    /// their own (grouped by their own `begin_edit` calls). `WriteQuit` /
    /// `QuitNoSave` end the session without touching the buffer.
    pub fn mutates(self) -> bool {
        match self {
            CmdKind::Motion(_) | CmdKind::Object(_) => false,
            CmdKind::Operator(op) => !matches!(op, Operator::Yank),
            CmdKind::EnterInsert(_) => true,
            CmdKind::EnterVisual(_) => false,
            CmdKind::Normal(cmd) => !matches!(
                cmd,
                NormalCmd::MarkSet
                    | NormalCmd::JumpMark { .. }
                    | NormalCmd::RecordMacro
                    | NormalCmd::PlayMacro
                    | NormalCmd::ScrollCenter
                    | NormalCmd::ScrollTop
                    | NormalCmd::ScrollBottom
                    | NormalCmd::RestoreVisual
                    | NormalCmd::Undo
                    | NormalCmd::Redo
                    | NormalCmd::RepeatChange
                    | NormalCmd::JumpBackward
                    | NormalCmd::JumpForward
                    | NormalCmd::OlderChange
                    | NormalCmd::NewerChange
                    | NormalCmd::WriteQuit
                    | NormalCmd::QuitNoSave
            ),
            CmdKind::Visual(cmd) => !matches!(
                cmd,
                VisualCmd::Exit | VisualCmd::ToggleKind { .. } | VisualCmd::SwapEnds
            ),
        }
    }
}

struct RowBuilder {
    rows: Vec<(Vec<Key>, Phase, CmdKind)>,
}

impl RowBuilder {
    fn new() -> Self {
        RowBuilder { rows: Vec::new() }
    }

    fn normal(&mut self, keys: &[&str], kind: CmdKind) {
        self.rows.push((parse(keys), Phase::Normal, kind));
    }

    fn pending(&mut self, keys: &[&str], kind: CmdKind) {
        self.rows.push((parse(keys), Phase::Pending, kind));
    }

    fn visual(&mut self, keys: &[&str], kind: CmdKind) {
        self.rows.push((parse(keys), Phase::Visual, kind));
    }

    /// Register a motion in all three phases.
    fn motion_all(&mut self, keys: &[&str], motion: Motion) {
        self.normal(keys, CmdKind::Motion(motion));
        self.pending(keys, CmdKind::Motion(motion));
        self.visual(keys, CmdKind::Motion(motion));
    }

    /// Register a text object in pending + visual phases.
    fn object(&mut self, keys: &[&str], object: TextObject) {
        self.pending(keys, CmdKind::Object(object));
        self.visual(keys, CmdKind::Object(object));
    }
}

fn parse(keys: &[&str]) -> Vec<Key> {
    keys.iter().map(|k| Key::parse(k)).collect()
}

fn build_rows() -> Vec<(Vec<Key>, Phase, CmdKind)> {
    let mut b = RowBuilder::new();

    // ---- motions (all phases) ------------------------------------------
    b.motion_all(&["h"], Motion::Left);
    b.motion_all(&["l"], Motion::Right);
    b.motion_all(&["j"], Motion::Down);
    b.motion_all(&["k"], Motion::Up);
    b.motion_all(&["g", "j"], Motion::Down); // no soft wrap in v1
    b.motion_all(&["g", "k"], Motion::Up);
    b.motion_all(&["0"], Motion::LineStart);
    b.motion_all(&["^"], Motion::FirstNonBlank);
    b.motion_all(&["$"], Motion::LineEnd);
    b.motion_all(&["g", "_"], Motion::LastLineNonBlank);
    b.motion_all(&["w"], Motion::WordStart { big: false });
    b.motion_all(&["W"], Motion::WordStart { big: true });
    b.motion_all(&["b"], Motion::WordBack { big: false });
    b.motion_all(&["B"], Motion::WordBack { big: true });
    b.motion_all(&["e"], Motion::WordEnd { big: false });
    b.motion_all(&["E"], Motion::WordEnd { big: true });
    b.motion_all(&["g", "e"], Motion::WordEndBack { big: false });
    b.motion_all(&["g", "E"], Motion::WordEndBack { big: true });
    b.motion_all(&["f"], Motion::FindChar { forward: true, till: false });
    b.motion_all(&["F"], Motion::FindChar { forward: false, till: false });
    b.motion_all(&["t"], Motion::FindChar { forward: true, till: true });
    b.motion_all(&["T"], Motion::FindChar { forward: false, till: true });
    b.motion_all(&[";"], Motion::RepeatFind { reverse: false });
    b.motion_all(&[","], Motion::RepeatFind { reverse: true });
    b.motion_all(&["%"], Motion::MatchBracket);
    b.motion_all(&["g", "g"], Motion::GoToLine { first: true });
    b.motion_all(&["G"], Motion::GoToLine { first: false });
    b.motion_all(&["}"], Motion::ParaNext);
    b.motion_all(&["{"], Motion::ParaPrev);
    b.motion_all(&[")"], Motion::SentenceNext);
    b.motion_all(&["("], Motion::SentencePrev);
    b.motion_all(&["n"], Motion::SearchNext { forward: true });
    b.motion_all(&["N"], Motion::SearchNext { forward: false });
    b.motion_all(&["*"], Motion::StarSearch { forward: true });
    b.motion_all(&["#"], Motion::StarSearch { forward: false });
    b.motion_all(&["|"], Motion::Column);
    b.motion_all(&["H"], Motion::ScreenTop);
    b.motion_all(&["M"], Motion::ScreenMiddle);
    b.motion_all(&["L"], Motion::ScreenBottom);
    b.motion_all(&["<C-d>"], Motion::ScrollHalfDown);
    b.motion_all(&["<C-u>"], Motion::ScrollHalfUp);
    b.motion_all(&["<C-f>"], Motion::PageDown);
    b.motion_all(&["<C-b>"], Motion::PageUp);
    b.motion_all(&["<CR>"], Motion::LineDownFirstNonBlank);
    b.motion_all(&["+"], Motion::LineDownFirstNonBlank);
    b.motion_all(&["-"], Motion::LineUpFirstNonBlank);
    b.pending(&["'"], CmdKind::Motion(Motion::MarkJump { linewise: true }));
    b.pending(&["`"], CmdKind::Motion(Motion::MarkJump { linewise: false }));
    b.normal(&["'"], CmdKind::Normal(NormalCmd::JumpMark { linewise: true }));
    b.normal(&["`"], CmdKind::Normal(NormalCmd::JumpMark { linewise: false }));

    // ---- operators -------------------------------------------------------
    b.normal(&["d"], CmdKind::Operator(Operator::Delete));
    b.normal(&["c"], CmdKind::Operator(Operator::Change));
    b.normal(&["y"], CmdKind::Operator(Operator::Yank));
    b.normal(&[">"], CmdKind::Operator(Operator::IndentRight));
    b.normal(&["<"], CmdKind::Operator(Operator::IndentLeft));
    b.normal(&["g", "u"], CmdKind::Operator(Operator::Lowercase));
    b.normal(&["g", "U"], CmdKind::Operator(Operator::Uppercase));
    b.normal(&["g", "~"], CmdKind::Operator(Operator::ToggleCase));
    b.normal(&["g", "q"], CmdKind::Operator(Operator::Format));
    b.normal(&["g", "w"], CmdKind::Operator(Operator::Format));
    // multi-key operator doubling (single-key doubling is generic)
    b.normal(&["g", "u", "u"], CmdKind::Normal(NormalCmd::LinewiseOp(Operator::Lowercase)));
    b.normal(&["g", "u", "g", "u"], CmdKind::Normal(NormalCmd::LinewiseOp(Operator::Lowercase)));
    b.normal(&["g", "U", "U"], CmdKind::Normal(NormalCmd::LinewiseOp(Operator::Uppercase)));
    b.normal(&["g", "U", "g", "U"], CmdKind::Normal(NormalCmd::LinewiseOp(Operator::Uppercase)));
    b.normal(&["g", "~", "~"], CmdKind::Normal(NormalCmd::LinewiseOp(Operator::ToggleCase)));
    b.normal(&["g", "q", "q"], CmdKind::Normal(NormalCmd::LinewiseOp(Operator::Format)));
    b.normal(&["g", "q", "g", "q"], CmdKind::Normal(NormalCmd::LinewiseOp(Operator::Format)));
    b.normal(&["g", "~", "g", "~"], CmdKind::Normal(NormalCmd::LinewiseOp(Operator::ToggleCase)));

    // ---- text objects (pending + visual) ---------------------------------
    b.object(&["i", "w"], TextObject::Word { inner: true, big: false });
    b.object(&["a", "w"], TextObject::Word { inner: false, big: false });
    b.object(&["i", "W"], TextObject::Word { inner: true, big: true });
    b.object(&["a", "W"], TextObject::Word { inner: false, big: true });
    b.object(&["i", "s"], TextObject::Sentence { inner: true });
    b.object(&["a", "s"], TextObject::Sentence { inner: false });
    b.object(&["i", "p"], TextObject::Paragraph { inner: true });
    b.object(&["a", "p"], TextObject::Paragraph { inner: false });
    for (q, key) in [('"', "\""), ('\'', "'"), ('`', "`")] {
        b.object(&["i", key], TextObject::Quote { inner: true, quote: q });
        b.object(&["a", key], TextObject::Quote { inner: false, quote: q });
    }
    for (open, close) in [('(', ')'), ('[', ']'), ('{', '}'), ('<', '>')] {
        let open_key: &'static str = match open {
            '(' => "(",
            '[' => "[",
            '{' => "{",
            '<' => "<",
            _ => unreachable!(),
        };
        let close_key: &'static str = match close {
            ')' => ")",
            ']' => "]",
            '}' => "}",
            '>' => ">",
            _ => unreachable!(),
        };
        b.object(&["i", open_key], TextObject::Block { inner: true, open, close });
        b.object(&["i", close_key], TextObject::Block { inner: true, open, close });
        b.object(&["a", open_key], TextObject::Block { inner: false, open, close });
        b.object(&["a", close_key], TextObject::Block { inner: false, open, close });
    }
    b.object(&["i", "b"], TextObject::Block { inner: true, open: '(', close: ')' });
    b.object(&["a", "b"], TextObject::Block { inner: false, open: '(', close: ')' });
    b.object(&["i", "B"], TextObject::Block { inner: true, open: '{', close: '}' });
    b.object(&["a", "B"], TextObject::Block { inner: false, open: '{', close: '}' });
    b.object(&["i", "t"], TextObject::Tag { inner: true });
    b.object(&["a", "t"], TextObject::Tag { inner: false });

    // ---- normal-mode actions ---------------------------------------------
    b.normal(&["x"], CmdKind::Normal(NormalCmd::DeleteCharForward));
    b.normal(&["<Del>"], CmdKind::Normal(NormalCmd::DeleteCharForward));
    b.normal(&["X"], CmdKind::Normal(NormalCmd::DeleteCharBackward));
    b.normal(&["s"], CmdKind::Normal(NormalCmd::SubstituteChar));
    b.normal(&["S"], CmdKind::Normal(NormalCmd::SubstituteLine));
    b.normal(&["C"], CmdKind::Normal(NormalCmd::ChangeToEnd));
    b.normal(&["D"], CmdKind::Normal(NormalCmd::DeleteToEnd));
    b.normal(&["Y"], CmdKind::Normal(NormalCmd::YankLine));
    b.normal(&["r"], CmdKind::Normal(NormalCmd::ReplaceChar));
    b.normal(&["~"], CmdKind::Normal(NormalCmd::ToggleChar));
    b.normal(&["p"], CmdKind::Normal(NormalCmd::PutAfter));
    b.normal(&["P"], CmdKind::Normal(NormalCmd::PutBefore));
    b.normal(&["J"], CmdKind::Normal(NormalCmd::Join));
    b.normal(&["g", "J"], CmdKind::Normal(NormalCmd::JoinLiteral));
    b.normal(&["u"], CmdKind::Normal(NormalCmd::Undo));
    b.normal(&["<C-r>"], CmdKind::Normal(NormalCmd::Redo));
    b.normal(&["m"], CmdKind::Normal(NormalCmd::MarkSet));
    b.normal(&["."], CmdKind::Normal(NormalCmd::RepeatChange));
    b.normal(&["q"], CmdKind::Normal(NormalCmd::RecordMacro));
    b.normal(&["<C-o>"], CmdKind::Normal(NormalCmd::JumpBackward));
    b.normal(&["<C-i>"], CmdKind::Normal(NormalCmd::JumpForward));
    b.normal(&["g", ";"], CmdKind::Normal(NormalCmd::OlderChange));
    b.normal(&["g", ","], CmdKind::Normal(NormalCmd::NewerChange));
    b.normal(&["<C-a>"], CmdKind::Normal(NormalCmd::IncrementNumber));
    b.normal(&["<C-x>"], CmdKind::Normal(NormalCmd::DecrementNumber));
    b.normal(&["@"], CmdKind::Normal(NormalCmd::PlayMacro));
    b.normal(&["z", "z"], CmdKind::Normal(NormalCmd::ScrollCenter));
    b.normal(&["z", "t"], CmdKind::Normal(NormalCmd::ScrollTop));
    b.normal(&["z", "b"], CmdKind::Normal(NormalCmd::ScrollBottom));
    b.normal(&["g", "v"], CmdKind::Normal(NormalCmd::RestoreVisual));
    b.normal(&["Z", "Z"], CmdKind::Normal(NormalCmd::WriteQuit));
    b.normal(&["Z", "Q"], CmdKind::Normal(NormalCmd::QuitNoSave));

    // ---- entering insert ---------------------------------------------------
    b.normal(&["i"], CmdKind::EnterInsert(InsertKind::Insert));
    b.normal(&["a"], CmdKind::EnterInsert(InsertKind::Append));
    b.normal(&["I"], CmdKind::EnterInsert(InsertKind::InsertFirstNonBlank));
    b.normal(&["A"], CmdKind::EnterInsert(InsertKind::AppendLineEnd));
    b.normal(&["o"], CmdKind::EnterInsert(InsertKind::OpenLine { below: true }));
    b.normal(&["O"], CmdKind::EnterInsert(InsertKind::OpenLine { below: false }));
    b.normal(&["g", "I"], CmdKind::EnterInsert(InsertKind::InsertAtColumnZero));
    b.normal(&["g", "i"], CmdKind::EnterInsert(InsertKind::LastInsertExit));
    b.normal(&["R"], CmdKind::EnterInsert(InsertKind::Replace));

    // ---- entering visual ---------------------------------------------------
    b.normal(&["v"], CmdKind::EnterVisual(crate::mode::VisualKind::Char));
    b.normal(&["V"], CmdKind::EnterVisual(crate::mode::VisualKind::Line));
    b.normal(&["<C-v>"], CmdKind::EnterVisual(crate::mode::VisualKind::Block));

    // ---- visual mode -------------------------------------------------------
    b.visual(&["v"], CmdKind::Visual(VisualCmd::ToggleKind { to: 'v' }));
    b.visual(&["V"], CmdKind::Visual(VisualCmd::ToggleKind { to: 'V' }));
    b.visual(&["<C-v>"], CmdKind::Visual(VisualCmd::ToggleKind { to: 'b' }));
    b.visual(&["o"], CmdKind::Visual(VisualCmd::SwapEnds));
    b.visual(&["d"], CmdKind::Operator(Operator::Delete));
    b.visual(&["x"], CmdKind::Operator(Operator::Delete));
    b.visual(&["<Del>"], CmdKind::Operator(Operator::Delete));
    b.visual(&["y"], CmdKind::Operator(Operator::Yank));
    b.visual(&["c"], CmdKind::Operator(Operator::Change));
    b.visual(&["s"], CmdKind::Operator(Operator::Change));
    b.visual(&["C"], CmdKind::Operator(Operator::Change));
    b.visual(&["S"], CmdKind::Operator(Operator::Change));
    b.visual(&["D"], CmdKind::Operator(Operator::Delete));
    b.visual(&["X"], CmdKind::Operator(Operator::Delete));
    b.visual(&[">"], CmdKind::Operator(Operator::IndentRight));
    b.visual(&["<"], CmdKind::Operator(Operator::IndentLeft));
    b.visual(&["u"], CmdKind::Operator(Operator::Lowercase));
    b.visual(&["U"], CmdKind::Operator(Operator::Uppercase));
    b.visual(&["~"], CmdKind::Operator(Operator::ToggleCase));
    b.visual(&["g", "u"], CmdKind::Operator(Operator::Lowercase));
    b.visual(&["g", "U"], CmdKind::Operator(Operator::Uppercase));
    b.visual(&["g", "~"], CmdKind::Operator(Operator::ToggleCase));
    b.visual(&["g", "q"], CmdKind::Operator(Operator::Format));
    b.visual(&["p"], CmdKind::Visual(VisualCmd::PutReplace));
    b.visual(&["P"], CmdKind::Visual(VisualCmd::PutReplace));
    b.visual(&["J"], CmdKind::Visual(VisualCmd::Join { literal: false }));
    b.visual(&["g", "J"], CmdKind::Visual(VisualCmd::Join { literal: true }));

    b.rows
}

/// Per-phase command tries plus derived lookup helpers.
pub struct CommandTables {
    tries: HashMap<Phase, Trie<CmdKind>>,
}

impl CommandTables {
    pub fn build() -> Self {
        let mut tries: HashMap<Phase, Trie<CmdKind>> = HashMap::new();
        for (keys, phase, kind) in build_rows() {
            tries
                .entry(phase)
                .or_default()
                .insert(&keys, kind);
        }
        // make sure every phase has a (possibly empty) trie
        for phase in [Phase::Normal, Phase::Pending, Phase::Visual] {
            tries.entry(phase).or_default();
        }
        CommandTables { tries }
    }

    pub fn trie(&self, phase: Phase) -> &Trie<CmdKind> {
        &self.tries[&phase]
    }

    /// Find the longest terminal command in `keys` (for trie-miss recovery).
    pub fn longest_terminal(&self, phase: Phase, keys: &[Key]) -> Option<(usize, CmdKind)> {
        let mut best = None;
        for len in 1..=keys.len() {
            if let Walk::Hit(kind) = self.trie(phase).get(&keys[..len]) {
                best = Some((len, *kind));
            }
        }
        best
    }
}

use crate::keymap::Walk;

/// Which mapping class applies to a phase/mode.
pub fn mapping_class_for(mode: crate::mode::Mode) -> ModeClass {
    match mode {
        crate::mode::Mode::Visual { .. } => ModeClass::Visual,
        crate::mode::Mode::Insert | crate::mode::Mode::Replace => ModeClass::Insert,
        _ => ModeClass::Normal,
    }
}
