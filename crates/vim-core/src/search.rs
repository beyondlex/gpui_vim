//! Search: `/` `?` `n` `N` `*` `#`, incremental highlighting.
//!
//! Patterns use Rust `regex` syntax, which covers common vim "magic" patterns
//! (documented divergence: `\bsomething` style lookarounds follow RE2 rules).

use crate::buffer::VimBuffer;
use crate::state::{Ctx, VimState};
use crate::word::is_word_char;
use regex::RegexBuilder;
use std::ops::Range;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchState {
    pub pattern: Option<String>,
    pub forward: bool,
    /// The match under the cursor for `n`/`N` stepping.
    pub last_matches: Vec<Range<usize>>,
    pub last_index: Option<usize>,
    /// Buffer edit generation `last_matches` was computed at. Every edit
    /// funnels through `VimState::edit_*` (or engine-driven undo/redo), which
    /// bumps the generation, so consecutive `n`/`N` keystrokes walk the
    /// cached list instead of re-scanning the buffer per keypress. An empty
    /// `last_matches` is never trusted from the cache: `:noh` empties it
    /// without an edit, and `n` must still jump (and re-arm highlights).
    pub matches_generation: Option<u64>,
}

impl Default for SearchState {
    fn default() -> Self {
        SearchState {
            pattern: None,
            forward: true,
            last_matches: Vec::new(),
            last_index: None,
            matches_generation: None,
        }
    }
}

pub fn compile(vim: &VimState, pattern: &str) -> Option<RegexBuilder> {
    let mut builder = RegexBuilder::new(pattern);
    builder
        .case_insensitive(vim.options.case_insensitive_for(pattern))
        .multi_line(true);
    Some(builder)
}

/// Find all matches of `pattern`, with a limit guard on pathological input.
pub fn all_matches(vim: &VimState, buf: &dyn VimBuffer, pattern: &str) -> Vec<Range<usize>> {
    let Some(builder) = compile(vim, pattern) else {
        return Vec::new();
    };
    let Ok(re) = builder.build() else {
        return Vec::new();
    };

    // NOTE: scanning per-line through the VimBuffer trait was measured at
    // ~20,000x SLOWER than one whole-buffer slice + scan (20k line_range
    // trait calls + allocations dwarf one contiguous memcpy), so the
    // whole-buffer scan stays. For per-edit cost control on huge files,
    // hosts use set_hlsearch_live_update(false) + refresh_highlights.
    let text = buf.slice(0..buf.len());
    re.find_iter(&text)
        .filter(|m| !m.is_empty())
        .map(|m| m.start()..m.end())
        .take(10_000)
        .collect()
}

/// Record a pattern (from the command line or `*`) and publish highlights.
pub fn set_pattern(vim: &mut VimState, ctx: &mut Ctx, pattern: String, forward: bool) {
    set_pattern_inner(vim, ctx.buf, ctx.host, pattern, forward);
}

/// [`set_pattern`] with split borrows, for callers that already hold `buf`.
pub fn set_pattern_inner(
    vim: &mut VimState,
    buf: &dyn VimBuffer,
    host: &mut dyn crate::host::VimHost,
    pattern: String,
    forward: bool,
) {
    let matches = all_matches(vim, buf, &pattern);
    vim.search.pattern = Some(pattern);
    vim.search.forward = forward;
    vim.search.last_matches = matches;
    vim.search.matches_generation = Some(vim.edit_generation);
    vim.search.last_index = None;
    if vim.options.hlsearch {
        host.set_search_highlights(&vim.search.last_matches, None);
    } else {
        host.set_search_highlights(&[], None);
    }
}

/// Clear highlights (`:noh`).
pub fn clear_highlights(vim: &mut VimState, ctx: &mut Ctx) {
    vim.search.last_matches.clear();
    vim.search.matches_generation = None;
    vim.search.last_index = None;
    ctx.host.set_search_highlights(&[], None);
}

/// `n` / `N`: move to the next match in `forward` direction (already flipped
/// by the caller for `N`). Also used after `*`.
pub fn jump_to_match(vim: &mut VimState, buf: &dyn VimBuffer, forward: bool, count: usize) -> Option<usize> {
    let pattern = vim.search.pattern.clone()?;
    // The full-buffer scan runs only when the text changed since the matches
    // were computed; a run of `n`/`N` keystrokes walks the cached list, which
    // keeps per-keypress cost O(1) instead of O(buffer) on large files.
    if vim.search.matches_generation != Some(vim.edit_generation) {
        let matches = all_matches(vim, buf, &pattern);
        vim.search.matches_generation = Some(vim.edit_generation);
        vim.search.last_matches = matches;
    }
    let matches = &vim.search.last_matches;
    if matches.is_empty() {
        return None;
    }

    let cursor = vim.cursor.offset;
    let start_index = if forward {
        matches.iter().position(|m| m.start > cursor).unwrap_or(0)
    } else {
        matches
            .iter()
            .rposition(|m| m.start < cursor)
            .unwrap_or(matches.len() - 1)
    };
    let index = (start_index as u64 + (count as u64 - 1)) as usize;
    Some(matches.get(index % matches.len())?.start)
}

/// `*` / `#`: build a whole-word pattern from the word under the cursor and
/// search for it.
pub fn search_word_under_cursor(
    vim: &mut VimState,
    buf: &dyn VimBuffer,
    host: &mut dyn crate::host::VimHost,
    forward: bool,
) {
    let offset = vim.cursor.offset;
    let Some((start, end)) = word_bounds_at(buf, offset) else {
        return;
    };
    let literal = buf.slice(start..end);
    let escaped = regex::escape(&literal);
    let pattern = format!(r"\b{escaped}\b");
    set_pattern_inner(vim, buf, host, pattern, forward);
    vim.search.forward = forward;
}

/// Word bounds around `offset` (the run of word chars containing it).
pub fn word_bounds_at(buf: &dyn VimBuffer, offset: usize) -> Option<(usize, usize)> {
    let mut start = offset;
    while let Some(prev) = buf.prev_char_offset(start) {
        match buf.char_at(prev) {
            Some(p) if is_word_char(p) => start = prev,
            _ => break,
        }
    }
    let mut end = buf.next_char_offset(offset).unwrap_or(offset);
    while let Some(next_char) = buf.char_at(end) {
        if !is_word_char(next_char) {
            break;
        }
        end += next_char.len_utf8();
    }
    Some((start, end))
}

/// Publish incremental highlights while the user types in the command line.
pub fn publish_incsearch(vim: &mut VimState, ctx: &mut Ctx, pattern: &str) {
    if !vim.options.incsearch {
        return;
    }
    let matches = all_matches(vim, ctx.buf, pattern);
    ctx.host.set_search_highlights(&matches, matches.first().cloned());
}
