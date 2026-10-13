# gpui-vim

A **reusable Vim engine** for text editing in [gpui](https://gpui.rs) (Zed's Rust UI framework) projects.

The architecture follows JetBrains IdeaVim's "pure engine + host adapter layer" split (design inspiration
only; no GPL code copied), letting any gpui app gain full vim editing capability in three steps.

```
┌────────────────────────────────────────────────┐
│ Your gpui app / editor component               │
├────────────────────────────────────────────────┤
│ gpui-vim   integration: key intercept, modes   │
├────────────────────────────────────────────────┤
│ vimcore    pure Rust engine (zero gpui deps)   │
│   mode machine · key pipeline · Trie tables    │
│   motion · operator · text object              │
│   registers · search · marks · undo semantics  │
├────────────────────────────────────────────────┤
│ Host traits: VimBuffer(Mut) + VimHost (yours)  │
└────────────────────────────────────────────────┘
```

## Features (v1)

- **Modes**: Normal / Insert / Visual (char & line) / command line (`/` `?` search)
- **Motions**: `h j k l 0 ^ $ g_ w W b B e E ge gE f F t T ; , % gg G { } ( ) n N * # | H M L zz zt zb C-d C-u C-f C-b + - ' ` etc., all count-aware (`3w`, `2gg`)
- **Operators**: `d c y > < gu gU g~`, composing orthogonally with motions / text objects (`dd` `cw` `ciw` `3dw` `dap` `g~~` `>j`…), implementing vim's range special-case rules (`dw` does not cross lines, `cw` equals `ce`, the column-1 rule, etc.)
- **Text objects**: `iw aw iW aW is as ip ap`, quotes `i" a" i' a' i` a`` ``, bracket blocks `i( a( i[ a[ i{ a{ i< a< ib aB`, `it at`
- **Registers**: unnamed, `"0` yank, `"1`-`"9` delete ring, `"-` small delete, `"_` blackhole, `"+` system clipboard, `"a-"z` named registers; linewise/charwise semantics; `p P` with count
- **Editing commands**: `x X s S D C Y r ~ J gJ p P u C-r o O i a I A gI >> << zz zt zb gv m ' ` plus arrow keys/page keys
- **Search**: `/ ? n N * #`, Rust `regex` syntax, incsearch / hlsearch highlighting, search history
- **Marks**: `m{a-z}`, `` `{char} ``, `'{char}`, automatic marks (`< > ^ .`)
- **Undo**: one command one group; one whole insert session one group (`i…<Esc>` undoes in one step); semantics emitted by the engine, executed by the host
- **User mappings**: programmatic `map`/`noremap` API (e.g. the classic `jk` → Esc, with pending state and a recursion limit)
- **UI companion**: mode indicator, showcmd, block/beam cursor, search highlighting, line numbers (absolute/relative)

## Quick start

```bash
cargo run --release -p gpui-vim-demo        # run the demo editor
GPUI_VIM_SMOKE=1 cargo run -p gpui-vim-demo # auto-injects a key sequence 2s after launch for a self-check
cargo test                                  # integration layer + demo tests (46; engine tests live in the vimcore repo)
```

The demo app is a complete multi-line editor (ropey buffer, line numbers, status line, search highlighting, mouse click/drag selection, IME); `crates/gpui-vim-demo/src/` is the reference implementation for new projects.

## Using it in your project

Three-step integration; details in [INTEGRATION.md](INTEGRATION.md):

1. Your buffer implements `VimBuffer` / `VimBufferMut` (see the demo's ropey implementation)
2. Your editor view implements the `gpui_vim::VimEditor` trait
3. `gpui_vim::attach(&editor_entity, cx)` + call `gpui_vim::dispatch_text` inside `InputHandler::replace_text_in_range`

```rust
// Key handling is fully data-driven: extending commands = adding a row to the table
b.normal(&["Z", "Z"], CmdKind::Normal(NormalCmd::SaveAndQuit));
```

## Key design (inherited from IdeaVim)

- **Zero engine dependencies**: `vimcore` does not depend on gpui; `KeyResult::Unknown` means "not handled by the engine, returned to the host", coexisting with host keymaps / system shortcuts
- **Data-driven command table**: a static command table + one key Trie per mode (the counterpart of IdeaVim's `KeyStrokeTrie`); Trie interior nodes are exactly the operator-pending states
- **The engine owns the caret**: under the plug-in model the engine manages cursor/mode/selection anchors itself; the host only implements buffer read/write and the viewport/clipboard/undo hooks (much simpler than IdeaVim's host-owned-caret model)
- **Single boundary coordinate**: the engine uses UTF-8 byte offsets internally throughout, converting once only at the gpui `InputHandler` (UTF-16) boundary
- **macOS dual key paths** (a gpui 0.2 platform trait, already handled in the integration layer): special keys (Esc/arrows/`C-*`) go through the `intercept_keystrokes` interceptor; printable characters are routed to the IME first by the system (`replace_text_in_range`) and must be funneled into the same engine pipeline via `dispatch_text`

## Layout

```
crates/
(The pure engine has been split into its own repo: beyondlex/vimcore, incl. the TCK contract)
├── gpui-vim/       gpui integration: VimEditor trait, attach interception, dispatch_text (IME),
│                   render components (overlay/line painting/blink), ~/.vimcorerc loading
└── gpui-vim-demo/  reference host: ropey buffer + editor view + IME + mouse + status line
```

## Embedding the engine in your gpui app

```rust
use gpui::prelude::*;
use vimcore::state::VimState;

struct Editor {
    vim: VimState,
    buffer: RopeBuffer,      // impl VimBuffer + VimBufferMut
    focus_handle: FocusHandle,
}

impl gpui_vim::VimEditor for Editor {
    fn vim_parts(&mut self) -> (&mut VimState, &mut dyn VimBufferMut, &mut dyn VimHost) {
        (&mut self.vim, &mut self.buffer, &mut self.host)
    }
    fn vim_accepts_keys(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle.contains_focused(window, cx)
    }
}

// inside Application::run:
let subscription = gpui_vim::attach(&editor_entity, cx); // must be kept alive (store it on the view)
```

Render via `gpui_vim::render`: per line `compute_line_overlays` → `paint_vim_line`
(including the inverted block cursor, the three visual selection styles, search highlights, and
caret blink via `CaretBlinker`); the returned `ShapedLine` is used for mouse/IME reverse lookup.
Configuration loads in two layers (`~/.vimcorerc` user layer +
`~/.config/<app>/vimrc` host layer; `set`, the `:map` family incl. `noremap`/`<Leader>`,
`source`, `"` comments). `:action SomeId<CR>`
in a mapping RHS calls the host action system (`VimHost::dispatch_host_action_hinted`).
The ready-made route-B component `gpui_vim::edit::VimEdit` additionally ships IME, mouse
drag selection, single-line collapse, event out-push, and an empty-buffer placeholder hint (`set_placeholder`).

Supported features: `hjkl w b e f t % gg G 0 ^ $`, operators `d c y > < gu gU g~`, text
objects `iw aw i" a( it ...`, `C-v` blockwise Visual (incl. block `I/A/c/p`), `R`, `.`, macros
`q/@`, jumplist `C-o/C-i`, `/ ? n N * #` (incsearch/hlsearch).

### Config layering (one rc shared across apps)

Multiple gpui apps can share one `~/.vimcorerc`: options and mappings take effect per app independently;
the only cross-app difference is the target of `:action <id>`. Loading uses two layers:

1. **User layer** `~/.vimcorerc` — cross-app key-binding habits. An unmatched `action` is **silently
   ignored** (mappings here may be written for another app);
2. **Host layer** `~/.config/<app>/vimrc` — app-specific mappings, **loaded later, same key overrides**;
   an unmatched `action` is **reported** (real errors must surface).

The host customizes paths via `gpui_vim::config::{Layers, load_layers}`; the `:action <id>`
bridge is handed to the host via `VimHost::dispatch_host_action_hinted(id, strict)` to decide whether to
report an error or ignore.

## Status

All 13 ROADMAP tasks (see `ROADMAP.md`) are complete: blockwise Visual, `.`, macros, jumplist,
`R`, the wide-character column model, `:` commands (`:noh`/`:set`/`:%s`/`:w`/`:q`), rc configuration and
the `:action` bridge, render componentization. Not supported (deliberately): multi-cursor, folding,
`'timeout'` waits, `init.lua`/full vimscript, `:g`/external commands.

## License

MIT OR Apache-2.0. Architectural ideas reference IdeaVim (GPL-3, design inspiration only) and Zed's vim mode (GPL-3, behavior comparison only); all code is an original implementation.
