//! makima on a phone.
//!
//! The phone is a client, not a workspace: it shows a session another machine
//! runs, the same way `makima attach` and the desktop do. This crate is the
//! window and its start-up; the session arrives over ACP.

#[cfg(any(target_os = "android", target_os = "ios"))]
mod app {
    use gpui::{
        App, AppContext, Application, Context, IntoElement, ParentElement, Render, Styled, Window,
        WindowOptions, div, rgb,
    };

    const TAG: &str = "makima-mobile";
    const TITLE: &str = "makima";
    const SUBTITLE: &str = "A coding agent, in your pocket";
    const BACKGROUND: u32 = 0x101014;
    const FOREGROUND: u32 = 0xf0f0f5;
    const MUTED: u32 = 0x9090a0;

    /// The first screen. It exists to prove the platform, text and GPU path on
    /// a device before a session is wired in.
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

    pub fn open_main_window(cx: &mut App) {
        gpui_kit::init(cx);
        gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
        let window = cx.open_window(
            WindowOptions {
                window_bounds: None,
                ..Default::default()
            },
            |window, cx| {
                let welcome = cx.new(|_| Welcome);
                cx.new(|cx| gpui_kit::component::Root::new(welcome, window, cx))
            },
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
        let Some(shared) = gpui_mobile::android::jni::shared_platform() else {
            log::error!("the Android platform did not come up");
            return;
        };
        // Blocks for the life of the activity, so the platform and the app
        // context it hands out stay alive on this stack frame.
        Application::with_platform(shared.into_rc()).run(open_main_window);
    }
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: android_activity::AndroidApp) {
    app::run(app);
}
