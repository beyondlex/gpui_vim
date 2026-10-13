# gpui-vim Roadmap

> **2026-09-28**: the pure engine has been extracted into a standalone repo,
> [beyondlex/vimcore](https://github.com/beyondlex/vimcore) (crate name
> `vimcore`). The engine-side tasks in this document (former `crates/vim-core`
> paths, `tests/engine.rs`, etc.) moved with the code to that repo; for the
> engine parts of the "global invariants" below, vimcore is authoritative. The
> tasks remaining in this repo cover only the integration layer and the demo.

Execution plan for later contributors / AI agents. Each task gives: the current
state (with code locations), the plan, acceptance criteria, and pitfalls.
**Read the "global invariants" section before starting work** — these are
semantic constraints this codebase paid for in already-stepped-on traps;
violating them regresses already-fixed bugs outright.

Project layout (current):

- `crates/gpui-vim` — integration layer: `attach()` (keystroke interceptor),
  `to_core_key` (gpui `Keystroke` → engine `Key` conversion), `dispatch_text`
  (IME text path), the `VimEditor` trait, the `edit::VimEdit` component (route
  B packaged form), `pager` (read-only route C), the `render` line-rendering
  component, `config` layered rc loading.
- `crates/gpui-vim-demo` — the reference host (multi-buffer tabs, rendering,
  IME, mouse, clipboard). `editor.rs` is the copy-out reference implementation
  of the attach route; for the consumption shapes of `edit.rs`/`pager.rs` see
  the three routes in INTEGRATION.md.

Verification commands:

```sh
cargo test                # all tests (86 currently, must be fully green before delivering)
cargo clippy --workspace  # currently 0 warnings, do not introduce new ones
cargo run -p gpui-vim-demo
```

---

## Global invariants (a violation is a regression)

1. **Key normalization happens in two places; both are required**
   - `to_core_key` (gpui-vim/src/lib.rs): on macOS the `key_char` for
     Enter/Tab is `"\n"`/`"\t"`, and must be normalized back to
     `Key::enter()`/`Key::tab()`, otherwise the cmdline appends Enter into the
     search term.
   - The `VimState::handle_key` entry (vim-core/src/state.rs): the `shift`
     flag on plain `Char` keys must be stripped. The command tables are all
     registered with **modifier-free** plain chars via `Key::parse("I")`-style
     notation; gpui delivers shift+I as `{shift, Char('I')}`, and the
     Hash/Eq of `Key` include modifiers — without stripping, every uppercase
     letter and shifted punctuation (`$ ^ % ~ < > "`) misses.
   - When adding key handling, do not write `{shift, ...}`-form Keys into the
     command table; use notation strings.

2. **Undo groups: one logical command = one group.** The host (demo host.rs
   and the test HostView) **takes a snapshot** on the `begin_undo_group(id, ..)`
   call. Therefore the engine must guarantee:
   - `begin_insert` **reuses** the group when `open_undo` already has one (the
     "delete + type" of the change family c/s/C/S is a one-step undo, see
     `change_C_types_and_undoes_in_one_step`);
   - `complete_operator_with_span` / `apply_visual_operator` must **not** call
     `end_edit()` when an insert session was entered;
   - `o`/`O` call `begin_edit` **before** mutating the buffer (otherwise the
     snapshot already contains the new line and undo can never remove it).
   After changing undo-related code, run the `change_*` and `open_line_o_*`
   tests.

3. **IME composition protocol** (gpui 0.2.2 macOS, see `handle_key_event` in
   platform/mac/window.rs): as long as `marked_text_range()` returns `Some`,
   **all** keyDown events are routed to the input method first (the
   `is_composing` branch), and the engine's interceptor receives no keys in
   between — this is platform behavior; do not try to bypass it. What the
   integration layer must do is implement all three composition-protocol
   callbacks correctly (see task 1).

4. **Returning `Unknown` for printable characters in insert mode** is deliberate
   (keeps IME/composed input working). Do not switch it to Consumed in the name
   of "the engine takes over everything".

5. The `Subscription` from `intercept_keystrokes` must be kept alive (in the
   demo it lives on the view); dropping it unsubscribes.

---

## P0 — Correctness

### Task 1: Chinese/Japanese IME composition input completely unusable ✅ Done, accepted by the user

**Symptom** (reproduced by the user under a Chinese pinyin input method): in
insert mode, after switching to the pinyin input method, (1) Esc cannot exit
insert mode; (2) after the candidate word is committed, the typed pinyin
letters remain in the buffer and the Han characters appear at the wrong
position (shaped like `nihao你好`).

**Root cause** (confirmed in the source; not a wide-character issue):

1. During composition, `replace_and_mark_text_in_range` (demo editor.rs)
   inserts the **raw pinyin** (e.g. `nihao`) directly into the buffer and
   records `marked_range`. This step itself is viable (as the composition
   preview), but—
2. When the candidate is committed, macOS calls `insertText:"你好"` → gpui
   turns it into `replace_text_in_range(None, "你好")`. The demo implementation
   treats `None` as "ordinary input" and goes through `dispatch_text` (appends
   at the cursor), **without replacing the marked_range** → pinyin residue.
   The correct contract: when `marked_range` is `Some`,
   `replace_text_in_range(None, ..)` must **replace the marked_range** with the
   new text and clear the mark.
3. `unmark_text` only sets `marked_range` to None; it does not delete the
   composed text from the buffer. If the commit/cancel path only fires unmark
   without first passing through an empty-string setMarkedText, the pinyin
   stays forever.
4. `marked_text_range` still returns `Some` for the **empty range**
   `start..start`: after a cancelled composition (when the IME ends with an
   empty-string setMarkedText, the demo is left holding `start..start`), gpui's
   `is_composing` is permanently true and **every key (including Esc) is
   permanently routed to the input method first** — this is a main cause of
   "Esc cannot exit insert". An empty range must return `None`.
5. Esc being consumed by the input method during composition (cancelling the
   composition) is standard macOS behavior; the correct stance is to accept
   "Esc during composition = cancel this composition, press again to exit
   insert", consistent with real vim GUIs; do not try to intercept it.

**Plan**:

- demo `editor.rs`:
  - `marked_text_range`: `if start == end { return None }`.
  - `replace_text_in_range`: at entry first evaluate `let composing = self.marked_range.take();`;
    when `composing` is `Some` and the incoming range is None, call
    `vim.replace_range(ctx, composing, text)` (bypassing `dispatch_text` —
    **composed text must not run through the engine key pipeline**, otherwise
    it falsely triggers insert-mode mappings such as `jk`); the cursor lands
    at `composing.start + text.len()`.
  - `unmark_text`: if `marked_range` is non-empty, delete the text in that
    range first, then set it to None (defensive against cancel paths that call
    unmark directly).
  - `replace_and_mark_text_in_range`: keep the existing "replace the previously
    marked text" logic, but set None directly when the new marked range is
    empty.
- No engine-side change needed (composition does not go through the engine
  pipeline).

**Acceptance** (manual; requires switching the system input method to pinyin):

1. Type `nihao` in insert mode → the buffer shows the pinyin (possibly with
   marked highlighting); pick the candidate `你好` → the buffer is exactly
   `…你好…`, no `nihao` residue, cursor after `好`.
2. Type `ni` then press Esc → the pinyin is removed (composition cancelled),
   nothing left in the buffer; press Esc again to exit insert mode.
3. Composed text lands at the cursor rather than at the line end / file end;
   two consecutive compositions (two Chinese phrases) are both replaced
   correctly.
4. Direct English input (no composition) does not regress: `cargo test` still
   passes and ordinary typing and the `jk` mapping still work.

**Pitfall**: `marked_range` is a UTF-8 byte range while the ranges gpui passes
in are UTF-16; for conversion use only the existing `utf16_to_byte`/
`byte_to_utf16` in demo buffer.rs — do not hand-roll conversions.

### Task 2: marks do not shift after edits ✅ Done
> The implementation went beyond the original design: all buffer changes now
> converge into the three wrappers `VimState::edit_insert/edit_delete/
> edit_replace` (22 call sites), which also fixed the pre-existing bug where
> `dw` on the buffer's last line ate the newline (the target of
> `span_from_motion` was not clamped to the line end). Equal-length replacements
> (`gu`/`gU`/`g~`) keep marks inside the region at their relative positions.

**Current state**: `vim-core/src/marks.rs` stores bare byte offsets. No
insert/delete adjusts `a-z`, `'^`, `'.'` or `'<`/`'>`; after inserting a line at
the head of the file every mark is misplaced.

**Plan**: add `fn adjust_marks(&mut self, at: usize, delta: isize)` to
`VimState`: for every stored offset, those `>= at` get delta added (clamped to
0..=len). Put the call at the **single convergence point** of
`ctx.buf.insert_text` / `delete_range` — currently every engine change goes
through `VimState::insert_text_at_cursor`, `replace_range`, `ops::delete_span`,
`ops::put`, `start_insert(OpenLine)`; recommend wrapping one private helper
(e.g. `edit_buf`) to avoid shotgun call sites. For a mark inside a deleted
range: vim semantics put it at the start of the deletion.

**Acceptance**: headless tests — marks after / before / inside the deleted
range; `'<`/`'>` shift with a visual-range delete; `'^` is correct after `yyp`.

**Pitfall**: register text does not need shifting (it stores text);
`last_change` is used by `.` and the changelist, with the same shifting rule.

---

## P1 — Everyday features

### Task 3: `:` Ex command line (`:noh` `:set` `:s` `:w` `:q`) ✅ Done
> Implementation: `:noh[lsearch]`, `:set` (`name`/`noname`/`name!`/
> `name=value`, numeric items `ts/sw/so`), `:[%]s/pat/rep/[g]` (Rust regex
> expansion, `$1` instead of vim's `\1`, empty pattern reuses the last search),
> `:w`/`:q`/`:q!`/`:wq`/`:x` (new host contract `save`/`request_close`; in the
> demo `:w` shows status and `:q` exits). History is kept per prompt; `:s` is a
> single-step undo; highlight publication always respects `hlsearch` (fixed the
> inconsistency where the jump path ignored the option). Not supported: range
> syntax (`1,3s`/`'<,'>`), `:\d` line numbers, confirm mode, `:g` (see "non-goals").

**Current state**: `Mode::CommandLine { prompt }` only has `'/'`/`'?'`; pressing
`:` in normal mode returns `Unknown`. `options.set_boolean` (options.rs) exists
precisely as the reservation for `:set`; `search::clear_highlights` (currently
dead code) is the `:noh` implementation; clearing highlights on Esc already
shipped (`:noh` semantics, `n`/`N` re-light them).

**Plan**:

1. Add `':'` to the prompt of `Mode::CommandLine` (either add an `ex_buffer` to
   the `Cmdline` struct or reuse the buffer + the prompt to distinguish).
   Change the `':'` branch of normal_key from `Unknown` to `begin_cmdline(':')`.
2. Add `execute_ex(ctx, line)` to `cmdline.rs`, first supporting:
   `:noh[lsearch]`, `:set <name>`/`:set no<name>` (via `set_boolean`, numeric
   items `:set ts=8` later), `:s/foo/bar/[g]` (current line, `:%s` whole file,
   `g` flag counts; reuse `search::compile`'s regex and smartcase rules), `:w`
   (new `VimHost` contract `save(&mut self)`), `:q` (new contract
   `request_close(&mut self)`; the host decides whether to pop "unsaved
   changes" — needs a message channel beyond `bell`, see task 13).
3. History: `Cmdline.history` already exists; `:` and `/` either share it or
   split per prompt (vim splits; recommend splitting:
   `history: HashMap<prompt, Vec<String>>` requires migrating the current field).
4. Status bar rendering: demo `mode_label` just needs to show the `:…` content
   for `':'`.

**Acceptance**: headless tests covering every command (including `:%s/x/y/g`
counts, no-match, and invalid regex only ringing the bell without panicking);
after `:set hls!`/`:set nohls`, `/` searches no longer publish highlights.

**Pitfall**: the key path after `:` is identical to `/` (including the
control-character normalization of task 1); do not open a separate mode branch
for the new prompt. For the `\1` backreference inside `:s` replacements, defer
to the regex crate's `replace_all` (`$1` syntax), and note the difference from
vim's `\1` in the docs.

### Task 4: `.` repeats the last change ✅ Done
> Implementation: `VimState` records two kinds of steps, "structural keys +
> text segments" (`RecordedStep`) — typing during insert does not go through
> the key pipeline on macOS (IME path), so text is recorded as `Text` steps and,
> on replay, dropped straight into the buffer via a synthetic marker
> (`DOT_TEXT_MARKER`, not producible by `Key::parse`, no collision) so the host
> does not insert twice. Commit points: `end_command`/
> `complete_operator_with_span`/`exit_insert`/`execute_ex`; visual changes are
> repeatable: in visual mode the commit stays accumulating (`v` + motion +
> operator as one change), committing when the selection lands; replaying the
> keys re-enters visual and rebuilds the selection; blockwise `I`/`A`/`c`/`p`
> are still not repeatable (the per-line copy depends on the line table at
> insert exit; `Text` steps cannot reproduce it; `recording_blocked`). `Esc`
> cancelling a half-finished command discards it; `N.` = replay N times
> (consistent with vim's multiplication semantics); IME composition previews are
> suppressed via `set_recording_suppressed` — only committed text is repeatable.
> Also fixed: keys in cmdline mode previously did not enter the recording path;
> replaying `:` no longer breaks the main loop early.

**Current state**: no implementation. Besides `NormalCmd::Redo` there is no
concept of a "last change".

**Plan**: at the completion points of `execute_command` /
`execute_normal_cmd` / `complete_operator_with_span`, record a replayable
`LastChange` enum:

```rust
enum LastChange {
    // record "operator + motion description + count/register"; on replay, re-evaluate the motion against the current cursor
    Operator { op: Operator, target: ReplayableTarget, register: Option<char> },
    Edit { cmd: NormalCmd },       // x, p, J, ~, >>, … (with count)
    Insert { entries: Vec<Key> },  // i/a/c… entry + typed text + the Esc key sequence
}
```

- Replay = feed the recorded key sequence back into `handle_key` (reuses the
  existing pipeline, naturally getting motion semantics at the new position).
  **Do not** record byte offsets and shift them — motions must be re-evaluated
  at replay time.
- `1.` / `3.`: the count overrides the original command's count.
- Text typed during composition (task 1) does not go through the pipeline, so
  it naturally does not enter `LastChange` — consistent with vim.

**Acceptance**: `dw.`, `dd.`, `3x.`, `ciw foo <Esc> j .`, `A text <Esc> .`
(repeating `A` + text + Esc is the most common `.` use case).

**Pitfall**: `u`/`C-r`/`:` themselves do not update LastChange; after undo, `.`
still replays the last change. During replay, do not trigger further map
expansion (the `map_depth` semantics stay unchanged).

### Task 5: Visual Block (`C-v`) ✅ Done
> Implementation: `BlockSpan` (display-column space + per-line byte ranges;
> `block_row_range` selects characters by "column-span intersection", and wide
> characters straddling the boundary are included as whole characters).
> Operators: `d` (delete bottom-up), `y` (pad to block width with spaces,
> Blockwise register, cursor to block start), `c` (delete block + type on the
> cursor line + copy to the remaining lines on exit); the block-edge inserts
> `I`/`A` copy the same way; short lines insert at the line end. Two layers of
> offset correction for multi-line insertion: deletion adjusts against the
> prefix of lines **above** (the first version had the direction reversed), and
> on copy, **the text length typed on the cursor line** compensates only the
> lines below the cursor line. Single undo group (the session group covers the
> copy). `p`/`P`: a Blockwise register replaces row-aligned; other register
> text is copied per line. Not supported: blocks extended to the line end with
> `$`, uppercase-register append, `o` corner swap under `C-v`.

**Current state**: `VisualKind::Block` and `RegisterKind::Blockwise` are only
enum placeholders; no entering command, no block span, no block put.

**Prerequisite**: **do task 8 (wide-character column model) first.** The
per-line ranges of a block must be "display column" ranges; byte ranges
misalign on lines containing CJK/emoji. Change the trait once, completely.

**Plan**:

- Command table: `b.normal(&["<C-v>"], CmdKind::EnterVisual(VisualKind::Block))`.
- `ops::span_from_visual` returns a list of per-line ranges for Block (new type
  `BlockSpan { col_start, col_end, lines: Vec<Range<usize>> }`); `d`/`y`/`c`
  apply over the list; `y` stores `RegisterKind::Blockwise`; `p` inserts per line.
- `I`/`A` in block-visual = block insert (vim's signature move), implemented as
  inserting the same text at each line's start/end, entering one undo group at
  the end.
- Rendering: demo `overlays_for_line` gains a block branch (draw a quad for
  each line's `col_start..col_end`); the `LineOverlays` struct need not change.

**Acceptance**: rectangular delete/copy/paste; block selection column-aligned
across CJK lines; after a block insert, one `u` restores everything (invariant 2).

### Task 6: `R` replace mode ✅ Done
> Implementation: `InsertKind::Replace` + a command-table row; `begin_insert`
> enters `Mode::Replace` per kind; the exit behavior matches insert (vim
> actually lands on the last overwritten character — the original assumption
> "R does not step back" was wrong, implemented per real vim behavior). Works
> with `.`: replaying `[R, Text, Esc]` repeats the overtype. `{count}R` (replay
> N times) not supported, left for later.

**Current state**: `Mode::Replace` exists; `insert_text_at_cursor` already has
the overwrite branch. Missing the entering command and exit details. Plan: add
`R → EnterInsert(Replace)` (new `InsertKind::Replace`) to the command table,
`begin_insert` no longer moves the cursor; `exit_insert` does not step back one
column for Replace (the current one-character step-back is insert semantics).
Note demo `cursor_is_block` should show an underline cursor for Replace
(cosmetic, optional).

### Task 7: macros `q` / `@` ✅ Done
> Implementation: macros are stored as `HashMap<char, Vec<RecordedStep>>`
> (sharing the step model with `.`: keys + text segments — typing goes through
> the IME on macOS and not the pipeline, so Text steps must be recorded too).
> `q{reg}` starts recording; pressing `q` while recording stops **immediately**
> (not through char-arg pending, otherwise the next key is eaten as the "stop
> key" and the real `q` is left inside the macro); `@{reg}`/`@@` replay
> (recording into a register makes it the "last used", so `@@` works right
> after recording); replay reuses the `replaying` suppression (`.` unchanged,
> the queue is not broken early in cmdline mode), with a 500-key guard against
> recursive macros (the guard path now resets replaying). The demo status bar
> shows a red `recording @x`. Not supported: uppercase-register append,
> `:registers` display (waits for task 13's message channel).

**Plan**: add `macro_recording: Option<(char, Vec<Key>)>` to `VimState`;
`q{reg}` starts, `q` stops, `@{reg}` replay = the inverse of
`parse_key_sequence` — feed the recorded `Vec<Key>` one by one into
`handle_key` (a `feed_raw` loop), `@@` refers to the previous one. Registers
reuse `Registers.named` (macros and text registers share one namespace;
replaying a text register feeds it character by character, consistent with
vim). Replay loops `count` times; infinite-loop protection: recording is
disabled while replaying.

---

## P2 — Engine core evolution (define the trait first, then add features)

### Task 8: wide-character / grapheme column model ✅ Done
> The implementation is better than the original design: **the `VimBuffer`
> trait was not touched**. Both width and graphemes are derived by the engine
> from `char_at` (new dependency `unicode-width`, pure tables, zero transitive
> dependencies), so hosts get correct behavior with zero migration. Key points:
> - `desired_col` changed from a byte column to a **display column**; the key
>   fix is that `apply_motion_result` must read the target column **before**
>   the cursor lands (deriving it from the landing point after wide-char
>   clamping loses the original column — vim's `wv_col` semantics)
> - `j`/`k` keep the column across wide-character lines; `|` by display column
>   (1-based, same as vim); out-of-range lands on the last character
> - grapheme boundary rules: zero-width characters (combining marks / variation
>   selectors) attach to the preceding character, ZWJ chains the whole cluster
>   (covering accents and emoji families); `h`/`l`/`x`/`X`/`r`/`~`/`a` all step
>   by cluster
> - demo: mouse clicks and the IME caret rect use the shaped geometry cached at
>   paint time (`closest_index_for_x`/`x_for_index`) for exact back-lookup;
>   CJK/emoji hits no longer misalign
> - known limitations (documented): `w`/`b`/`e` word motions still step by char
>   (mostly no-op on emoji strings); tab width counted as 1 (not expanded per
>   tabstop)

> **Status: byte-arithmetic cursor bugs are at zero** (the `len - 1` cursor of
> `p`/`~`/visual `p`, the demo's silent append when inserting at a non-boundary,
> caret width `byte+1` — see the `cjk_*` regression tests). What remains of this
> task is **column semantics** (desired_col's byte column → display column,
> grapheme stepping, mouse hit-testing).

**Current state**: `Cursor.desired_col`, `desired_column`, all vertical motions,
`|`, `H/M/L` all use **byte columns**. CJK double-width characters and emoji
(multi-char graphemes, ZWJ sequences) cause j/k column drift; mouse clicks
(demo `byte_at_point` uses a uniform `char_width`) misalign on Chinese text.
**This is not the cause of task 1's IME problem** (IME is a protocol bug, see
task 1), but it determines the cursor/click experience after composed text is
committed, and task 5's block column alignment.

**Plan** (trait change, once and for all):

1. Add two methods to `VimBuffer` (with default implementations, old hosts
   migrate at zero cost):

```rust
/// Display width of the char at this offset (0=combining mark, 1=narrow, 2=wide/East Asian Wide/Fullwidth)
fn char_width(&self, offset: usize) -> usize { /* default: unicode-width, needs a new dependency or a built-in table */ }
/// Boundary of the grapheme cluster containing offset (back up to the complete cluster)
fn next_grapheme_offset(&self, offset: usize) -> Option<usize>;
```

2. Inside the engine, change "column" semantics from bytes to `usize` display
   columns: the conversions for `desired_column`, `Motion::Up/Down/Column` go
   through `line_start + accumulated char_width`. The lexical boundaries of
   f/t/w/e switch to graphemes (`is_word_char` in `word.rs` stays char-based,
   but one step forward/back = one grapheme rather than one char).
3. demo: `byte_at_point` uses the shaped line's `x_for_index` for back-lookup
   (shaped already has exact geometry); delete the uniform-`char_width`
   approximation.

**Acceptance**: on lines containing `中文 mix English` and emoji (`👨‍👩‍👧`, with
variation selectors): `j`/`k` keep the column, `0`/`$`/`|`, click positioning,
`x` deletes one whole grapheme at a time.

**Pitfall**: do not introduce a gpui dependency into the engine; for the width
table use the `unicode-width` crate (pure tables, no transitive-dependency
issues) or a built-in reduced table with the differences documented. **All**
affected tests must first get "wide-character regression" cases written before
changing the implementation.

### Task 9: soft wrap and real `gj`/`gk` ✅ Downgraded and closed (explicitly unsupported in v1)
> Handled via the downgrade option this task reserved: engine and demo are both
> implemented as "no soft wrap", and `gj`/`gk` stay aliases of `j`/`k`. If wrap
> is introduced later, the host must provide a line→display-row mapping
> contract and it will depend on task 8's display columns; open it then.

**Current state**: `gj`/`gk` are registered as aliases of `Motion::Down/Up`
(tables.rs notes "no soft wrap in v1"). This needs the host to report a
"display row ↔ logical line/column" mapping: add a minimal contract like
`wrapped_line_bounds(&self, line) -> Vec<display-row start columns>` to
`VimHost`, or step back: in v1 only guarantee that `j`/`k` move per display row
across wrapped rows (hosts reading the viewport already have a line concept,
which must be upgraded to (line, display_row)). **Depends on task 8's display
columns**. This task changes the contract substantially, so downgrading to
"explicitly document that wrap is unsupported" and removing the misleading
`gj`/`gk` rows is allowed.

### Task 10: jumplist (`C-o`/`C-i`) ✅ Done
> Implementation: `jumps: Vec<usize>` + `jump_pos` (index of the current
> entry). `goto_motion` records origin→dest after success for `is_jump()`
> motions (G/gg/n/N/*/`%`/`(`)`/`{`/`}`/H/M/L/C-d/u/f/b/'`` ` ``); the
> `` '{char}`` and `/pattern<CR>` execution paths record too. A new jump
> truncates the forward branch; cap of 100 entries. `C-o`/`C-i` step
> backward/forward by index; out of range rings the bell. v1 does not cross
> buffers and has no `:jumps` display (waits for task 13).

**Plan**: add `jumps: Vec<(usize, u64)>` to `VimState` (offset + buffer content
epoch; for the epoch use `undo_seq` or the host `changed()` counter, to guard
against stale jumps); the big jumps (`gg/G/n/N/'{char}/{`/`}`/`%`/`*` etc.) are
pushed in `goto_motion` on success when the displacement exceeds a threshold
(the current position is pushed first). `C-o`/`C-i` are registered in the
command table (`Key::ctrl_char('o'/'i')`). No host contract needed (v1 does not
cross buffers).

---

## P4 — Configuration files and host bridge (added by the user)

### Task 14: `~/.gpui-vimrc` config + `:action` host bridge ✅ Done
> Motivation: IdeaVim compatibility (the same subset as `~/.ideavimrc`; does not
> read `init.lua` — an embedded engine cannot execute Lua; users can
> `source ~/.vimrc` to migrate options and mappings).
> - `vim-core::config`: parses `set` (`on/no/!`/`=`), the `:map` family
>   (`[nv]noremap`/`[nvi]map`), `let mapleader = "x"`, `source`, `"` comments;
>   unrecognized lines go into `ignored` and are silently skipped (IdeaVim's
>   same policy); `<Leader>` is expanded per mapleader in both LHS and RHS
>   (default `\`)
> - `Keymaps` upgraded to `Trie<Mapping>` (rhs + noremap); while consuming a
>   noremap expansion the engine main loop no longer consults the mapping table
>   (true noremap semantics, not recursive); when a mapping RHS enters the
>   cmdline it does not break the queue early (`:action Foo<CR>` runs through
>   in one go)
> - `VimHost::dispatch_host_action(id)` bridges `:action <id>`; gpui-vim's
>   `load_config_file` (`~` expansion, source recursion capped at one level,
>   limit 4); the demo loads `~/.gpui-vimrc` at startup and routes action
>   dispatch through the FocusHandle to the application's own `on_action`
>   handlers
> - Multiple apps sharing one rc: `load_layers` splits into two layers
>   (user-layer action misses are silent, host-layer misses are reported and
>   the same key overrides the user layer); `dispatch_host_action_hinted(id, strict)`
>   hands the policy to the host
> - known limitations: `:map` has no buffer argument (everything is global), no
>   `:unmap`, `<expr>`, `silent`; gpui actions with JSON arguments are not
>   supported (only no-arg actions are bridged)


### Task 11: rendering as components ✅ Done
> Implementation: new module `gpui_vim::render` — `compute_line_overlays`
> (pure computation: search highlights / IME marks / the three visual selection
> kinds / cursor, headless-testable), `paint_vim_line` (inverted-caret run
> splitting + shaping + quad painting; returns the shaped line for the host's
> mouse/IME back-lookup), `CaretBlinker` (500ms blink, input resets the phase,
> `Rc` + generic spawn). The demo now calls the library APIs throughout; the
> local overlay structs and painting guts are deleted. Overlay computation has
> headless tests.

**Current state**: cursor/selection/search highlights/line numbers/blink all
live in demo `editor.rs` (~800 lines), whose comment self-describes as "the
file to copy". Every rendering bug this plan fixed (V-line 0-width canvas,
block-caret inversion, blink) will be stepped on again by each host.

**Plan**: add a public module `overlay` to `gpui-vim`:

- `struct VimOverlays { quads: Vec<Quad>, cursor: Option<CursorGeom> }` +
  `fn compute(state: &VimState, buf: &dyn VimBuffer, line, line_start, line_end) -> Self`
  (move the demo `overlays_for_line` logic over, including task 5's block branch);
- `fn paint(window, bounds, shaped, overlays, style) ` (move the paint closure
  over, cursor color/width configurable);
- blink provided as a standalone helper (`CaretBlinker::new(cx)`, keeping the
  input-reset semantics through the task).
- The demo becomes a caller; `editor.rs` shrinks to buffer/IME/mouse glue only.

**Acceptance**: demo behavior unchanged (screenshot comparison); `cargo test`
fully green; README gains a minimal integration example under 100 lines.

### Task 12: cross-platform key-path validation ✅ Documented (this repo is validated on macOS only)
> macOS has been verified event by event (source level for gpui 0.2.2 + live
> probes). Linux (X11/Wayland)/Windows are unverified on real hardware —
> **integrators on other platforms must first run the `GPUI_VIM_DEBUG_KEYS=1`
> smoke test**; converge the differences into `to_core_key`/`dispatch_text`.

**Current state**: the interceptor → keymap → IME ordering analysis of
`attach()` is entirely based on gpui 0.2.2 macOS sources; the
`pending_unknown_char` mechanism (characters the interceptor returns in insert
mode are re-delivered by the platform) is mac-specific. On
Linux (X11/Wayland)/Windows: printable characters most likely **go straight
into the interceptor** (no IME interception); the `already_dispatched` branch of
`dispatch_text` was written for exactly this, but has never been verified.

**Plan**: run the demo smoke test once on each of the three platforms (the
existing `GPUI_VIM_SMOKE` covers the engine pipeline; the real keyboard path
needs adding); where problems concentrate, converge the differences into
`to_core_key`/`dispatch_text` — do not make the demo platform-aware.

### Task 13: `VimHost` contract extensions ✅ Done (delivered with tasks 3/11)
> Delivered: `status_message` (E486/E492/replacement counts go through the
> status UI; the demo status bar shows them), `buffer_name` (the demo uses
> "untitled", referenced by `:w` messages), `save`/`request_close` (delivered
> with `:w`/`:q`). **Not delivered**: upgrading the viewport to carry column
> info (task 5 worked around it with display columns), multi-buffer model — to
> be designed when a real multi-buffer host shows up.

- `fn status_message(&mut self, msg: &str) {}` (E-level error channel: `E37`,
  `:q` with unsaved changes, no search match — replacements for a bare bell);
- `fn buffer_name(&self) -> &str` (the `%` register, status bar, `dw` error wording);
- `fn save(&mut self) -> bool` / `fn request_close(&mut self)` (task 3);
- upgrade the viewport to carry column info (prerequisite of tasks 5/9, can be
  done together with them).

---

## Small items (each ≤ half a day, can ride along with adjacent tasks)

- `gi`: the data is ready (`marks.last_insert_exit` = `'^`); just add a
  command-table row.
- Insert-mode `C-o`: temporarily switch to normal for one command, then return
  to insert (reuse `InsertSession`, do not close the group).
- `C-a`/`C-x`: at/next number relative to the cursor ± count, with carry handling.
- `g;`/`g,`: changelist (needs a position stack recorded on every `bump()`;
  mind the interplay with task 2's shifting).
- `:jumps`/`:registers`/`:marks` (depends on task 3; pure informational
  display, via status_message).
- `ga`/`g8`: character info display.
- demo: `relativenumber` is supported but has no toggle entry; usable once
  `:set rnu!` (task 3) lands.

## Known non-goals (explicitly not in v1, do not add by mistake)

- multiple cursors (a Zed feature, not vim semantics);
- `:g`/`:v` global commands, `:norm`, external commands `:!`;
- folding (zf/zo), diff mode, terminal integration;
- cross-buffer `A-Z` marks and `:bnext` (waits for task 13's multi-buffer model).
