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

pub mod render;
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
        if debug_keys() {
            eprintln!("[gpui-vim] interceptor fired: {:?}", event.keystroke);
        }
        let Some(editor) = weak.upgrade() else { return };
        let accepts = editor.read(cx).vim_accepts_keys(window, cx);
        if debug_keys() {
            eprintln!("[gpui-vim]   accepts={accepts}");
        }
        if !accepts {
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
    vim.record_typed_text(text);
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
        // macOS reports Enter and Tab with a control character in `key_char`
        // ("\n", "\t") while `key` carries the canonical name. Convert them
        // back to named keys so the engine sees <Enter>/<Tab> instead of
        // printable text — otherwise the search prompt appends the Enter to
        // the pattern instead of executing it.
        match (keystroke.key.as_str(), c) {
            ("enter", '\n' | '\r') => KeyKind::Named("enter".to_owned()),
            ("tab", '\t') => KeyKind::Named("tab".to_owned()),
            _ => KeyKind::Char(c),
        }
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
    use std::cell::RefCell;
    use std::ops::Range;
    use std::rc::Rc;
    use vim_core::buffer::{VimBuffer, VimBufferMut};
    use vim_core::host::VimHost;

    /// Minimal host fixture so tests can run keys through the full
    /// conversion → engine pipeline.
    #[derive(Clone)]
    pub(crate) struct TestBuf(pub Rc<RefCell<String>>);

    impl VimBuffer for TestBuf {
        fn len(&self) -> usize {
            self.0.borrow().len()
        }
        fn line_count(&self) -> usize {
            self.0.borrow().split('\n').count()
        }
        fn char_at(&self, offset: usize) -> Option<char> {
            self.0.borrow()[offset..].chars().next()
        }
        fn prev_char_offset(&self, offset: usize) -> Option<usize> {
            if offset == 0 || offset > self.0.borrow().len() {
                return None;
            }
            self.0.borrow()[..offset].chars().next_back().map(|c| offset - c.len_utf8())
        }
        fn line_range(&self, line: usize) -> Range<usize> {
            let text = self.0.borrow();
            let mut start = 0;
            for (i, part) in text.split('\n').enumerate() {
                if i == line {
                    return start..start + part.len();
                }
                start += part.len() + 1;
            }
            text.len()..text.len()
        }
        fn offset_to_line(&self, offset: usize) -> usize {
            self.0.borrow()[..offset.min(self.0.borrow().len())].split('\n').count() - 1
        }
        fn slice(&self, range: Range<usize>) -> String {
            self.0.borrow()[range].to_owned()
        }
    }

    impl VimBufferMut for TestBuf {
        fn insert_text(&mut self, offset: usize, text: &str) {
            self.0.borrow_mut().insert_str(offset, text);
        }
        fn delete_range(&mut self, range: Range<usize>) {
            self.0.borrow_mut().replace_range(range, "");
        }
    }

    struct NoopHost;
    impl VimHost for NoopHost {
        fn viewport(&self) -> (usize, usize) {
            (0, 24)
        }
        fn scroll_to_line(&mut self, _: usize) {}
        fn clipboard_write(&mut self, _: &str) {}
        fn clipboard_read(&self) -> Option<String> {
            None
        }
        fn set_search_highlights(&mut self, _: &[Range<usize>], _: Option<Range<usize>>) {}
        fn begin_undo_group(&mut self, _: u64, _: usize) {}
        fn undo(&mut self) -> Option<usize> {
            None
        }
        fn redo(&mut self) -> Option<usize> {
            None
        }
        fn changed(&mut self) {}
    }

    pub(crate) fn dispatch(vim: &mut VimState, buf: &mut TestBuf, key: Key) -> KeyResult {
        let mut ctx = Ctx { buf, host: &mut NoopHost };
        vim.handle_key(&mut ctx, key)
    }

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

    #[test]
    fn conversion_enter_and_tab_carry_control_key_chars_on_macos() {
        // the shape `parse_keystroke` produces on macOS: named key + control
        // char in key_char
        let enter = Keystroke {
            key: "enter".into(),
            key_char: Some("\n".into()),
            modifiers: Default::default(),
        };
        assert_eq!(to_core_key(&enter), Key::enter());
        let tab = Keystroke {
            key: "tab".into(),
            key_char: Some("\t".into()),
            modifiers: Default::default(),
        };
        assert_eq!(to_core_key(&tab), Key::tab());
    }

    #[test]
    fn macos_enter_executes_search_end_to_end() {
        // the reported bug: `/` + pattern + Enter executed nothing and the
        // Enter landed in the pattern. This drives the exact macOS keystroke
        // shape through to_core_key into the engine.
        let buf = TestBuf(Rc::new(RefCell::new("foo bar foo baz".to_owned())));
        let mut vim = VimState::new();

        for c in "/foo".chars() {
            assert_eq!(dispatch(&mut vim, &mut buf.clone(), Key::char(c)), KeyResult::Consumed);
        }
        assert!(matches!(vim.mode(), vim_core::Mode::CommandLine { prompt: '/' }));
        assert_eq!(vim.cmdline.buffer, "foo");

        let enter = Keystroke {
            key: "enter".into(),
            key_char: Some("\n".into()),
            modifiers: Default::default(),
        };
        assert_eq!(dispatch(&mut vim, &mut buf.clone(), to_core_key(&enter)), KeyResult::Consumed);
        assert_eq!(vim.mode(), vim_core::Mode::Normal);
        assert_eq!(vim.cursor_offset(), 8); // jumped to the second "foo"
        assert_eq!(vim.cmdline.buffer, "");
    }

    #[test]
    fn macos_shift_letter_reaches_normal_commands() {
        // `I`, `A`, `V` arrive as shift + base key + uppercase key_char
        let mk = |key: &str, ch: char| Keystroke {
            key: key.into(),
            key_char: Some(ch.to_string().into()),
            modifiers: gpui::Modifiers { shift: true, ..Default::default() },
        };

        // `I`: first non-blank + insert mode
        let buf = TestBuf(Rc::new(RefCell::new("    indented\n".to_owned())));
        let mut vim = VimState::new();
        dispatch(&mut vim, &mut buf.clone(), to_core_key(&mk("i", 'I')));
        assert_eq!(vim.mode(), vim_core::Mode::Insert);
        assert_eq!(vim.cursor_offset(), 4);

        // `A`: line end + insert mode
        let buf = TestBuf(Rc::new(RefCell::new("tail\n".to_owned())));
        let mut vim = VimState::new();
        dispatch(&mut vim, &mut buf.clone(), to_core_key(&mk("a", 'A')));
        assert_eq!(vim.mode(), vim_core::Mode::Insert);
        assert_eq!(vim.cursor_offset(), 4);

        // `V`: visual-line mode
        let buf = TestBuf(Rc::new(RefCell::new("alpha\nbeta\n".to_owned())));
        let mut vim = VimState::new();
        dispatch(&mut vim, &mut buf.clone(), to_core_key(&mk("v", 'V')));
        assert_eq!(
            vim.mode(),
            vim_core::Mode::Visual { kind: vim_core::VisualKind::Line }
        );
    }
}

#[cfg(test)]
mod render_tests {
    use super::render::{compute_line_overlays, LineOverlayInputs, OverlayStyle};
    use super::tests::{dispatch, TestBuf};
    use gpui::{px, FontStyle, FontWeight, Hsla};
    use std::cell::RefCell;
    use std::rc::Rc;
    use vim_core::key::Key;
    use vim_core::state::VimState;

    fn style() -> OverlayStyle {
        OverlayStyle {
            font: gpui::Font {
                family: "test".into(),
                features: Default::default(),
                fallbacks: None,
                weight: FontWeight::NORMAL,
                style: FontStyle::Normal,
            },
            font_size: px(14.0),
            text: Hsla::default(),
            background: Hsla::default(),
            cursor: Hsla::default(),
            selection: Hsla::default(),
            search: Hsla::default(),
            search_current: Hsla::default(),
            mark: Hsla::default(),
            caret_fallback_width: 8.0,
        }
    }

    fn overlays(
        vim: &VimState,
        buf: &TestBuf,
        line: usize,
        highlights: &[std::ops::Range<usize>],
        caret_visible: bool,
    ) -> super::render::LineOverlays {
        compute_line_overlays(&LineOverlayInputs {
            vim,
            buf,
            line,
            search_highlights: highlights,
            search_current: None,
            ime_marked: None,
            caret_visible,
            style: &style(),
        })
    }

    #[test]
    fn search_highlights_render_as_quads() {
        let buf = TestBuf(Rc::new(RefCell::new("foo bar foo\n".into())));
        let vim = VimState::new();
        let highlights = vec![0..3, 8..11];
        let o = overlays(&vim, &buf, 0, &highlights, true);
        assert_eq!(o.quads.len(), 2);
        assert_eq!(o.quads[0].0, 0..3);
        assert_eq!(o.quads[1].0, 8..11);
    }

    #[test]
    fn char_selection_and_caret_line_gating() {
        let buf = TestBuf(Rc::new(RefCell::new("abcdef\ngh\n".into())));
        let mut vim = VimState::new();
        // enter charwise visual and extend to cover "bcde"
        dispatch(&mut vim, &mut buf.clone(), Key::char('v'));
        dispatch(&mut vim, &mut buf.clone(), Key::char('l'));
        dispatch(&mut vim, &mut buf.clone(), Key::char('l'));
        dispatch(&mut vim, &mut buf.clone(), Key::char('l'));
        let o = overlays(&vim, &buf, 0, &[], true);
        assert_eq!(o.quads.len(), 1);
        assert_eq!(o.quads[0].0, 0..4); // v..cursor inclusive (cursor on 'd')
        assert_eq!(o.cursor, Some(3));
        assert!(o.cursor_block);

        // another line: no quads, no cursor
        let o = overlays(&vim, &buf, 1, &[], true);
        assert!(o.quads.is_empty());
        assert_eq!(o.cursor, None);

        // caret hidden by the blink phase
        let o = overlays(&vim, &buf, 0, &[], false);
        assert_eq!(o.cursor, None);
        assert!(!o.quads.is_empty());
    }

    #[test]
    fn line_selection_is_full_width() {
        let buf = TestBuf(Rc::new(RefCell::new("abcdef\ngh\n".into())));
        let mut vim = VimState::new();
        dispatch(&mut vim, &mut buf.clone(), Key::char('V'));
        let o = overlays(&vim, &buf, 0, &[], true);
        assert_eq!(o.quads.len(), 1);
        assert!(o.quads[0].2, "linewise quad spans the full width");
    }

    #[test]
    fn block_selection_quad_per_row() {
        let buf = TestBuf(Rc::new(RefCell::new("abcd\nefgh\n".into())));
        let mut vim = VimState::new();
        // C-v j l: block cols 0..1 on lines 0-1
        dispatch(&mut vim, &mut buf.clone(), vim_core::key::Key::ctrl_char('v'));
        dispatch(&mut vim, &mut buf.clone(), Key::char('j'));
        dispatch(&mut vim, &mut buf.clone(), Key::char('l'));
        let o0 = overlays(&vim, &buf, 0, &[], true);
        assert_eq!(o0.quads.len(), 1);
        assert_eq!(o0.quads[0].0, 0..2);
        let o1 = overlays(&vim, &buf, 1, &[], true);
        assert_eq!(o1.quads.len(), 1);
        assert_eq!(o1.quads[0].0, 0..2);
    }
}
