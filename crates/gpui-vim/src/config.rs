//! Reading `~/.gpui-vimrc`-style config files into a host's engine.
//!
//! Multiple apps may share one user rc file; action ids differ per app, so
//! hosts should load a HOST-SPECIFIC layer on top (it wins) and may opt into
//! lenient action dispatch (unknown ids ignored instead of reported).

use std::path::{Path, PathBuf};

use vim_core::config::{self, ConfigStats};

use crate::VimEditor;

/// `source` directives may not chain forever (a cycle or a prank rc would
/// otherwise loop); vim caps `:source` depth similarly.
const MAX_SOURCE_DEPTH: usize = 4;

/// The default user config path: `$HOME/.gpui-vimrc` (None without `$HOME`).
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

/// How `:action <id>` behaves when the host does not know the id. Shared rc
/// files contain mappings aimed at OTHER apps, so multi-app setups usually
/// want [`ActionPolicy::Ignore`] in the user layer and strict reporting only
/// in the host-specific layer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ActionPolicy {
    /// Report unknown ids through the status channel (default).
    #[default]
    Report,
    /// Silently ignore unknown ids.
    Ignore,
}

/// Layered loading plan. The HOST layer is applied after (and therefore
/// wins over) the USER layer: later mappings/options override earlier ones
/// per vim's layered-rc convention.
#[derive(Debug, Default)]
pub struct Layers {
    /// User's cross-app config, typically `~/.gpui-vimrc`. Loaded with
    /// [`ActionPolicy::Ignore`] — its `:action` mappings may target other
    /// apps, so unknown ids here are expected.
    pub user: Option<PathBuf>,
    /// App-specific config, typically `~/.config/<app>/vimrc` or embedded
    /// defaults. Overrides the user layer; loaded with
    /// [`ActionPolicy::Report`] so real mistakes surface.
    pub host: Option<PathBuf>,
}

impl Layers {
    /// User layer at `$HOME/.gpui-vimrc`.
    pub fn with_default_user() -> Self {
        Layers {
            user: default_config_path(),
            host: None,
        }
    }
}

/// One planned layer load (internal to [`load_layers`]).
struct LayerPlan {
    path: PathBuf,
    action_policy: ActionPolicy,
}

/// Stats across all applied layers.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LoadedStats {
    pub options: usize,
    pub mappings: usize,
    pub ignored: usize,
    /// Files that existed and were applied.
    pub files: usize,
}

impl std::ops::Add for LoadedStats {
    type Output = Self;
    fn add(mut self, rhs: Self) -> Self {
        self.options += rhs.options;
        self.mappings += rhs.mappings;
        self.ignored += rhs.ignored;
        self.files += rhs.files;
        self
    }
}

/// Load a single config file (and its `source` directives, depth-capped).
/// Missing files are an io::Error.
pub fn load_config_file<E: VimEditor>(editor: &mut E, path: &Path) -> std::io::Result<ConfigStats> {
    let (vim, _, _) = editor.vim_parts();
    vim.set_lenient_actions(false);
    load(editor, &expand_tilde(path), 0)
}

/// Load [`Layers`] in order (user first, host second) — layers that don't
/// exist are skipped. Each layer's action policy is active while it loads.
/// Returns the combined stats of what was applied. Missing or unreadable
/// files are silent here; use [`load_layers_checked`] to surface them.
pub fn load_layers<E: VimEditor>(editor: &mut E, layers: &Layers) -> LoadedStats {
    load_layers_checked(editor, layers).0
}

/// [`load_layers`] that also reports per-layer read errors: without this,
/// a typo'd rc path is indistinguishable from "no config to load".
/// (`source` targets stay silent by design — a broken directive inside
/// someone else's shared rc should not fail this app's startup.)
pub fn load_layers_checked<E: VimEditor>(
    editor: &mut E,
    layers: &Layers,
) -> (LoadedStats, Vec<std::io::Error>) {
    let plans = [
        layers.user.as_ref().map(|path| LayerPlan {
            path: path.clone(),
            action_policy: ActionPolicy::Ignore,
        }),
        layers.host.as_ref().map(|path| LayerPlan {
            path: path.clone(),
            action_policy: ActionPolicy::Report,
        }),
    ];
    let mut total = LoadedStats::default();
    let mut errors = Vec::new();
    for plan in plans.into_iter().flatten() {
        let (vim, _, _) = editor.vim_parts();
        vim.set_lenient_actions(plan.action_policy == ActionPolicy::Ignore);
        match load(editor, &expand_tilde(&plan.path), 0) {
            Ok(stats) => accumulate(&mut total, stats, true),
            Err(error) => errors.push(error),
        }
    }
    // restore strict dispatch after the shared layers
    let (vim, _, _) = editor.vim_parts();
    vim.set_lenient_actions(false);
    (total, errors)
}

/// Fold one applied file's stats into the running totals.
/// (`ConfigStats` is foreign, so an `Add` impl would violate orphan rules.)
fn accumulate(total: &mut LoadedStats, stats: ConfigStats, count_file: bool) {
    total.options += stats.options;
    total.mappings += stats.mappings;
    total.ignored += stats.ignored;
    if count_file {
        total.files += 1;
    }
}

fn load<E: VimEditor>(editor: &mut E, path: &Path, depth: usize) -> std::io::Result<ConfigStats> {
    let text = std::fs::read_to_string(path)?;
    let config = config::parse(&text);
    let (vim, _, _) = editor.vim_parts();
    let mut stats = vim.apply_config(&config);

    // `source` directives apply after the sourcing file (documented
    // divergence from vim's in-place semantics)
    if depth < MAX_SOURCE_DEPTH {
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
