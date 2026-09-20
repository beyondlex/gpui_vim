//! Marks: user marks `a-z`, special marks (` ^ . < >), and jump helpers.

use std::collections::HashMap;
use std::ops::Range;

/// Local (buffer) marks.
#[derive(Clone, Debug, Default)]
pub struct Marks {
    offsets: HashMap<char, usize>,
    /// Range of the last visual selection (`< .. >`).
    pub last_visual: Option<(usize, usize)>,
    /// Live `(anchor, cursor_end)` of the current visual selection, kept in
    /// sync by the engine (used for `'<`/`'>` inside visual mode).
    pub(crate) active_visual: Option<(usize, usize)>,
    /// Position of the last change (`.`).
    pub last_change: Option<usize>,
    /// Position where the last insert session ended (`^`).
    pub last_insert_exit: Option<usize>,
}

impl Marks {
    pub fn get(&self, name: char) -> Option<usize> {
        self.offsets.get(&name).copied()
    }

    /// All set marks as `(name, offset)`, sorted by name — for `:marks`
    /// style listings (the map itself stays private).
    pub fn items(&self) -> Vec<(char, usize)> {
        let mut v: Vec<(char, usize)> = self.offsets.iter().map(|(c, o)| (*c, *o)).collect();
        v.sort();
        v
    }

    pub fn set(&mut self, name: char, offset: usize) {
        if name.is_ascii_alphabetic() || matches!(name, '^' | '.') {
            self.offsets.insert(name, offset);
        }
    }

    /// Live bounds of the ACTIVE visual selection (anchor..cursor+1), if any.
    pub fn active_visual(&self) -> Option<(usize, usize)> {
        // set by the engine each time the selection changes; keeps resolve()
        // valid inside visual mode, before '< '/'> are written on exit
        self.active_visual
    }

    /// Resolve special names used by `` ` ``/`'` jumps.
    pub fn resolve(&self, name: char) -> Option<usize> {
        match name {
            '<' => self.last_visual.map(|(a, _)| a),
            '>' => self.last_visual.map(|(_, b)| b),
            _ => self.get(name),
        }
    }

    /// Shift every stored mark after an insertion of `len` bytes at `at`.
    /// A mark exactly AT `at` stays: vim keeps it before the inserted text.
    pub fn adjust_insert(&mut self, at: usize, len: usize) {
        if len == 0 {
            return;
        }
        self.for_each_pos(|pos| {
            if *pos > at {
                *pos += len;
            }
        });
    }

    /// Shift marks around a deletion of `range`: marks past the end move
    /// back by its length, marks strictly inside land on the range start
    /// (a mark exactly at the start stays there).
    pub fn adjust_delete(&mut self, range: Range<usize>) {
        if range.start >= range.end {
            return;
        }
        let len = range.end - range.start;
        self.for_each_pos(|pos| {
            if *pos >= range.end {
                *pos -= len;
            } else if *pos > range.start {
                *pos = range.start;
            }
        });
    }

    /// Marks around a replacement (`gu`/`~`/`:s`): past-the-end marks shift
    /// by the length delta, marks inside the replaced range land on its start.
    pub fn adjust_replace(&mut self, range: Range<usize>, new_len: usize) {
        if range.start >= range.end {
            return;
        }
        let delta = new_len as isize - (range.end - range.start) as isize;
        // case operators replace with equal-length text: keep inner marks at
        // their relative position, like vim's column-preserving adjustment
        let preserve_inner = new_len == range.end - range.start;
        self.for_each_pos(move |pos| {
            if *pos >= range.end {
                *pos = (*pos as isize + delta).max(0) as usize;
            } else if *pos > range.start && !preserve_inner {
                *pos = range.start;
            }
        });
    }

    fn for_each_pos(&mut self, mut f: impl FnMut(&mut usize)) {
        for pos in self.offsets.values_mut() {
            f(pos);
        }
        if let Some((a, b)) = self.last_visual.as_mut() {
            f(a);
            f(b);
        }
        if let Some(p) = self.last_change.as_mut() {
            f(p);
        }
        if let Some(p) = self.last_insert_exit.as_mut() {
            f(p);
        }
    }
}
