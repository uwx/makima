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

#[cfg(any(target_os = "android", target_os = "ios"))]
mod app {
    use std::path::PathBuf;

    use gpui::{AnyView, App, AppContext, Application, WindowOptions};
    use gpui_mobile::android::jni::{init_platform, install_panic_hook, shared_platform};

    use super::session::Phone;

    const TAG: &str = "makima-mobile";

    pub fn open_main_window(cx: &mut App, dirs: &[PathBuf]) {
        gpui_kit::init(cx);
        gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
        let dirs = dirs.to_vec();
        let window = cx.open_window(
            WindowOptions {
                window_bounds: None,
                ..Default::default()
            },
            |window, cx| {
                let phone: AnyView = cx.new(|cx| Phone::new(window, cx, dirs)).into();
                cx.new(|cx| gpui_kit::component::Root::new(phone, window, cx))
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
        install_panic_hook();
        let _platform = init_platform(&app);
        let dirs: Vec<PathBuf> = [app.internal_data_path(), app.external_data_path()]
            .into_iter()
            .flatten()
            .collect();
        let Some(shared) = shared_platform() else {
            log::error!("the Android platform did not come up");
            return;
        };
        // Blocks for the life of the activity, so the platform and the app
        // context it hands out stay alive on this stack frame.
        Application::with_platform(shared.into_rc()).run(move |cx| open_main_window(cx, &dirs));
    }
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: android_activity::AndroidApp) {
    app::run(app);
}
