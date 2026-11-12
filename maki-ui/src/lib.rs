//! Single-threaded ratatui event loop; the agent runs on smol tasks in a separate thread.
//! `AgentHandles` bundles all flume channels to the agent. `dispatch()` processes
//! `Action`s returned by `App::update()`. Scroll and drag events are coalesced from
//! the queue to avoid jank.

pub mod animation;
pub mod app;
pub mod chat;
mod click;
mod clipboard;
mod clock;
mod color_compat;
mod command_runtime;
mod components;
pub use components::keybindings;
mod highlight;
pub use highlight::highlight_ansi;
pub mod image;
mod markdown;
mod model_capabilities;
pub use markdown::text_to_lines;
pub mod provider_usage;
mod render_worker;
pub mod repaint;
mod selection;
pub mod splash;
mod storage_writer;
mod text_buffer;
mod theme;
pub use theme::BUNDLED_THEMES;

/// The active CLI theme's palette, for frontends that render their own colors.
pub fn theme_palette() -> (String, Vec<(&'static str, [u8; 3])>) {
    theme::palette()
}

/// Every theme the CLI knows, for a frontend's theme picker.
pub fn theme_names() -> Vec<String> {
    theme::names()
}

/// Installs and persists `name`, so the next run and the TUI agree with the
/// frontend that asked.
pub fn select_theme(name: &str) -> Result<(), String> {
    theme::select(name)
}
mod shared_picker;
pub use shared_picker::{SharedSession, pick_shared_session};
pub(crate) mod terminal_image;
mod trust_card;
pub use trust_card::ask_trust;
pub mod update;
pub mod wrap;

mod agent;
mod event_loop;
mod input;
mod plan_approval;
mod terminal;
mod terminal_da;

use std::time::Instant;

use color_eyre::Result;
use maki_agent::ToolOutput;
use maki_providers::Message;
use maki_providers::TokenUsage;
use maki_storage::id::MakiId;

pub type AppSession = maki_storage::sessions::Session<Message, TokenUsage, ToolOutput>;

pub(crate) use agent::AgentCommand;
pub use event_loop::{EventLoopParams, RemoteSession};

/// How a UI generation ended. On `Reload`, each tab carries its in-memory
/// session so the caller reopens everything without re-reading from disk.
pub enum RunOutcome {
    Exit {
        session_id: Option<MakiId>,
        code: i32,
    },
    Reload {
        tabs: Vec<AppSession>,
        focused: usize,
        /// Whether the agent that served them listened for other machines, so
        /// the window restored here listens again.
        shared: bool,
        /// The session that agent holds, so the window reopens on it.
        hosted: Option<MakiId>,
    },
    /// The focused session should reopen on an agent this window starts, so
    /// other machines can watch it.
    Share { session: Option<MakiId> },
    /// The window should reopen on another session the agent it serves holds.
    Switch { session: MakiId, shared: bool },
}

pub fn run(params: EventLoopParams, initial_prompt: Option<String>) -> Result<RunOutcome> {
    let report = {
        let (_guard, mut terminal) = terminal::TerminalGuard::init()?;
        color_compat::init();
        let el = event_loop::EventLoop::new(&mut terminal, params)?;
        el.run(initial_prompt)?
    };
    let (shared, hosted) = (report.shared, report.hosted);
    Ok(match report.exit {
        components::ExitRequest::Reload => RunOutcome::Reload {
            tabs: report.tabs,
            focused: report.focused,
            shared,
            hosted,
        },
        components::ExitRequest::Share => RunOutcome::Share {
            session: report.tabs.get(report.focused).map(|tab| tab.id),
        },
        components::ExitRequest::Switch { session } => RunOutcome::Switch { session, shared },
        exit => {
            // The conversation lives in the agent when this window started
            // one, so that is the id a resume hint must name.
            let session_id = hosted.or_else(|| {
                report
                    .tabs
                    .get(report.focused)
                    .filter(|s| app::session_has_content(s))
                    .map(|s| s.id)
            });
            let started = Instant::now();
            drop(report);
            tracing::info!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                "session buffers dropped"
            );
            RunOutcome::Exit {
                session_id,
                code: exit.code(),
            }
        }
    })
}
