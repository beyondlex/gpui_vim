//! Regression tests from the 2026-09 code review.
//!
//! Every test here pins a behavior that was verified against real vim 9.1
//! (macOS) before the fix, so the vim-parity contract stays executable.

// type_text and a few harness helpers are exercised by the other test
// targets that share this module
#[allow(dead_code)]
mod common;

use common::{edit, Fixture};
use gpui_vim_core::key::Key;

// ---- gq format operator ------------------------------------------------------

#[test]
fn gq_on_later_line_does_not_delete_text() {
    // format_lines used to feed the span's byte OFFSET into its line-index
    // loop: for any line past the first the loop was empty and gq REPLACED
    // the whole paragraph with nothing. `gqq` on line 1 must reflow it.
    let mut f = Fixture::at("keep\nsecond line here is long enough to wrap\n", 1, 0);
    f.vim.options_mut().textwidth = 12;
    f.feed(["g", "q", "q"]);
    assert_eq!(
        f.text(),
        "keep\nsecond line\nhere is long\nenough to\nwrap\n"
    );
    // vim: cursor on the first non-blank of the LAST formatted line
    assert_eq!(f.line(), 4);
}

#[test]
fn gq_cursor_lands_on_last_formatted_line() {
    let mut f = Fixture::at("the quick brown fox jumps over the lazy dog\nnext\n", 0, 0);
    f.vim.options_mut().textwidth = 20;
    f.feed(["g", "q", "g", "q"]);
    assert_eq!(
        f.text(),
        "the quick brown fox\njumps over the lazy\ndog\nnext\n"
    );
    assert_eq!(f.cursor(), 40); // the "dog" line
}

// ---- C-a / C-x number increment ----------------------------------------------

#[test]
fn c_a_on_last_digit_increments_whole_number() {
    // The digit run's START must be found: cursor on `9` of `129` used to
    // read only the `9` (129+1 producing 1210).
    let f = edit("x129y", 0, 3, &["<C-a>"]);
    assert_eq!(f.text(), "x130y");
    assert_eq!(f.cursor(), 3); // last digit of 130 ("x130": '0' at byte 3)

    // mid-number behaves the same
    let f = edit("x129y", 0, 2, &["<C-a>"]);
    assert_eq!(f.text(), "x130y");
}

#[test]
fn c_a_ignores_numbers_before_cursor() {
    // vim: nothing "at or after the cursor" -> E18, buffer unchanged.
    // The old code walked BACKWARDS and incremented the 123.
    let f = edit("123 abc", 0, 5, &["<C-a>"]);
    assert_eq!(f.text(), "123 abc");
}

#[test]
fn c_a_at_line_end_uses_trailing_number() {
    // the engine parks the cursor at the line-end byte (visually on the
    // last char), which must still count as "on" the trailing number
    let f = edit("n 42\n", 0, 4, &["<C-a>"]);
    assert_eq!(f.text(), "n 43\n");
}

// ---- J join bookkeeping --------------------------------------------------------

#[test]
fn join_updates_marks_through_edit_replace() {
    // The `J` separator replacement used to call the raw buffer, skipping
    // the mark/search-generation sync every other edit funnels through.
    // Mark `a` sits on `t` of `  two` (offset 6); after `J` the newline +
    // indent collapse and the `t` lands at offset 4.
    let mut f = Fixture::at("one\n  two\n", 0, 0);
    f.feed(["j", "0", "w", "m", "a"]); // mark on the 't' of "two"
    f.feed(["g", "g", "J"]);
    assert_eq!(f.text(), "one two\n");
    assert_eq!(f.vim.marks.get('a'), Some(4));
}

// ---- linewise put cursor --------------------------------------------------------

#[test]
fn linewise_put_cursor_on_first_pasted_line() {
    // vim 9.1 (verified for p/P, 1..4 lines): the cursor lands on the FIRST
    // line of the put text, first non-blank — not the last pasted line.
    let mut f = Fixture::at("alpha\nbeta\ngamma\n", 0, 0);
    f.feed(["y", "y", "j", "p"]);
    assert_eq!(f.text(), "alpha\nbeta\nalpha\ngamma\n");
    assert_eq!(f.cursor(), 11); // start of the pasted "alpha"

    // P pastes above: cursor on the pasted line as well
    let mut f = Fixture::at("alpha\nbeta\n", 1, 0);
    f.feed(["y", "y", "k", "P"]);
    assert_eq!(f.text(), "beta\nalpha\nbeta\n");
    assert_eq!(f.cursor(), 0); // the pasted "beta" is now line 0
}

// ---- ? + empty Enter repeats backward --------------------------------------------

#[test]
fn question_enter_repeats_search_backward() {
    // vim: an empty pattern re-runs the last search in the direction of the
    // CURRENT prompt. `?` + Enter used to repeat forward.
    let mut f = Fixture::at("b b b", 0, 0);
    f.feed(["/", "b", "<CR>"]); // forward to the second b (offset 2)
    assert_eq!(f.cursor(), 2);
    f.feed(["?", "<CR>"]);
    assert_eq!(f.cursor(), 0, "? + Enter must search backward");
    assert!(!f.vim.search.forward);
}

// ---- multi-char case mappings (ß ↔ SS) -------------------------------------------

#[test]
fn toggle_char_expands_and_shrinks_multi_char_case_mapping() {
    // `~` on ß produces SS in vim; the mapping must expand in place.
    let f = edit("aßc", 0, 1, &["~"]);
    assert_eq!(f.text(), "aSSc");

    // the byte length can also SHRINK (ẞ -> ß): the replaced range must be
    // the consumed span, or the trailing bytes would be eaten (or the old
    // `start + mapped.len()` range would even cut mid-character and panic)
    let f = edit("ẞx", 0, 0, &["~"]);
    assert_eq!(f.text(), "ßx");

    // gU over ß expands too
    let f = edit("aß", 0, 0, &["g", "U", "U"]);
    assert_eq!(f.text(), "ASS");
}

// ---- dw over trailing blanks keeps the newline ------------------------------------

#[test]
fn dw_on_trailing_blanks_keeps_newline() {
    // vim's exclusive-motion column-1 rule moves the end to the last char
    // of the previous line; the newline itself survives. The old span ended
    // at line_end + 1 and joined the lines.
    let f = edit("foo   \nbar", 0, 3, &["d", "w"]);
    assert_eq!(f.text(), "foo\nbar");
}

// ---- is_idle (the VimEdit local-undo gate) -----------------------------------------

#[test]
fn is_idle_reflects_pending_command_input() {
    let mut f = Fixture::new("foo\n");
    assert!(f.vim.is_idle());

    f.feed(["g"]);
    assert!(!f.vim.is_idle(), "`g` is a live two-key prefix");

    f.feed(["u"]); // completes the `gu` operator
    assert!(!f.vim.is_idle(), "operator waits for its motion");

    f.feed(["w"]); // guw executes
    assert!(f.vim.is_idle());

    f.feed(["\""]); // register prefix
    assert!(!f.vim.is_idle());
    f.feed(["a", "d", "d"]); // "add executes
    assert!(f.vim.is_idle());

    f.feed(["g"]);
    f.feed(["<Esc>"]);
    assert!(f.vim.is_idle(), "Esc clears pending state");
}

// ---- space key / <Space> leader normalization ---------------------------------------

#[test]
fn space_leader_from_vimrc_space_notation() {
    // `let mapleader = "<Space>"` used to shatter into S,p,a,c,e: the leader
    // notation was substituted back into the raw string and re-parsed.
    let text = "let mapleader = \"<Space>\"\nmap <Leader>q :action T.Q<CR>\n";
    let mut f = Fixture::at("foo\n", 0, 0);
    let config = gpui_vim_core::config::parse(text);
    f.vim.apply_config(&config);
    f.feed_raw(Key::named("space"));
    f.feed(["q"]);
    assert_eq!(f.host.actions, vec!["T.Q".to_owned()]);
}

#[test]
fn space_key_matches_char_space_mappings() {
    // Named("space") keystrokes (the gpui path) canonicalize to Char(' '),
    // so a mapping declared either spelling fires from either path. The RHS
    // avoids insert typing (headless fixtures place text via the IME path).
    let mut f = Fixture::at("foo\n", 0, 0);
    f.vim
        .keymaps_mut()
        .map_str_noremap(gpui_vim_core::keymap::ModeClass::Normal, "<Space>x", "x", true);
    f.feed_raw(Key::named("space"));
    f.feed(["x"]);
    assert_eq!(f.text(), "oo\n");
}
