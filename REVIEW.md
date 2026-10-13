# Code review log (2026-09)

> **Since 2026-09-28 this repo contains only the integration layer and the
> demo**: the pure engine was extracted into
> [beyondlex/vimcore](https://github.com/beyondlex/vimcore). The engine parts
> reviewed in sections I–V below (`gq`, `C-a`/`C-x`, register rotation, etc.)
> moved with the code to the vimcore repo, with their regression tests under
> its `tests/`; the `crates/vim-core` paths mentioned below should be read as
> historical names.

Full read-through of the three crates `vim-core`, `gpui-vim`,
`gpui-vim-demo` (~12k lines); every suspicious behavior was benchmarked against
system vim 9.1 (macOS). This document records:

- bugs confirmed and fixed (each with a regression test);
- points suspected during the review that were verified **not** to be bugs;
- points confirmed as divergences from vim but **not fixed** (graded by
  recommended handling level).

The regression tests for the fixes are collected in
`crates/vim-core/tests/review_regressions.rs`.

---

## I. Fixed bugs

### 1. `gq` applied after line 1 deletes the whole paragraph (severe)

The signature of `ops::format_lines` is `(start: byte offset, last_line: line
number)`, but the inner loop `for line in start..=last_line` used the byte
offset as a line number. When the span starts on line 0 the two happen to both
be 0, so the existing tests all stayed green; starting from line 1 the loop
range is empty, `out` is an empty string, and the following
`edit_replace(start..span_end, "")` deletes the entire text.

Fix: `let first_line = ctx.buf.offset_to_line(start)` before the loop.
Test: `gq_on_later_line_does_not_delete_text`.

### 2. Linewise `p`/`P` completely ignored the `after` argument (missing feature)

The linewise branch of `ops::put` unconditionally inserted the text "after the
current line", so `P` (put above) actually pasted below. The existing test used
"yank the current line then P", whose pasted content is identical to the
current line, so the text result coincidentally matched and the bug was never
exposed.

Fix: when `after == false`, insert at `line_start(line)`.
Test: `linewise_put_cursor_on_first_pasted_line` (the P half).

### 3. After a linewise put the cursor landed on the last line

vim 9.1 measured (`p`/`P`, pastes of 1–4 lines verified one by one): the cursor
lands on the **first line** of the pasted text, at its first non-blank
character. The old code landed on the last line.

Fix: `cursor_line = offset_to_line(insert_at)` (right after insertion this is
the first pasted line).

### 4. Two numeric-boundary errors in `C-a`/`C-x`

- When the cursor sits on the **last digit** of a multi-digit number (e.g. the
  `9` of `129`), only the text from the cursor onward was scanned, so `129` was
  treated as `9` and incremented (`129+1` produced `1210`). vim takes the whole
  number 129 → 130.
- When there is no number after the cursor, the old code **scanned backwards**
  and incremented a number before the cursor (`123 abc` with the cursor on `a`
  still incremented to 124); vim reports E18 and does nothing.

Fix: when the cursor is on a digit (or parked on the line-end byte, visually
pressing the trailing digit), first scan back to the start of the digit run;
otherwise search forward only, and return false if nothing is found.
Tests: `c_a_on_last_digit_increments_whole_number`,
`c_a_ignores_numbers_before_cursor`, `c_a_at_line_end_uses_trailing_number`.

### 5. `J` bypassed `edit_replace`, desyncing marks and the search cache

The non-literal branch of `ops::join_lines` called `ctx.buf.replace_range(...)`
directly, bypassing the `edit_*` wrappers every engine edit must go through:
marks did not shift, `edit_generation` did not grow (so the cached match list
of `n`/`N` read stale offsets), `last_visual` did not follow. The `gJ` branch
uses `edit_delete` and was unaffected.

Fix: route through `vim.edit_replace(...)`.
Test: `join_updates_marks_through_edit_replace`.

### 6. `dw` on trailing blanks swallowed the newline

The second branch of the exclusive-motion "column-one rule" (start not on the
line's first non-blank) should move the end point to **the last character of
the previous line** and turn inclusive — i.e. the span end is `line_end` and
the newline survives. The old code's `line_end + 1` deleted the `\n` as well,
joining two lines. vim 9.1 measured: `dw` keeps the newline.

Fix: drop the `+ 1` (the first branch's linewise rule is unchanged).
Test: `dw_on_trailing_blanks_keeps_newline`.

### 7. `?` + empty Enter repeated the search in the old direction

vim: an empty pattern repeats **the previous search**, but the direction
follows **the current prompt** (`?` + Enter goes backward, `/` + Enter
forward). The engine, on an empty pattern, kept using `self.search.forward`
(the old direction), so `?` + Enter actually jumped forward.

Fix: use the `forward` passed into `execute_search` (i.e. this prompt's
direction).
Test: `question_enter_repeats_search_backward`.

### 8. Case operators truncated multi-character case mappings

`gu`/`gU`/`g~` per character used `to_uppercase().next()`, taking only the
first character: `ß` should map to `SS` but produced only `S`. Worse,
`toggle_chars` inferred the replaced range from the replacement text's length
(`start + mapped.len()`); when the mapping changes the byte length it slices
the range wrong — for `ẞ` (3 bytes) → `ß` (2 bytes) the range end falls inside
a multi-byte character, and `BufferView`'s boundary guard panics outright.

Fix: mapping uses `collect::<String>()`; the replacement range uses the
actually consumed `start..o`.
Test: `toggle_char_expands_and_shrinks_multi_char_case_mapping`.

### 9. `gpui_vim::edit`'s local-undo interception swallowed `gu`-family commands

`VimEdit::process_key` intercepted `u`/`C-r` to the local undo stack whenever
`mode == Normal`, but the engine may be waiting for exactly that key: the `u`
after `gu` (operator pending on a motion) and the trailing key of `guu`
(linewise doubling) were all eaten as local undo, so
`gu`/`guw`/`guu`/`gugu` all failed; moreover `3u`'s count leaked into the next
command.

Fix: new `is_idle()` on `VimState` (no pending count/register/operator/
char-arg/cmd_seq/queued keys); intercept only while the engine is idle.
Test: `is_idle_reflects_pending_command_input` (the interception gate itself is
inside `process_key`, which needs a gpui Context and cannot be tested headlessly).

### 10. `let mapleader = "<Space>"` was split into five keys

rc parsing expanded `<Leader>` by "substituting the `leader.notation()` string
back into the raw text and re-parsing it": `<Space>`'s notation is `Space`
(without angle brackets), and re-parsing shattered it into the five keys
`S`,`p`,`a`,`c`,`e`, so the mapping could never match.

Fix: parse `<Leader>` verbatim into a marker key `Named("leader")` first, then
replace it with the leader key **on the parsed key sequence**.

### 11. Normalizing the space key's two forms (knock-on fix)

While investigating 9/10 it was confirmed: the space arrives as two Key shapes
depending on the path — `Named("space")` (gpui keystroke path, `<Space>`
notation) and `Char(' ')` (text-input path, bare space); the keymap table
looks up by exact equality, so the two spellings never matched each other.

Fix: `Key::parse("<Space>")` normalizes to `Char(' ')`; the `handle_key` entry
normalizes a plain `Named("space")` to `Char(' ')` (same location as the
existing uppercase shift-stripping). From then on `let mapleader = " "` and
`"<Space>"` are equivalent.
Tests: `space_leader_from_vimrc_space_notation`,
`space_key_matches_char_space_mappings` (plus an updated unit test in `key.rs`).

### 12. Along-the-way fixes

- **demo `char_width` measurement never written back**: the comment claimed the
  single-char width is written back from shaped values after the first paint,
  but the repo had no `char_width.set` call anywhere, so the gutter width and
  the mouse fallback always used the 8.4 estimate. The measured advance is now
  written back in the paint callback.
- **Wrong comment in `registers.rs`**: `store_yank`'s comment said `"_yy`
  writes the unnamed register and "vim does not do this". Measured on vim 9.1:
  after `"_yy`, `@"` returns the just-yanked content (the unnamed register is an
  alias of the "last written register"; a yank always re-points it, including
  pointing at the blackhole register). The engine's behavior matches vim and
  only the comment was wrong; corrected (the delete path `"_dd` does not touch
  the unnamed register, matching vim, guaranteed by `store`'s early return).

---

## II. Suspected, verified not bugs

- **`"_yy` writing the unnamed register** (see above, engine = vim).
- **The `w` motion stopping at `line_start` on an empty line**: for an empty
  line that value is the `\n` byte, consistent with vim's "cursor on an empty
  line at col 0" semantics; the rendering layer already treats it as the line end.
- **`gp`/visual-mode `:` seeding `'<,'>`, and `gv`'s `hi-1`**: `last_visual`
  stores the exclusive end, so `hi-1` is exactly the start of the last
  character; no underflow.
- **`mapping_step` returning Wait on an insert-mode mapping prefix**: equivalent
  to vim `set notimeout` (see "design trade-offs" below).

---

## III. Confirmed divergences from vim, not fixed

### Recommended follow-up

1. **`gq` wrapping width counts characters, not display width**
   (`ops::flush_paragraph`): CJK counts as 1 column (vim counts 2), so lines of
   Chinese text visibly exceed the visual width. The fix needs replacing
   `chars().count()` with accumulated `char_display_width`; the `col + w >
   width` test must also be width-based.
2. **Replace-mode Backspace does not restore the overwritten character**: vim's
   R-mode BS restores the covered character and moves left; the engine treats it
   as a plain delete. Requires saving the overwritten text at replace time.
3. **`:{n}` typed alone does not jump**: vim's `:5` jumps to line 5; the engine
   parses the range, sees an empty command and returns. Cheap to add.
4. **`:s` flags recognize only `g`**: `c`/`i`/`I` etc. are silently ignored; the
   empty-match semantics differ from vim (vim advances one character after an
   empty match, the engine filters empty matches outright), so `:s/x*/Y`
   behavior is not comparable. If `i` is added, `case_insensitive_for` must be
   updated in sync.
5. **`VimEdit`'s undo architecture limitation**: `u`/`C-r` go through the
   widget-local stack (bypassing the engine), so (a) `3u` only rings the bell
   instead of undoing 3 times (`EditHost::undo` returns None); (b) macros and
   `.` repeats can never record an undo. For full semantics, sink the undo
   stack into `EditHost` and implement `VimHost::undo/redo`, deleting the
   `process_key` interception.

### Design trade-offs (known, acceptable)

6. **No `timeout`/`timeoutlen`**: an insert-mode mapping prefix (the `j` of
   `jk`) keeps swallowing the key waiting for the next one, equivalent to vim
   `set notimeout`.
7. **`gq` applies the first line's indent to the whole paragraph**: vim has its
   own indent rules for a paragraph's following lines; the engine is a simplified
   version (documented).
8. **Sentence motions recognize only `.!?` single characters** (noted in code);
   no `...`, no line-start rules.
9. **`:s` replacements use Rust regex expansion (`$1`)**, not vim's `\1`
   (documented).
10. **Search patterns use Rust regex syntax**; `\b`, lookaround follow RE2
    (documented).
11. **`jump_to_match`'s count wraps modulo**: vim errors past the end; the
    engine wraps (the code notes the u64 wrap-around is deliberate).
12. **`:d` ignores register/count suffixes** (the code notes named registers
    not supported); `:{range}d 3`'s "delete 3 lines from the range" is not
    implemented.
13. **Text containing newlines typed inside blockwise visual `I`/`A`/`c`**: when
    the session exits and copies to the other rows, the row-offset logic assumes
    single-line text, so the landing point of a multi-line paste is undefined.
    Not benchmarked against vim.
14. **Charwise `p` steps right by "one character"**: an emoji ZWJ family gets
    split mid-cluster; `h`/`l`/`x` already step by grapheme, put does not.
15. **`marks.adjust_replace` keeps interior marks at absolute offsets on
    equal-length replacement**: may be off inside multi-byte characters;
    approximates vim's column-keeping behavior.
16. **`PagerBuf`'s inherent `line_start`/`line_end` shadow the trait methods of
    the same name with different parameter semantics** (byte offset vs line
    number): noted in the type docs, still an error-prone spot; recommend
    renaming to `line_start_at`/`line_end_at` later.
17. **`VimState::replace_range` (the IME commit path) neither closes the undo
    group nor `bump`s**: by design the insert session wraps it up; a host
    calling it outside insert mode leaves a dangling open group (the demo's
    `unmark_text` has no mode protection, so it is theoretically reachable).
    Recommend adding a mode assertion or wrap-up inside that function later.

---

## IV. Notes on the vim benchmarking method

Drive real vim 9.1 with a keystroke script (`vim -u NONE -N -s keys.vim file`)
and capture `line(".")`/`col(".")`/`getline(...)` via `:redir`. Two traps:

- **In `-es` (silent Ex) mode, `yy` etc. inside `normal!` do not take effect**
  (E353 Nothing in register); use the `-s` keystroke script;
- **Ex lines in a keystroke script must have a leading `:`**, and if a normal
  command is followed by a bare newline it parses as `+` (move down one line),
  polluting cursor-position assertions — this round's "second line" false
  positive for the `p` cursor rule came from exactly this.

---

## V. Second review round (2026-09-21, driven by the crossterm_vim integration)

While doing the full alignment review for the crossterm-vim integration,
another batch of points was confirmed; they are **fixed** in this round
(regression tests in `crates/vim-core/tests/parity_round2.rs`):

1. **`n`/`N` ignored the search direction**: after `?foo`, `n` should keep
   searching backward; the old implementation hardcoded forward. `SearchNext`'s
   `forward` flag is reinterpreted as "repeat polarity".
2. **`*`'s word boundary**: when the cursor is on whitespace, the old
   `word_bounds_at` rolled the leading whitespace into the pattern (searching
   `" bar"`), and on the buffer's last character it produced an empty range; and
   unlike vim — when not on a word it should take the nearest preceding word in
   the line. Both fixed.
3. **`t`/`T` crossed lines**: when the target character is at the line start or
   end, the till stop point, via `prev/next_char_offset`, could land on the
   previous line's `\n` or past the line end; the stop point must now lie within
   this line, otherwise the motion fails (vim rings the bell).
4. **Uppercase-register append**: `"Ayy` should append into register a; the old
   implementation treated it as a separate register `A`; `get`/`store`/
   `store_yank` gained the case mapping and merge semantics (if either side is
   linewise the result is linewise), and the unnamed register points at the
   merged whole.
5. **Empty text objects**: `ci"`/`ci(` on `""`/`()` rang the bell in the old
   implementation; vim allows inserting between an empty string / empty parens
   (zero-width inner range); the "cursor pressing the `(`" block selection was
   added along the way.
6. **`C-e`/`C-y` missing**: the command tables lacked these two keys (the
   integration layer's `scroll_lines` hook had no trigger); added the
   `ScrollLines` command — scrolls the view, cursor follows only when it would
   be scrolled out of the viewport.
7. **`<Tab>` missing**: in terminals Tab and `C-i` share the same code, and vim
   uses both for jumplist-forward; added the normal-table row.
8. **Visual-mode `Y` missing**: added as yank-selection.
9. **Counts on `%`/`$`**: `5%` jumps by file percentage; `2$`/`d2$` go to the
   end of a following line.
10. **`:N` line jump**: after parsing the range the command was empty and it
    returned directly (section III item 3); now, per vim, jumps to that line's
    first non-blank.
11. **`:s`'s `i`/`I` flags** (part of section III item 4): force this replacement
    to ignore / respect case.
12. **`'a` marks in Ex ranges**: the range scanner accepted `'x` but
    `base_line` only knew `'<`/`'>`, so `:'a,'b d` always errored E16; now
    resolved through `marks.resolve`.
13. **`@:` repeats the last Ex command**: `Cmdline` records `last_command`, and
    `@:` replays it via `execute_ex`.
14. **Replace-mode Backspace** (section III item 2): typing under `R` stashes
    the overwritten original character and BS restores it off a stack; past the
    line end the appended part has no original to restore, so BS deletes plainly.
15. **`gq` wraps by display width** (section III item 1): `flush_paragraph`
    switched to `char_display_width`, so CJK words are no longer estimated at
    1 column.
16. **`/` and `?` search history**: the comment claimed sharing while the
    implementation bucketed by prompt; now one unified key.
17. **`ensure_normal_mode` did not handle Replace**: on implicit host exit a
    Replace session was left dangling.
18. **`VimHost::request_close_forced`**: separates `:q` (non-forced) from
    `:q!`/`:wq`/`ZZ`/`ZQ` (forced); the host may refuse to close an unsaved
    buffer per E37; the default implementation forwards to the old
    `request_close`, so existing hosts are unaffected.

The entries that remain "design trade-offs" are the ones in section III above;
untouched in this round.

---

## VI. Third review round (2026-10-01, repo-level re-review after the engine extraction)

The first full re-review after the engine left the repo. Scope: `gpui-vim`
(lib/edit/pager/render/config) and `gpui-vim-demo` (buffer/host/editor/main),
~4900 lines. **Fixed** in this round (each with a regression test):

1. **The rc `source` directive did not expand `~` in paths** (`config.rs`):
   top-level layer paths went through `expand_tilde`, but a nested
   `source ~/more-maps` called `read_to_string` directly — always `NotFound`,
   silently skipped, while the engine's `config::parse` explicitly comments
   "tilde expansion is the loader's job". Fix: apply the same `expand_tilde` to
   source targets in the `load` recursion.
   Test: `source_directive_expands_tilde_like_top_level_paths`.
   (Lesson: regression assertions should pick an option whose **default is
   false** — the first version asserted on `expandtab`, which defaults to true,
   so the test was vacuously green.)
2. **The demo host's new undo group did not clear the redo branch**
   (`host.rs`): `undo → type new content → C-r` stamped the pre-undo snapshot
   back over everything, and the newly typed content vanished out of thin air —
   `begin_undo_group` must clear `redo_stack` when it sees a new id (vim
   semantics: a new edit invalidates the redo branch).
   Test: `new_edit_after_undo_discards_the_redo_branch`.
3. **`VimEdit::splice_range` recorded no undo point and cleared no redo**
   (`edit.rs`): completion insertion goes through this side path, so
   undo → completion → redo was likewise overwritten by a stale snapshot. It now
   has the same standing as typing: records an undo point, invalidates the redo
   branch. The undo stack was extracted into a headlessly testable `TextUndo`
   struct (`VimEdit::new` needs a gpui `App` and cannot be constructed directly).
   Tests: 3 in total, e.g. `text_undo_record_invalidates_redo`.
4. **`offset_rc` panicked on non-char-boundary offsets** (`edit.rs`): the
   `line[..rem]` slice panics when rem falls inside a multi-byte character. This
   entry also receives host-side offsets (IME restoration, stale values after a
   splice); changed to floor down to the nearest boundary.
   Test: `offset_rc_floors_mid_char_offsets_instead_of_panicking`.
5. **Mouse clicks mis-fed the engine while a prompt/pending state was active**
   (`edit.rs`): with the search prompt open, `mouse_down` went through
   `enter_visual_char` and spliced a `'v'` into the pattern; with an operator
   pending (`d` already pressed) a click repositioned the cursor and broke the
   command. Now clicks are ignored outright in cmdline mode and in non-idle
   states; inside visual mode a click only extends the selection and no longer
   re-feeds `'v'`.

### Along-the-way fixes/optimizations in this round

6. **Demo Cmd-V did nothing in normal mode, silently**:
   `insert_text_at_cursor` is a direct no-op outside insert state. Now the
   insert path lands text as before; normal/visual route through the engine's
   `"+p` command (undo group, marks, `.` repeat identical to hand-typed `p`);
   the host `"+` mirror stays in sync with the system clipboard.
7. **The demo tab bar was not clickable**: added `cx.listener` +
   `stop_propagation` (without it a click maps into a body cursor move), plus
   hover feedback.
8. **`CaretBlinker` kept repainting background windows**: added
   `spawn_loop_gated`, with the host supplying a "window active" predicate;
   invisible carets no longer drive one frame every 500ms.
9. **`LinesBuf` character probes joined the whole buffer**: `char_at`/
   `prev_char_offset` are the engine's per-character motion hot probes, and the
   old implementation allocated a full `join` O(all text) every call; changed to
   line-by-line walking, O(offset), zero allocation (the TCK contract fully green).
10. **`Pager::take_clip`'s dead `yanked` field**: record-then-take was
    pointless; deleted; the register fallback semantics are unchanged
    (repeatable reads).

### New

11. **`VimEdit` placeholder text**: `set_placeholder` + a
    `VimEditStyle::placeholder` color slot; an empty buffer renders the gray
    hint — display-only, never enters the buffer.

### Verified not bugs / kept as is

- **`mouse_up`/`undo` writing `vim.mode = Normal` directly**: the engine's
  `visual_selection` returns None for non-Visual modes (the stale anchor is
  guarded), so switching the mode directly is safe.
- **`process_key` returning true (consumed) for Unknown keys**: `VimEdit` is the
  route-B form with self-contained fields; swallowing unknown keys while the
  fields own the input matches the "field-exclusive keystrokes" expectation; a
  host that needs pass-through should watch `VimEditEvent`/route itself.
- **`VimEdit`'s local undo stack has no cap**: editing depth in form scenarios
  is limited and entries are COW line vectors (cheap to clone); the demo host's
  stack has a 200-step cap. No configuration surface for now.
- **`PagerBuf`'s inherent `line_start`/`line_end` shadowing the trait methods of
  the same name**: keeping section III item 16's "rename recommended" — since
  pandamail already consumes this API, renaming requires cross-repo
  coordination, so this repo will not move unilaterally.

### Performance notes

- `LinesBuf::slice`/UTF-16 conversion still joins the whole buffer — they only
  serve IME boundaries and host text extraction, at low frequency; kept simple.
  The per-character probe path (`char_at`/`prev_char_offset`) is optimized (see
  item 9 above).
- Blink gating (see item 8 above) is pure gain for resident gpui applications:
  background windows drop from 2Hz repaints to 0.

---

## VII. Fourth review round (2026-10-03, repo-level re-review)

Scope same as round VI (integration layer + demo after the engine left the
repo, ~5000 lines); this pass targeted "unexpected operations / unusual data".
**Fixed** in this round (each with a regression test, except pure demo
interaction paths):

1. **`VimEdit::set_text` rebuilt the whole `VimState`; host configuration was
   lost silently** (severe): `VimState::new()` zeroed the `options`/`keymaps`
   that layered rc loading had brought in — every time the host swapped content
   (form reset, one widget switching buffers), the user's `jk`→Esc mapping and
   all options silently died. Fix: extracted the headlessly testable
   `reset_engine_for_new_text` — transient state (mode/cursor/visual anchor/
   pending commands/search state) reset to zero, configuration preserved.
   Test: `engine_reset_for_new_text_keeps_options_and_mappings`.
2. **`LinesBuf::byte_to_utf16` zeroed non-boundary offsets**: when a stale
   host-side offset fell inside a multi-byte character, `get(..mid)` returned
   None and was treated as 0, and the IME candidate box jumped to the buffer
   start. Fix: floor down to the start of the containing character.
   Test: `byte_to_utf16_floors_mid_char_offsets`.
3. **With a UTF-8 BOM, the first directive of an rc file was silently eaten**:
   the engine's `config::parse` could not recognize `\u{feff}set`, so line 1 was
   discarded and the rest parsed normally — a deeply misleading symptom (only
   the first setting/mapping fails). Environment adaptation belongs to the
   loader (same rationale as tilde expansion); strip the BOM before loading.
   Test: `utf8_bom_does_not_eat_the_first_directive`.
4. **Demo `on_mouse_down` lacked the CommandLine/pending guard** (the demo-side
   escape of VI.5): with the search prompt open, a click repositioned the cursor
   and dirtied incsearch; with an operator pending, a click broke the command.
   The guard now follows the same rule as `VimEdit::mouse_down`.
5. **Demo clicks in visual state left an empty selection behind**: releasing a
   drag keeps visual (vim semantics, correct), but a click that never moved also
   left `set_cursor_offset`'s re-anchor in place, stopping in visual mode with no
   way to exit by mouse. `on_mouse_up` now exits visual for clicks where
   "cursor == drag anchor" (`VimEdit::mouse_up`, same semantics).
6. **Demo `paste` polluted the search term while a prompt was open**: `cmd-v`
   sent the three keys `"`,`+`,`p` directly, and the cmdline appended them to
   the pattern as ordinary input. Now, while a prompt is open, the text is
   appended character by character through the keystroke pipeline; the normal
   path gained follow-scroll after paste.
7. **Demo `HostState::save` reported 0L for buffers without a trailing
   newline**: `len_lines()-1` undercounts buffers like "abc" by one line; fixed
   with the same rule as `line_count_real`.
   Test: `save_counts_last_line_without_trailing_newline`.
8. **Demo `sync_visible_state` early-returned on an out-of-range viewport**: in
   edge cases like an empty buffer, the early return skipped writing
   `host.viewport`, so the engine's scroll motions (`C-d`/`H`/`M`/`L`) read a
   stale viewport and scrolled by the wrong distance. Now it clamps and continues.
9. **`Pager::set_cursor` did not floor non-boundary offsets**: a host hit-test
   handing the engine a mid-char offset made probes like `offset_to_line` panic.
   Floor down (same rule as the engine's `set_cursor_offset`).
   Test: `set_cursor_floors_mid_char_offsets`.
10. **`VimEdit` IME geometry lacked scroll compensation in two places**:
    `bounds_for_range` (the IME candidate box) used absolute content line
    numbers, and `character_index_for_point` (click positioning) added no scroll
    distance; after multi-line scrolling they flew outside the visible area /
    mapped to the wrong line. Unified with `offset_at_point` on the assumption
    "the IME canvas overlays the viewport": the former subtracts the scroll
    distance, the latter adds it back.

### Along the way in this round

11. `cycle_tab` closes the dangling prompt of the tab being left; `copy` on an
    empty selection gives a status message.
12. The config test `with_fake_home` now restores `$HOME` (rather than deleting
    it), avoiding pollution of later tests in the same process; removed one
    unused import.
13. **Added `search_status`**: `EditHost::search_status()` + a public free
    function of the same semantics (hosts' own `VimHost` implementations each
    hold their highlight list, so both can delegate directly); returns the
    1-based index of the current match and the total count; the demo status bar
    shows the vim-style `3/17`, hidden entirely when there is no current match.
    Test: `search_status_indexes_current_highlight`.

### Suspected, verified — leaving unchanged (recorded)

- **`set_text`'s multi-line path keeps `\r`**: consistent with vim's `^M`
  (fileformat=dos), and the `text()` round-trip is correct; `insert_text` strips
  `\r` only for the platform input path, so the two do not conflict.
- **`Pager`'s blockwise-visual `selection()` collapses to a charwise range**:
  the pager's public surface only promises char/line selection mapping; block
  semantics are reachable only when the host itself `feed(C-v)`s, in which case
  it returns the bounding rectangle — recorded as a known boundary, no block
  awareness added (a host wanting block semantics will also want rendering,
  which is a future design topic for the pager component).
- **`to_core_key` taking only the first character when `key_char` has several**:
  on the normal macOS path `key_char` is always single-char; no multi-char
  source observed, so defensive first-char-taking is fine.
- **`process_key` with empty `dispatch_text("")`**: zero-char loop, no side
  effects.

---

## VIII. Engine now follows git main (2026-10-04)

`vimcore = "0.1"` → `git = "https://github.com/beyondlex/vimcore", branch =
"main"` (single source in workspace.dependencies, inherited by both crates):
crates.io has released nothing since 0.1.4 (2026-09-29), and the parity/fuzz
fixes (0.1.5–0.1.19, 591 tests + 21 fuzz rounds) live only on git main.
Cargo.lock pins the rev (`067240cf`) for reproducibility; for iterating on the
engine locally, use `[patch.crates-io]` to temporarily swap in a path.

After switching to 0.1.19, the new TCK immediately caught one violation by this
repo:

1. **`PagerBuf::prev_char_offset` panicked on non-boundary offsets**: the new
   TCK's "phantom-line tolerance" probe calls it with offsets inside multi-byte
   characters; the contract allows returning None or a boundary offset, but the
   `text[..offset]` slice panicked outright. Added an `is_char_boundary` guard
   (`LinesBuf`/`RopeBuffer` were already compliant; `PagerBuf` was the only
   escapee).
   Test: `pager_buf_read_contract` (the engine's own TCK, with multi-seed data).
