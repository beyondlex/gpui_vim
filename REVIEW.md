# 代码审查记录（2026-09）

全量通读 `vim-core`、`gpui-vim`、`gpui-vim-demo` 三个 crate（约 1.2 万行），
对每个可疑行为用系统 vim 9.1（macOS）做了基准验证。本文记录：

- 已确认并修复的 bug（每项附回归测试）；
- 审查中怀疑、经核实**不是** bug 的点；
- 确认与 vim 有分歧但**未修复**的点（按建议处理级别分级）。

修复的回归测试集中在
`crates/vim-core/tests/review_regressions.rs`。

---

## 一、已修复的 bug

### 1. `gq` 作用于第 1 行之后会整段误删（严重）

`ops::format_lines` 的签名是 `(start: 字节偏移, last_line: 行号)`，但
内部循环 `for line in start..=last_line` 把字节偏移当行号用。span 起点在
第 0 行时两者恰好都是 0，所以既有测试全绿；从第 1 行起，循环区间为空、
`out` 为空串，随后 `edit_replace(start..span_end, "")` 把整段文字删掉。

修复：循环前 `let first_line = ctx.buf.offset_to_line(start)`。
测试：`gq_on_later_line_does_not_delete_text`。

### 2. linewise `p`/`P` 完全忽略 `after` 参数（功能缺失）

`ops::put` 的 linewise 分支无条件把文本插到「当前行之后」，`P`
（上方粘贴）实际贴到了下方。既有测试用「yank 当前行再 P」，粘贴内容与
当前行相同，文本结果恰好一样，因此从未暴露。

修复：`after == false` 时插到 `line_start(line)`。
测试：`linewise_put_cursor_on_first_pasted_line`（P 半区）。

### 3. linewise 粘贴后光标落在最后一行

vim 9.1 实测（`p`/`P`、1–4 行粘贴逐一验证）：光标落在粘贴文本**第一行**
的第一个非空白字符上。旧代码落在最后一行。

修复：`cursor_line = offset_to_line(insert_at)`（插入后即第一粘贴行）。

### 4. `C-a`/`C-x` 的数字边界两处错误

- 光标落在多位数字的**最后一位**上（如 `129` 的 `9`）时，只从光标位向后
  扫描，把 `129` 当成 `9` 来加（`129+1` 变出 `1210`）。vim 是整个数字
  129 → 130。
- 光标之后没有任何数字时，旧代码会**反向回扫**并加光标前方的数字
  （`123 abc` 光标在 `a` 上也能加出 124）；vim 报 E18、无操作。

修复：光标压在数字上（或停在行尾字节、视觉上压着行尾数字）时先回扫到
数字串起点；否则只向前找，找不到返回 false。
测试：`c_a_on_last_digit_increments_whole_number`、
`c_a_ignores_numbers_before_cursor`、`c_a_at_line_end_uses_trailing_number`。

### 5. `J` 绕过 `edit_replace`，marks 与搜索缓存失同步

`ops::join_lines` 的非 literal 分支直接调 `ctx.buf.replace_range(...)`，
绕过了所有引擎编辑都必须经过的 `edit_*` 包装：marks 不平移、
`edit_generation` 不增长（`n`/`N` 的缓存匹配列表因此读到过期偏移）、
`last_visual` 不跟随。`gJ` 分支用的是 `edit_delete`，不受影响。

修复：改走 `vim.edit_replace(...)`。
测试：`join_updates_marks_through_edit_replace`。

### 6. `dw` 在行尾空白处吞掉换行

exclusive 运动的「列一规则」第二分支（起点不在行首非空白）应把终点移到
**前一行的最后一个字符**并转为 inclusive——即 span 终点是 `line_end`，
换行符存活。旧代码 `line_end + 1` 把 `\n` 一并删掉，两行被合并。vim 9.1
实测 `dw` 保留换行。

修复：去掉 `+ 1`（第一分支的 linewise 规则不变）。
测试：`dw_on_trailing_blanks_keeps_newline`。

### 7. `?` + 空回车沿旧方向重复搜索

vim：空模式重复**上一次搜索**，但方向随**当前提示符**（`?` + 回车向后，
`/` + 回车向前）。引擎在空模式下沿用 `self.search.forward`（旧方向），
`?` + 回车实际向前跳。

修复：使用 `execute_search` 传入的 `forward`（即本次提示符方向）。
测试：`question_enter_repeats_search_backward`。

### 8. 大小写算子截断多字符大小写映射

`gu`/`gU`/`g~` 逐字符用 `to_uppercase().next()` 只取第一个字符：`ß`
应映射为 `SS`，只得到 `S`。更严重的是 `toggle_chars` 用替换文本长度
反推被替换区间（`start + mapped.len()`），当映射改变字节长度时会切错
区间——`ẞ`（3 字节）→`ß`（2 字节）时区间终点落在多字节字符中间，
`BufferView` 的边界保护会直接 panic。

修复：映射改为 `collect::<String>()`；替换区间用实际消耗的
`start..o`。
测试：`toggle_char_expands_and_shrinks_multi_char_case_mapping`。

### 9. `gpui_vim::edit` 本地撤销拦截吞掉 `gu` 系列命令

`VimEdit::process_key` 只要 `mode == Normal` 且按键是 `u`/`C-r` 就拦截
走本地 undo 栈，但引擎此时可能正等着这个键：`gu` 之后的 `u`（操作符等
motion）、`guu` 的尾键（行级加倍）都被吞成本地 undo，
`gu`/`guw`/`guu`/`gugu` 全部失效；且 `3u` 的 count 泄漏给下一条命令。

修复：`VimState` 新增 `is_idle()`（无 pending count/register/operator/
char-arg/cmd_seq/队列键），仅引擎空闲时拦截。
测试：`is_idle_reflects_pending_command_input`（拦截门本身在
`process_key` 内，需要 gpui Context，无法 headless 直测）。

### 10. `let mapleader = "<Space>"` 被拆成五个键

rc 解析用「把 `leader.notation()` 字符串替换回原文再重新解析」的方式
展开 `<Leader>`：`<Space>` 的 notation 是 `Space`（无尖括号），重新解析
时碎成 `S`,`p`,`a`,`c`,`e` 五个键，映射永远匹配不上。

修复：`<Leader>` 先按原样解析成 `Named("leader")` 标记键，再在**解析后
的键序列**上替换成 leader 键。

### 11. 空格键双形态归一（连带修复）

排查 9/10 时确认：空格在不同路径下以 `Named("space")`（gpui 按键路径、
`<Space>` 记法）和 `Char(' ')`（文本输入路径、裸空格）两种 Key 出现，
映射表按精确相等查找，两套写法互不命中。

修复：`Key::parse("<Space>")` 规范化为 `Char(' ')`；`handle_key` 入口把
plain `Named("space")` 归一为 `Char(' ')`（与既有的大写 shift 剥离同一
位置）。`let mapleader = " "` 与 `"<Space>"` 两种写法自此等价。
测试：`space_leader_from_vimrc_space_notation`、
`space_key_matches_char_space_mappings`（另更新 `key.rs` 单测）。

### 12. 顺手修复

- **demo `char_width` 测量回写缺失**：注释声称首帧绘制后用 shaped 值
  回写单字符宽度，但全仓没有任何 `char_width.set` 调用，gutter 宽度与
  鼠标 fallback 永远用 8.4 预估值。已在 paint 回调中回写实测 advance。
- **`registers.rs` 错误注释**：`store_yank` 的注释称 `"_yy` 写无名寄存器
  「vim 不会这样做」。实测 vim 9.1 的 `"_yy` 之后 `@"` 返回刚 yank 的
  内容（无名寄存器是「最后写入寄存器」的别名，yank 总是重指向，包括指向
  黑洞寄存器）。引擎行为与 vim 一致，仅注释错误，已改正（删除路径
  `"_dd` 不碰无名寄存器，与 vim 一致，由 `store` 的早退保证）。

---

## 二、怀疑过、核实后不是 bug 的点

- **`"_yy` 改写无名寄存器**（见上，引擎 = vim）。
- **`w` 运动在空行停在 `line_start`**：对空行该值即 `\n` 字节，与 vim
  「光标落空行 col 0」语义一致，渲染层已按行尾处理。
- **`gp`/可视模式 `:` 种子 `'<,'>`、`gv` 的 `hi-1`**：`last_visual` 存的
  是 exclusive 终点，`hi-1` 恰是最后一个字符起点，不会下溢。
- **`mapping_step` 在 insert 模式 mapping 前缀上 Wait**：等价 vim
  `set notimeout`（见下「设计取舍」）。

---

## 三、确认与 vim 有分歧、未修复的点

### 建议后续处理

1. **`gq` 折行宽度按字符数而非显示宽度**（`ops::flush_paragraph`）：
   CJK 记 1 列（vim 记 2），中文段落的行会明显超视觉宽度。修复需把
   `chars().count()` 换成 `char_display_width` 累加，注意 `col + w >
   width` 的判断也要按宽度。
2. **Replace 模式 Backspace 不恢复原字符**：vim 的 R 模式 BS 会恢复被
   覆盖字符并左移；引擎当普通删除。需在替换时保存被覆盖文本。
3. **`:{n}` 单独输入不跳转**：vim 的 `:5` 跳到第 5 行；引擎解析完 range
   后命令为空直接返回。低成本补齐。
4. **`:s` 的 flags 只认 `g`**：`c`/`i`/`I` 等静默忽略；空匹配语义与 vim
   不同（vim 在空匹配后前进一个字符，引擎直接过滤空匹配），`:s/x*/Y`
   行为不可比。若补 `i` 需同步 `case_insensitive_for`。
5. **`VimEdit` 的 undo 架构限制**：`u`/`C-r` 走 widget 本地栈（绕过引擎），
   (a) `3u` 只响铃不撤销 3 次（`EditHost::undo` 返回 None）；
   (b) 宏与 `.` 重复永远录不到 undo。若要完整语义，应把 undo 栈下沉到
   `EditHost` 并实现 `VimHost::undo/redo`，删除 `process_key` 拦截。

### 设计取舍（已知、可接受）

6. **无 `timeout`/`timeoutlen`**：insert 模式 mapping 前缀（`jk` 的
   `j`）会一直吞键等下一个键，等价 vim `set notimeout`。
7. **`gq` 把首行缩进应用到整段**：vim 对段落后续行有自己的缩进规则；
   引擎是简化版（文档已注明）。
8. **句子运动只认 `.!?` 单字符**（代码已注明），无 `...`、行首规则。
9. **`:s` 替换用 Rust regex 展开（`$1`）**，非 vim 的 `\1`（文档已注明）。
10. **搜索模式为 Rust regex 语法**，`\b`、lookaround 遵循 RE2（文档已注明）。
11. **`jump_to_match` 的 count 取模环绕**：vim 超界报错；引擎环绕
    （代码已注明 u64 防溢出是有意的）。
12. **`:d` 忽略 register/count 后缀**（代码已注明 named registers not
    supported）；`:{range}d 3` 的「从 range 起删 3 行」未实现。
13. **块可视 `I`/`A`/`c` 中输入含换行文本**：退出会话复制到其他行时，
    row 偏移假设单行文本，多行粘贴的落点未定义。未对照 vim。
14. **charwise `p` 按「一个字符」后移**：emoji ZWJ 家族会被从中间拆开；
    `h`/`l`/`x` 已按 grapheme 处理，put 未做。
15. **`marks.adjust_replace` 等长替换时内部 mark 保持绝对偏移**：多字节
    字符内可能失准；近似 vim 的列保持行为。
16. **`PagerBuf` 固有 `line_start`/`line_end` 遮蔽 trait 同名方法且参数
    语义不同**（字节偏移 vs 行号）：类型文档已注明，仍是易错点，建议
    后续改名为 `line_start_at`/`line_end_at`。
17. **`VimState::replace_range`（IME 提交路径）不关闭 undo group、不
    `bump`**：设计上由 insert 会话收口；宿主若在非 insert 模式调用会
    留下悬挂的 open group（demo 的 `unmark_text` 无模式保护，理论上
    可达）。建议后续在该函数内加模式断言或收口。

---

## 四、vim 基准验证方法备忘

用 keystroke 脚本（`vim -u NONE -N -s keys.vim file`）驱动真实 vim 9.1，
`:redir` 捕获 `line(".")`/`col(".")`/`getline(...)`。两个坑：

- **`-es`（silent Ex）模式下 `normal!` 里的 `yy` 等不生效**（E353
  Nothing in register），要用 `-s` keystroke 脚本；
- **keystroke 脚本里 Ex 行必须有行首 `:`**，且 normal 命令后若跟裸换行
  会被解析成 `+`（下移一行），污染光标位置断言——本次 `p` 光标规则的
  「第二行」假阳性即由此产生。
