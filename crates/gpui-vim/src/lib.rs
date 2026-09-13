//! gpui integration for the `vim-core` engine.
//!
//! Hosts embed a [`VimState`] in their editor view, implement the
//! [`VimEditor`] trait, and register the engine with [`attach`]. The engine
//! then sits **in front of** every other key handler: keys it consumes never
//! reach your keymap or IME; keys it does not know (`Cmd-C`, unbound chords,
//! printable text in insert mode) pass through untouched.
//!
//! ```text
//! // inside Application::run, after creating your editor entity:
//! gpui_vim::attach(&editor_entity, cx);
//! ```
//!
//! ## How keystrokes reach the engine (gpui 0.2, macOS)
//!
//! Two paths, both ending in the same pipeline:
//!
//! 1. **Special keys** (`Esc`, arrows, `Space`, `Enter`, `Ctrl-*`, function
//!    keys) hit the interceptor installed by [`attach`] *before* gpui's
//!    keymap. Consumed keys are stopped; unknown ones fall through.
//! 2. **Printable characters** never reach the interceptor on macOS: the
//!    platform hands them to the text-input (IME) path, and they arrive at
//!    your `InputHandler::replace_text_in_range`. Call [`dispatch_text`]
//!    there — it feeds the characters through the same engine pipeline and
//!    places the leftovers into the buffer in insert mode.

use gpui::{App, Context, Entity, KeyContext, Keystroke, Subscription, WeakEntity, Window};
use vim_core::buffer::VimBufferMut;
use vim_core::host::VimHost;
use vim_core::key::{Key, KeyKind, Modifiers};
use vim_core::state::{Ctx, KeyResult, VimState};
use vim_core::Mode;

/// The host-side contract for an editor that embeds the engine.
///
/// `vim_parts` must return three borrows of **disjoint** fields: the engine,
/// the buffer it edits, and the side-effect host. (This is what keeps the
/// single `&mut Ctx` the engine operates on legal.)
pub trait VimEditor: 'static {
    fn vim_parts(&mut self) -> (&mut VimState, &mut dyn VimBufferMut, &mut dyn VimHost);

    /// Whether the engine should receive keys right now — typically
    /// `self.focus_handle.contains_focused(window, cx)`.
    fn vim_accepts_keys(&self, window: &Window, cx: &App) -> bool;

    /// Post-key hook: repaint, flush pending clipboard writes, etc.
    fn vim_did_process_key(&mut self, result: KeyResult, window: &mut Window, cx: &mut Context<Self>)
    where
        Self: Sized,
    {
        let _ = (result, window, cx);
    }
}

/// Feed one converted key into the engine of `editor`.
pub fn dispatch_key<E: VimEditor>(editor: &mut E, key: Key) -> KeyResult {
    let (vim, buf, host) = editor.vim_parts();
    let mut ctx = Ctx { buf, host };
    vim.handle_key(&mut ctx, key)
}

/// Route every keystroke through the engine before gpui's keymap and IME.
///
/// Keys the engine reports as [`KeyResult::Consumed`] are stopped with
/// `cx.stop_propagation()`; everything else falls through untouched — this is
/// the same "engine first, host second" contract IdeaVim has with IntelliJ.
pub fn attach<E: VimEditor>(entity: &Entity<E>, cx: &mut App) -> Subscription {
    let weak: WeakEntity<E> = entity.downgrade();
    cx.intercept_keystrokes(move |event, window, cx| {
        let Some(editor) = weak.upgrade() else { return };
        if !editor.read(cx).vim_accepts_keys(window, cx) {
            return;
        }
        let key = to_core_key(&event.keystroke);
        let consumed = editor.update(cx, |editor, cx| {
            {
                let (vim, _, _) = editor.vim_parts();
                vim.set_pending_unknown_char(None);
            }
            let result = dispatch_key(editor, key);
            if debug_keys() {
                eprintln!("[gpui-vim] key {:?} -> {:?}", event.keystroke, result);
            }
            editor.vim_did_process_key(result, window, cx);
            result == KeyResult::Consumed
        });
        if consumed {
            cx.stop_propagation();
        }
    })
}

/// Feed printable text (the `replace_text_in_range` path) through the engine.
///
/// Characters the engine consumes (normal-mode commands, insert-mode
/// mappings like `jk`, search-prompt input) are swallowed; characters it
/// declines in insert mode are placed into the buffer at the cursor.
pub fn dispatch_text<E: VimEditor>(editor: &mut E, text: &str) {
    for c in text.chars() {
        // On Linux/Windows the interceptor already declined this exact char
        // and the platform delivers it a second time as text: just place it.
        let already_dispatched = {
            let (vim, _, _) = editor.vim_parts();
            vim.take_pending_unknown_char() == Some(c)
        };
        if already_dispatched {
            place_text(editor, &c.to_string());
            continue;
        }
        let result = dispatch_key(editor, Key::char(c));
        if debug_keys() {
            eprintln!("[gpui-vim] text {c:?} -> {result:?}");
        }
        if result == KeyResult::Unknown {
            place_text(editor, &c.to_string());
        }
    }
    let (vim, _, _) = editor.vim_parts();
    vim.set_pending_unknown_char(None);
}

fn place_text<E: VimEditor>(editor: &mut E, text: &str) {
    let (vim, buf, host) = editor.vim_parts();
    let mut ctx = Ctx { buf, host };
    vim.insert_text_at_cursor(&mut ctx, text);
}

fn debug_keys() -> bool {
    std::env::var_os("GPUI_VIM_DEBUG_KEYS").is_some()
}

/// Convert a gpui keystroke into the engine's key model.
pub fn to_core_key(keystroke: &Keystroke) -> Key {
    let modifiers = Modifiers {
        control: keystroke.modifiers.control,
        alt: keystroke.modifiers.alt,
        shift: keystroke.modifiers.shift,
        platform: keystroke.modifiers.platform,
    };
    let key = &keystroke.key;
    let kind = if modifiers.control || modifiers.alt {
        // command chords use the base key name; single chars become Char
        let mut chars = key.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => KeyKind::Char(c),
            _ => KeyKind::Named(key.clone()),
        }
    } else if let Some(c) = keystroke.key_char.as_deref().and_then(|s| s.chars().next()) {
        KeyKind::Char(c)
    } else if modifiers.shift {
        // shift + single char without key_char (parse path): uppercase it
        let mut chars = key.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => KeyKind::Char(c.to_ascii_uppercase()),
            _ => KeyKind::Named(key.clone()),
        }
    } else {
        // `Keystroke::parse` leaves key_char empty for single characters
        let mut chars = key.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => KeyKind::Char(c),
            _ => KeyKind::Named(key.clone()),
        }
    };
    Key { modifiers, kind }
}

/// A key context exposing the current mode to gpui keymap predicates, so
/// hosts can bind extra keys per mode (`"mode == insert"` etc.).
pub fn key_context(mode: Mode) -> KeyContext {
    let mut ctx = KeyContext::new_with_defaults();
    ctx.add("Vim");
    ctx.set("mode", mode.context_name());
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversion_plain_chars() {
        let k = Keystroke::parse("a").unwrap();
        assert_eq!(to_core_key(&k), Key::char('a'));
        let k = Keystroke::parse("shift-d").unwrap();
        assert_eq!(to_core_key(&k).printable_char(), Some('D'));
    }

    #[test]
    fn conversion_named_and_chords() {
        let k = Keystroke::parse("escape").unwrap();
        assert_eq!(to_core_key(&k), Key::escape());
        let k = Keystroke::parse("ctrl-a").unwrap();
        assert_eq!(to_core_key(&k), Key::ctrl_char('a'));
        let k = Keystroke::parse("space").unwrap();
        assert_eq!(to_core_key(&k).printable_char(), Some(' '));
    }
}
