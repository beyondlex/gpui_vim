//! Editor modes.

/// Which visual sub-mode is active.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisualKind {
    Char,
    Line,
    Block,
}

impl VisualKind {
    /// Short indicator text, as vim shows it (`-- VISUAL --`, `-- V-LINE --`).
    pub fn indicator(self) -> &'static str {
        match self {
            VisualKind::Char => "VISUAL",
            VisualKind::Line => "V-LINE",
            VisualKind::Block => "V-BLOCK",
        }
    }
}

/// The engine mode. `OperatorPending` is intentionally *not* a mode: it is
/// transient state inside Normal mode (see [`crate::state::VimState`]), the
/// same way a `CommandBuilder` works in IdeaVim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    /// Replace mode is modeled as insert-with-overwrite; reserved for v1.x.
    Replace,
    Visual {
        kind: VisualKind,
    },
    /// Command-line input (`/` and `?` search prompts).
    CommandLine {
        prompt: char,
    },
}

impl Mode {
    /// Status-bar indicator, as vim shows it.
    pub fn indicator(self) -> &'static str {
        match self {
            Mode::Normal => "",
            Mode::Insert => "INSERT",
            Mode::Replace => "REPLACE",
            Mode::Visual { kind } => kind.indicator(),
            Mode::CommandLine { .. } => "CMDLINE",
        }
    }

    /// Stable name usable in gpui key-context predicates (`mode == normal`).
    pub fn context_name(self) -> &'static str {
        match self {
            Mode::Normal => "normal",
            Mode::Insert | Mode::Replace => "insert",
            Mode::Visual { .. } => "visual",
            Mode::CommandLine { .. } => "cmdline",
        }
    }
}
