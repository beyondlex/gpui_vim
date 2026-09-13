# gpui-vim

为 [gpui](https://gpui.rs)（Zed 的 Rust UI 框架）文本编辑项目打造的**可复用 Vim 引擎**。

架构参考 JetBrains IdeaVim 的"纯引擎 + 宿主适配层"分层（只借鉴设计，未复制任何 GPL 代码），让任何 gpui 应用通过三个步骤获得完整的 vim 编辑能力。

```
┌────────────────────────────────────────────────┐
│ 你的 gpui 应用 / 编辑器组件                      │
├────────────────────────────────────────────────┤
│ gpui-vim        集成层：按键拦截、模式上下文      │
├────────────────────────────────────────────────┤
│ vim-core        纯 Rust 引擎（零 gpui 依赖）     │
│   模式机 · 按键流水线 · Trie 命令表               │
│   motion · operator · text object               │
│   寄存器 · 搜索 · marks · undo 语义              │
├────────────────────────────────────────────────┤
│ 宿主 trait：VimBuffer(Mut) + VimHost（你实现）   │
└────────────────────────────────────────────────┘
```

## 功能（v1）

- **模式**：Normal / Insert / Visual（char & line）/ 命令行（`/` `?` 搜索）
- **Motion**：`h j k l 0 ^ $ g_ w W b B e E ge gE f F t T ; , % gg G { } ( ) n N * # | H M L zz zt zb C-d C-u C-f C-b + - ' ` 等，全部支持 count（`3w`、`2gg`）
- **Operator**：`d c y > < gu gU g~`，与 motion / text object 正交组合（`dd` `cw` `ciw` `3dw` `dap` `g~~` `>j`…），实现 vim 的范围特判规则（`dw` 不跨行、`cw` 等价 `ce`、列 1 规则等）
- **Text object**：`iw aw iW aW is as ip ap`、引号 `i" a" i' a' i` a`` `、括号块 `i( a( i[ a[ i{ a{ i< a< ib aB`、`it at`
- **寄存器**：unnamed、`"0` yank、`"1`-`"9` 删除环、`"-` 小删除、`"_` blackhole、`"+` 系统剪贴板、`"a-"z` 命名寄存器，linewise/charwise 语义，`p P` 支持 count
- **编辑命令**：`x X s S D C Y r ~ J gJ p P u C-r o O i a I A gI >> << zz zt zb gv m ' ` 及方向键/翻页键
- **搜索**：`/ ? n N * #`，Rust `regex` 语法，incsearch / hlsearch 高亮，搜索历史
- **Marks**：`m{a-z}`、`` `{char} ``、`'{char}`、自动 marks（`< > ^ .`）
- **undo**：一次命令一组、整个 insert 会话一组（`i…<Esc>` 一次 undo），语义由引擎发出、宿主执行
- **用户映射**：`map`/`noremap` 程序化 API（如经典的 `jk` → Esc，支持等待态与递归上限）
- **UI 配套**：模式指示、showcmd、block/beam 光标、搜索高亮、行号（absolute/relative）

## 快速开始

```bash
cargo run --release -p gpui-vim-demo        # 运行演示编辑器
GPUI_VIM_SMOKE=1 cargo run -p gpui-vim-demo # 启动 2 秒后自动注入按键序列做自检
cargo test                                  # 引擎表驱动测试（27 个）
```

演示应用是一个完整的多行编辑器（ropey buffer、行号、状态栏、搜索高亮、鼠标点选/拖选、IME），`crates/gpui-vim-demo/src/` 就是新项目的参考实现。

## 在你的项目里使用

三步接入，详见 [INTEGRATION.md](INTEGRATION.md)：

1. 你的 buffer 实现 `VimBuffer` / `VimBufferMut`（参考 demo 的 ropey 实现）
2. 你的编辑器视图实现 `gpui_vim::VimEditor` trait
3. `gpui_vim::attach(&editor_entity, cx)` + 在 `InputHandler::replace_text_in_range` 里调用 `gpui_vim::dispatch_text`

```rust
// 按键处理完全是数据驱动的，扩展命令 = 表里加一行
b.normal(&["Z", "Z"], CmdKind::Normal(NormalCmd::SaveAndQuit));
```

## 关键设计（承自 IdeaVim）

- **引擎零依赖**：`vim-core` 不依赖 gpui，`KeyResult::Unknown` 表示"引擎不处理、还给宿主"，与宿主 keymap / 系统快捷键共存
- **数据驱动命令表**：静态命令表 + 每模式一棵按键 Trie（对应 IdeaVim 的 `KeyStrokeTrie`），Trie 中间节点即 operator-pending 等待态
- **引擎持有光标**：插件模型下引擎自己管理 cursor/mode/选区锚点，宿主只实现 buffer 读写与视口/剪贴板/undo 钩子（比 IdeaVim 的宿主持有 caret 模型简单得多）
- **单一边界坐标**：引擎内部统一 UTF-8 字节偏移，仅在 gpui `InputHandler`（UTF-16）边界转换一次
- **macOS 按键双路径**（gpui 0.2 平台特性，集成层已处理）：特殊键（Esc/方向键/`C-*`）走 `intercept_keystrokes` 拦截器；可打印字符被系统先路由给 IME（`replace_text_in_range`），需经 `dispatch_text` 汇入同一条引擎流水线

## 目录

```
crates/
├── vim-core/       纯引擎：模式、流水线、motions、operators、objects、
│                   registers、search、marks、undo 语义（27 个表驱动测试）
├── gpui-vim/       gpui 集成：VimEditor trait、attach 拦截、dispatch_text、
│                   key_context（mode == normal/insert/visual 谓词）
└── gpui-vim-demo/  参考宿主：ropey buffer + 编辑器视图 + IME + 鼠标 + 状态栏
```

## 路线图

- [ ] `:s` 替换、`:` Ex 命令子集、vimrc（map/set 行解析）
- [ ] macro 录制回放（`q`/`@`）、dot repeat（`.`）
- [ ] Visual-Block（`C-v`）与多光标
- [ ] Replace 模式（`R`，引擎状态机已预留）
- [ ] 折叠（fold）、`'timeout'`/`timeoutlen`、jumplist（`C-o`/`C-i`）
- [ ] gpui-kit 编辑器组件的官方适配层

## License

MIT OR Apache-2.0。架构思想参考 IdeaVim（GPL-3，仅设计借鉴）与 Zed vim 模式（GPL-3，仅行为对照），全部代码为原创实现。
