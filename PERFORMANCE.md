# Host performance guide (gpui-vim)

Performance reference for apps embedding the gpui-vim engine (e.g. pandagit). Two parts:
[engine-side cost model](#engine-side-cost-model) (measured, with benchmarks you can re-run) and
[host-side checklist](#host-side-checklist) (what must be done for production).

> The engine has been split into its own repository [beyondlex/vimcore](https://github.com/beyondlex/vimcore);
> re-run the benchmarks inside that repo: `cargo run --release -p vimcore --example bench_probe`.

## Engine-side cost model

The engine's typical per-keystroke cost is **microsecond-scale**; every path is far below the frame budget (16ms):

| Operation | Measured (release) | Notes |
|---|---|---|
| plain motion (`w`, 11KB buffer) | 3.9 μs/key | typical path |
| `w` motion (pathological 1MB single line) | 69 μs/key | word motion walks character by character |
| `x` delete (10k lines) | ~5 μs/key | incl. marks shifting, undo group |
| `:%s/foo/bar/g` (600KB, 50k lines) | 2.5ms | one-shot |
| `gqG` reformat 200 lines | 2.5ms | one-shot |
| **hlsearch highlight rescan** (900KB, 10k matches) | **0.3 ms/edit** | see below |
| `n` repeated jumps (900KB, 10k matches, ropey buffer) | **3.4 μs/key** (~308 μs on a cache miss) | match list cached per edit generation |

The **only keystroke path that grows linearly with file size** is the hlsearch highlight rescan: with
`hlsearch` on and a search pattern active, every edit (each character typed in insert) triggers a
full-file regex rescan. At 900KB, 0.3ms/key is imperceptible; at 10MB, ~3ms/key starts becoming
perceptible; larger files need throttling (see item 5 of the host checklist).

`n`/`N` is not among them: the match list is cached per edit generation, and any edit (the three
`edit_*` entry points) or engine-driven undo/redo, or `:set`, invalidates it. On a cache hit, consecutive
`n` only walks the list without rescanning — before the optimization each `n` rescanned the whole file
at about 308 μs; after a hit it is 3.4 μs (~90x). An empty list is never trusted (after `:noh` clears
it, `n` must still jump and re-light the highlights), so empty-result scenarios rescan on every `n`.

Engine semantic guarantees (hosts may rely on these):

- each logical command = one undo group; snapshot at `VimHost::begin_undo_group`.
  ropey's `Rope` clone is an O(log n) tree clone — **undo snapshots with ropey are cheap**
- the engine funnels all buffer changes into the three entry points
  `edit_insert/edit_delete/edit_replace` (inside `VimState`); marks, visual selections, and search
  highlights shift accordingly
- search match cap of 10,000 (protecting rendering against pathological regexes)
- the `n`/`N` match list is cached per edit generation (`SearchState::matches_generation`);
  the host only needs to guarantee that all buffer changes go through the engine (undo/redo initiated by
  the engine is already covered)

A measured lesson (it is written into a code comment): replacing a whole-buffer `slice` + scan with a
**line-by-line** scan through the `VimBuffer` trait measured about 20,000x slower (20,000 trait calls +
small allocations are far more expensive than one contiguous memcpy). Don't do this kind of
"optimization" — whole-buffer reads are the intended usage of `VimBuffer::slice`.

## Host-side checklist

### 1. Buffer implementation: use a rope; line operations must be O(log n)

`VimBuffer`'s `offset_to_line` / `line_range` / `char_at` are called at high frequency by the engine.
ropey (used by the demo) provides O(log n) `byte_to_line` / `line_to_byte`.
**Do not** implement `offset_to_line` with `String` + `split('\n')` — that is
O(offset) per call; on a large file a single keystroke reaches milliseconds (the naive implementation
in the benchmark measured 219μs/key; under ropey it is ~5μs).

`slice` implementations returning String are allowed to allocate (the engine's whole-buffer scans
rely on it), but small line-level slices are called even more frequently, and ropey's byte→char
conversion path is already efficient.

### 2. Undo snapshots

Take the snapshot inside `begin_undo_group`. ropey clone is cheap; store the `Rope` directly.
If the host stores text as `String`/`Vec<u8>`, each snapshot is an O(file) copy — in insert mode the
engine guarantees only one snapshot per undo group, which is still acceptable; but the bigger
problem is that the redo stack doubles too. Recommend using ropey directly.

### 3. Text rendering: shaping has a gpui-internal cache; the residual cost is small per-frame allocations

Font shaping is the most expensive rendering step. gpui's (since 0.2)
`LineLayoutCache` already caches layouts across frames keyed by (text, font_size, run split),
so a line is not reshaped while its content and style are unchanged; cursor movement/blink only affects
the run split of the cursor's line, a negligible cost. Host-layer caching of `ShapedLine` mainly
pays off by bypassing the per-call key hashing and `ShapedLine` construction — an optional optimization.

What is really worth doing is pressing down the fixed **per-line-per-frame** overhead (the demo
already implements this and can be copied):

- `paint_vim_line` hands the text over by ownership to `shape_line` (gpui's
  `SharedString` clone is a heap copy; once per line per frame is already enough)
- in the paint callback, `shaped` moves directly into the hit-test cache without cloning
- pre-filter search highlights **once per frame over the visible line range** (`sync_visible_state`);
  don't hand 10k highlights to `compute_line_overlays` to clamp for every visible line
- evict the line-geometry cache (`shaped_lines`) to the visible range each frame (see item 7)

### 4. Repaint granularity and viewport sync

After an edit, a single `cx.notify()` lets gpui repaint the visible area (uniform_list
only renders visible lines, naturally viewport-clipped). Avoid full state recomputation in
high-frequency events (mouse moves, scrolling). During IME composition,
`replace_and_mark_text_in_range` triggers a notify for every syllable — this is expected behavior;
don't add extra work on that path.

**Scrolling motions depend on the host reporting the viewport**: the step and placement of
`C-d`/`C-f`/`C-b`/`H`/`M`/`L` read `VimHost::viewport()`. The host must sync the actual visible line
range on scroll/resize (the demo writes `host.viewport` alongside `visible_lines` in the uniform_list
callback); otherwise these keys scroll with the default (0, 24), and the taller the window,
the worse the error.

### 5. hlsearch rescan throttling (mandatory for large files)

Beyond ~2MB files or with very many search highlight matches, hlsearch's default "rescan the whole
file on every edit" becomes the main source of keystroke latency. The engine provides a switch:

```rust
// At startup (or when :editing a large file):
vim.set_hlsearch_live_update(false);

// Host-side throttle: refresh once 150ms after the last edit
// (use your own debounce/timer; the engine has no clock)
if idle_for(150.ms()) {
    let (vim, buf, host) = editor.vim_parts();
    let mut ctx = Ctx { buf, host };
    vim.refresh_highlights(&mut ctx);
}
```

`refresh_highlights` is internally just `republish_search`: full-file scan +
`set_search_highlights` publish. Briefly stale highlights during throttling is acceptable
(vim behaves similarly on large files).

### 6. Multi-buffer

One independent `VimState` + `VimBuffer` + host state per buffer (see the demo's
`BufferTab`). Note: **host-level mappings (e.g. gt/gT) must be installed on every tab's engine**;
installing only on the active tab leaves them broken after switching. Unfocused tabs' engines cost
zero (no timers, no background tasks).

### 7. Cache hygiene

Any structure that "caches render products per line" needs an eviction policy. The demo's `shaped_lines`
retains to the visible range in each frame's uniform_list callback — when scrolling a large file the
cache size stays constant at "one screen + drift" and does not grow with line count.

## When you don't need to worry

- Files < 1MB: with default configuration every path is imperceptible; of the checklist above only items
  1 and 3 are worth doing (buffer via ropey + shape caching)
- The engine starts no timers, runs no background tasks, does no IO; unfocused engine instances cost zero
- Config files (`~/.vimcorerc`) are read once at startup

## Component-layer notes (2026-10)

- `edit::VimEdit`'s `LinesBuf` targets forms/short text: per-character probes such as
  `char_at`/`prev_char_offset` now walk per line (O(offset), zero allocation), but `slice` and UTF-16
  conversion still `join` the whole buffer — they only serve the low-frequency IME boundaries and host
  text fetching; don't use it as a large-text buffer
  (for large text follow the demo's ropey route).
- The demo editor uses `CaretBlinker::spawn_loop_gated` to wire "window active" into the blink loop:
  background windows no longer repaint a frame every 500ms for an invisible caret. Resident applications
  should copy these two lines.
