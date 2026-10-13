# gpui-vim: a reusable Vim engine for gpui text-editing projects

## 1. Research conclusions (basis of the plan)

- **The ideavim architecture** is a strict two-layer "pure engine + host adapter" structure: the engine (`vim-engine/`) programs only against interfaces like `VimEditor/VimCaret` and reaches the host through the `injector` service locator; core mechanisms include the sealed `Mode` state machine, the `KeyStrokeTrie` keystroke prefix tree, `CommandBuilder` (count/register/motion parameter assembly), the Consumer keystroke pipeline, and the unified signatures where motions return offsets / text objects return TextRange. All of these are worth replicating in Rust.
- **gpui capabilities verified** (local zed source + docs.rs 0.2.2): `App::intercept_keystrokes` fires **before all action/event machinery**, and `stop_propagation` inside it lets the engine eat keys first; `KeyBinding` natively supports multi-key sequences like `"g g"` plus `KeyContext` predicates; `InputHandler`/`ElementInputHandler` provide IME; `uniform_list` + `paint_quad` do the editor rendering. gpui itself has no built-in multiline editor; `examples/input.rs` is the official template.
- Licensing constraint: both ideavim and zed's vim crate are GPL — **borrow the architecture only, copy no code**; this project is MIT/Apache-2.0.

## 2. Overall architecture

Three-layer workspace, the engine with zero gpui dependencies (the ideavim layering copied directly, unit-testable straight away):

```
gpui_vim/
├── Cargo.toml                  # workspace
├── crates/
│   ├── vim-core/               # pure Rust vim engine (no gpui dependency)
│   ├── gpui-vim/               # gpui integration layer: keystroke bridge + UI state + key_context
│   └── gpui-vim-demo/          # reference host: ropey buffer + multiline editor widget (the template new projects copy)
└── README.md                   # Chinese README + English integration guide
```

**Host adapter surface (`vim-core` defines the traits, the host implements them)** — much simpler than ideavim (under the plugin model the engine itself owns the caret, rather than the host owning a caret with two-way syncing):

```rust
pub trait VimBuffer {                      // read-only text + line semantics
    fn line_count(&self) -> usize;
    fn line_range(&self, line: usize) -> Range<usize>;   // offset range including the trailing newline
    fn char_indices / char_at / line_content(...);
    fn offset_to_pos(&self, off: usize) -> (usize, usize); // (line, col), byte offset
    fn pos_to_offset(&self, line: usize, col: usize) -> usize;
}
pub trait VimBufferMut: VimBuffer {
    fn insert(&mut self, offset: usize, text: &str);
    fn delete_range(&mut self, range: Range<usize>);
}
pub trait VimHost {                        // viewport / feedback / system side effects
    fn scroll_caret_into_view(&mut self);
    fn clipboard_write(&mut self, text: &str, explicit_register: bool);
    fn clipboard_read(&self) -> Option<String>;
    fn set_search_highlights(&mut self, matches: &[Range<usize>], current: Option<usize>);
    fn undo_group(&mut self, event: UndoEvent); // Begin(cmd_id) / BeginInsert / Split / End
    fn changed(&mut self);                 // request a repaint
}
```

Internal coordinates are uniformly **UTF-8 byte offsets** (ropey native), with UTF-16↔UTF-8 mapping only at the gpui `InputHandler` boundary (avoiding ideavim's coordinate pain points).

## 3. vim-core design (ideavim concepts Rust-ified)

- **State machine**: `enum Mode { Normal, Insert, Replace(placeholder), Visual{kind: Char|Line|Block(placeholder)}, CmdLine(prompt), OperatorPending }` plus the nested `return_to` field (matching ideavim's `Mode.kt`). All engine state lives in `VimState`: mode, cursor (offset + desired_col + visual_anchor), pending keys, count, pending register, macro placeholder.
- **Keystroke pipeline** (`VimState::handle_key(&mut, key: Key, buf, host) -> KeyResult::{Consumed, Unknown}`; `Unknown` means "hand the key back to the host", matching ideavim's `KeyProcessResult`); the consumer order simplified to: cmdline input → user mappings (recursive, with a depth cap) → count parsing → `"x` register → operator-pending → command trie → character arguments like `f/t/r` → insert-mode characters. Synchronous execution (ideavim's async two-phase snapshot dropped).
- **Command tables (data-driven)**: a static table `(keys: &[&str], modes: &[Mode], handler: fn)`, built per mode into a `Trie` at startup (matching `KeyStrokeTrie`); trie intermediate nodes are exactly the pending state; adding a command = one table row + one handler function.
- **Motion**: `enum Motion` (hjkl 0 ^ $ w W b B e E ge f/F/t/T ; , % gg G { } n N * # | H M L zz zt zb gj gk); handlers return `MotionResult { offset, kind: Exclusive|Inclusive|Linewise }`; operators compute the delete range from the kind, with a special case for vim's `cw` semantics.
- **TextObject**: `fn(iw/aw/i"/a(/it...) -> Option<TextRange>`, reusing one set of word-boundary scanning functions (shared by motions and objects).
- **Operator**: `enum Operator { Delete, Change, Yank, Indent(Dir), Case(...) }`, combining orthogonally with motions/objects (`2ciw` = count×operator×object, matching `CommandBuilder`'s count multiplication).
- **Registers**: `HashMap<char, Register>` + unnamed/numbered(0,1..)/`_` blackhole semantics + linewise/charwise marks; `+` goes through the host clipboard.
- **Search**: the `regex` crate; `/ ? n N * #`; `hlsearch`/`incsearch` highlights handed to the host for drawing via `set_search_highlights`; cmdline mode maintains an input buffer and history.
- **marks**: a-z local + `'` `` ` `` jumps + automatic marks (`` `. `` `'^` `'<` `'>`).
- **Undo semantics**: one normal command = one undo group; an entire `i…<Esc>` span = one group (merged via `undo_group` events notified to the host); u/C-r are executed against the host's stack operations (the demo implements them with a snapshot stack).
- **keymap API**: `VimState::map(mode, "jj", "esc")` / `noremap` programmatic API (vimrc file parsing deferred).
- **Testing**: a `StringBuffer` test host + table-driven tests (`press dd in normal mode on "..." → assert buffer/cursor/register`), covering the motion×operator×object combination matrix.

## 4. gpui-vim integration layer

- `gpui_vim::attach(&mut App, WeakEntity<VimSession>)`: registers `intercept_keystrokes`; the callback checks that focus belongs to this editor → `vim.update(...)` → `handle_key` → on `Consumed` call `cx.stop_propagation()`, on `Unknown` fall back to the host keymap/IME.
- Modes are exposed as `key_context("vim")` + `mode == normal/insert/visual` predicates, so the host can bind its own supplementary keybindings per mode (the same model as Zed vim).
- `VimSession` is an `EventEmitter<VimEvent>` (ModeChanged/StateChanged) that the host repaints from; it offers `mode()`, `showcmd()` (e.g. `3d`) and `cursor_shape()` (block/beam) queries.
- Three steps to integrate into a new project: implement `VimBuffer(+Mut)` (copy the demo's ropey implementation) → `cx.new(VimSession::new)` → `attach`, then read the session at render time to draw cursor/selection/status.

## 5. Demo host (gpui-vim-demo: reference implementation and smoke app)

- A ropey `Rope` implements `VimBuffer(Mut)`; undo is a snapshot stack + group merging.
- Editor view: `uniform_list` renders line by line with `shape_line` (monospace font); selections/search highlights are translucent quads; block(normal/visual)/beam(insert) carets via `paint_quad` (the input.rs pattern); `track_focus` + `key_context("vim")`.
- `InputHandler` implementation (IME): `replace_text_in_range` and friends all forward to the engine, which mutates the buffer in insert mode and advances the engine cursor; UTF-16↔UTF-8 mapping; marked text (composition) rendered as an underline via an overlay range; `accepts_text_input=false` in normal mode.
- UI: status line (-- INSERT --/VISUAL mode indicator, showcmd, search prompt) + bottom cmdline input row + line-number column (number/relativenumber).

## 6. Implementation order (every step verifiable)

1. workspace scaffolding + vim-core: Mode/Key/VimState/buffer traits/StringBuffer test skeleton
2. Normal-mode core: motions + d/c/y + count + registers + undo events, table-driven tests passing
3. gpui-vim bridge + demo v0: render the buffer in a window, interceptor wired, manually verify hjkl/dd/cw/yiw
4. Insert mode: IME InputHandler + i/a/o/O/I/A/s/C/x/r/J/p/P etc. + Esc semantics
5. Visual (char/line) + the full operator/text-object set + v/o/gv
6. Search (/?nN*#) + incsearch/hlsearch highlights + cmdline UI + marks
7. Polish: status line/showcmd/keymap API/line numbers and a scrolloff option subset
8. Full test suite + README (Chinese) + INTEGRATION.md (English integration guide)

## 7. Explicitly out of scope (recorded in the README roadmap)

vimrc script parsing, `:s` substitution, macro recording, Visual-Block multi-cursor, Replace mode, folding, multi-buffer/window commands, `'timeout'` timeouts — the architecture leaves extension points; add later as needed.

## 8. Verification

- `cargo test` (mostly vim-core table-driven) + `cargo clippy`;
- `cargo run -p gpui-vim-demo` manual smoke checklist (hjkl/dd/cw/yiw p/3dw/ciw/v j d/adding a count from the numeric keypad/i a o Esc//search nN/m'{mark}/u C-r/mode indicator and cursor-shape switching/Chinese IME input).

Dependencies: `gpui = "0.2"`, `ropey`, `regex` (all MIT/Apache compatible).
