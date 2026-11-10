//! makima on a phone.
//!
//! The phone is a client, not a workspace: it shows a session another machine
//! runs, the same way `makima attach` and the desktop do. This crate is the
//! window and its start-up; the session arrives over ACP.

mod attach;

#[cfg(any(target_os = "android", target_os = "ios"))]
mod session;

const BACKGROUND: u32 = 0x101014;
const FOREGROUND: u32 = 0xf0f0f5;
const MUTED: u32 = 0x9090a0;
const NOTICE: u32 = 0x2a2a33;

#[cfg(any(target_os = "android", target_os = "ios"))]
mod app {
    use std::path::PathBuf;

    use gpui::{
        AnyView, App, AppContext, Application, Context, IntoElement, ParentElement, Render, Styled,
        Window, WindowOptions, div, rgb,
    };

    use super::attach::Attach;
    use super::session::Session;
    use super::{BACKGROUND, FOREGROUND, MUTED};

    const TAG: &str = "makima-mobile";
    const TITLE: &str = "makima";
    const SUBTITLE: &str = "A coding agent, in your pocket";

    /// What a phone with no session configured shows.
    struct Welcome;

    impl Render for Welcome {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .bg(rgb(BACKGROUND))
                .child(div().text_2xl().text_color(rgb(FOREGROUND)).child(TITLE))
                .child(div().text_sm().text_color(rgb(MUTED)).child(SUBTITLE))
        }
    }

    pub fn open_main_window(cx: &mut App, files: Option<PathBuf>) {
        gpui_kit::init(cx);
        gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
        let content: AnyView = match files.as_deref().and_then(Attach::read) {
            Some(attach) => Session::open(cx, attach).into(),
            None => cx.new(|_| Welcome).into(),
        };
        let window = cx.open_window(
            WindowOptions {
                window_bounds: None,
                ..Default::default()
            },
            |window, cx| cx.new(|cx| gpui_kit::component::Root::new(content, window, cx)),
        );
        if let Err(error) = window {
            log::error!("could not open the window: {error:#}");
            return;
        }
        cx.activate(true);
    }

    #[cfg(target_os = "android")]
    pub fn run(app: android_activity::AndroidApp) {
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(log::LevelFilter::Info)
                .with_tag(TAG),
        );
        gpui_mobile::android::jni::install_panic_hook();
        let _platform = gpui_mobile::android::jni::init_platform(&app);
        let files = app.internal_data_path();
        let Some(shared) = gpui_mobile::android::jni::shared_platform() else {
            log::error!("the Android platform did not come up");
            return;
        };
        // Blocks for the life of the activity, so the platform and the app
        // context it hands out stay alive on this stack frame.
        Application::with_platform(shared.into_rc()).run(move |cx| open_main_window(cx, files));
    }
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: android_activity::AndroidApp) {
    app::run(app);
}
