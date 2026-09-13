mod common;

use common::{edit, Fixture};

const THREE_LINES: &str = "one two three\nhello world\nrust vim engine\n";
const WORDS: &str = "foo bar baz\n";
const MULTI: &str = "alpha\nbeta\ngamma\ndelta\n";

// ---- basic motions ---------------------------------------------------------

#[test]
fn motions_hjkl() {
    let mut f = edit(THREE_LINES, 0, 0, &["l", "l"]);
    assert_eq!(f.cursor(), 2);

    let mut f = edit(THREE_LINES, 0, 0, &["j"]);
    assert_eq!(f.line(), 1);
    assert_eq!(f.cursor(), 14); // column preserved

    let mut f = edit(THREE_LINES, 1, 4, &["k"]);
    assert_eq!(f.line(), 0);
    assert_eq!(f.cursor(), 4);

    let mut f = edit(WORDS, 0, 0, &["h"]);
    assert_eq!(f.cursor(), 0); // stuck at line start

    let mut f = edit(WORDS, 0, 10, &["l"]);
    assert_eq!(f.cursor(), 10); // stuck on 'z', never on the newline
}

#[test]
fn motions_word_family() {
    let mut f = edit(WORDS, 0, 0, &["w"]);
    assert_eq!(f.cursor(), 4); // 'bar'

    let mut f = edit(WORDS, 0, 0, &["e"]);
    assert_eq!(f.cursor(), 2); // end of foo

    let mut f = edit(WORDS, 0, 0, &["b"]);
    assert_eq!(f.cursor(), 0); // stuck

    let mut f = edit("foo   bar", 0, 0, &["w"]);
    assert_eq!(f.cursor(), 6); // skips spaces

    // w stops on a blank line
    let mut f = edit("ab\n\ncd\n", 0, 0, &["w"]);
    assert_eq!(f.line(), 1);
}

#[test]
fn motions_line() {
    let mut f = edit(THREE_LINES, 0, 4, &["0"]);
    assert_eq!(f.cursor(), 0);

    let mut f = edit("  indented", 0, 5, &["^"]);
    assert_eq!(f.cursor(), 2);

    let mut f = edit("  indented", 0, 0, &["$"]);
    assert_eq!(f.cursor(), 9);

    let mut f = edit(MULTI, 0, 0, &["G"]);
    assert_eq!(f.line(), 3);
    assert_eq!(f.cursor(), 17); // first non-blank of last line... 'delta' start

    let mut f = edit(MULTI, 0, 0, &["g", "g"]);
    assert_eq!(f.line(), 0);

    let mut f = edit(MULTI, 0, 0, &["2", "G"]);
    assert_eq!(f.line(), 1);
}

#[test]
fn motions_find_char() {
    let mut f = edit("hello world", 0, 0, &["f", "o"]);
    assert_eq!(f.cursor(), 4);

    let mut f = edit("hello world", 0, 0, &["t", "o"]);
    assert_eq!(f.cursor(), 3);

    // ; repeats, , reverses
    let mut f = edit("a b a b a b", 0, 0, &["f", "b", ";", ";"]);
    assert_eq!(f.cursor(), 10);
    let mut f = edit("a b a b a b", 0, 0, &["f", "b", ";", ","]);
    assert_eq!(f.cursor(), 2);

    // find wraps within the line only
    let mut f = edit("abc\ndef", 0, 0, &["f", "f"]);
    assert_eq!(f.cursor(), 0);
}

#[test]
fn motions_percent() {
    // from `a` the first bracket ahead is `)`; % jumps to its match `(`
    let mut f = edit("fn main(a, b) {}", 0, 8, &["%"]);
    assert_eq!(f.cursor(), 7);

    let mut f = edit("fn main(a, b) {}", 0, 0, &["f", "(", "%"]);
    assert_eq!(f.cursor(), 12);
}

// ---- operators -------------------------------------------------------------

#[test]
fn delete_dw_and_special_cases() {
    // plain dw
    let mut f = edit(WORDS, 0, 0, &["d", "w"]);
    assert_eq!(f.text(), "bar baz\n");
    assert_eq!(f.cursor(), 0);

    // dw never joins lines
    let mut f = edit("foo\nbar", 0, 0, &["d", "w"]);
    assert_eq!(f.text(), "\nbar");

    // trailing whitespace is eaten
    let mut f = edit("foo   \nbar", 0, 0, &["d", "w"]);
    assert_eq!(f.text(), "\nbar");

    // on whitespace before a word: linewise-style (deletes the blanks+nl)
    let mut f = edit("  \nbar", 0, 0, &["d", "w"]);
    assert_eq!(f.text(), "bar");
}

#[test]
fn delete_counts_and_motions() {
    let mut f = edit(WORDS, 0, 0, &["d", "2", "w"]);
    assert_eq!(f.text(), "baz\n");

    let mut f = edit("hello world", 0, 5, &["d", "$"]);
    assert_eq!(f.text(), "hello");

    let mut f = edit("hello world", 0, 0, &["d", "f", "o"]);
    assert_eq!(f.text(), " world");

    let mut f = edit("hello world", 0, 0, &["d", "t", "o"]);
    assert_eq!(f.text(), "lo world");
}

#[test]
fn delete_dd_linewise() {
    let mut f = edit(MULTI, 1, 2, &["d", "d"]);
    assert_eq!(f.text(), "alpha\ngamma\ndelta\n");
    assert_eq!(f.line(), 1);
    assert_eq!(f.cursor(), 6);

    let mut f = edit(MULTI, 0, 1, &["2", "d", "d"]);
    assert_eq!(f.text(), "gamma\ndelta\n");

    // 2dd via doubling with count after
    let mut f = edit(MULTI, 0, 0, &["d", "2", "d"]);
    assert_eq!(f.text(), "gamma\ndelta\n");
}

#[test]
fn change_cw_ciw_cc() {
    // cw acts like ce: trailing space kept
    let mut f = edit("foo bar", 0, 0, &["c", "w"]);
    assert_eq!(f.vim.mode(), vim_core::Mode::Insert);
    f.type_text("XX");
    f.feed(["<Esc>"]);
    assert_eq!(f.text(), "XX bar");

    // ciw: inner word
    let mut f = edit("say hello now", 0, 5, &["c", "i", "w"]);
    f.type_text("bye");
    f.feed(["<Esc>"]);
    assert_eq!(f.text(), "say bye now");

    // cc clears the line, keeps autoindent
    let mut f = edit("    indented\nnext", 0, 6, &["c", "c"]);
    f.type_text("new");
    f.feed(["<Esc>"]);
    assert_eq!(f.text(), "    new\nnext");

    // c$ = change to line end
    let mut f = edit("keep this tail", 0, 5, &["c", "$"]);
    f.type_text("end");
    f.feed(["<Esc>"]);
    assert_eq!(f.text(), "keep end");
}

#[test]
fn yank_and_put() {
    // yy + j + p: linewise paste below the line under the cursor
    let mut f = edit(MULTI, 0, 0, &["y", "y", "j", "p"]);
    assert_eq!(f.text(), "alpha\nbeta\nalpha\ngamma\ndelta\n");

    // P pastes above
    let mut f = edit(MULTI, 1, 0, &["y", "y", "P"]);
    assert_eq!(f.text(), "alpha\nbeta\nbeta\ngamma\ndelta\n");

    // yiw + p: charwise
    let mut f = edit("foo bar", 0, 0, &["y", "i", "w", "w", "p"]);
    assert_eq!(f.text(), "foo bfooar");

    // xp swap
    let mut f = edit("ab", 0, 0, &["x", "p"]);
    assert_eq!(f.text(), "ba");

    // p then 2p repeats
    let mut f = edit("x\n", 0, 0, &["y", "i", "w", "p", "2", "p"]);
    assert_eq!(f.text(), "xxxx\n");
}

#[test]
fn registers_explicit() {
    // "ayiw then "ap
    let mut f = edit("one two", 0, 0, &["\"", "a", "y", "i", "w"]);
    f.feed(["w", "\"", "a", "p"]);
    assert_eq!(f.text(), "one tonewo");

    // blackhole register discards
    let mut f = edit("hello", 0, 0, &["\"", "_", "d", "i", "w"]);
    assert_eq!(f.text(), "");
}

#[test]
fn indent_operators() {
    let mut f = edit("a\nb\nc\n", 0, 0, &["2", ">", ">"]);
    assert_eq!(f.text(), "    a\n    b\nc\n");

    let mut f = edit("    a\nb\n", 0, 0, &["<", "<"]);
    assert_eq!(f.text(), "a\nb\n");

    let mut f = edit(MULTI, 0, 0, &[">", "j"]);
    assert_eq!(f.text(), "    alpha\n    beta\ngamma\ndelta\n");
}

#[test]
fn case_operators() {
    let mut f = edit("hello world", 0, 0, &["g", "U", "i", "w"]);
    assert_eq!(f.text(), "HELLO world");

    let mut f = edit("hello world", 0, 0, &["g", "u", "i", "w"]);
    let _ = f;
    let mut f = edit("HELLO world", 0, 0, &["g", "~", "i", "w"]);
    assert_eq!(f.text(), "hello world");

    // guu / gUU line variants
    let mut f = edit("MiXeD", 0, 0, &["g", "u", "u"]);
    assert_eq!(f.text(), "mixed");
    let mut f = edit("mixed", 0, 0, &["g", "U", "U"]);
    assert_eq!(f.text(), "MIXED");
}

#[test]
fn misc_edit_commands() {
    // x
    let mut f = edit("abc", 0, 1, &["x"]);
    assert_eq!(f.text(), "ac");
    // X
    let mut f = edit("abc", 0, 1, &["X"]);
    assert_eq!(f.text(), "bc");
    // r
    let mut f = edit("abc", 0, 1, &["r", "Z"]);
    assert_eq!(f.text(), "aZc");
    // 3r
    let mut f = edit("abcdef", 0, 0, &["3", "r", "-"]);
    assert_eq!(f.text(), "---def");
    // ~
    let mut f = edit("aBc", 0, 0, &["~"]);
    assert_eq!(f.text(), "ABc");
    // J joins with a space
    let mut f = edit("foo\nbar\nbaz", 0, 0, &["J"]);
    assert_eq!(f.text(), "foo bar\nbaz");
    // gJ literal join
    let mut f = edit("foo\nbar", 0, 0, &["g", "J"]);
    assert_eq!(f.text(), "foobar");
    // D / C / Y
    let mut f = edit("hello world", 0, 5, &["D"]);
    assert_eq!(f.text(), "hello");
    let mut f = edit("keep tail", 0, 4, &["Y"]);
    f.feed(["p"]);
    assert_eq!(f.text(), "keep tail\nkeep tail");
}

#[test]
fn undo_redo() {
    let mut f = Fixture::at(MULTI, 0, 0);
    f.feed(["d", "d"]);
    assert_eq!(f.text(), "beta\ngamma\ndelta\n");
    f.feed(["u"]);
    assert_eq!(f.text(), MULTI);
    // cursor restored to the pre-edit position
    assert_eq!(f.cursor(), 0);
    f.feed(["<C-r>"]);
    assert_eq!(f.text(), "beta\ngamma\ndelta\n");

    // insert session = ONE undo group
    let mut f = Fixture::at("abc", 0, 0);
    f.feed(["i"]);
    f.type_text("123");
    f.feed(["<Esc>"]);
    assert_eq!(f.text(), "123abc");
    f.feed(["u"]);
    assert_eq!(f.text(), "abc");
}

#[test]
fn visual_mode_ops() {
    // v e d
    let mut f = edit("foo bar", 0, 0, &["v", "e", "d"]);
    assert_eq!(f.text(), " bar");
    assert_eq!(f.vim.mode(), vim_core::Mode::Normal);

    // V j d
    let mut f = edit(MULTI, 0, 0, &["V", "j", "d"]);
    assert_eq!(f.text(), "gamma\ndelta\n");

    // viw y + p: paste goes after the char under the cursor
    let mut f = edit("copy me", 0, 0, &["v", "i", "w", "y"]);
    f.feed(["w", "p"]);
    assert_eq!(f.text(), "copy mcopye");

    // visual ~
    let mut f = edit("hello", 0, 0, &["v", "e", "~"]);
    assert_eq!(f.text(), "HELLO");

    // visual indent
    let mut f = edit("a\nb\n", 0, 0, &["V", "j", ">"]);
    assert_eq!(f.text(), "    a\n    b\n");

    // o swaps ends
    let mut f = edit("abcdef", 0, 0, &["v", "3", "l", "o"]);
    assert_eq!(f.cursor(), 0);

    // gv restores selection
    let mut f = Fixture::at("abcdef", 0, 0);
    f.feed(["v", "e", "y", "g", "v"]);
    assert_eq!(f.vim.mode(), vim_core::Mode::Visual { kind: vim_core::VisualKind::Char });
}

#[test]
fn insert_commands() {
    // i / a / I / A
    let mut f = edit("abc", 0, 1, &["a"]);
    f.type_text("X");
    f.feed(["<Esc>"]);
    assert_eq!(f.text(), "abXc");
    let mut f = edit("abc", 0, 1, &["A"]);
    f.type_text("X");
    f.feed(["<Esc>"]);
    assert_eq!(f.text(), "abcX");
    let mut f = edit("   abc", 0, 4, &["I"]);
    assert_eq!(f.cursor(), 3);
    let mut f = edit("abc", 0, 0, &["A"]);
    assert_eq!(f.cursor(), 3);

    // o / O with autoindent
    let mut f = edit("  foo\nbar", 0, 2, &["o"]);
    assert_eq!(f.text(), "  foo\n  \nbar");
    assert_eq!(f.cursor(), 8);
    let mut f = edit("  foo", 0, 2, &["O"]);
    assert_eq!(f.text(), "  \n  foo");

    // s / S / C
    let mut f = edit("abc", 0, 0, &["s"]);
    f.type_text("X");
    f.feed(["<Esc>"]);
    assert_eq!(f.text(), "Xbc");
    let mut f = edit("  abc", 0, 2, &["S"]);
    f.type_text("z");
    f.feed(["<Esc>"]);
    assert_eq!(f.text(), "  z");
    let mut f = edit("abc def", 0, 3, &["C"]);
    f.type_text("!");
    f.feed(["<Esc>"]);
    assert_eq!(f.text(), "abc!");

    // Esc exits back one char
    let mut f = Fixture::at("hello", 0, 0);
    f.feed(["i"]);
    f.type_text("X");
    f.feed(["<Esc>"]);
    assert_eq!(f.text(), "Xhello");
    assert_eq!(f.cursor(), 0);

    // enter splits with autoindent
    let mut f = Fixture::at("  ab", 0, 3);
    f.feed(["i", "<CR>"]);
    f.type_text("c");
    f.feed(["<Esc>"]);
    assert_eq!(f.text(), "  a\n  cb");
}

#[test]
fn search_and_nN() {
    let mut f = Fixture::at("foo bar foo baz foo", 0, 0);
    f.feed(["/", "f", "o", "o", "<CR>"]);
    assert_eq!(f.cursor(), 8); // search moves strictly past the cursor
    f.feed(["n"]);
    assert_eq!(f.cursor(), 16);
    f.feed(["n"]);
    assert_eq!(f.cursor(), 0); // wraps
    f.feed(["n"]);
    assert_eq!(f.cursor(), 8);

    f.feed(["N"]);
    assert_eq!(f.cursor(), 0);

    // highlights published
    assert_eq!(f.host.highlights.len(), 3);

    // * searches the word under the cursor
    let mut f = Fixture::at("one two one three two", 0, 4);
    f.feed(["*"]);
    assert_eq!(f.cursor(), 18); // next "two"
}

#[test]
fn marks() {
    let f = {
        let mut f = Fixture::at(MULTI, 0, 0);
        f.feed(["m", "a", "G", "`", "a"]);
        f
    };
    assert_eq!(f.cursor(), 0);
    let f = {
        let mut f = Fixture::at(MULTI, 0, 0);
        f.feed(["m", "a", "G", "'", "a"]);
        f
    };
    assert_eq!(f.cursor(), 0);

    // linewise jump goes to first non-blank
    let f = {
        let mut f = Fixture::at("  hello\nworld", 0, 0);
        f.feed(["m", "b", "j", "'", "b"]);
        f
    };
    assert_eq!(f.cursor(), 2);
}

#[test]
fn user_mappings() {
    let mut f = Fixture::at("abc", 0, 0);
    f.vim
        .keymaps_mut()
        .map_str(vim_core::keymap::ModeClass::Insert, "jk", "<Esc>");
    f.feed(["i"]);
    f.type_text("X");
    assert_eq!(f.vim.mode(), vim_core::Mode::Insert);
    // typing j alone waits
    f.feed(["j"]);
    assert_eq!(f.vim.mode(), vim_core::Mode::Insert);
    f.feed(["k"]);
    assert_eq!(f.vim.mode(), vim_core::Mode::Normal);
    assert_eq!(f.text(), "Xabc");
    assert_eq!(f.cursor(), 0); // esc moved back one

    // normal-mode mapping
    let mut f = Fixture::at(MULTI, 0, 0);
    f.vim
        .keymaps_mut()
        .map_str(vim_core::keymap::ModeClass::Normal, "Q", "g");
    // Q expands to g, which then waits for the second g of gg
    f.feed(["Q", "g"]);
    assert_eq!(f.line(), 0);
}

#[test]
fn escape_with_any_modifiers_exits_modes() {
    // hyper-key taps (e.g. caps-lock -> Esc in Karabiner) can deliver the
    // escape with command modifiers still attached
    let mut f = Fixture::at("abc", 0, 0);
    f.feed(["i"]);
    assert_eq!(f.vim.mode(), vim_core::Mode::Insert);
    f.feed(["<D-Esc>"]);
    assert_eq!(f.vim.mode(), vim_core::Mode::Normal);

    let mut f = Fixture::at("abc", 0, 0);
    f.feed(["v"]);
    assert_eq!(f.vim.mode(), vim_core::Mode::Visual { kind: vim_core::VisualKind::Char });
    f.feed(["<C-M-S-Esc>"]);
    assert_eq!(f.vim.mode(), vim_core::Mode::Normal);
}

#[test]
fn unknown_keys_fall_through() {
    // ctrl chords the engine does not know go to the host
    let mut f = Fixture::at("abc", 0, 0);
    let result = f.feed_raw(vim_core::key::Key::ctrl_char('a'));
    assert_eq!(result, vim_core::KeyResult::Unknown);
}

// ---- I / A insert entry ------------------------------------------------------

#[test]
fn insert_entry_I_and_A() {
    // `I`: first non-blank of the line + insert mode
    let mut f = edit("    indented line\nsecond\n", 0, 8, &["I"]);
    assert_eq!(f.vim.mode(), vim_core::Mode::Insert);
    assert_eq!(f.cursor(), 4); // first non-blank
    f.type_text("XX");
    assert_eq!(f.text(), "    XXindented line\nsecond\n");

    // `A`: end of the line + insert mode
    let mut f = edit("tail\n", 0, 0, &["A"]);
    assert_eq!(f.vim.mode(), vim_core::Mode::Insert);
    assert_eq!(f.cursor(), 4); // line end (before the newline)
    f.type_text("!");
    assert_eq!(f.text(), "tail!\n");

    // `a` on a non-empty line moves one char right; at line end it stays
    let mut f = edit("abc\n", 0, 2, &["a"]);
    assert_eq!(f.vim.mode(), vim_core::Mode::Insert);
    assert_eq!(f.cursor(), 3);
}

// ---- V linewise visual -------------------------------------------------------

#[test]
fn visual_line_V() {
    // `V` enters visual-line mode from normal mode
    let mut f = Fixture::at(MULTI, 1, 0);
    f.feed(["V"]);
    assert_eq!(
        f.vim.mode(),
        vim_core::Mode::Visual { kind: vim_core::VisualKind::Line }
    );
    // `j` extends the selection one line down; `V` again exits
    f.feed(["j"]);
    assert_eq!(f.vim.mode(), vim_core::Mode::Visual { kind: vim_core::VisualKind::Line });
    f.feed(["V"]);
    assert_eq!(f.vim.mode(), vim_core::Mode::Normal);

    // `Vj d` deletes whole lines
    let mut f = Fixture::at(MULTI, 1, 0);
    f.feed(["V", "j", "d"]);
    assert_eq!(f.text(), "alpha\ndelta\n");
    assert_eq!(f.line(), 1); // cursor on 'delta' after the join point

    // `Vj y` yanks linewise; `p` puts below the cursor line
    let mut f = Fixture::at(MULTI, 0, 0);
    f.feed(["V", "j", "y", "p"]);
    assert_eq!(f.text(), "alpha\nbeta\nalpha\nbeta\ngamma\ndelta\n");
}

// ---- search: Enter reaching the prompt through the text path -----------------

#[test]
fn cmdline_enter_arriving_as_text_still_executes() {
    // macOS hands Enter to the text-input path as "\n" (key_char); the
    // engine must submit the search, not append a newline to the pattern.
    let mut f = Fixture::at("foo bar foo baz", 0, 0);
    f.feed(["/"]);
    assert_eq!(f.vim.mode(), vim_core::Mode::CommandLine { prompt: '/' });
    f.feed(["f", "o", "o"]);
    let result = f.feed_raw(vim_core::key::Key::char('\n'));
    assert_eq!(result, vim_core::KeyResult::Consumed);
    assert_eq!(f.vim.mode(), vim_core::Mode::Normal);
    assert_eq!(f.cursor(), 8);

    // same for "\r"
    let mut f = Fixture::at("foo bar foo baz", 0, 0);
    f.feed(["/", "b", "a"]);
    let _ = f.feed_raw(vim_core::key::Key::char('\r'));
    assert_eq!(f.vim.mode(), vim_core::Mode::Normal);
    assert_eq!(f.cursor(), 4);

    // a carriage return typed mid-pattern via text must not land in the
    // pattern either
    let mut f = Fixture::at("foo bar foo baz", 0, 0);
    f.feed(["/", "f"]);
    let _ = f.feed_raw(vim_core::key::Key::char('\n'));
    assert_eq!(f.vim.cmdline.buffer, "");
}

#[test]
fn cmdline_backspace_arriving_as_text_still_deletes() {
    let mut f = Fixture::at("foo bar", 0, 0);
    f.feed(["/", "f", "o"]);
    assert_eq!(f.vim.cmdline.buffer, "fo");
    let _ = f.feed_raw(vim_core::key::Key::char('\x7f'));
    assert_eq!(f.vim.cmdline.buffer, "f");
    // backspacing past the start cancels the prompt
    let _ = f.feed_raw(vim_core::key::Key::char('\x7f'));
    let _ = f.feed_raw(vim_core::key::Key::char('\x7f'));
    assert_eq!(f.vim.mode(), vim_core::Mode::Normal);
}

// ---- undo grouping: change family and open-line are single undo steps --------

#[test]
fn change_C_types_and_undoes_in_one_step() {
    let mut f = Fixture::at("hello world\nsecond\n", 0, 0);
    f.feed(["C"]);
    assert_eq!(f.vim.mode(), vim_core::Mode::Insert);
    f.type_text("XX");
    f.feed(["<Esc>"]);
    assert_eq!(f.text(), "XX\nsecond\n");
    // one `u` must restore the deleted text (not just the typed text)
    f.feed(["u"]);
    assert_eq!(f.text(), "hello world\nsecond\n");
    assert_eq!(f.host.group_count, 1);
}

#[test]
fn change_ciw_is_one_undo_step() {
    let mut f = Fixture::at("foo bar\n", 0, 0);
    f.feed(["c", "i", "w"]);
    assert_eq!(f.vim.mode(), vim_core::Mode::Insert);
    f.type_text("hi");
    f.feed(["<Esc>"]);
    assert_eq!(f.text(), "hi bar\n");
    f.feed(["u"]);
    assert_eq!(f.text(), "foo bar\n");
}

#[test]
fn open_line_o_undo_removes_the_line() {
    let mut f = Fixture::at("alpha\nbeta\n", 0, 3);
    f.feed(["o"]);
    assert_eq!(f.vim.mode(), vim_core::Mode::Insert);
    f.type_text("new");
    f.feed(["<Esc>"]);
    assert_eq!(f.text(), "alpha\nnew\nbeta\n");
    f.feed(["u"]);
    assert_eq!(f.text(), "alpha\nbeta\n");
}

// ---- Esc dismisses search highlights (:noh semantics) -------------------------

#[test]
fn escape_clears_search_highlights_until_next_search() {
    let mut f = Fixture::at("foo bar foo baz foo", 0, 0);
    f.feed(["/", "f", "o", "o", "<CR>"]);
    assert_eq!(f.host.highlights.len(), 3);

    // Esc dismisses the highlights...
    f.feed(["<Esc>"]);
    assert!(f.host.highlights.is_empty());

    // ...but `n` still works and re-publishes them (like :noh + n)
    f.feed(["n"]);
    assert_eq!(f.cursor(), 16);
    assert_eq!(f.host.highlights.len(), 3);

    // a fresh search also re-highlights
    f.feed(["<Esc>"]);
    assert!(f.host.highlights.is_empty());
    f.feed(["/", "b", "a", "<CR>"]);
    assert!(!f.host.highlights.is_empty());

    // Esc without active highlights is a no-op (no host churn)
    let mut f = Fixture::at("foo", 0, 0);
    f.feed(["<Esc>"]);
    assert!(f.host.highlights.is_empty());
}

// ---- CJK / multi-byte cursor placement ----------------------------------------

#[test]
fn cjk_put_repeats_stay_on_char_boundaries() {
    // user repro: yank `三四`, put repeatedly — every cursor must sit on a
    // char boundary and every paste must land after the cursor char. The old
    // `end - 1` byte math parked the cursor INSIDE a char, and the next `p`
    // then inserted mid-char (which the rope host turned into an append at
    // buffer end).
    let mut f = Fixture::at("三四五\n", 0, 0);
    f.feed(["v", "l", "y"]); // yank 三四; cursor back on 四 (byte 3)
    f.feed(["p"]);
    assert_eq!(f.text(), "三四三四五\n");
    assert_eq!(f.cursor(), 9); // START of the second 四 (was 11, mid-char)
    f.feed(["p"]);
    assert_eq!(f.text(), "三四三四三四五\n");
    assert_eq!(f.cursor(), 15);
    f.feed(["p"]);
    assert_eq!(f.text(), "三四三四三四三四五\n");
    assert_eq!(f.cursor(), 21);
}

#[test]
fn cjk_toggle_case_cursor_moves_right() {
    // vim's `~` toggles and moves right: 中文 with cursor on 中 lands on
    // 文 (byte 3); the old `start + len - 1` kept it mid-buffer on CJK and
    // on the toggled char for ASCII
    let mut f = Fixture::at("中文\n", 0, 0);
    f.feed(["~"]);
    assert_eq!(f.cursor(), 3);
    let mut f = edit("abc\n", 0, 0, &["~"]);
    assert_eq!(f.cursor(), 1);
}

#[test]
fn cjk_visual_put_replace_cursor_on_last_char() {
    let mut f = Fixture::at("中文\nxy\n", 0, 0);
    f.feed(["v", "l", "y"]); // yank 中文
    f.feed(["j", "0"]); // line 1, col 0 (x)
    f.feed(["v", "l", "p"]); // replace xy with it
    assert_eq!(f.text(), "中文\n中文\n");
    assert_eq!(f.cursor(), 10); // start of the pasted 文 (was 11, mid-char)
}
