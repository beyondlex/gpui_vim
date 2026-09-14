# 宿主性能指南（gpui-vim）

给嵌入 gpui-vim 引擎的应用（如 pandagit）的性能参考。分两部分：
[引擎侧成本模型](#引擎侧成本模型)（已实测、有基准可复测）与
[宿主侧清单](#宿主侧清单)（生产化必须做的事）。

复测基准：`cargo run --release -p vim-core --example bench_probe`

## 引擎侧成本模型

引擎每次按键的典型成本是**微秒级**，全部路径都远低于帧预算（16ms）：

| 操作 | 实测（release） | 备注 |
|---|---|---|
| 普通 motion（`w`，11KB buffer） | 3.9 μs/键 | 典型路径 |
| `w` 移动（1MB 单行的病态情况） | 69 μs/键 | 词移动逐字符走查 |
| `x` 删除（1 万行） | ~5 μs/键 | 含 marks 平移、undo 组 |
| `:%s/foo/bar/g`（600KB，5 万行） | 2.5ms | 一次性 |
| `gqG` 重排 200 行 | 2.5ms | 一次性 |
| **hlsearch 高亮重扫**（900KB，1 万匹配） | **0.3 ms/次编辑** | 见下文 |

**唯一随文件大小线性增长的按键路径**是 hlsearch 高亮重扫：开启
`hlsearch` 且有搜索词时，每次编辑（insert 打字的每个字符）都会全文件
正则重扫。900KB 时 0.3ms/键无感；10MB 约 3ms/键开始可感；更大文件需要
节流（见宿主清单第 5 条）。

引擎语义保证（宿主可以依赖）：

- 每个逻辑命令 = 一个 undo 组；`VimHost::begin_undo_group` 时快照。
  ropey 的 `Rope` clone 是 O(log n) 树克隆——**用 ropey 做 undo 快照是便宜的**
- 引擎把所有 buffer 变更收敛到 `edit_insert/edit_delete/edit_replace`
  三个入口（`VimState` 内部），marks、可视选区、搜索高亮随之平移
- 搜索匹配上限 10,000 条（防病态正则拖垮渲染）

一个实测教训（写进代码注释了）：通过 `VimBuffer` trait **逐行**扫描
替代整缓冲 `slice` + 扫描，实测慢约 20,000 倍（2 万次 trait 调用 +
小分配远贵于一次连续 memcpy）。不要做这种"优化"——整缓冲读是
`VimBuffer::slice` 的预期用法。

## 宿主侧清单

### 1. buffer 实现：用 rope，行操作必须 O(log n)

`VimBuffer` 的 `offset_to_line` / `line_range` / `char_at` 会被引擎高频
调用。ropey（demo 用的）提供 O(log n) 的 `byte_to_line` / `line_to_byte`。
**不要**用 `String` + `split('\n')` 实现 `offset_to_line`——那是
O(offset) 每次调用，在大文件上一次按键就是毫秒级（基准里的朴素实现
实测 219μs/键，ropey 下是 ~5μs）。

`slice` 返回 String 的实现允许分配（引擎的整缓冲扫描依赖它），但
行级小 slice 的调用频率更高，ropey 的 byte→char 转换路径已经很高效。

### 2. undo 快照

`begin_undo_group` 里拍快照。ropey clone 便宜，直接存 `Rope`。
如果宿主用 `String`/`Vec<u8>` 存文本，每次快照是 O(文件) 的拷贝——
insert 模式下每个 undo 组只快照一次（引擎保证），尚可接受；但更大的
问题是 redo 栈同样翻倍。建议直接用 ropey。

### 3. 文本渲染：shaped line 缓存（生产化的最大一项）

文本整形（font shaping）是渲染最贵的步骤。`gpui_vim::render::paint_vim_line`
返回 `gpui::ShapedLine`——**缓存它**：

- 键：行号 + 文本内容代次（或行内容 hash）
- 失效：收到引擎编辑后，只失效编辑行区间内的缓存行（引擎的
  `edit_*` 语义保证其余行的字节偏移由 marks/匹配平移处理，行内容不变）
- 数量：只缓存可见行（viewport 内 + 少量余量），滚动时淘汰

demo（`gpui-vim-demo/src/editor.rs`）为简单起见每帧重新 shape 且
`shaped_lines` 缓存不淘汰——**这是参考实现的刻意简化，生产宿主不要照抄**。

### 4. 重绘粒度

编辑后调用一次 `cx.notify()` 即可让 gpui 重绘可见区（uniform_list
只渲染可见行，天然按视口裁剪）。避免在高频事件（鼠标移动、滚动）里
做全量状态重算。IME 合成期间的 `replace_and_mark_text_in_range` 每个
音节都会触发 notify——这是预期行为，不要在路径上加额外工作。

### 5. hlsearch 重扫节流（大文件必须）

文件超过 ~2MB 或搜索高亮匹配数很大时，开启 hlsearch 的默认"每次编辑
全文件重扫"会成为按键延迟的主要来源。引擎提供开关：

```rust
// 启动时（或 :edit 大文件时）：
vim.set_hlsearch_live_update(false);

// 宿主侧节流：编辑后 150ms 无新编辑时刷新一次
// （用你自己的 debounce/timer；引擎没有时钟）
if idle_for(150.ms()) {
    let (vim, buf, host) = editor.vim_parts();
    let mut ctx = Ctx { buf, host };
    vim.refresh_highlights(&mut ctx);
}
```

`refresh_highlights` 内部就是 `republish_search`：全文件扫描 +
`set_search_highlights` 发布。节流期间高亮短暂滞后是可接受的
（vim 对大文件的表现类似）。

### 6. 多 buffer

每个 buffer 一个独立 `VimState` + `VimBuffer` + 宿主状态（见 demo 的
`BufferTab`）。注意：**宿主级映射（如 gt/gT）必须装到每个 tab 的引擎上**，
只装 active tab 的话切换后失效。未聚焦 tab 的引擎零开销（无定时器、
无后台任务）。

### 7. 缓存卫生

凡是"按行缓存渲染产物"的结构都要有淘汰策略（demo 的 `shaped_lines`
没有，是已知简化）。按可见范围 + LRU 或直接在 uniform_list 回调里
重建都行。

## 什么时候不需要担心

- 文件 < 1MB：默认配置下所有路径都无感，上面的清单只有第 1、3 条
  值得做（buffer 用 ropey + shape 缓存）
- 引擎不启动定时器、不做后台任务、不做 IO；未聚焦的引擎实例零开销
- 配置文件（`~/.gpui-vimrc`）只在启动时读一次
