# Integrating gpui-vim into your gpui editor

This guide walks through wiring the vim engine into an existing gpui text
editor. For a complete working reference, read `crates/gpui-vim-demo/src/` —
it is meant to be copied.

## 1. Dependencies

```toml
[dependencies]
gpui = "0.2"
vim-core = { path = "../vim-core" }     # or a git/path dep on this repo
gpui-vim = { path = "../gpui-vim" }
```

## 2. Implement the buffer traits

The engine never owns your text. It reads through `VimBuffer` and edits
through `VimBufferMut`. All offsets are **UTF-8 byte offsets**; line ranges
include the terminating `\n`; a trailing newline does not open a phantom
last line.

```rust
use vim_core::buffer::{VimBuffer, VimBufferMut};

#[derive(Clone)]
pub struct RopeBuffer(std::rc::Rc<std::cell::RefCell<ropey::Rope>>);

impl VimBuffer for RopeBuffer {
    fn len(&self) -> usize { /* rope.len_bytes() */ todo!() }
    fn line_count(&self) -> usize { /* len_lines(), minus 1 if text ends with \n */ todo!() }
    fn char_at(&self, offset: usize) -> Option<char> { todo!() }
    fn prev_char_offset(&self, offset: usize) -> Option<usize> { todo!() }
    fn line_range(&self, line: usize) -> std::ops::Range<usize> { todo!() }
    fn offset_to_line(&self, offset: usize) -> usize { todo!() }
    fn slice(&self, range: std::ops::Range<usize>) -> String { todo!() }
}

impl VimBufferMut for RopeBuffer {
    fn insert_text(&mut self, offset: usize, text: &str) { todo!() }
    fn delete_range(&mut self, range: std::ops::Range<usize>) { todo!() }
}
```

> ropey gotcha (1.x): `Rope::slice/insert/remove` take **char** indices —
> convert with `try_byte_to_char` at the boundary. See the demo's
> `crates/gpui-vim-demo/src/buffer.rs`.

## 3. Implement the host trait

`VimHost` is how the engine reaches the outside world:

```rust
impl VimHost for HostState {
    fn viewport(&self) -> (usize, usize) { self.first_last_visible_line }
    fn scroll_to_line(&mut self, line: usize) { /* scroll handle */ }
    fn clipboard_write(&mut self, text: &str) { /* defer; flush with App later */ }
    fn clipboard_read(&self) -> Option<String> { todo!() }
    fn set_search_highlights(&mut self, matches: &[Range<usize>], current: Option<Range<usize>>) { }
    fn begin_undo_group(&mut self, id: u64, cursor: usize) {
        // fresh id = new undo unit; same id = same unit (an insert session).
        // Snapshot (text, cursor) here; `cursor` is where to jump on undo.
    }
    fn undo(&mut self) -> Option<usize> { /* restore, return cursor */ }
    fn redo(&mut self) -> Option<usize> { todo!() }
    fn changed(&mut self) { }
    fn bell(&mut self) { /* visual feedback for ignored keys */ }
}
```

Keep the buffer view and the host view in **disjoint fields** of your editor
(e.g. `buffer: RopeBuffer`, `host: HostState`); the engine context borrows
both at once.

## 4. Implement `VimEditor` on your view

```rust
impl gpui_vim::VimEditor for Editor {
    fn vim_parts(&mut self) -> (&mut VimState, &mut dyn VimBufferMut, &mut dyn VimHost) {
        (&mut self.vim, &mut self.buffer, &mut self.host)
    }

    fn vim_accepts_keys(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle.contains_focused(window, cx)
    }

    fn vim_did_process_key(&mut self, _r: KeyResult, _w: &mut Window, cx: &mut Context<Self>) {
        self.flush_clipboard(cx);   // engine clipboard writes are deferred
        self.flush_scroll();        // engine scroll requests
        cx.notify();                // repaint
    }
}
```

## 5. Register the engine

```rust
cx.open_window(options, |window, cx| {
    let editor = cx.new(|cx| Editor::new(cx));
    window.focus(&editor.read(cx).focus_handle);   // engine needs focus to intercept
    let _sub = gpui_vim::attach(&editor, cx);      // keystroke interceptor
    editor
})?;
```

## 6. Route the two key paths

gpui 0.2 delivers keystrokes on **two paths** (macOS behavior), and both must
feed the engine:

**Path A — special keys** (`Esc`, arrows, `Space`, `Enter`, `Ctrl-*`, F-keys):
`attach` installs an `intercept_keystrokes` hook that runs before gpui's
keymap. Consumed keys are stopped via `cx.stop_propagation()`; the rest fall
through to your own bindings and shortcuts.

**Path B — printable characters**: on macOS these never reach the
interceptor; the platform hands them to your `InputHandler` as
`replace_text_in_range`. Forward them to the same pipeline:

```rust
impl EntityInputHandler for Editor {
    fn replace_text_in_range(&mut self, range: Option<Range<usize>>, text: &str,
                             _window: &mut Window, cx: &mut Context<Self>) {
        // explicit non-empty range = IME editing its own composition text
        if let Some(explicit) = range.and_then(|r| to_bytes(r).filter(|r| !r.is_empty())) {
            if matches!(self.vim.mode(), Mode::Insert | Mode::Replace) {
                let (vim, buf, host) = gpui_vim::VimEditor::vim_parts(self);
                vim.replace_range(&mut Ctx { buf, host }, explicit, text);
            }
        } else {
            gpui_vim::dispatch_text(self, text);   // <- the engine pipeline
        }
        cx.notify();
    }
    // ... rest of the trait; see demo/src/editor.rs
}
```

`dispatch_text` runs each character through the engine: normal-mode commands,
insert-mode mappings (`jk` → Esc) and search-prompt input all work; chars the
engine declines in insert mode are placed at the cursor. (On Linux/Windows
the interceptor already saw the char once — `dispatch_text` detects and
de-duplicates that.)

## 7. Render from engine state

Read state after each key and paint it yourself:

```rust
editor.vim.mode();                    // Mode::Normal/Insert/Visual{..}/CommandLine
editor.vim.mode_indicator();          // "INSERT", "V-LINE", ...
editor.vim.showcmd();                 // "3d" while a count/operator is pending
editor.vim.cursor_offset();           // byte offset of the cursor
editor.vim.cursor_is_block();         // block in normal/visual, bar in insert
editor.vim.visual_selection();        // Some((anchor, cursor, kind)) in visual mode
editor.vim.options.number = true;     // option subset the engine reads
editor.host.highlights.clone();       // search matches to highlight
```

Your element can also expose the mode to gpui keymap predicates so users can
bind extra keys per mode:

```rust
div().key_context(gpui_vim::key_context(editor.vim.mode()))
// then: KeyBinding::new("mode == insert && escape", ...)
```

## 8. Configure the engine

```rust
let vim = &mut editor.vim;

// user mappings (classic `jk` -> Esc)
vim.keymaps_mut().map_str(ModeClass::Insert, "jk", "<Esc>");

// options
vim.options.relativenumber = true;
vim.options.scrolloff = 4;

// inspect registers / marks / search state
vim.registers.get('"');
vim.marks.get('a');
vim.search.pattern.clone();
```

## Notes & current limitations

- The engine is single-caret; visual-block and multi-cursor are on the roadmap.
- Search patterns use Rust `regex` syntax (covers most vim "magic" patterns).
- `:` ex commands, `:s`, macros (`q`/`@`), dot repeat (`.`) and folds are not
  implemented yet; the command table is data-driven so adding them is
  additive (see `crates/vim-core/src/tables.rs`).
- The clipboard hook is synchronous but gpui's clipboard needs `&mut App`:
  stage writes in the host and flush them in `vim_did_process_key`, and sync
  the system clipboard into the host on window focus / paste (see demo).
