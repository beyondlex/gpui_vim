//! vim-core: a host-agnostic Vim engine.
//!
//! The engine knows nothing about gpui, windows, fonts or the system
//! clipboard. It operates on a buffer through the [`buffer::VimBufferMut`]
//! trait and surfaces side effects through the [`host::VimHost`] trait. Feed
//! it keys with [`state::VimState::handle_key`]; draw the cursor, selection
//! and highlights from the state it exposes.
//!
//! The layering (borrowed from IdeaVim's `vim-engine`):
//!
//! ```text
//! ┌──────────────────────────────────────────────┐
//! │ your gpui app / editor widget                │
//! ├──────────────────────────────────────────────┤
//! │ gpui-vim (integration crate)                 │  keystroke interception,
//! │  · VimSession entity + key interception      │  mode indicator helpers
//! ├──────────────────────────────────────────────┤
//! │ vim-core (this crate)                        │  pure engine:
//! │  · mode machine, key pipeline, tries         │  testable without a GUI
//! │  · motions, operators, objects, registers,   │
//! │    search, marks, undo semantics             │
//! ├──────────────────────────────────────────────┤
//! │ host traits: VimBuffer(Mut) + VimHost        │  implemented by your
//! └──────────────────────────────────────────────┘  buffer + widget
//! ```

pub mod buffer;
pub mod cmdline;
pub mod host;
pub mod insert_mode;
pub mod key;
pub mod keymap;
pub mod marks;
pub mod mode;
pub mod motions;
pub mod objects;
pub mod ops;
pub mod options;
pub mod registers;
pub mod search;
pub mod state;
pub mod tables;
pub mod word;

pub use buffer::{VimBuffer, VimBufferMut};
pub use host::VimHost;
pub use key::{Key, KeyKind, Modifiers};
pub use mode::{Mode, VisualKind};
pub use motions::Motion;
pub use objects::TextObject;
pub use ops::Operator;
pub use registers::RegisterKind;
pub use state::{Ctx, KeyResult, VimState};
pub use keymap::ModeClass;
