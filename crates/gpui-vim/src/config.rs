//! Reading `~/.gpui-vimrc`-style config files into a host's engine.

use std::path::{Path, PathBuf};

use vim_core::config::{self, ConfigStats};

/// The default config path: `$HOME/.gpui-vimrc` (None without `$HOME`).
pub fn default_config_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".gpui-vimrc"))
}

/// Expand a leading `~` / `~/` using `$HOME`.
fn expand_tilde(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = text.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

/// Parse and apply a config file (and its `source` directives, one level
/// deep). Returns the combined stats. Missing files are an io::Error.
pub fn load_config_file<E: VimEditor>(editor: &mut E, path: &Path) -> std::io::Result<ConfigStats> {
    load(editor, &expand_tilde(path), 0)
}

fn load<E: VimEditor>(editor: &mut E, path: &Path, depth: usize) -> std::io::Result<ConfigStats> {
    let text = std::fs::read_to_string(path)?;
    let config = config::parse(&text);
    let (vim, _, _) = editor.vim_parts();
    let mut stats = vim.apply_config(&config);

    // `source` directives apply after the sourcing file (documented
    // divergence from vim's in-place semantics)
    if depth < 4 {
        for source in &config.sources {
            if let Ok(sub) = load(editor, source, depth + 1) {
                stats.options += sub.options;
                stats.mappings += sub.mappings;
                stats.ignored += sub.ignored;
            }
        }
    }
    Ok(stats)
}

use crate::VimEditor;
