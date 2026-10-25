//! The `:` / `/` command-line bar for [`crate::edit::VimEdit`] and
//! [`crate::pager::Pager`] hosts.
//!
//! The ENGINE owns the prompt state machine (line editing, history,
//! `<C-r>` pastes, execution — `vim.cmdline`); this module renders it and
//! adds the one piece the engine leaves to the UI: Tab completion
//! (`:h cmdline-completion`). Candidates come from
//! [`VimState::ex_completions`](vimcore::state::VimState::ex_completions)
//! (host-registered commands, plus their argument completions once the
//! command word is finished) merged with a host-supplied built-in name
//! list — one source of truth per side, rendered here.
//!
//! Tab behavior (vim's `wildmode=list:longest` + menu cycling):
//!
//! 1. first `Tab` inserts the longest common prefix of the candidates;
//! 2. when that no longer narrows (or a second `Tab` lands), the bar
//!    cycles the full candidates — `Tab` forward, `S-Tab` backward;
//! 3. any edit restarts from 1.

use vimcore::mode::Mode;
use vimcore::state::VimState;

/// Wildmenu state for one open prompt (the host keeps one per editor and
/// resets it when the prompt closes).
#[derive(Default)]
pub struct CmdlineEdit {
    /// The candidates the last Tab computed (rendered as the wildmenu).
    pub candidates: Vec<String>,
    /// Cycling position inside `candidates` (`None` = prefix-insert stage).
    pub selected: Option<usize>,
    /// A Tab showed the candidates without narrowing (vim: the first Tab
    /// lists, the second picks). The next Tab starts cycling.
    primed: bool,
    /// The word the candidates were computed for — a different word (any
    /// edit) restarts from the prefix stage.
    word: String,
}

/// The completion word span of a `:` line: `(word_start, word_end)` in
/// bytes, where the word is the first whitespace-delimited token. An empty
/// rest means "the command name is still being typed".
fn word_span(line: &str) -> (usize, usize) {
    let start = 0;
    let end = line.find(char::is_whitespace).unwrap_or(line.len());
    (start, end)
}

/// One Tab/S-Tab press. `line`/`cursor` are the engine cmdline buffer and
/// its editing cursor; `extra` is the host's built-in command-name list
/// merged with the engine's host-command candidates. Returns the new
/// `(line, cursor)` when the key was consumed, `None` when there is
/// nothing to complete (the caller lets the key fall through).
pub fn apply_tab(
    vim: &VimState,
    state: &mut CmdlineEdit,
    shift: bool,
    extra_builtin_names: &[&str],
) -> Option<(String, usize)> {
    let Mode::CommandLine { prompt: ':' } = vim.mode() else {
        return None;
    };
    let buffer = &vim.cmdline.buffer;
    let cursor = vim.cmdline.cursor.min(buffer.len());
    // complete only the text BEFORE the cursor (vim: the cursor ends the
    // word being completed); a cursor mid-line completes the prefix
    let line = &buffer[..cursor];
    let (word_start, word_end) = word_span(line);
    let word = &line[word_start..word_end];
    let had_space = word_end < line.len();
    let arg = line[word_end..].trim_start();

    // Cycling FROZE its candidate list at the moment it started; the only
    // word changes during cycling are the choices themselves (the buffer
    // holds the selected candidate between Tabs). An edit to anything else
    // restarts from the prefix stage.
    let frozen = state.selected.is_some() && state.candidates.iter().any(|c| c == word);
    if !frozen && state.word != word {
        state.selected = None;
        state.primed = false;
        state.word = word.to_string();
    }

    // candidates: the engine's registry (names or argument completion,
    // depending on the stage) merged with the host's built-in names
    let candidates = if frozen {
        state.candidates.clone()
    } else {
        let mut c = vim.ex_completions(line);
        if !had_space {
            c.extend(
                extra_builtin_names
                    .iter()
                    .filter(|n| n.starts_with(word))
                    .map(|n| n.to_string()),
            );
            c.sort();
            c.dedup();
        }
        c
    };
    if candidates.is_empty() {
        state.candidates.clear();
        state.selected = None;
        return None;
    }
    state.candidates = candidates.clone();

    // stage 1: insert the longest common prefix; a completed single
    // argument takes vim's trailing separator
    if state.selected.is_none() {
        let lcp = longest_common_prefix(&candidates);
        let narrows = if had_space {
            !lcp.is_empty() && lcp.len() > arg.len()
        } else {
            !lcp.is_empty() && lcp.len() > word.len()
        };
        if narrows {
            let single = candidates.len() == 1;
            let new_line = match (had_space, single) {
                (true, true) => format!("{} {} ", &line[..word_end], lcp),
                (true, false) => format!("{} {}", &line[..word_end], lcp),
                (false, _) => format!("{}{}", &line[..word_start], lcp),
            };
            let cursor = new_line.len();
            return Some((new_line, cursor));
        }
        // the prefix no longer narrows: the first Tab just LISTS (vim), the
        // next one starts cycling
        if !state.primed {
            state.primed = true;
            return None;
        }
    }

    // stage 2: cycle the full candidates
    let index = match state.selected {
        Some(i) if shift => (i + candidates.len() - 1) % candidates.len(),
        Some(i) => (i + 1) % candidates.len(),
        None => 0,
    };
    state.selected = Some(index);
    let choice = candidates[index].clone();
    // the choice REPLACED the word in the buffer — track it, or the next
    // Tab would see a "different word" and reset the cycling
    state.word = choice.clone();
    // command-name cycling replaces the word; argument cycling (the word
    // is complete — had_space) replaces the argument text and appends the
    // trailing separator, like vim's completed argument
    let new_line = if had_space {
        format!("{}{} ", &line[..word_end], choice)
    } else {
        format!("{}{}", &line[..word_start], choice)
    };
    let cursor = new_line.len();
    Some((new_line, cursor))
}

pub fn longest_common_prefix(items: &[String]) -> String {
    let Some(first) = items.first() else {
        return String::new();
    };
    let mut end = first.len();
    for other in &items[1..] {
        let shared = first
            .chars()
            .zip(other.chars())
            .take_while(|(a, b)| a == b)
            .map(|(a, _)| a.len_utf8())
            .sum::<usize>();
        end = end.min(shared);
        if end == 0 {
            return String::new();
        }
    }
    first[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vim_with_commands() -> VimState {
        let mut vim = VimState::new();
        vim.register_ex("hunk-take", &["ht"], None, std::rc::Rc::new(|_, _, _| true));
        vim.register_ex(
            "hunk-next",
            &[],
            Some(std::rc::Rc::new(|arg: &str| {
                ["left", "right"]
                    .iter()
                    .map(|s| s.to_string())
                    .filter(|s| s.starts_with(arg))
                    .collect::<Vec<_>>()
            })),
            std::rc::Rc::new(|_, _, _| true),
        );
        vim
    }

    fn open_colon(vim: &mut VimState, line: &str) {
        use vimcore::state::Ctx;
        let mut buf = vimcore_tests::Buf(vimcore_tests::Shared::default());
        let mut host = vimcore_tests::Host;
        let mut ctx = Ctx {
            buf: &mut buf,
            host: &mut host,
        };
        vim.handle_key(&mut ctx, vimcore::key::Key::char(':'));
        for c in line.chars() {
            vim.handle_key(&mut ctx, vimcore::key::Key::char(c));
        }
    }

    mod vimcore_tests {
        // A minimal buffer/host pair so the tests can drive the engine
        // without the shared harness (this crate's tests are standalone).
        use std::cell::RefCell;
        use std::ops::Range;
        use std::rc::Rc;
        use vimcore::buffer::{VimBuffer, VimBufferMut};
        use vimcore::host::VimHost;

        pub struct Shared(pub Rc<RefCell<String>>);
        impl Default for Shared {
            fn default() -> Self {
                Shared(Rc::new(RefCell::new(String::new())))
            }
        }
        pub struct Buf(pub Shared);
        impl VimBuffer for Buf {
            fn len(&self) -> usize {
                self.0 .0.borrow().len()
            }
            fn line_count(&self) -> usize {
                1
            }
            fn char_at(&self, offset: usize) -> Option<char> {
                self.0 .0.borrow()[offset..].chars().next()
            }
            fn prev_char_offset(&self, offset: usize) -> Option<usize> {
                let text = self.0 .0.borrow();
                text[..offset]
                    .chars()
                    .next_back()
                    .map(|c| offset - c.len_utf8())
            }
            fn line_range(&self, line: usize) -> Range<usize> {
                let len = self.0 .0.borrow().len();
                line..line.min(len)
            }
            fn offset_to_line(&self, _offset: usize) -> usize {
                0
            }
            fn slice(&self, range: Range<usize>) -> String {
                self.0 .0.borrow()[range].to_string()
            }
        }
        impl VimBufferMut for Buf {
            fn insert_text(&mut self, offset: usize, text: &str) -> bool {
                self.0 .0.borrow_mut().insert_str(offset, text);
                true
            }
            fn delete_range(&mut self, range: Range<usize>) -> bool {
                self.0 .0.borrow_mut().replace_range(range, "");
                true
            }
        }
        pub struct Host;
        impl VimHost for Host {
            fn viewport(&self) -> (usize, usize) {
                (0, 9)
            }
            fn begin_undo_group(&mut self, _id: u64, _cursor_offset: usize) {}
            fn scroll_to_line(&mut self, _line: usize) {}
            fn clipboard_write(&mut self, _text: &str) {}
            fn clipboard_read(&self) -> Option<String> {
                None
            }
            fn set_search_highlights(
                &mut self,
                _matches: &[Range<usize>],
                _current: Option<Range<usize>>,
            ) {
            }
            fn undo(&mut self) -> Option<usize> {
                None
            }
            fn redo(&mut self) -> Option<usize> {
                None
            }
        }
    }

    #[test]
    fn first_tab_lists_then_second_tab_picks() {
        let mut vim = vim_with_commands();
        open_colon(&mut vim, "hunk-");
        let mut state = CmdlineEdit::default();
        // "hunk-" already IS the longest common prefix of hunk-take /
        // hunk-next: the first Tab just lists (line unchanged, not consumed)
        assert!(apply_tab(&vim, &mut state, false, &[]).is_none());
        assert_eq!(state.candidates.len(), 2);
        assert_eq!(state.selected, None);
        // the second Tab starts cycling at the first candidate (sorted)
        let (line, _) = apply_tab(&vim, &mut state, false, &[]).unwrap();
        assert_eq!(line, "hunk-next");
    }

    #[test]
    fn unambiguous_prefix_completes_in_one_tab() {
        let mut vim = vim_with_commands();
        open_colon(&mut vim, "hunk-ta");
        let mut state = CmdlineEdit::default();
        let (line, _) = apply_tab(&vim, &mut state, false, &[]).unwrap();
        assert_eq!(line, "hunk-take");
    }

    #[test]
    fn tab_cycles_and_shift_tab_goes_back() {
        let mut vim = vim_with_commands();
        open_colon(&mut vim, "hunk-");
        let mut state = CmdlineEdit::default();
        let _ = apply_tab(&vim, &mut state, false, &[]); // list
        let (first, _) = apply_tab(&vim, &mut state, false, &[]).unwrap();
        assert_eq!(first, "hunk-next");
        vim.cmdline.buffer = first;
        vim.cmdline.cursor = vim.cmdline.buffer.len();
        let (second, _) = apply_tab(&vim, &mut state, false, &[]).unwrap();
        assert_eq!(second, "hunk-take");
        vim.cmdline.buffer = second;
        vim.cmdline.cursor = vim.cmdline.buffer.len();
        let (back, _) = apply_tab(&vim, &mut state, true, &[]).unwrap();
        assert_eq!(back, "hunk-next");
    }

    #[test]
    fn argument_completion_after_space() {
        let mut vim = vim_with_commands();
        open_colon(&mut vim, "hunk-next l");
        let mut state = CmdlineEdit::default();
        let (line, _) = apply_tab(&vim, &mut state, false, &[]).unwrap();
        assert_eq!(line, "hunk-next left ");
    }

    #[test]
    fn builtin_names_join_name_completion() {
        let mut vim = vim_with_commands();
        open_colon(&mut vim, "w");
        let mut state = CmdlineEdit::default();
        // lcp("w", "write", "wq") == "w": the first Tab lists, the second
        // cycles
        assert!(apply_tab(&vim, &mut state, false, &["write", "wq"]).is_none());
        assert!(state.candidates.contains(&"write".to_string()));
        assert!(state.candidates.contains(&"wq".to_string()));
        let (line, _) = apply_tab(&vim, &mut state, false, &["write", "wq"]).unwrap();
        assert_eq!(line, "wq", "sorted candidates cycle from the first");
    }

    #[test]
    fn no_candidates_is_not_consumed() {
        let mut vim = vim_with_commands();
        open_colon(&mut vim, "zzz");
        let mut state = CmdlineEdit::default();
        assert!(apply_tab(&vim, &mut state, false, &[]).is_none());
    }

    #[test]
    fn lcp_basics() {
        assert_eq!(longest_common_prefix(&["abc".into(), "abd".into()]), "ab");
        assert_eq!(longest_common_prefix(&["x".into(), "y".into()]), "");
        assert_eq!(longest_common_prefix(&["only".into()]), "only");
    }
}

// ---------- rendering ----------

use gpui::prelude::*;
use gpui::{div, px, rgb, SharedString};

/// Visual parameters for the command-line bar (same construction pattern
/// as [`crate::edit::VimEditStyle`]: hosts build it from their theme).
#[derive(Debug, Clone, PartialEq)]
pub struct CmdlineStyle {
    pub mono_font: String,
    pub font_size: f32,
    pub line_height: f32,
    pub char_width: f32,
    pub bg: u32,
    pub fg: u32,
    /// The prompt character (`:` / `/` / `?`).
    pub prompt: u32,
    /// The completion cursor block and the selected wildmenu entry.
    pub accent: u32,
    /// Unselected wildmenu entries.
    pub muted: u32,
}

impl Default for CmdlineStyle {
    fn default() -> Self {
        Self {
            mono_font: "Menlo".to_string(),
            font_size: 13.0,
            line_height: 20.0,
            char_width: 7.6,
            bg: 0x1f2335,
            fg: 0xc0caf5,
            prompt: 0x7aa2f7,
            accent: 0x7aa2f7,
            muted: 0x565f89,
        }
    }
}

/// Render the engine's open prompt (prompt char + buffer + block cursor at
/// the editing position) with the wildmenu row below when a Tab has
/// candidates. Returns `None` when no prompt is open — hosts typically
/// reserve no space for it in that case.
pub fn render_cmdline(
    vim: &VimState,
    state: &CmdlineEdit,
    style: &CmdlineStyle,
) -> Option<gpui::Div> {
    let Mode::CommandLine { prompt } = vim.mode() else {
        return None;
    };
    let mono = SharedString::from(style.mono_font.clone());
    let font_size = px(style.font_size);
    let line_h = px(style.line_height);

    // the buffer with the editing cursor as a block over the char at
    // cmdline.cursor (past the end = a one-space block)
    let buffer = &vim.cmdline.buffer;
    let cursor_char = vim.cmdline.cursor.min(buffer.len());
    let before: String = buffer[..cursor_char].to_string();
    let under: String = buffer[cursor_char..]
        .chars()
        .next()
        .map(|c| c.to_string())
        .unwrap_or_default();
    let after: String = if under.is_empty() {
        String::new()
    } else {
        buffer[cursor_char + under.len()..].to_string()
    };

    let mut bar = div()
        .flex()
        .flex_col()
        .w_full()
        .px_2()
        .py_1()
        .bg(rgb(style.bg))
        .font_family(mono.clone())
        .text_size(font_size)
        .line_height(line_h)
        .child(
            div()
                .flex()
                .flex_row()
                .gap_1()
                .child(
                    div()
                        .text_color(rgb(style.prompt))
                        .child(SharedString::from(prompt.to_string())),
                )
                .child(
                    div()
                        .text_color(rgb(style.fg))
                        .child(SharedString::from(before)),
                )
                .map(|d| {
                    if under.is_empty() {
                        d.child(
                            div()
                                .w(px(style.char_width))
                                .h(line_h)
                                .bg(rgb(style.accent)),
                        )
                    } else {
                        d.child(
                            div()
                                .bg(rgb(style.accent))
                                .text_color(rgb(style.bg))
                                .child(SharedString::from(under)),
                        )
                    }
                })
                .child(
                    div()
                        .text_color(rgb(style.fg))
                        .child(SharedString::from(after)),
                ),
        );

    if !state.candidates.is_empty() {
        bar = bar.child(div().flex().flex_row().flex_wrap().gap_2().children(
            state.candidates.iter().enumerate().map(|(i, name)| {
                let selected = state.selected == Some(i);
                div()
                    .text_color(rgb(if selected { style.accent } else { style.muted }))
                    .when(selected, |d| d.underline())
                    .child(SharedString::from(name.clone()))
            }),
        ));
    }
    Some(bar)
}
