//! Reading `~/.vimcorerc`-style config files into a host's engine.
//!
//! Multiple apps may share one user rc file; action ids differ per app, so
//! hosts should load a HOST-SPECIFIC layer on top (it wins) and may opt into
//! lenient action dispatch (unknown ids ignored instead of reported).

use std::path::{Path, PathBuf};

use vimcore::config::{self, ConfigStats};

use crate::VimEditor;

/// `source` directives may not chain forever (a cycle or a prank rc would
/// otherwise loop); vim caps `:source` depth similarly.
const MAX_SOURCE_DEPTH: usize = 4;

/// The default user config path: `$HOME/.vimcorerc` (None without `$HOME`).
pub fn default_config_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".vimcorerc"))
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
    /// User's cross-app config, typically `~/.vimcorerc`. Loaded with
    /// [`ActionPolicy::Ignore`] — its `:action` mappings may target other
    /// apps, so unknown ids here are expected.
    pub user: Option<PathBuf>,
    /// App-specific config, typically `~/.config/<app>/vimrc` or embedded
    /// defaults. Overrides the user layer; loaded with
    /// [`ActionPolicy::Report`] so real mistakes surface.
    pub host: Option<PathBuf>,
}

impl Layers {
    /// User layer at `$HOME/.vimcorerc`.
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
    // divergence from vim's in-place semantics). Targets get the same
    // tilde expansion as top-level layer paths — the engine hands the raw
    // text back, and `source ~/more-maps` must not silently fail while the
    // identical top-level path works.
    if depth < MAX_SOURCE_DEPTH {
        for source in &config.sources {
            if let Ok(sub) = load(editor, &expand_tilde(source), depth + 1) {
                stats.options += sub.options;
                stats.mappings += sub.mappings;
                stats.ignored += sub.ignored;
            }
        }
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::ops::Range;
    use std::rc::Rc;
    use vimcore::buffer::VimBufferMut;
    use vimcore::host::VimHost;
    use vimcore::state::VimState;

    /// Minimal editor stub: config loading only mutates the engine state;
    /// the buffer/host halves just need to exist behind real references.
    struct CfgEditor {
        vim: VimState,
        buf: crate::tests::TestBuf,
        host: NoopCfgHost,
    }

    impl VimEditor for CfgEditor {
        fn vim_parts(&mut self) -> (&mut VimState, &mut dyn VimBufferMut, &mut dyn VimHost) {
            (&mut self.vim, &mut self.buf, &mut self.host)
        }
        fn vim_accepts_keys(&self, _: &gpui::Window, _: &gpui::App) -> bool {
            false
        }
    }

    struct NoopCfgHost;
    impl VimHost for NoopCfgHost {
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
    }

    fn editor() -> CfgEditor {
        CfgEditor {
            vim: VimState::new(),
            buf: crate::tests::TestBuf(Rc::new(RefCell::new(String::new()))),
            host: NoopCfgHost,
        }
    }

    /// Write `files` under a temp dir laid out as a fake $HOME and run the
    /// test body with HOME pointing there (config paths are all HOME-derived).
    /// Serialization: HOME is process-global, so the tests that swap it must
    /// not run concurrently.
    fn with_fake_home(files: &[(&str, &str)], f: impl FnOnce(&Path)) {
        static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("gpui-vim-cfg-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (path, content) in files {
            let full = dir.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, content).unwrap();
        }
        // 换环境必须还原：remove 会让同进程后续测试读到「无 HOME」的世界
        // （default_config_path 静默返回 None），并行测试也可能读到假 HOME
        let saved_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &dir);
        f(&dir);
        match saved_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn source_directive_expands_tilde_like_top_level_paths() {
        // The rc chain: main rc → source ~/sourced.rc → :set expandtab.
        // Regression: only top-level layer paths were tilde-expanded, so the
        // sourced file silently failed to load.
        with_fake_home(&[
            (".vimcorerc", "set number\nsource ~/sourced.rc\n"),
            ("sourced.rc", "set expandtab\n"),
        ], |dir| {
            let mut editor = editor();
            let layers = Layers {
                user: Some(dir.join(".vimcorerc")),
                host: None,
            };
            let stats = load_layers(&mut editor, &layers);
            assert_eq!(stats.files, 1, "files counts top-level layers only");
            assert!(editor.vim.options.expandtab, "option from the sourced file is live");
        });
    }

    #[test]
    fn missing_files_are_skipped_but_reported_by_checked_variant() {
        with_fake_home(&[(".vimcorerc", "set number\n")], |dir| {
            let mut editor = editor();
            let layers = Layers {
                user: Some(dir.join(".vimcorerc")),
                host: Some(dir.join("nope/vimrc")),
            };
            let (stats, errors) = load_layers_checked(&mut editor, &layers);
            assert_eq!((stats.files, stats.options), (1, 1));
            assert_eq!(errors.len(), 1, "the missing host layer surfaces");
            assert!(editor.vim.options.number);
        });
    }

    #[test]
    fn host_layer_overrides_user_layer_same_key() {
        with_fake_home(&[
            (".vimcorerc", "set relativenumber\n"),
            (".config/app/vimrc", "set norelativenumber\n"),
        ], |dir| {
            let mut editor = editor();
            let layers = Layers {
                user: Some(dir.join(".vimcorerc")),
                host: Some(dir.join(".config/app/vimrc")),
            };
            load_layers(&mut editor, &layers);
            assert!(!editor.vim.options.relativenumber, "host layer wins");
        });
    }
}
