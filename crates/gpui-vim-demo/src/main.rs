//! gpui-vim demo: a minimal multi-line editor with the full vim engine
//! attached. Run with `cargo run -p gpui-vim-demo`.

mod buffer;
mod editor;
mod host;

use gpui::prelude::*;
use gpui::{px, size, App, Application, Bounds, KeyBinding, WindowBounds, WindowOptions};

use crate::editor::{Copy, Editor, Paste, Save, SAMPLE};

fn main() {
    Application::new().run(|cx: &mut App| {
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
                window.focus(&editor.read(cx).focus_handle);
                // route every keystroke through the vim engine first.
                // The Subscription MUST be kept alive — it unsubscribes on
                // drop — so store it on the view instead of a local.
                let subscription = gpui_vim::attach(&editor, cx);
                editor.update(cx, |editor, _cx| {
                    editor.set_vim_subscription(subscription);
                    // load the user's rc file when it exists
                    if let Some(path) = gpui_vim::config::default_config_path() {
                        if path.exists() {
                            match gpui_vim::config::load_config_file(editor, &path) {
                                Ok(stats) => {
                                    editor.status_message = Some(format!(
                                        "loaded {} mappings, {} options from {}",
                                        stats.mappings,
                                        stats.options,
                                        path.display()
                                    ));
                                }
                                Err(error) => {
                                    editor.status_message =
                                        Some(format!("E: {}: {error}", path.display()));
                                }
                            }
                        }
                    }
                });
                schedule_smoke_test(&editor, cx);
                editor
            },
        )
        .unwrap();

        cx.activate(true);
    });
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
        gpui::Timer::after(std::time::Duration::from_secs(2)).await;
        let result = editor.update(cx, |editor, cx| {
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
        if let Err(e) = result {
            eprintln!("smoke test failed: {e}");
        }
    })
    .detach();
}
