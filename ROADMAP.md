# gpui-vim Roadmap

给后续贡献者 / AI agent 的执行计划。每个任务给出：现状（含代码位置）、方案、
验收标准、陷阱。**动手前先读「全局不变量」一节**——这些是本代码库已经踩过坑
换来的语义约束，违反它们会直接回归已修复的 bug。

工程布局：

- `crates/vim-core` — 纯 Rust 引擎，不依赖 gpui。宿主通过 `VimBuffer(Mut)` +
  `VimHost` 两个 trait 接入。所有行为改动都要能在 `crates/vim-core/tests/engine.rs`
  里用 headless 测试表达。
- `crates/gpui-vim` — 集成层：`attach()`（keystroke interceptor）、
  `to_core_key`（gpui `Keystroke` → 引擎 `Key` 转换）、`dispatch_text`（IME 文本
  路径）、`VimEditor` trait。
- `crates/gpui-vim-demo` — 参考宿主（渲染、IME、鼠标、剪贴板）。`editor.rs` 目前
  是「复制走」的参考实现；任务 11 会把它组件化。

验证命令：

```sh
cargo test                # 全部测试（当前 39 个，必须全绿再交付）
cargo clippy --workspace  # 当前 0 警告，不要引入新警告
cargo run -p gpui-vim-demo
```

---

## 全局不变量（违反即回归）

1. **Key 规范化发生在两处，缺一不可**
   - `to_core_key`（gpui-vim/src/lib.rs）：macOS 给 Enter/Tab 的 `key_char` 是
     `"\n"`/`"\t"`，必须归一化回 `Key::enter()`/`Key::tab()`，否则 cmdline 会把
     Enter 追加进搜索词。
   - `VimState::handle_key` 入口（vim-core/src/state.rs）：plain `Char` 键上的
     `shift` 标志必须剥离。命令表全部用 `Key::parse("I")` 这类 **无修饰符** 的
     plain char 注册；gpui 送达 shift+I 是 `{shift, Char('I')}`，`Key` 的
     Hash/Eq 包含 modifiers，不剥离则所有大写字母与 shift 标点（`$ ^ % ~ < > "`）
     全部 miss。
   - 新增按键处理时不要在命令表里写 `{shift, ...}` 形式的 Key，用 notation 字符串。

2. **undo 组：一个逻辑命令 = 一个组**。宿主（demo host.rs 与测试 HostView）在
   `begin_undo_group(id, ..)` 调用时**拍快照**。因此引擎必须保证：
   - `begin_insert` 在 `open_undo` 已有组时**复用**该组（change 家族 c/s/C/S
     的「删除+输入」是一步撤销，见 `change_C_types_and_undoes_in_one_step`）；
   - `complete_operator_with_span` / `apply_visual_operator` 在进入了 insert
     session 时**不得** `end_edit()`；
   - `o`/`O` 在 mutate buffer **之前** `begin_edit`（否则快照已含新行，undo
     永远删不掉它）。
   改动 undo 相关代码后必须跑 `change_*` 与 `open_line_o_*` 测试。

3. **IME 合成协议**（gpui 0.2.2 macOS，见 platform/mac/window.rs 的
   `handle_key_event`）：只要 `marked_text_range()` 返回 `Some`，**所有** keyDown
   先路由给输入法（`is_composing` 分支），引擎的 interceptor 在其间收不到键——
   这是平台行为，不要试图绕过；集成层要做的是把合成协议的三个回调都实现正确
   （见任务 1）。

4. **打印字符在 insert 模式返回 `Unknown`** 是刻意设计（保 IME/组合输入可用），
   不要为了「引擎全接管」改成 Consumed。

5. `intercept_keystrokes` 的 `Subscription` 必须保活（demo 里存在 view 上），
   drop 即退订。

---

## P0 — 正确性

### 任务 1：中文/日文 IME 合成输入完全不可用 ✅ 已完成并通过用户验收

**症状**（已由用户在中文拼音输入法下复现）：insert 模式中切换到拼音输入法后，
(1) Esc 退不出 insert 模式；(2) 候选词上屏后，已输入的拼音字母残留在 buffer
里，汉字出现在错误位置（形如 `nihao你好`）。

**根因**（已在源码中确认，非宽字符问题）：

1. 合成期间 `replace_and_mark_text_in_range`（demo editor.rs）把**原始拼音**
   （如 `nihao`）直接插入 buffer 并记录 `marked_range`。这一步本身可行（作为
   合成预览），但——
2. 候选上屏时 macOS 调 `insertText:"你好"` → gpui 转成
   `replace_text_in_range(None, "你好")`。demo 的实现把 `None` 当作「普通输入」
   走 `dispatch_text`（追加到光标），**没有替换 marked_range** → 拼音残留。
   正确契约：`marked_range` 为 `Some` 时，`replace_text_in_range(None, ..)` 必须
   用新文本**替换 marked_range** 并清除标记。
3. `unmark_text` 只把 `marked_range` 置 None，不删除 buffer 中的合成文本。若
   上屏/取消路径只触发 unmark 而未先经过空串 setMarkedText，拼音永久残留。
4. `marked_text_range` 对 `start..start` **空区间**仍返回 `Some`：合成被取消后
   （IME 以空串 setMarkedText 结束时，demo 会留下 `start..start`），gpui 的
   `is_composing` 恒为 true，**所有按键（含 Esc）永久先路由给输入法**——这就是
   「Esc 退不出 insert」的主因之一。空区间必须返回 `None`。
5. Esc 在合成期间被输入法消费（取消合成）是 macOS 标准行为，正确的做法是接受
   「合成中按 Esc = 取消本次合成，再按一次才退出 insert」，与真实 vim GUI 一致；
   不要尝试拦截。

**方案**：

- demo `editor.rs`：
  - `marked_text_range`：`if start == end { return None }`。
  - `replace_text_in_range`：进入时先判 `let composing = self.marked_range.take();`；
    当 `composing` 为 `Some` 且传入 range 为 None 时，执行
    `vim.replace_range(ctx, composing, text)`（不经 `dispatch_text`，**合成文本
    不得跑引擎按键管线**，否则会误触发 insert 模式映射如 `jk`），游标落在
    `composing.start + text.len()`。
  - `unmark_text`：若 `marked_range` 非空，先删除该区间的文本再置 None（防御
    直接 unmark 的取消路径）。
  - `replace_and_mark_text_in_range`：沿用现有「替换上一段 marked 文本」逻辑，
    但新 marked 区间为空时直接置 None。
- 引擎侧无需改动（合成不经过引擎管线）。

**验收**（手动，需切系统输入法为拼音）：

1. insert 模式输入 `nihao` → buffer 显示拼音（可带 marked 高亮），选候选「你好」
   → buffer 恰为 `…你好…`，无 `nihao` 残留，游标在 `好` 之后。
2. 输入 `ni` 后按 Esc → 拼音被移除（取消合成），buffer 不残留；再按 Esc 退出
   insert 模式。
3. 合成文本落在游标处而非行尾/文件尾；连续两次合成（两句中文）均正确替换。
4. 英文直接输入（无合成）路径不回归：`cargo test` 与普通打字、`jk` 映射仍工作。

**陷阱**：`marked_range` 是 UTF-8 字节区间，gpui 传入的 range 是 UTF-16；转换
只用 demo buffer.rs 已有的 `utf16_to_byte`/`byte_to_utf16`，不要自写换算。

### 任务 2：marks 在编辑后不平移 ✅ 已完成
> 实现超出原设计：所有 buffer 变更收敛到 `VimState::edit_insert/edit_delete/
> edit_replace` 三个包装器（22 个调用点），顺带修复了 `dw` 在 buffer 最后一行
> 吃掉换行的既有 bug（`span_from_motion` 的 target 未钳到行尾）。等长替换
> （`gu`/`gU`/`g~`）保持区域内 mark 的相对位置。

**现状**：`vim-core/src/marks.rs` 存裸字节偏移。任何插入/删除都不会调整
`a-z`、`'^`、`'.'` 与 `'<`/`'>`；在文件头部插入一行后所有 mark 错位。

**方案**：在 `VimState` 加 `fn adjust_marks(&mut self, at: usize, delta: isize)`：
对所有存储的偏移量，`>= at` 的加 delta（clamp 到 0..=len）。调用点放在
`ctx.buf.insert_text` / `delete_range` 的**唯一汇聚处**——目前引擎所有变更都经
`VimState::insert_text_at_cursor`、`replace_range`、`ops::delete_span`、
`ops::put`、`start_insert(OpenLine)`；建议直接包一层私有 helper（如
`edit_buf`），避免散弹式调用。删除区间内部的 mark：vim 语义是落在删除起点。

**验收**：headless 测试——mark 在删除点之后/之前/区间内三种情况；`'<`/`'>`
随可视区删除平移；`yyp` 后 `'^` 正确。

**陷阱**：寄存器文本不需要平移（存的是文本）；`last_change` 用于 `.` 与
changelist，平移规则相同。

---

## P1 — 日常功能

### 任务 3：`:` Ex 命令行（`:noh` `:set` `:s` `:w` `:q`）✅ 已完成
> 实现：`:noh[lsearch]`、`:set`（`name`/`noname`/`name!`/`name=value`，数值项
> `ts/sw/so`）、`:[%]s/pat/rep/[g]`（Rust regex 展开，`$1` 代替 vim 的 `\1`，
> 空 pattern 复用上次搜索）、`:w`/`:q`/`:q!`/`:wq`/`:x`（新 host 契约
> `save`/`request_close`，demo 里 `:w` 显示状态、`:q` 退出）。历史按 prompt
> 分开；`:s` 是单步 undo；高亮发布全部尊重 `hlsearch`（修了 jump 路径无视
> 该选项的不一致）。不支持：范围语法（`1,3s`/`'<,'>`）、`:\d` 行号、确认模式、
> `:g`（见「非目标」）。

**现状**：`Mode::CommandLine { prompt }` 只有 `'/'`/`'?'`；normal 模式按 `:`
返回 `Unknown`。`options.set_boolean`（options.rs）就是为 `:set` 预留的；
`search::clear_highlights`（现为死代码）就是 `:noh` 的实现；Esc 清高亮的行为
已上线（`:noh` 语义，`n`/`N` 重新点亮）。

**方案**：

1. `Mode::CommandLine` 的 prompt 增加 `':'`（`Cmdline` 结构加一个
   `ex_buffer` 或复用 buffer + prompt 区分）。normal_key 的 `':'` 分支从
   `Unknown` 改为 `begin_cmdline(':')`。
2. `cmdline.rs` 增加 `execute_ex(ctx, line)`，先支持：
   `:noh[lsearch]`、`:set <name>`/`:set no<name>`（走 `set_boolean`，数字项
   `:set ts=8` 后置）、`:s/foo/bar/[g]`（当前行，`:%s` 全文，flags `g` 计数；
   复用 `search::compile` 的 regex 与 smartcase 规则）、`:w`（新 `VimHost`
   契约 `save(&mut self)`）、`:q`（新契约 `request_close(&mut self)`；宿主
   决定是否弹「未保存」——需要 `bell` 之外的消息通道，见任务 13）。
3. 历史：`Cmdline.history` 已有，`:` 与 `/` 共用或按 prompt 分开（vim 分开，
   建议分开：`history: HashMap<prompt, Vec<String>>` 需迁移现有字段）。
4. 状态栏渲染：demo `mode_label` 对 `':'` 显示 `:…` 内容即可。

**验收**：headless 测试覆盖每个命令（含 `:%s/x/y/g` 的计数、未命中、regex
非法输入只 bell 不 panic）；`:set hls!`/`:set nohls` 后 `/` 搜索不再发布高亮。

**陷阱**：`:` 之后按键路径与 `/` 完全一致（含任务 1 的控制字符归一化）；
不要为新 prompt 单开模式分支。`:s` 的替换里 `\1` 后向引用交给 regex crate
的 `replace_all`（`$1` 语法），文档中注明与 vim 的 `\1` 差异。

### 任务 4：`.` 重复上一变更

**现状**：无实现。`NormalCmd::Redo` 之外没有「上一个变更」的概念。

**方案**：在 `execute_command` / `execute_normal_cmd` / `complete_operator_with_span`
的完成点记录一个可重放的 `LastChange` 枚举：

```rust
enum LastChange {
    // 记录「算子 + 运动描述 + count/register」，回放时用当前游标重新求 motion
    Operator { op: Operator, target: ReplayableTarget, register: Option<char> },
    Edit { cmd: NormalCmd },       // x, p, J, ~, >>, …（含 count）
    Insert { entries: Vec<Key> },  // i/a/c… 进入 + 输入文本 + Esc 的按键序列
}
```

- 回放 = 把记录的 key 序列重新喂 `handle_key`（复用现有管线，天然获得新位置的
  motion 语义）。**不要**记录字节偏移再平移——运动必须在回放时重新求值。
- `1.` / `3.`：count 覆盖原命令的 count。
- 合成期间（任务 1）输入的文本不经管线，天然不进 `LastChange`，符合 vim。

**验收**：`dw.`、`dd.`、`3x.`、`ciw foo <Esc> j .`、`A text <Esc> .`（`A` +
文本 + Esc 的重复是 `. ` 最常见用例）。

**陷阱**：`u`/`C-r`/`:` 本身不更新 LastChange；undo 后 `.` 重放的仍是最后的
变更。回放期间禁止再触发映射展开（`map_depth` 语义不变即可）。

### 任务 5：Visual Block（`C-v`）

**现状**：`VisualKind::Block` 与 `RegisterKind::Blockwise` 只是 enum 占位；
无进入命令、无块级 span、无块级 put。

**前置**：**先做任务 8（宽字符列模型）**。块的行内区间必须是「显示列」区间，
字节区间在含 CJK/emoji 的行上会错位。trait 变更一次到位。

**方案**：

- 命令表：`b.normal(&["<C-v>"], CmdKind::EnterVisual(VisualKind::Block))`。
- `ops::span_from_visual` 对 Block 返回逐行区间列表（新类型
  `BlockSpan { col_start, col_end, lines: Vec<Range<usize>> }`）；`d`/`y`/`c`
  按 list 应用；`y` 存 `RegisterKind::Blockwise`；`p` 逐行插入。
- `I`/`A` in block-visual = 块插入（vim 招牌），实现为对每行 start/end 插入
  同一文本，最后一次性进 undo 组。
- 渲染：demo `overlays_for_line` 增加块分支（每行画 `col_start..col_end` 的
  quad）；`LineOverlays` 结构不必改。

**验收**：矩形删除/复制/粘贴；跨 CJK 行的块选择列对齐；块插入后一次 `u`
全部还原（不变量 2）。

### 任务 6：`R` 替换模式

**现状**：`Mode::Replace` 存在；`insert_text_at_cursor` 已有 overwrite 分支。
缺进入命令与退出细节。方案：命令表加 `R → EnterInsert(Replace)`（新
`InsertKind::Replace`），`begin_insert` 不再移动游标；`exit_insert` 对 Replace
不回退一格（现逻辑回退一字符是 insert 的语义）。注意 demo `cursor_is_block`
对 Replace 显示下划线光标（cosmetic，可选）。

### 任务 7：宏 `q` / `@`

**方案**：`VimState` 加 `macro_recording: Option<(char, Vec<Key>)>`；`q{reg}`
开始、`q` 结束、`@{reg}` 回放 = `parse_key_sequence` 反向——直接把录得的
`Vec<Key>` 逐个喂 `handle_key`（`feed_raw` 循环），`@@` 引用上一个。寄存器
复用 `Registers.named`（宏与文本寄存器同名空间，回放文本寄存器时按字符逐个
喂，与 vim 一致）。回放走 `count` 次循环；防死循环：回放期间禁用录制。

---

## P2 — 引擎核心演进（先定 trait 再加功能）

### 任务 8：宽字符 / grapheme 列模型

> **状态：字节算术类光标 bug 已清零**（`p`/`~`/visual `p` 的 `len - 1` 光标、
> demo 非边界插入静默追加、光标宽度 `byte+1`——见 `cjk_*` 回归测试）。本任务
> 剩余部分是**列语义**（desired_col 的字节列 → 显示列、grapheme 步进、鼠标
> 命中）。

**现状**：`Cursor.desired_col`、`desired_column`、所有上下移动、`|`、`H/M/L`
都用 **字节列**。CJK 双宽字符、emoji（多 char grapheme、ZWJ 序列）导致 j/k
列漂移；鼠标点击（demo `byte_at_point` 用统一 `char_width`）在中文文本上
错位。**这不是任务 1 的 IME 问题的原因**（IME 是协议 bug，见任务 1），但它
决定了合成文字上屏后的游标/点击体验，以及任务 5 的块列对齐。

**方案**（trait 变更，一次到位）：

1. `VimBuffer` 增加两个方法（有默认实现则旧宿主零成本迁移）：

```rust
/// 该 char 的显示宽度（0=组合符, 1=窄, 2=宽/East Asian Wide/Fullwidth）
fn char_width(&self, offset: usize) -> usize { /* 默认: unicode-width, 需新依赖或内置表 */ }
/// offset 所在 grapheme cluster 的边界（向后取完整 cluster）
fn next_grapheme_offset(&self, offset: usize) -> Option<usize>;
```

2. 引擎内部把「列」语义从字节改为 `usize` 显示列：`desired_column`、
   `Motion::Up/Down/Column` 的换算经 `line_start + 累加 char_width`。f/t/w/
   e 的词法边界改用 grapheme（`word.rs` 的 `is_word_char` 保持按 char，但
   前进/后退一步 = 一个 grapheme 而非一个 char）。
3. demo：`byte_at_point` 用 shaped line 的 `x_for_index` 反查（shaped 已有
   精确几何），删掉 `char_width` 均摊近似。

**验收**：含 `中文 mix English` 与 emoji（`👨‍👩‍👧`、带变体选择符）的行上：
`j`/`k` 列保持、`0`/`$`/`|`、点击定位、`x` 一次删一个完整 grapheme。

**陷阱**：不要在引擎里引入对 gpui 的依赖；宽度表用 `unicode-width` crate
（纯表，无传递依赖问题）或内置精简表并文档化差异。**所有**受影响测试要先
写「宽字符回归」用例再改实现。

### 任务 9：soft wrap 与真实 `gj`/`gk`

**现状**：`gj`/`gk` 注册为 `Motion::Down/Up` 的别名（tables.rs 注明 "no soft
wrap in v1"）。需要 host 报告「显示行 ↔ 逻辑行/列」映射：`VimHost` 增加
`wrapped_line_bounds(&self, line) -> Vec<显示行首列>` 之类的最小契约，或退一步：
v1 只保证 `j`/`k` 在 wrap 行间按显示行移动（读 viewport 的 host 已有行概念，
需要升级为 (line, display_row)）。**依赖任务 8 的显示列**。此任务动契约较大，
允许降级为「文档明确声明不支持 wrap」并移除误导性的 `gj`/`gk` 行。

### 任务 10：jumplist（`C-o`/`C-i`）

**方案**：`VimState` 加 `jumps: Vec<(usize, u64)>`（offset + buffer 内容纪元，
纪元用 `undo_seq` 或 host `changed()` 计数，防陈旧跳转）；`gg/G/n/N/'{char}/
{`/`}`/`%`/`*` 等大跳转在 `goto_motion` 成功且位移 > 阈值时入栈（当前位先入栈）。
`C-o`/`C-i` 注册进命令表（`Key::ctrl_char('o'/'i')`）。无需 host 契约（v1 不跨
buffer）。

---

## P3 — gpui 集成产品化

### 任务 11：渲染组件化

**现状**：光标/选区/搜索高亮/行号/闪烁全在 demo `editor.rs`（约 800 行），
注释自称 "the file to copy"。本计划修过的渲染 bug（V-line 0 宽 canvas、块光标
反色、闪烁）每个宿主都会再踩一遍。

**方案**：`gpui-vim` 增加公开模块 `overlay`：

- `struct VimOverlays { quads: Vec<Quad>, cursor: Option<CursorGeom> }` +
  `fn compute(state: &VimState, buf: &dyn VimBuffer, line, line_start, line_end) -> Self`
  （把 demo `overlays_for_line` 的逻辑搬过来，含任务 5 的 block 分支）；
- `fn paint(window, bounds, shaped, overlays, style) `（把 paint 闭包搬过来，
  cursor 颜色/宽度可配）；
- blink 提供为独立 helper（`CaretBlinker::new(cx)`，任务中保留输入重置语义）。
- demo 改为调用方；`editor.rs` 缩到只做 buffer/IME/鼠标粘合。

**验收**：demo 行为不变（截图对比）；`cargo test` 全绿；README 增加最小集成
示例 <100 行。

### 任务 12：跨平台按键路径验证

**现状**：`attach()` 的 interceptor → keymap → IME 顺序分析全部基于 gpui
0.2.2 macOS 源码；`pending_unknown_char` 机制（insert 模式 interceptor 退回
的字符由平台再投递一次）是 mac 特化。Linux(X11/Wayland)/Windows 上：打印字符
大概率**直接进 interceptor**（没有 IME 截流），`dispatch_text` 的
`already_dispatched` 分支就是为此写的，但从未验证。

**方案**：三平台各跑一遍 demo 冒烟（现有 `GPUI_VIM_SMOKE` 覆盖引擎管线，需补
真键盘路径）；问题集中处把差异收敛进 `to_core_key`/`dispatch_text`，不要让
demo 感知平台。

### 任务 13：`VimHost` 契约扩展

- `fn status_message(&mut self, msg: &str) {}`（E 级错误提示通道：`E37`、
  `:q` 未保存、搜索无匹配替代裸 bell）；
- `fn buffer_name(&self) -> &str`（`%` 寄存器、状态栏、`dw` 报错文案）；
- `fn save(&mut self) -> bool` / `fn request_close(&mut self)`（任务 3）；
- viewport 升级为含列信息（任务 5/9 的前置，可与之一并做）。

---

## 小件清单（每个 ≤ 半天，可与相邻任务捎带）

- `gi`：数据已就绪（`marks.last_insert_exit` = `'^`），加命令表行即可。
- insert 模式 `C-o`：临时切 normal 执行一条命令后回 insert（复用
  `InsertSession`，不关组）。
- `C-a`/`C-x`：游标处/下一个数字 ± count，进位处理。
- `g;`/`g,`：changelist（需记录每次 `bump()` 的位置栈，注意与任务 2 平移联动）。
- `:jumps`/`:registers`/`:marks`（依赖任务 3，纯信息展示，走 status_message）。
- `ga`/`g8`：字符信息展示。
- demo：`relativenumber` 已支持但无开关入口；`:set rnu!`（任务 3）后即用。

## 已知非目标（v1 明确不做，勿误加）

- 多光标（Zed 特性，非 vim 语义）；
- `:g`/`:v` 全局命令、`:norm`、外部命令 `:!`；
- fold（zf/zo）、diff 模式、terminal 集成；
- 跨 buffer 的 `A-Z` marks 与 `:bnext`（等任务 13 的多 buffer 模型）。
