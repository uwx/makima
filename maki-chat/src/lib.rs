//! The chat panel both frontends draw.
//!
//! One ACP connection, its transcript, tool cards, composer and status bar,
//! built with gpui-kit. The desktop wraps it in a shell with tabs and a
//! sidebar; the phone draws it alone.

pub mod backend;
pub mod panel;

pub use backend::{Backend, Command, Started};
pub use panel::{PanelEvent, SessionPanel};
