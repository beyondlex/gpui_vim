//! Keystroke model.
//!
//! The engine deliberately mirrors the shape of `gpui::Keystroke` so the
//! integration layer can convert between the two without loss: `key` is the
//! base key name (`"a"`, `"escape"`, `"space"`, `"@"`) and `key_char` is the
//! printable character produced by the keystroke, if any.

use std::fmt;

/// Modifier flags attached to a key press.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Modifiers {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    pub platform: bool,
}

impl Modifiers {
    pub const NONE: Modifiers = Modifiers {
        control: false,
        alt: false,
        shift: false,
        platform: false,
    };

    pub fn ctrl() -> Self {
        Modifiers {
            control: true,
            ..Self::NONE
        }
    }

    pub fn is_plain(&self) -> bool {
        !self.control && !self.alt && !self.platform
    }
}

/// The non-modifier part of a keystroke.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum KeyKind {
    /// A named key ("escape", "enter", ...). `String` keeps this open-ended so
    /// exotic keys survive conversion instead of being dropped.
    Named(String),
    /// A printable character key.
    Char(char),
}

/// A single keystroke fed into the engine.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub modifiers: Modifiers,
    pub kind: KeyKind,
}

impl Key {
    pub const fn plain(kind: KeyKind) -> Self {
        Key {
            modifiers: Modifiers::NONE,
            kind,
        }
    }

    pub fn named(name: &str) -> Self {
        Key::plain(KeyKind::Named(name.to_owned()))
    }

    pub fn ctrl(name: &str) -> Self {
        Key {
            modifiers: Modifiers::ctrl(),
            kind: KeyKind::Named(name.to_owned()),
        }
    }

    pub fn ctrl_char(c: char) -> Self {
        Key {
            modifiers: Modifiers::ctrl(),
            kind: KeyKind::Char(c),
        }
    }

    pub fn char(c: char) -> Self {
        Key::plain(KeyKind::Char(c))
    }

    pub fn escape() -> Self {
        Key::named("escape")
    }

    pub fn enter() -> Self {
        Key::named("enter")
    }

    pub fn backspace() -> Self {
        Key::named("backspace")
    }

    pub fn tab() -> Self {
        Key::named("tab")
    }

    /// The printable character carried by this keystroke, ignoring ones held
    /// with control/alt/platform modifiers (those are command keys, not text).
    pub fn printable_char(&self) -> Option<char> {
        if !self.modifiers.is_plain() {
            return None;
        }
        match &self.kind {
            KeyKind::Char(c) => Some(*c),
            KeyKind::Named(name) => match name.as_str() {
                "space" => Some(' '),
                "tab" => Some('\t'),
                _ => None,
            },
        }
    }

    /// Normalize `self` into a canonical notation such as `<C-a>`, `<Esc>`, `d`.
    pub fn notation(&self) -> String {
        let mut s = String::new();
        if self.modifiers.control {
            s.push_str("C-");
        }
        if self.modifiers.alt {
            s.push_str("M-");
        }
        if self.modifiers.platform {
            s.push_str("D-");
        }
        if self.modifiers.shift {
            s.push_str("S-");
        }
        match &self.kind {
            KeyKind::Char(c) => {
                s.push(*c);
            }
            KeyKind::Named(name) => match name.as_str() {
                "escape" => s.push_str("Esc"),
                "enter" => s.push_str("Enter"),
                "backspace" => s.push_str("BS"),
                "space" => s.push_str("Space"),
                "tab" => s.push_str("Tab"),
                other => s.push_str(other),
            },
        }
        s
    }

    /// Parse a keystroke from vim-like notation: `"d"`, `"<Esc>"`, `"<C-a>"`,
    /// `"<Space>"`, `"<CR>"`, `"<lt>"`. Used by the command table (which is
    /// declared as `&'static [&'static str]`) and by the mapping API.
    pub fn parse(s: &str) -> Key {
        // single characters are themselves, including '<' and '>'
        if s.chars().count() == 1 {
            return Key::char(s.chars().next().unwrap());
        }
        if s.starts_with('<') {
            let inner = s
                .strip_prefix('<')
                .and_then(|r| r.strip_suffix('>'))
                .unwrap_or(s);
            return Key::parse_angle(inner);
        }
        // bare modifier sequence like `C-a`
        if let Some(rest) = s.strip_prefix("C-") {
            let mut chars = rest.chars();
            if let Some(c) = chars.next() {
                if chars.next().is_none() {
                    return Key::ctrl_char(c);
                }
            }
            return Key::ctrl(rest);
        }
        Key::named(s)
    }

    fn parse_angle(inner: &str) -> Key {
        let (modifiers, rest) = match inner.strip_prefix("C-") {
            Some(rest) => (Modifiers::ctrl(), rest),
            None => (Modifiers::NONE, inner),
        };
        let key = match rest.to_ascii_lowercase().as_str() {
            "esc" => Key::named("escape"),
            "cr" | "return" | "enter" => Key::named("enter"),
            "bs" => Key::named("backspace"),
            "del" => Key::named("delete"),
            "space" => Key::named("space"),
            "tab" => Key::named("tab"),
            "lt" => Key::char('<'),
            "bar" => Key::char('|'),
            _ if rest.chars().count() == 1 => Key {
                modifiers,
                kind: KeyKind::Char(rest.chars().next().unwrap()),
            },
            _ => Key {
                modifiers,
                kind: KeyKind::Named(rest.to_ascii_lowercase()),
            },
        };
        if modifiers.control {
            if let KeyKind::Char(c) = key.kind {
                return Key::ctrl_char(c);
            }
        }
        key
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.notation())
    }
}

/// Parse a sequence like `"g g"` or `"dd"` into keystrokes.
pub fn parse_keys(seq: &str) -> Vec<Key> {
    seq.split_whitespace()
        .map(Key::parse)
        .collect()
}

/// Parse a vim mapping sequence like `"jk"`, `"<Esc>x"` into keystrokes.
/// Angle-bracket names group multiple characters into one key.
pub fn parse_key_sequence(seq: &str) -> Vec<Key> {
    let mut keys = Vec::new();
    let mut chars = seq.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' {
            let mut name = String::from("<");
            for inner in chars.by_ref() {
                name.push(inner);
                if inner == '>' {
                    break;
                }
            }
            keys.push(Key::parse(&name));
        } else {
            keys.push(Key::char(c));
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic() {
        assert_eq!(Key::parse("d"), Key::char('d'));
        assert_eq!(Key::parse("<Esc>"), Key::escape());
        assert_eq!(Key::parse("<C-a>"), Key::ctrl_char('a'));
        assert_eq!(Key::parse("<Space>"), Key::named("space"));
        assert_eq!(Key::parse("<CR>"), Key::enter());
        assert_eq!(parse_keys("g g"), vec![Key::char('g'), Key::char('g')]);
    }

    #[test]
    fn notation_roundtrip() {
        for s in ["d", "<Esc>", "<C-a>", "g"] {
            assert_eq!(Key::parse(s).notation().to_lowercase(), s.trim_start_matches('<').trim_end_matches('>').to_lowercase());
        }
    }
}
