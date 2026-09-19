//! Minimal text probe in an EXTERNAL workspace (gpui consumed via git patch).

use gpui::{
    canvas, div, prelude::*, px, rgb, size, App, Bounds, Context, Window, WindowBounds,
    WindowOptions,
};
use gpui_platform::application;

struct Probe;

impl Render for Probe {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgb(0x202020))
            .text_color(rgb(0xf0f0f0))
            .flex()
            .flex_col()
            .gap_2()
            .justify_center()
            .items_center()
            .text_size(px(24.))
            .child("default: Hello external probe 你好")
            .child(
                div().font_family("Menlo").child("Menlo: external probe"),
            )
            .child(div().child(
                // custom shape_line + paint path
                canvas(
                    move |_, _, _| {},
                    move |bounds, _, window, cx| {
                        let run = gpui::TextRun {
                            len: "shaped: external probe".len(),
                            font: gpui::font("Menlo"),
                            color: gpui::black(),
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        };
                        let line = window.text_system().shape_line(
                            "shaped: external probe".into(),
                            px(24.),
                            &[run],
                            None,
                        );
                        let _ = line.paint(
                            gpui::point(bounds.origin.x + px(40.), bounds.origin.y + px(10.)),
                            px(24.),
                            gpui::TextAlign::Left,
                            None,
                            window,
                            cx,
                        );
                    },
                ),
            ))
    }
}

fn main() {
    application().run(|cx: &mut App| {
        cx.activate(true);
        let bounds = Bounds::centered(None, size(px(500.), px(300.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(|_| Probe),
        )
        .unwrap();

        cx.spawn(async move |cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(6))
                .await;
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
}
