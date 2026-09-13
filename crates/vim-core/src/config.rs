//! Parser for `~/.gpui-vimrc` style configuration — the IdeaVim-compatible
//! subset: `set` options, `:map`-family mappings (with `<Leader>`), `"`
//! comments and `source`. Anything else (Lua, functions, autocmds, plugin
//! managers) is collected into `Config::ignored` and skipped, like IdeaVim.

use crate::key::{parse_key_sequence, Key};
use crate::keymap::ModeClass;
use std::path::PathBuf;

/// One `set` item. `On`/`Off`/`Toggle` are booleans; `Value` is `name=value`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Setting {
    On(String),
    Off(String),
    Toggle(String),
    Value(String, String),
}

/// One mapping from the `:map` family, resolved against the file's
/// `mapleader` (so `<Leader>` in the LHS is already a concrete key).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigMapping {
    pub class: ModeClass,
    pub lhs: Vec<Key>,
    pub rhs: Vec<Key>,
    pub noremap: bool,
}

/// A parsed configuration file.
#[derive(Debug, Default)]
pub struct Config {
    pub settings: Vec<Setting>,
    pub mappings: Vec<ConfigMapping>,
    /// `source <path>` directives, in file order. The loader applies them.
    pub sources: Vec<PathBuf>,
    /// Lines that were not understood (Lua, functions, autocmds, ...).
    pub ignored: Vec<String>,
}

/// Counts reported by [`VimState::apply_config`](crate::state::VimState::apply_config).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ConfigStats {
    pub options: usize,
    pub mappings: usize,
    pub ignored: usize,
}

fn default_leader() -> Key {
    Key::char('\\')
}

/// Parse a configuration text. `mapleader` (via `let mapleader = "x"`)
/// affects subsequent `<Leader>` occurrences, like vim.
pub fn parse(text: &str) -> Config {
    let mut config = Config::default();
    let mut leader = default_leader();

    for raw_line in text.lines() {
        let line = raw_line.trim();
        // strip a leading colon (`:map ...` in the user's example)
        let line = line.strip_prefix(':').unwrap_or(line);
        if line.is_empty() || line.starts_with('"') {
            continue;
        }

        if let Some(rest) = line.strip_prefix("let mapleader") {
            // let mapleader = " " / "," / "<Space>"
            if let Some(value) = rest.split('=').nth(1) {
                let value = value.trim().trim_matches('"');
                if let Some(key) = parse_key_sequence(value).first() {
                    leader = key.clone();
                }
            }
            continue;
        }

        if let Some(rest) = line.strip_prefix("set").filter(|r| r.is_empty() || r.starts_with(' ')) {
            for arg in rest.split_whitespace() {
                let setting = if let Some(name) = arg.strip_suffix('!') {
                    Some(Setting::Toggle(name.to_owned()))
                } else if let Some(name) = arg.strip_prefix("no") {
                    Some(Setting::Off(name.to_owned()))
                } else if let Some((name, value)) = arg.split_once('=') {
                    Some(Setting::Value(name.to_owned(), value.to_owned()))
                } else {
                    Some(Setting::On(arg.to_owned()))
                };
                config.settings.push(setting.unwrap());
            }
            continue;
        }

        if let Some(path) = line.strip_prefix("source").filter(|r| r.starts_with(' ')) {
            // tilde expansion is the loader's job (parse stays pure)
            config.sources.push(PathBuf::from(path.trim()));
            continue;
        }

        // the :map family — the optional leading `:` was already stripped
        let (noremap, classes, after_cmd) = if let Some(rest) = strip_map_cmd(line, "noremap").map(str::trim_start) {
            (true, vec![ModeClass::Normal, ModeClass::Visual], rest)
        } else if let Some(rest) = strip_map_cmd(line, "nnoremap").map(str::trim_start) {
            (true, vec![ModeClass::Normal], rest)
        } else if let Some(rest) = strip_map_cmd(line, "vnoremap").map(str::trim_start) {
            (true, vec![ModeClass::Visual], rest)
        } else if let Some(rest) = strip_map_cmd(line, "inoremap").map(str::trim_start) {
            (true, vec![ModeClass::Insert], rest)
        } else if let Some(rest) = strip_map_cmd(line, "nmap").map(str::trim_start) {
            (false, vec![ModeClass::Normal], rest)
        } else if let Some(rest) = strip_map_cmd(line, "vmap").map(str::trim_start) {
            (false, vec![ModeClass::Visual], rest)
        } else if let Some(rest) = strip_map_cmd(line, "imap").map(str::trim_start) {
            (false, vec![ModeClass::Insert], rest)
        } else if let Some(rest) = strip_map_cmd(line, "map").map(str::trim_start) {
            (false, vec![ModeClass::Normal, ModeClass::Visual], rest)
        } else {
            config.ignored.push(raw_line.to_owned());
            continue;
        };

        let (lhs_str, rhs_str) = match after_cmd.split_once(' ') {
            Some((lhs, rhs)) if !lhs.is_empty() && !rhs.trim().is_empty() => {
                (lhs, rhs.trim_start())
            }
            _ => {
                config.ignored.push(raw_line.to_owned());
                continue;
            }
        };
        let lhs: Vec<Key> = parse_key_sequence(&resolve_leader(lhs_str, &leader))
            .into_iter()
            .collect();
        if lhs.is_empty() {
            config.ignored.push(raw_line.to_owned());
            continue;
        }
        let rhs = parse_key_sequence(&resolve_leader(rhs_str, &leader));
        for class in classes {
            config.mappings.push(ConfigMapping {
                class,
                lhs: lhs.clone(),
                rhs: rhs.clone(),
                noremap,
            });
        }
    }
    config
}

fn strip_map_cmd<'a>(line: &'a str, cmd: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(cmd)?;
    // command boundary: end of line or whitespace ("map" must not match "mapping")
    if rest.is_empty() || rest.starts_with(' ') {
        Some(rest)
    } else {
        None
    }
}

fn resolve_leader(text: &str, leader: &Key) -> String {
    // exact-case `<Leader>` only: the notation is ASCII, so a blind string
    // replace is byte-safe
    if text.contains("<Leader>") {
        text.replace("<Leader>", &leader.notation())
    } else {
        text.to_owned()
    }
}

