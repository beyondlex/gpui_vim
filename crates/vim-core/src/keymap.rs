//! Keystroke tries and user mappings.
//!
//! One generic prefix trie backs both the built-in command tables (per
//! phase: normal / operator-pending / visual) and user `:map`-style mappings.
//! Walking the trie key by key gives us vim's "pending keys" behavior for
//! free: an intermediate node means "waiting for more keys".

use crate::key::Key;
use std::collections::HashMap;

#[derive(Debug)]
pub struct Trie<T> {
    children: HashMap<Key, Trie<T>>,
    value: Option<T>,
}

impl<T> Default for Trie<T> {
    fn default() -> Self {
        Trie {
            children: HashMap::new(),
            value: None,
        }
    }
}

/// Result of advancing a trie walk by one key.
#[derive(Debug)]
pub enum Walk<'a, T> {
    /// A terminal node was reached.
    Hit(&'a T),
    /// An intermediate node: the engine must wait for more keys.
    Pending,
    /// No child matched.
    Miss,
}

impl<T> Trie<T> {
    pub fn is_empty(&self) -> bool {
        self.children.is_empty() && self.value.is_none()
    }

    pub fn insert(&mut self, keys: &[Key], value: T) {
        let mut node = self;
        for key in keys {
            node = node.children.entry(key.clone()).or_default();
        }
        node.value = Some(value);
    }

    /// Walk from the root following `keys`. Terminal on the last key wins;
    /// anything else is reported per [`Walk`].
    pub fn get(&self, keys: &[Key]) -> Walk<'_, T> {
        let mut node = self;
        for (i, key) in keys.iter().enumerate() {
            match node.children.get(key) {
                Some(child) => node = child,
                None => return Walk::Miss,
            }
            if i + 1 < keys.len() && node.value.is_some() && node.children.is_empty() {
                // dead end before the sequence finished
                return Walk::Miss;
            }
        }
        match &node.value {
            Some(value) => Walk::Hit(value),
            None if node.children.is_empty() => Walk::Miss,
            None => Walk::Pending,
        }
    }

    /// Is `keys` a proper prefix of at least one stored sequence?
    pub fn is_prefix(&self, keys: &[Key]) -> bool {
        let mut node = self;
        for key in keys {
            match node.children.get(key) {
                Some(child) => node = child,
                None => return false,
            }
        }
        !node.children.is_empty()
    }
}

/// Which mapping table applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ModeClass {
    Normal,
    Visual,
    Insert,
}

/// A user mapping's right-hand side. `noremap` mappings expand WITHOUT
/// re-consulting the mapping table (vim's `:noremap` family); `map` mappings
/// re-resolve recursively, bounded by the engine's key guard.
#[derive(Clone, Debug)]
pub struct Mapping {
    pub rhs: Vec<Key>,
    pub noremap: bool,
}

/// User mappings (`:map` / `:noremap` families).
#[derive(Default)]
pub struct Keymaps {
    tables: HashMap<ModeClass, Trie<Mapping>>,
}

impl Keymaps {
    pub fn map(&mut self, class: ModeClass, from: &[Key], to: Vec<Key>, noremap: bool) {
        self.tables.entry(class).or_default().insert(
            from,
            Mapping {
                rhs: to,
                noremap,
            },
        );
    }

    pub fn map_str(&mut self, class: ModeClass, from: &str, to: &str) {
        self.map_str_noremap(class, from, to, false);
    }

    pub fn map_str_noremap(&mut self, class: ModeClass, from: &str, to: &str, noremap: bool) {
        self.map(
            class,
            &crate::key::parse_key_sequence(from),
            crate::key::parse_key_sequence(to),
            noremap,
        );
    }

    pub fn table(&self, class: ModeClass) -> Option<&Trie<Mapping>> {
        self.tables.get(&class)
    }
}

/// Resolution of the pending input queue against the mapping table.
pub enum MappingMatch {
    /// A full mapping matched `used` keys; replay `expansion`.
    Match {
        used: usize,
        expansion: Vec<Key>,
        noremap: bool,
    },
    /// The queue is a proper prefix of a mapping: keep waiting.
    Waiting,
    /// No mapping is involved; process input normally.
    None,
}

pub fn lookup(table: Option<&Trie<Mapping>>, queue: &[Key]) -> MappingMatch {
    let Some(table) = table else {
        return MappingMatch::None;
    };
    if queue.is_empty() {
        return MappingMatch::None;
    }
    match table.get(queue) {
        Walk::Hit(mapping) => MappingMatch::Match {
            used: queue.len(),
            expansion: mapping.rhs.clone(),
            noremap: mapping.noremap,
        },
        Walk::Pending => MappingMatch::Waiting,
        Walk::Miss => {
            // a shorter prefix of the queue may still be waiting (e.g. the
            // queue holds "jk" while "j" is unmapped): only relevant when
            // the *last* keys form a pending prefix — handled by Waiting
            // during incremental feeding, so a miss here is final.
            MappingMatch::None
        }
    }
}
