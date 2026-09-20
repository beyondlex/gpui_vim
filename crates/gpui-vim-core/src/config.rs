//! Parser for `~/.gpui-vimrc` style configuration — the IdeaVim-compatible
//! subset: `set` options, `:map`-family mappings (with `<Leader>`), `"`
//! comments and `source`. Anything else (Lua, functions, autocmds, plugin
//! managers) is collected into `Config::ignored` and skipped, like IdeaVim.

use crate::key::{parse_key_sequence, Key, KeyKind};
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
            // let mapleader = " " / "," / "<Space>" — a bare space parses to
            // Char(' ') via parse_key_sequence, and `<Space>` normalizes to
            // Char(' ') in Key::parse_angle, so both spellings converge.
            if let Some(value) = rest.split('=').nth(1) {
                let value = value.trim().trim_matches('"');
                if let Some(key) = parse_key_sequence(value).first() {
                    leader = key.clone();
                }
            }
            continue;
        }

        if let Some(rest) = line
            .strip_prefix("set")
            .filter(|r| r.is_empty() || r.starts_with(' '))
        {
            for arg in rest.split_whitespace() {
                // Branch order matters twice over: `=` must win over `no`
                // (`set no=3`? unlikely, but `name=value` is never a negation),
                // and a leading `no` is only negation when the remainder names
                // a real boolean option — otherwise `set number` would parse
                // as `Off("mber")`.
                let setting = if let Some((name, value)) = arg.split_once('=') {
                    Setting::Value(name.to_owned(), value.to_owned())
                } else if let Some(name) = arg.strip_suffix('!') {
                    Setting::Toggle(name.to_owned())
                } else if let Some(name) = arg
                    .strip_prefix("no")
                    .filter(|n| crate::options::is_bool_option(n))
                {
                    Setting::Off(name.to_owned())
                } else {
                    Setting::On(arg.to_owned())
                };
                config.settings.push(setting);
            }
            continue;
        }

        if let Some(path) = line.strip_prefix("source").filter(|r| r.starts_with(' ')) {
            // tilde expansion is the loader's job (parse stays pure)
            config.sources.push(PathBuf::from(path.trim()));
            continue;
        }

        // the :map family — the optional leading `:` was already stripped
        let (noremap, classes, after_cmd) =
            if let Some(rest) = strip_map_cmd(line, "noremap").map(str::trim_start) {
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
        let lhs = parse_with_leader(lhs_str, &leader);
        if lhs.is_empty() {
            config.ignored.push(raw_line.to_owned());
            continue;
        }
        let rhs = parse_with_leader(rhs_str, &leader);
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

/// Parse a mapping side, resolving the `<Leader>`/`<leader>` token against
/// the file's `mapleader`. The substitution happens on the PARSED sequence
/// (`<Leader>` parses to a `Named("leader")` marker key), never by string
/// replacement: re-parsing `leader.notation()` shatters multi-char keys —
/// a `<Space>` leader would come back as S,p,a,c,e.
fn parse_with_leader(seq: &str, leader: &Key) -> Vec<Key> {
    parse_key_sequence(seq)
        .into_iter()
        .map(|k| {
            if matches!(&k.kind, KeyKind::Named(n) if n == "leader") {
                leader.clone()
            } else {
                k
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_number_is_on_not_off_mber() {
        // regression: `no` used to be stripped before anything else, so
        // `set number` parsed as `Off("mber")`
        let config = parse("set number");
        assert_eq!(config.settings, vec![Setting::On("number".to_owned())]);
    }

    #[test]
    fn set_no_prefix_negates_known_boolean_options() {
        let config = parse("set nonumber\nset nonu");
        assert_eq!(
            config.settings,
            vec![
                Setting::Off("number".to_owned()),
                Setting::Off("nu".to_owned()),
            ]
        );
    }

    #[test]
    fn set_value_and_toggle_forms() {
        let config = parse("set tabstop=8\nset number!");
        assert_eq!(
            config.settings,
            vec![
                Setting::Value("tabstop".to_owned(), "8".to_owned()),
                Setting::Toggle("number".to_owned()),
            ]
        );
    }

    #[test]
    fn set_unknown_option_falls_through_to_on() {
        // `wrap` is not in the engine's option subset: it parses as `On` and
        // is silently ignored at apply time (like IdeaVim's unknown sets)
        let config = parse("set wrap");
        assert_eq!(config.settings, vec![Setting::On("wrap".to_owned())]);
    }
}
