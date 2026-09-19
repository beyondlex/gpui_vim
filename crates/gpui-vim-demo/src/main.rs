//! gpui-vim demo: a minimal multi-line editor with the full vim engine
//! attached. Run with `cargo run -p gpui-vim-demo`.

mod buffer;
mod editor;
mod host;

use gpui::prelude::*;
use gpui::{px, size, App, Bounds, KeyBinding, WindowBounds, WindowOptions};
use gpui_platform::application;

use crate::editor::{Copy, Editor, Paste, Save, SAMPLE};

fn main() {
    application().run(|cx: &mut App| {
        // host-level bindings: platform chords the engine passes through
        cx.bind_keys([
            KeyBinding::new("cmd-s", Save, None),
            KeyBinding::new("cmd-c", Copy, None),
            KeyBinding::new("cmd-v", Paste, None),
        ]);

        let bounds = Bounds::centered(None, size(px(960.0), px(640.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("gpui-vim demo".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                let editor = cx.new(|cx| Editor::new(SAMPLE, cx));
                let focus = editor.read(cx).focus_handle.clone();
                window.focus(&focus, cx);
                // route every keystroke through the vim engine first.
                // The Subscription MUST be kept alive — it unsubscribes on
                // drop — so store it on the view instead of a local.
                let subscription = gpui_vim::attach(&editor, cx);
                editor.update(cx, |editor, _cx| {
                    editor.set_vim_subscription(subscription);
                    load_rc_layers(editor);
                    register_default_mappings(editor);
                });
                schedule_smoke_test(&editor, cx);
                editor
            },
        )
        .unwrap();

        cx.activate(true);
    });
}

/// Layered rc loading: the shared user layer (`~/.gpui-vimrc`, unknown
/// `:action` ids ignored) then the app-specific layer
/// (`~/.config/gpui-vim-demo/vimrc`, unknown ids reported). Both paths come
/// from `$HOME` independently — the app layer must load even when no shared
/// rc exists yet.
fn load_rc_layers(editor: &mut Editor) {
    let user = gpui_vim::config::default_config_path();
    let host = home_dir().map(|home| home.join(".config/gpui-vim-demo/vimrc"));
    let layers = gpui_vim::config::Layers { user, host };
    let stats = gpui_vim::config::load_layers(editor, &layers);
    if stats.files > 0 {
        editor.status_message = Some(format!(
            "loaded {} mappings, {} options ({} files)",
            stats.mappings, stats.options, stats.files
        ));
    }
}

/// Demo defaults: gt/gT switch buffer tabs. Every tab has its OWN engine,
/// so the mapping goes on all of them. The user rc loads first, so these
/// entries win over (i.e. re-map) anything the user set — delete this
/// function to give users the final say.
fn register_default_mappings(editor: &mut Editor) {
    editor.tabs_mut().iter_mut().for_each(|tab| {
        tab.vim.keymaps_mut().map_str_noremap(
            vim_core::keymap::ModeClass::Normal,
            "gt",
            ":action demo.tab-next<CR>",
            true,
        );
        tab.vim.keymaps_mut().map_str_noremap(
            vim_core::keymap::ModeClass::Normal,
            "gT",
            ":action demo.tab-prev<CR>",
            true,
        );
    });
}

/// `$HOME`, for deriving rc paths without depending on another layer's
/// location.
fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

/// Scripted key injection 2s after launch — a deterministic in-app smoke
/// test of both key paths (special keys + printable text). Remove when
/// driving the app by hand.
fn schedule_smoke_test(editor: &gpui::Entity<Editor>, cx: &mut App) {
    use vim_core::key::Key;

    if std::env::var_os("GPUI_VIM_SMOKE").is_none() {
        return;
    }
    let editor = editor.clone();
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(std::time::Duration::from_secs(2))
            .await;
        editor.update(cx, |editor, cx| {
            // motions via the interceptor path
            for k in ["j", "w"] {
                gpui_vim::dispatch_key(editor, Key::parse(k));
            }
            // `ciw` + typing + Esc via the text-input path:
            // "fn main() {" becomes "fn hello() {"
            gpui_vim::dispatch_text(editor, "ciw");
            gpui_vim::dispatch_text(editor, "hello");
            gpui_vim::dispatch_key(editor, Key::escape());
            // duplicate the line with `yy` `p`, then undo it with `u`
            gpui_vim::dispatch_text(editor, "yyp");
            eprintln!("[smoke] after yyp: {:?}", editor.text());
            gpui_vim::dispatch_key(editor, Key::parse("u"));
            eprintln!("[smoke] after u:   {:?}", editor.text());
            cx.notify();
        });
    })
    .detach();
}
