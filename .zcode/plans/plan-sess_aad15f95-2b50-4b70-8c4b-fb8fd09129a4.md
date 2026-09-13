# gpui-vim：面向 gpui 文本编辑项目的可复用 Vim 引擎

## 一、调研结论（方案依据）

- **ideavim 架构**是严格的"纯引擎 + 宿主适配层"双层结构：引擎（`vim-engine/`）只面向 `VimEditor/VimCaret` 等接口编程，通过 `injector` 服务定位器访问宿主；核心机制包括 sealed `Mode` 状态机、`KeyStrokeTrie` 按键前缀树、`CommandBuilder`（count/register/motion 参数组装）、Consumer 按键流水线、motion 返回 offset / text object 返回 TextRange 的统一签名。这些全部值得在 Rust 中复刻。
- **gpui 能力已核实**（本地 zed 源码 + docs.rs 0.2.2）：`App::intercept_keystrokes` 在**所有 action/事件机制之前**触发、可在其中 `stop_propagation` 实现引擎优先吃键；`KeyBinding` 原生支持 `"g g"` 多键序列与 `KeyContext` 谓词；`InputHandler`/`ElementInputHandler` 提供 IME；`uniform_list` + `paint_quad` 做编辑器渲染。gpui 本身无内置多行编辑器，`examples/input.rs` 是官方模板。
- 版权约束：ideavim 与 zed 的 vim crate 均为 GPL，**只借鉴架构、不复制代码**；本项目 MIT/Apache-2.0。

## 二、总体架构

三层 workspace，引擎零 gpui 依赖（照搬 ideavim 分层，可直接单测）：

```
gpui_vim/
├── Cargo.toml                  # workspace
├── crates/
│   ├── vim-core/               # 纯 Rust vim 引擎（无 gpui 依赖）
│   ├── gpui-vim/               # gpui 集成层：按键桥接 + UI 状态 + key_context
│   └── gpui-vim-demo/          # 参考宿主：ropey buffer + 多行编辑器 widget（新项目的抄写模板）
└── README.md                   # 中文 README + 英文集成指南
```

**宿主适配面（vim-core 定义 trait，宿主实现）**——比 ideavim 大幅简化（插件模型下引擎自己持有光标，而非宿主持有 caret 再双向同步）：

```rust
pub trait VimBuffer {                      // 只读文本 + 行语义
    fn line_count(&self) -> usize;
    fn line_range(&self, line: usize) -> Range<usize>;   // 含行尾换行的 offset 区间
    fn char_indices / char_at / line_content(...);
    fn offset_to_pos(&self, off: usize) -> (usize, usize); // (line, col)，字节 offset
    fn pos_to_offset(&self, line: usize, col: usize) -> usize;
}
pub trait VimBufferMut: VimBuffer {
    fn insert(&mut self, offset: usize, text: &str);
    fn delete_range(&mut self, range: Range<usize>);
}
pub trait VimHost {                        // 视口 / 反馈 / 系统副作用
    fn scroll_caret_into_view(&mut self);
    fn clipboard_write(&mut self, text: &str, explicit_register: bool);
    fn clipboard_read(&self) -> Option<String>;
    fn set_search_highlights(&mut self, matches: &[Range<usize>], current: Option<usize>);
    fn undo_group(&mut self, event: UndoEvent); // Begin(cmd_id) / BeginInsert / Split / End
    fn changed(&mut self);                 // 请求重渲染
}
```

内部坐标统一用 **UTF-8 字节 offset**（ropey 原生），仅在 gpui `InputHandler` 边界做 UTF-16↔UTF-8 映射（避开 ideavim 的坐标痛点）。

## 三、vim-core 设计（ideavim 概念的 Rust 化）

- **状态机**：`enum Mode { Normal, Insert, Replace(占位), Visual{kind: Char|Line|Block(占位)}, CmdLine(prompt), OperatorPending }` + `return_to` 嵌套字段（对应 ideavim `Mode.kt`）。全部引擎状态集中在 `VimState`：mode、cursor（offset + desired_col + visual_anchor）、pending keys、count、pending register、macro 占位。
- **按键流水线**（`VimState::handle_key(&mut, key: Key, buf, host) -> KeyResult::{Consumed, Unknown}`，`Unknown` 即"还给宿主"，对应 ideavim `KeyProcessResult`），consumer 顺序简化为：cmdline 输入 → 用户映射（递归带深度上限）→ count 解析 → `"x` 寄存器 → operator-pending → 命令 trie → `f/t/r` 等字符参数 → insert 模式字符。同步执行（砍掉 ideavim 的异步两段快照）。
- **命令表（数据驱动）**：静态表 `(keys: &[&str], modes: &[Mode], handler: fn)`，启动时按 mode 建 `Trie`（对应 `KeyStrokeTrie`），trie 中间节点即 pending；新增命令 = 加一行表项 + 一个 handler 函数。
- **Motion**：`enum Motion`（hjkl 0 ^ $ w W b B e E ge f/F/t/T ; , % gg G { } n N * # | H M L zz zt zb gj gk），handler 返回 `MotionResult { offset, kind: Exclusive|Inclusive|Linewise }`；operator 按 kind 计算删除区间，特判 vim 的 `cw` 语义。
- **TextObject**：`fn(iw/aw/i"/a(/it...) -> Option<TextRange>`，复用一套字词边界扫描函数（供 motion 与 object 共用）。
- **Operator**：`enum Operator { Delete, Change, Yank, Indent(Dir), Case(...) }`，与 motion/object 正交组合（`2ciw` = count×operator×object，对应 `CommandBuilder` 的 count 相乘）。
- **寄存器**：`HashMap<char, Register>` + unnamed/numbered(0,1..)/`_` blackhole 语义 + linewise/charwise 标记；`+` 走 host 剪贴板。
- **搜索**：`regex` crate；`/ ? n N * #`、`hlsearch`/`incsearch` 高亮经 `set_search_highlights` 交给宿主绘制；cmdline 模式维护输入缓冲与历史。
- **marks**：a-z 本地 + `'` `` ` `` 跳转 + 自动 marks（`` `. `` `'^` `'<` `'>`）。
- **undo 语义**：一次 normal 命令 = 一个 undo 组；`i…<Esc>` 整段 = 一个组（经 `undo_group` 事件通知宿主归并）；u/C-r 由宿主执行栈操作（demo 用快照栈实现）。
- **keymap API**：`VimState::map(mode, "jj", "esc")` / `noremap` 程序化 API（vimrc 文件解析留到后续）。
- **测试**：`StringBuffer` 测试宿主 + 表驱动测试（`normal 模式在 "..." 上按 dd → buffer/cursor/register 断言`），覆盖 motion×operator×object 组合矩阵。

## 四、gpui-vim 集成层

- `gpui_vim::attach(&mut App, WeakEntity<VimSession>)`：注册 `intercept_keystrokes`，回调里检查 focus 属于本编辑器 → `vim.update(...)` → `handle_key` → `Consumed` 则 `cx.stop_propagation()`，`Unknown` 落回宿主 keymap/IME。
- 模式暴露为 `key_context("vim")` + `mode == normal/insert/visual` 谓词，宿主可按模式绑定自己的补充键位（Zed vim 同款模型）。
- `VimSession` 为 `EventEmitter<VimEvent>`（ModeChanged/StateChanged），宿主据此重绘；提供 `mode()`、`showcmd()`（如 `3d`）、`cursor_shape()`（block/beam）查询。
- 新项目集成三步：实现 `VimBuffer(+Mut)`（可抄 demo 的 ropey 实现）→ `cx.new(VimSession::new)` → `attach` + 渲染时读 session 画光标/选区/状态。

## 五、demo 宿主（gpui-vim-demo，参考实现兼冒烟应用）

- ropey `Rope` 实现 `VimBuffer(Mut)`；undo 为快照栈 + 组归并。
- 编辑器 view：`uniform_list` 逐行 `shape_line` 渲染（等宽字体），选区/搜索高亮半透明 quad，block(普通/可视)/beam(插入)光标 `paint_quad`（input.rs 模式）；`track_focus` + `key_context("vim")`。
- `InputHandler` 实现（IME）：`replace_text_in_range` 等全部转发引擎，在插入模式下修改 buffer 并推进引擎光标；UTF-16↔UTF-8 映射；marked text（组字）以 overlay range 渲染下划线；normal 模式 `accepts_text_input=false`。
- UI：状态栏（-- INSERT --/VISUAL 模式指示、showcmd、搜索提示符）+ 底部 cmdline 输入行 + 行号列（number/relativenumber）。

## 六、实施顺序（每步可验证）

1. workspace 脚手架 + vim-core：Mode/Key/VimState/buffer trait/StringBuffer 测试骨架
2. Normal 模式核心：motions + d/c/y + count + 寄存器 + undo 事件，表驱动测试通过
3. gpui-vim 桥 + demo v0：窗口内渲染 buffer、拦截器接通，手动验证 hjkl/dd/cw/yiw
4. Insert 模式：IME InputHandler + i/a/o/O/I/A/s/C/x/r/J/p/P 等 + Esc 语义
5. Visual（char/line）+ operator/text object 全集 + v/o/gv
6. 搜索（/?nN*#）+ incsearch/hlsearch 高亮 + cmdline UI + marks
7. 状态栏/showcmd/keymap API/行号与 scrolloff 选项子集打磨
8. 全量测试 + README（中文）+ INTEGRATION.md（英文集成指南）

## 七、明确不做（记入 README 路线图）

vimrc 脚本解析、`:s` 替换、macro 录制、Visual-Block 多光标、Replace 模式、fold、多 buffer/窗口命令、`'timeout'` 超时——架构已留扩展点，后续按需加。

## 八、验证

- `cargo test`（vim-core 表驱动为主）+ `cargo clippy`；
- `cargo run -p gpui-vim-demo` 手动冒烟清单（hjkl/dd/cw/yiw p/3dw/ciw/v j d/数字键盘加 count/i a o Esc//搜索 nN/m'{mark}/u C-r/mode 指示与光标形状切换/中文 IME 输入）。

依赖：`gpui = "0.2"`、`ropey`、`regex`（均 MIT/Apache 兼容）。