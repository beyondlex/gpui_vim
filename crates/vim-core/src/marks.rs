//! Marks: user marks `a-z`, special marks (` ^ . < >), and jump helpers.

use std::collections::HashMap;

/// Local (buffer) marks.
#[derive(Clone, Debug, Default)]
pub struct Marks {
    offsets: HashMap<char, usize>,
    /// Range of the last visual selection (`< .. >`).
    pub last_visual: Option<(usize, usize)>,
    /// Position of the last change (`.`).
    pub last_change: Option<usize>,
    /// Position where the last insert session ended (`^`).
    pub last_insert_exit: Option<usize>,
}

pub const MARK_LIMIT: char = 'z';

impl Marks {
    pub fn get(&self, name: char) -> Option<usize> {
        self.offsets.get(&name).copied()
    }

    pub fn set(&mut self, name: char, offset: usize) {
        if name.is_ascii_alphabetic() || matches!(name, '^' | '.') {
            self.offsets.insert(name, offset);
        }
    }

    /// Resolve special names used by `` ` ``/`'` jumps.
    pub fn resolve(&self, name: char) -> Option<usize> {
        match name {
            '<' => self.last_visual.map(|(a, _)| a),
            '>' => self.last_visual.map(|(_, b)| b),
            _ => self.get(name),
        }
    }
}
