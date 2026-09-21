//! Vim-parity regressions, round two (2026-09 crossterm_vim 集成审查).
//!
//! 每个测试钉住一个与 vim 9.1 对齐的行为：`n`/`N` 方向、`*` 词边界、
//! `t`/`T` 行界、大写寄存器追加、空文本对象、`C-e`/`C-y`、`%` 百分比、
//! `:N` 跳行、`:s` 大小写标志、mark 范围、`@:`、Replace 退格、gq 显示
//! 宽度、`:q` 强制标志、搜索历史共享。
//!
//! 注意：`Fixture::feed` 的每个元素经 `Key::parse` 解析——多字符字符串会
//! 变成 `Named` 键！单字符与 `<CR>` 之类记号之外，必须逐字符喂。

#[allow(dead_code)]
mod common;

use common::{edit, Fixture};
use gpui_vim_core::key::{Key, parse_key_sequence};
use gpui_vim_core::registers::{RegisterKind, CLIPBOARD, UNNAMED, YANK};

fn feed(edit: &mut Fixture, keys: &str) {
    for k in parse_key_sequence(keys) {
        edit.feed_raw(k);
    }
}

// ---- n / N 尊重上次搜索的方向 -------------------------------------------------

#[test]
fn n_after_question_search_goes_backward() {
    // vim: `?` + `n` 沿向后方向重复；旧实现写死向前
    let mut f = Fixture::at("b b b", 0, 4);
    feed(&mut f, "?b<CR>");
    assert_eq!(f.cursor(), 2);
    f.feed(["n"]);
    assert_eq!(f.cursor(), 0, "`?` 之后的 `n` 必须继续向后");
    f.feed(["N"]);
    assert_eq!(f.cursor(), 2, "`N` 镜像回向前");
}

#[test]
fn n_after_slash_search_stays_forward() {
    let mut f = Fixture::at("b b b", 0, 0);
    feed(&mut f, "/b<CR>");
    assert_eq!(f.cursor(), 2);
    f.feed(["n"]);
    assert_eq!(f.cursor(), 4);
    f.feed(["N"]);
    assert_eq!(f.cursor(), 2);
}

// ---- * / # 在非单词字符上 -------------------------------------------------------

#[test]
fn star_on_whitespace_finds_next_word_on_line() {
    // vim：光标不在单词上时，`*` 用同一行上光标之后最近的单词——旧实现把
    // 前导空白一起卷进模式（搜索 " bar"）
    let mut f = Fixture::at("foo bar baz", 0, 3); // 光标在 foo 后的空格
    feed(&mut f, "*");
    assert_eq!(f.cursor(), 4, "应跳到 bar");
    assert_eq!(
        f.vim.search.pattern.as_deref(),
        Some(r"\bbar\b"),
        "模式不应含空白"
    );
}

#[test]
fn star_on_last_word_char_of_buffer() {
    // 旧 word_bounds_at 在缓冲末字符上给出空区间 → 空模式
    let mut f = Fixture::at("a b c", 0, 4);
    feed(&mut f, "*");
    assert_eq!(f.vim.search.pattern.as_deref(), Some(r"\bc\b"));
}

// ---- t / T 的行界 ----------------------------------------------------------------

#[test]
fn till_target_at_line_start_fails_instead_of_crossing_lines() {
    // `tx` 的 x 在行首：vim 响铃失败；旧实现把光标放上同行 `\n`
    let f = edit("xabc\nxabc", 1, 2, &["t", "x"]);
    assert_eq!(f.line(), 1, "运动失败光标应留在原行");
}

#[test]
fn backward_till_hit_at_line_end_fails() {
    // `Tz` 的 z 是行末字符：右侧无可停字符，vim 失败
    let f = edit("abcz", 0, 0, &["T", "z"]);
    assert_eq!(f.cursor(), 0, "运动失败光标不动");
}

#[test]
fn backward_till_within_line_still_works() {
    // `Ta` 停在 a 右侧一个字符
    let f = edit("abcd", 0, 3, &["T", "a"]);
    assert_eq!(f.cursor(), 1);
}

#[test]
fn till_finds_within_line_still_works() {
    // `t` 落点在目标字符的前一个字符上
    let f = edit("abcb", 0, 0, &["t", "b"]);
    assert_eq!(f.cursor(), 0);
    let f = edit("abcb", 0, 2, &["t", "b"]);
    assert_eq!(f.cursor(), 2);
}

// ---- 大写寄存器追加 ---------------------------------------------------------------

#[test]
fn uppercase_register_appends() {
    let mut f = Fixture::at("one\ntwo\nthree\n", 0, 0);
    f.feed(["\"", "a", "y", "y", "j", "\"", "A", "y", "y"]);
    let reg = f.vim.registers.get('a').cloned().expect("register a");
    assert_eq!(reg.text, "one\ntwo\n");
    assert_eq!(reg.kind, RegisterKind::Linewise);
    // unnamed 指向合并后的整体（vim：无名寄存器是最后写入寄存器的别名）
    let unnamed = f.vim.registers.get(UNNAMED).cloned().unwrap();
    assert_eq!(unnamed.text, "one\ntwo\n");
}

#[test]
fn uppercase_charwise_append() {
    let mut f = Fixture::at("abcd", 0, 0);
    f.feed(["\"", "a", "y", "l", "l", "\"", "A", "y", "l"]);
    let reg = f.vim.registers.get('a').cloned().unwrap();
    assert_eq!(reg.text, "ab", "字符级追加应拼接进同一寄存器");
    assert_eq!(reg.kind, RegisterKind::Charwise);
}

#[test]
fn uppercase_delete_appends_and_pastes() {
    let mut f = Fixture::at("aa\nbb\ncc\n", 0, 0);
    f.feed(["\"", "a", "d", "d"]); // "a = "aa\n"，剩 bb,cc
    f.feed(["\"", "A", "d", "d"]); // 删除 bb → "a = "aa\nbb\n"，剩 cc
    f.feed(["\"", "a", "p"]); // 粘出合并的两行
    assert_eq!(f.text(), "cc\naa\nbb\n", "追加的删除应可整段粘出");
}

// ---- 空文本对象 -----------------------------------------------------------------

#[test]
fn ci_quote_on_empty_pair_enters_insert() {
    let mut f = Fixture::at("\"\"", 0, 1);
    f.feed(["c", "i", "\""]);
    f.type_text("x");
    f.feed(["<Esc>"]);
    assert_eq!(f.text(), "\"x\"", "空引号内应可插入");
}

#[test]
fn ci_paren_on_empty_pair_enters_insert() {
    let mut f = Fixture::at("f()", 0, 2);
    f.feed(["c", "i", "("]);
    f.type_text("y");
    f.feed(["<Esc>"]);
    assert_eq!(f.text(), "f(y)", "空括号内应可插入");
}

#[test]
fn ci_paren_with_cursor_on_open_paren() {
    // 光标压在 `(` 上：vim 仍选中包含它的块
    let mut f = Fixture::at("(ab)", 0, 0);
    f.feed(["c", "i", "("]);
    f.type_text("x");
    f.feed(["<Esc>"]);
    assert_eq!(f.text(), "(x)", "光标在 ( 上应选中括号内部");
}

#[test]
fn visual_i_paren_on_empty_pair_collapses() {
    let mut f = Fixture::at("()", 0, 0);
    f.feed(["v", "i", "("]);
    assert_eq!(f.cursor(), 1, "空选区应塌缩到内区间起点");
}

// ---- C-e / C-y 与 Tab ------------------------------------------------------------

#[test]
fn ctrl_e_and_ctrl_y_scroll_the_view() {
    let text = (0..20).map(|i| format!("l{i}")).collect::<Vec<_>>().join("\n");
    let mut f = Fixture::new(&text);
    f.host.viewport = (0, 4); // 5 行视口
    f.feed(["G"]);
    f.host.viewport = (0, 4);
    f.host.scrolled_to.clear();
    let before = f.line();
    f.feed(["<C-y>"]);
    assert!(
        f.host.scrolled_to.contains(&before),
        "C-y 应保证光标行仍可见（视图上移）"
    );
}

#[test]
fn tab_jumps_forward_like_c_i() {
    let text = "one\ntwo\nthree\nfour".to_owned();
    let mut f = Fixture::at(&text, 1, 0);
    f.feed(["G"]); // jump: line 1 -> line 3
    assert_eq!(f.line(), 3);
    f.feed(["<C-o>"]); // back to line 1
    assert_eq!(f.line(), 1);
    f.feed(["<Tab>"]); // forward again — same entry as <C-i>
    assert_eq!(f.line(), 3, "Tab 应等价 C-i（跳转列表前进）");
}

// ---- % 的 count 百分比 ------------------------------------------------------------

#[test]
fn percent_with_count_jumps_to_file_percentage() {
    let text = (0..101).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
    let f = edit(&text, 0, 0, &["5", "0", "%"]);
    assert_eq!(f.line(), 50, "50% 应到第 51 行（0-based 50）");
    let f = edit(&text, 10, 0, &["1", "0", "0", "%"]);
    assert_eq!(f.line(), 100);
}

// ---- :N 跳行 ----------------------------------------------------------------------

#[test]
fn colon_number_jumps_to_line() {
    let text = "a\nb\nc\nd\n".to_owned();
    let f = edit(&text, 0, 0, &[":", "3", "<CR>"]);
    assert_eq!(f.line(), 2);
}

// ---- :s 的 i / I 标志 ---------------------------------------------------------------

#[test]
fn substitute_i_flag_forces_case_insensitive() {
    // smartcase 默认开启：含大写的模式按大小写敏感；i 标志覆盖之
    let mut f = Fixture::at("Foo foo FOO", 0, 0);
    f.feed([":", "s", "/", "F", "o", "o", "/", "X", "/", "g", "i", "<CR>"]);
    assert_eq!(f.text(), "X X X", "i 标志应强制忽略大小写");
}

#[test]
fn substitute_capital_i_flag_forces_case_sensitive() {
    let mut f = Fixture::at("Foo foo FOO", 0, 0);
    f.feed([":", "%", "s", "/", "f", "o", "o", "/", "X", "/", "g", "I", "<CR>"]);
    assert_eq!(f.text(), "Foo X FOO", "I 标志应强制大小写敏感");
}

// ---- Ex 范围里的 mark ---------------------------------------------------------------

#[test]
fn mark_range_substitute() {
    let mut f = Fixture::at("x\nfoo\nfoo\nfoo\nx", 0, 0);
    f.feed(["j", "m", "a", "j", "m", "b"]); // 'a=line1, 'b=line2
    f.feed([":", "'", "a", ",", "'", "b", "s", "/", "f", "o", "o", "/", "b", "a", "r", "/", "<CR>"]);
    assert_eq!(f.text(), "x\nbar\nbar\nfoo\nx", "'a,'b 两行都应替换");
}

// ---- @: 重复上次 Ex 命令 -------------------------------------------------------------

#[test]
fn at_colon_repeats_last_ex_command() {
    let mut f = Fixture::at("foo\nbar\nfoo\nbaz\nfoo\nqux", 0, 0);
    f.feed([":", "s", "/", "f", "o", "o", "/", "X", "/", "<CR>"]);
    assert_eq!(f.text(), "X\nbar\nfoo\nbaz\nfoo\nqux");
    f.feed(["j", "j", "@", ":"]); // line 2
    assert_eq!(f.text(), "X\nbar\nX\nbaz\nfoo\nqux");
    f.feed(["j", "j", "@", ":"]); // line 4
    assert_eq!(f.text(), "X\nbar\nX\nbaz\nX\nqux");
}

// ---- Replace 模式 Backspace 恢复 -----------------------------------------------------

#[test]
fn replace_mode_backspace_restores_overwritten_chars() {
    // BS 恢复的是「被覆盖的原字符」：Z 覆盖了 c，先还原 c，再还原 b
    let mut f = Fixture::at("abcdef", 0, 0);
    f.feed(["R"]);
    f.type_text("XYZ");
    assert_eq!(f.text(), "XYZdef");
    f.feed_raw(Key::named("backspace"));
    f.feed_raw(Key::named("backspace"));
    f.feed(["<Esc>"]);
    assert_eq!(f.text(), "Xbcdef", "BS 应把 c、b 依次还原");
    assert_eq!(f.cursor(), 0);

    // 三次 BS 完全撤销本次替换
    let mut f = Fixture::at("abcdef", 0, 0);
    f.feed(["R"]);
    f.type_text("XYZ");
    for _ in 0..3 {
        f.feed_raw(Key::named("backspace"));
    }
    f.feed(["<Esc>"]);
    assert_eq!(f.text(), "abcdef", "逐字符 BS 应完整恢复原文");
}

#[test]
fn replace_mode_backspace_past_line_end_deletes() {
    // 追加（行尾之后）的部分没有原文可恢复：BS 普通删除
    let mut f = Fixture::at("abc", 0, 0);
    f.feed(["R"]);
    f.type_text("abcdefgh");
    assert_eq!(f.text(), "abcdefgh");
    f.feed_raw(Key::named("backspace"));
    f.feed_raw(Key::named("backspace"));
    f.feed(["<Esc>"]);
    assert_eq!(f.text(), "abcdef");
}

// ---- gq 按显示宽度折行 ----------------------------------------------------------------

#[test]
fn gq_wraps_by_display_width_for_cjk() {
    // 每个词 2 汉字 = 4 显示列；宽 10 时每行至多放两个词（9 列）
    let mut f = Fixture::at("中中 中中 中中 中中 中中\n", 0, 0);
    f.vim.options_mut().textwidth = 10;
    f.feed(["g", "q", "q"]);
    for line in f.text().lines() {
        let w: usize = line.chars().map(gpui_vim_core::buffer::char_display_width).sum();
        assert!(w <= 10, "折行后每行显示宽度不得超过 textwidth: {line}");
    }
    assert_eq!(f.text().lines().count(), 3, "5 个词应折成 2+2+1 三行");
}

// ---- :q / :q! 强制标志 ------------------------------------------------------------------

#[test]
fn plain_q_is_not_forced_but_q_bang_is() {
    let mut f = Fixture::at("x", 0, 0);
    f.feed([":", "q", "<CR>"]);
    assert!(f.host.close_requested);
    assert!(!f.host.close_forced, ":q 不应带强制标志");

    let mut f = Fixture::at("x", 0, 0);
    f.feed([":", "q", "!", "<CR>"]);
    assert!(f.host.close_requested);
    assert!(f.host.close_forced, ":q! 应带强制标志");

    let mut f = Fixture::at("x", 0, 0);
    f.feed(["Z", "Z"]);
    assert!(f.host.close_forced, "ZZ 写后强制关闭");
    let mut f = Fixture::at("x", 0, 0);
    f.feed(["Z", "Q"]);
    assert!(f.host.close_forced, "ZQ 等价 :q!");
}

// ---- / 与 ? 共享搜索历史 -----------------------------------------------------------------

#[test]
fn search_history_is_shared_between_slash_and_question() {
    let mut f = Fixture::at("abc abc", 0, 0);
    feed(&mut f, "/abc<CR>");
    feed(&mut f, "?"); // 打开 ? 提示符但不执行
    f.feed_raw(Key::named("up"));
    assert_eq!(f.vim.cmdline.buffer, "abc", "? 应能浏览 / 的历史");
}

// ---- 杂项：寄存器组回归守护 ------------------------------------------------------------

#[test]
fn yank_register_roundtrip_intact() {
    let mut f = Fixture::at("hello", 0, 0);
    f.feed(["y", "y"]);
    assert_eq!(f.vim.registers.get(YANK).map(|r| r.text.clone()), Some("hello".into()));
    assert_eq!(f.vim.registers.get(CLIPBOARD), None, "显式 yank 不碰 + 寄存器");
}
