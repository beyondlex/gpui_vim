//! Host contract tests (TCK)——INTEGRATION.md 坑位清单的可执行形态。
//!
//! 每个函数是一条宿主必须满足的契约。宿主在自己的 buffer / host 实现上
//! 调用它们（通常在 `#[cfg(test)]` 里 `.unwrap()`），把「接入质量靠读文档」
//! 变成「接入质量有测试下限」。全部纯 headless：不依赖 gpui、不开窗口。
//!
//! 典型用法（宿主仓库）：
//!
//! ```ignore
//! #[test]
//! fn my_buffer_meets_engine_contract() {
//!     let mut buf = MyBuffer::default();
//!     vim_core::tck::buffer_edit_contract(&mut buf).unwrap();
//!     let mut host = MyHost::default();
//!     vim_core::tck::engine_smoke_contract(&mut buf, &mut host).unwrap();
//! }
//! ```
//!
//! 刻意不覆盖的部分：undo/redo（宿主语义各异——PandaGit 走应用级统一撤销
//! 栈，`undo()` 合法地返回 None）、剪贴板、视口滚动。这些由宿主自己的测试
//! 与 [`TckHost`] 的存在边界共同划出。

use std::ops::Range;

use crate::buffer::{VimBuffer, VimBufferMut};
use crate::host::VimHost;
use crate::key::Key;
use crate::state::{Ctx, VimState};

/// 只读契约：`VimBuffer` 的行语义与字符边界。
///
/// 对宿主缓冲的**当前内容**逐条校验；建议用含 CJK、多字节字符与空行的
/// 多行内容运行。校验项：
///
/// - `line_count() >= 1`（空缓冲也有恰好 1 行）；
/// - `line_range(i)` 含终止 `\n`（非末行），末行延伸到缓冲区尾——引擎的
///   linewise 操作（dd/yy/V-line）依赖这一点，缺 `\n` 会在原位留空行；
/// - `slice(range).len() == range.len()`；
/// - `offset_to_line(line_start(i)) == i`；
/// - `char_at` 在每个字符边界返回 `Some`、非边界/越界返回 `None`；
/// - `prev_char_offset` 从 `len` 逐步回链到 0，步数恰等于字符数。
pub fn buffer_read_contract(buf: &(impl VimBuffer + ?Sized)) -> Result<(), String> {
    let len = buf.len();
    let lines = buf.line_count();
    if lines == 0 {
        return Err("line_count() == 0：空缓冲也必须有恰好 1 行".into());
    }
    for line in 0..lines {
        let range = buf.line_range(line);
        if range.start > range.end || range.end > len {
            return Err(format!("line {line}: line_range {range:?} 越界（len={len}）"));
        }
        let text = buf.slice(range.clone());
        if text.len() != range.len() {
            return Err(format!(
                "line {line}: slice(range).len() = {} ≠ range.len() = {}",
                text.len(),
                range.len()
            ));
        }
        if line + 1 < lines {
            if !text.ends_with('\n') {
                return Err(format!(
                    "line {line}: 非末行的 line_range 必须包含终止 \\n（引擎的 dd/yy 依赖它）"
                ));
            }
        } else if range.end != len {
            return Err(format!(
                "line {line}: 末行 range 必须延伸到缓冲区尾（end={} ≠ len={len}）",
                range.end
            ));
        }
        if buf.offset_to_line(range.start) != line {
            return Err(format!(
                "line {line}: offset_to_line(行首={}) = {}",
                range.start,
                buf.offset_to_line(range.start)
            ));
        }
    }
    let full = buf.slice(0..len);
    if full.len() != len {
        return Err(format!("slice(0..len).len() = {} ≠ len = {len}", full.len()));
    }
    let mut o = 0;
    while o < len {
        let Some(c) = buf.char_at(o) else {
            return Err(format!("char_at({o}) = None：字符边界上必须返回 Some"));
        };
        o += c.len_utf8();
    }
    if len > 0 {
        if buf.char_at(len).is_some() {
            return Err("char_at(len) 必须是 None".into());
        }
        if buf.prev_char_offset(0).is_some() {
            return Err("prev_char_offset(0) 必须是 None".into());
        }
        let chars = full.chars().count();
        let mut o = len;
        let mut steps = 0;
        while let Some(prev) = buf.prev_char_offset(o) {
            steps += 1;
            o = prev;
            if steps > chars {
                return Err("prev_char_offset 链不收敛（步数超过字符数）".into());
            }
        }
        if o != 0 || steps != chars {
            return Err(format!(
                "prev_char_offset 链终点 {o}（应为 0）或步数 {steps} ≠ 字符数 {chars}"
            ));
        }
    }
    Ok(())
}

/// 可变契约：`VimBufferMut` 两种编辑原语的 UTF-8 安全性。
///
/// 要求传入**空缓冲**；每步编辑之后只读契约必须仍然成立。序列覆盖：
/// 末尾多字节插入、行首插入拆行、删除换行、清空。
pub fn buffer_edit_contract(buf: &mut impl VimBufferMut) -> Result<(), String> {
    if !buf.is_empty() {
        return Err("buffer_edit_contract 要求空缓冲".into());
    }
    buf.insert_text(0, "ab");
    assert_content(buf, "ab")?;
    buf.insert_text(buf.len(), "中"); // 末尾多字节插入
    assert_content(buf, "ab中")?;
    buf.insert_text(0, "\n"); // 行首插入拆行
    assert_content(buf, "\nab中")?;
    if buf.line_count() != 2 {
        return Err(format!("line_count() = {} ≠ 2", buf.line_count()));
    }
    if buf.offset_to_line(1) != 1 {
        return Err(format!(
            "offset_to_line(1) = {} ≠ 1（\\n 之后的字节属于第 1 行）",
            buf.offset_to_line(1)
        ));
    }
    buf.delete_range(0..1); // 只删换行
    assert_content(buf, "ab中")?;
    buf.delete_range(0..buf.len()); // 清空
    if !buf.is_empty() || buf.line_count() != 1 {
        return Err(format!(
            "清空后 len={} line_count={}（应为 0 与 1）",
            buf.len(),
            buf.line_count()
        ));
    }
    Ok(())
}

/// 引擎冒烟契约：用引擎按键管线在宿主 buffer 上跑一段确定性编辑，每步
/// 校验全文与只读契约。
///
/// 要求传入**空缓冲**（宿主的 host 语义各异时可用 [`TckHost`]）。序列：
/// `ggx`（删除）、`$`（行尾光标）、`Vjd`（可视行删除）、`p`（linewise
/// 粘贴）、`gg$A` + 文本 + Esc（行尾追加）。刻意不含 `u`/`C-r`——见模块头。
pub fn engine_smoke_contract(
    buf: &mut impl VimBufferMut,
    host: &mut dyn VimHost,
) -> Result<(), String> {
    if !buf.is_empty() {
        return Err("engine_smoke_contract 要求空缓冲".into());
    }
    buf.insert_text(0, "alpha\nbeta\ngamma\n");
    let mut vim = VimState::new();

    feed(&mut vim, buf, host, "gg");
    assert_content(buf, "alpha\nbeta\ngamma\n")?;
    feed(&mut vim, buf, host, "x");
    assert_content(buf, "lpha\nbeta\ngamma\n")?;
    feed(&mut vim, buf, host, "$");
    let want = buf.line_end(0).saturating_sub(1);
    if vim.cursor_offset() != want {
        return Err(format!("$ 后 cursor_offset() = {} ≠ 行尾字符 {want}", vim.cursor_offset()));
    }
    feed(&mut vim, buf, host, "Vjd");
    assert_content(buf, "gamma\n")?;
    feed(&mut vim, buf, host, "p");
    assert_content(buf, "gamma\nlpha\nbeta\n")?;
    feed(&mut vim, buf, host, "gg$A");
    type_text(&mut vim, buf, host, "x");
    {
        let mut ctx = Ctx { buf, host };
        vim.handle_key(&mut ctx, Key::escape());
    }
    assert_content(buf, "gammax\nlpha\nbeta\n")?;
    Ok(())
}

/// 无副作用宿主：只想让引擎驱动 buffer 时使用。宿主自身的 undo、剪贴板、
/// 高亮等语义不属于 TCK 范围，由宿主自己的测试覆盖。
#[derive(Default)]
pub struct TckHost;

impl VimHost for TckHost {
    fn viewport(&self) -> (usize, usize) {
        (0, 24)
    }
    fn scroll_to_line(&mut self, _: usize) {}
    fn clipboard_write(&mut self, _: &str) {}
    fn clipboard_read(&self) -> Option<String> {
        None
    }
    fn set_search_highlights(&mut self, _: &[Range<usize>], _: Option<Range<usize>>) {}
    fn begin_undo_group(&mut self, _: u64, _: usize) {}
    fn undo(&mut self) -> Option<usize> {
        None
    }
    fn redo(&mut self) -> Option<usize> {
        None
    }
    fn changed(&mut self) {}
}

fn feed(vim: &mut VimState, buf: &mut dyn VimBufferMut, host: &mut dyn VimHost, keys: &str) {
    let mut ctx = Ctx { buf, host };
    for c in keys.chars() {
        vim.handle_key(&mut ctx, Key::char(c));
    }
}

/// insert 模式的打字路径：打印字符在 insert 模式刻意返回 `Unknown`
///（IME 契约），文本经 `record_typed_text` + `insert_text_at_cursor` 直落
/// buffer——与集成层 `place_text` 同款。
fn type_text(vim: &mut VimState, buf: &mut dyn VimBufferMut, host: &mut dyn VimHost, text: &str) {
    let mut ctx = Ctx { buf, host };
    vim.record_typed_text(text);
    vim.insert_text_at_cursor(&mut ctx, text);
}

fn assert_content(buf: &impl VimBuffer, want: &str) -> Result<(), String> {
    buffer_read_contract(buf)?;
    let got = buf.slice(0..buf.len());
    if got != want {
        return Err(format!("内容 {got:?} ≠ 期望 {want:?}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// String-backed 合同参考实现（line_range 含终止 \n）。
    #[derive(Default)]
    struct StrBuf(pub String);

    impl VimBuffer for StrBuf {
        fn len(&self) -> usize {
            self.0.len()
        }
        fn line_count(&self) -> usize {
            if self.0.is_empty() {
                return 1;
            }
            let n = self.0.split('\n').count();
            if self.0.ends_with('\n') {
                n - 1
            } else {
                n
            }
        }
        fn char_at(&self, offset: usize) -> Option<char> {
            self.0[offset..].chars().next()
        }
        fn prev_char_offset(&self, offset: usize) -> Option<usize> {
            if offset == 0 || offset > self.0.len() {
                return None;
            }
            self.0[..offset].chars().next_back().map(|c| offset - c.len_utf8())
        }
        fn line_range(&self, line: usize) -> Range<usize> {
            if line >= self.line_count() {
                return self.0.len()..self.0.len();
            }
            let mut start = 0;
            for (i, part) in self.0.split('\n').enumerate() {
                if i == line {
                    // 终止 \n 存在就包含进 range；末行延伸到缓冲区尾
                    let end = if i + 1 < self.0.split('\n').count() {
                        start + part.len() + 1
                    } else {
                        self.0.len()
                    };
                    return start..end;
                }
                start += part.len() + 1;
            }
            unreachable!()
        }
        fn offset_to_line(&self, offset: usize) -> usize {
            self.0[..offset.min(self.0.len())].split('\n').count() - 1
        }
        fn slice(&self, range: Range<usize>) -> String {
            self.0[range].to_owned()
        }
    }

    impl VimBufferMut for StrBuf {
        fn insert_text(&mut self, offset: usize, text: &str) {
            self.0.insert_str(offset, text);
        }
        fn delete_range(&mut self, range: Range<usize>) {
            self.0.replace_range(range, "");
        }
    }

    /// 反例：非末行 range 不含 \n——常见实现错误，TCK 必须抓住。
    struct NoNewlineBuf(pub String);

    impl VimBuffer for NoNewlineBuf {
        fn len(&self) -> usize {
            self.0.len()
        }
        fn line_count(&self) -> usize {
            StrBuf(self.0.clone()).line_count()
        }
        fn char_at(&self, offset: usize) -> Option<char> {
            StrBuf(self.0.clone()).char_at(offset)
        }
        fn prev_char_offset(&self, offset: usize) -> Option<usize> {
            StrBuf(self.0.clone()).prev_char_offset(offset)
        }
        fn line_range(&self, line: usize) -> Range<usize> {
            let mut start = 0;
            for (i, part) in self.0.split('\n').enumerate() {
                if i == line {
                    return start..start + part.len(); // 错误：丢了 \n
                }
                start += part.len() + 1;
            }
            self.0.len()..self.0.len()
        }
        fn offset_to_line(&self, offset: usize) -> usize {
            StrBuf(self.0.clone()).offset_to_line(offset)
        }
        fn slice(&self, range: Range<usize>) -> String {
            self.0[range].to_owned()
        }
    }

    #[test]
    fn read_contract_accepts_reference_impl() {
        let buf = StrBuf("alpha\nbeta 中文\n\n尾行\n".into());
        buffer_read_contract(&buf).unwrap();
        let buf = StrBuf("中文 mix emoji 👨‍👩‍👧".into());
        buffer_read_contract(&buf).unwrap();
        let buf = StrBuf(String::new());
        buffer_read_contract(&buf).unwrap();
    }

    #[test]
    fn read_contract_catches_missing_newline_in_range() {
        let buf = NoNewlineBuf("alpha\nbeta".into());
        assert!(buffer_read_contract(&buf).is_err());
    }

    #[test]
    fn edit_contract_passes_reference_impl() {
        let mut buf = StrBuf::default();
        buffer_edit_contract(&mut buf).unwrap();
    }

    #[test]
    fn engine_smoke_passes_reference_impl() {
        let mut buf = StrBuf::default();
        let mut host = TckHost;
        engine_smoke_contract(&mut buf, &mut host).unwrap();
        assert_eq!(buf.0, "gammax\nlpha\nbeta\n");
    }
}
