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
| `n` 连跳（900KB，1 万匹配，ropey buffer） | **3.4 μs/键**（缓存未命中时 ~308 μs） | 匹配列表按编辑代次缓存 |

**唯一随文件大小线性增长的按键路径**是 hlsearch 高亮重扫：开启
`hlsearch` 且有搜索词时，每次编辑（insert 打字的每个字符）都会全文件
正则重扫。900KB 时 0.3ms/键无感；10MB 约 3ms/键开始可感；更大文件需要
节流（见宿主清单第 5 条）。

`n`/`N` 不在其中：匹配列表带编辑代次缓存，任何编辑（`edit_*` 三个
入口）或引擎驱动的 undo/redo、`:set` 都会失效它。缓存命中时连续
`n` 只走列表，不重扫——优化前每次 `n` 全文件重扫约 308 μs，命中后
3.4 μs（约 90 倍）。空列表永不信任（`:noh` 清空后 `n` 仍要能跳转并
重新点亮高亮），所以空结果场景每次 `n` 仍会重扫。

引擎语义保证（宿主可以依赖）：

- 每个逻辑命令 = 一个 undo 组；`VimHost::begin_undo_group` 时快照。
  ropey 的 `Rope` clone 是 O(log n) 树克隆——**用 ropey 做 undo 快照是便宜的**
- 引擎把所有 buffer 变更收敛到 `edit_insert/edit_delete/edit_replace`
  三个入口（`VimState` 内部），marks、可视选区、搜索高亮随之平移
- 搜索匹配上限 10,000 条（防病态正则拖垮渲染）
- `n`/`N` 的匹配列表按编辑代次缓存（`SearchState::matches_generation`），
  宿主只需保证 buffer 变更全部经由引擎（undo/redo 由引擎发起，已覆盖）

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

### 3. 文本渲染：shape 有 gpui 内部缓存，剩余成本在每帧的小额分配

文本整形（font shaping）是渲染最贵的步骤。gpui（0.2 起）的
`LineLayoutCache` 已经跨帧缓存 layout，键是（text、font_size、run 切分），
所以同一行在内容与样式不变时不会重复整形；光标移动/闪烁只影响光标
所在那一行的 run 切分，成本可忽略。宿主层再缓存 `ShapedLine` 的收益
主要在绕开每次调用的键哈希与 `ShapedLine` 构造，属可选优化。

真正值得做的是把**每行每帧**的固定开销压住（demo 已照此实现，可照抄）：

- `paint_vim_line` 把文本按所有权移交 `shape_line`（gpui 的
  `SharedString` clone 是堆拷贝，一行一帧一次已经足够）
- paint 回调里 `shaped` 直接 move 进命中测试缓存，不再 clone
- 搜索高亮按可见行区间**每帧预过滤一次**（`sync_visible_state`），
  不要把 1 万条高亮交给 `compute_line_overlays` 对每条可见行做 clamp
- 行几何缓存（`shaped_lines`）按可见范围每帧淘汰（见第 7 条）

### 4. 重绘粒度与 viewport 同步

编辑后调用一次 `cx.notify()` 即可让 gpui 重绘可见区（uniform_list
只渲染可见行，天然按视口裁剪）。避免在高频事件（鼠标移动、滚动）里
做全量状态重算。IME 合成期间的 `replace_and_mark_text_in_range` 每个
音节都会触发 notify——这是预期行为，不要在路径上加额外工作。

**滚动 motion 依赖宿主回报 viewport**：`C-d`/`C-f`/`C-b`/`H`/`M`/`L`
的步长与定位读 `VimHost::viewport()`。宿主必须在滚动/尺寸变化时把
实际可见行区间同步进去（demo 在 uniform_list 回调里随 `visible_lines`
一起写 `host.viewport`），否则这些键按默认值 (0, 24) 滚动，窗口越高
错得越多。

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

凡是"按行缓存渲染产物"的结构都要有淘汰策略。demo 的 `shaped_lines`
在每帧的 uniform_list 回调里 retain 到可见区间——滚动大文件时缓存
大小恒定在"一屏 + 漂移"，不会随行数增长。

## 什么时候不需要担心

- 文件 < 1MB：默认配置下所有路径都无感，上面的清单只有第 1、3 条
  值得做（buffer 用 ropey + shape 缓存）
- 引擎不启动定时器、不做后台任务、不做 IO；未聚焦的引擎实例零开销
- 配置文件（`~/.gpui-vimrc`）只在启动时读一次
