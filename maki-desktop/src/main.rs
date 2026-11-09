mod acp;
mod app;
mod backend;

use gpui_kit::assets::Assets;
use gpui_kit::*;

fn main() {
    maki_storage::paths::init().expect("failed to initialise storage paths");
    let application = gpui_kit::application().with_assets(Assets);
    application.run(|cx| {
        gpui_kit::init(cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1180.), px(820.)), cx)),
            titlebar: Some(TitlebarOptions {
                title: Some("Makima".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| app::ChatApp::new(window, cx))
        })
        .expect("failed to open window");
    });
}
