//! The phone's window: one chat panel, no shell.
//!
//! The desktop wraps `SessionPanel` in tabs and a sidebar; a phone has room for
//! the panel alone, so this is the panel plus the connection it was given.

use gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Window, div, rgb,
};
use maki_chat::SessionPanel;
use maki_chat::backend::Backend;

use crate::BACKGROUND;
use crate::attach::Attach;

pub struct Phone {
    panel: Entity<SessionPanel>,
}

impl Phone {
    pub fn new(window: &mut Window, cx: &mut Context<Self>, attach: Attach) -> Self {
        let panel = cx.new(|cx| {
            let panel = SessionPanel::new(window, cx);
            panel.focus(window, cx);
            panel
        });
        cx.spawn(async move |this, cx| {
            let started = cx
                .background_executor()
                .spawn(async move { attach.target().and_then(Backend::start) })
                .await;
            this.update(cx, |phone, cx| {
                phone
                    .panel
                    .update(cx, |panel, cx| panel.attach(started, true, cx));
            })
            .ok();
        })
        .detach();
        Self { panel }
    }
}

impl Render for Phone {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgb(BACKGROUND))
            .child(self.panel.clone())
    }
}
