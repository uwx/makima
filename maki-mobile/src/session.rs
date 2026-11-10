//! The phone's window: the chat panel, or what it takes to find one.
//!
//! The desktop wraps `SessionPanel` in tabs and a sidebar; a phone has room for
//! the panel alone, so this is the panel plus the connection it was given. With
//! no connection saved yet it asks for the link and token the sharing machine
//! shows, and keeps them for the next start.

use std::path::PathBuf;

use gpui::{
    AppContext, Context, Entity, Focusable as _, IntoElement, ParentElement, Render, SharedString,
    Styled, Subscription, Window, div, rgb,
};
use gpui_kit::component::{
    button::Button,
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use maki_chat::SessionPanel;
use maki_chat::backend::Backend;

use crate::attach::Attach;
use crate::{BACKGROUND, FOREGROUND, MUTED};

const TITLE: &str = "makima";
const SUBTITLE: &str = "A coding agent, in your pocket";
const LINK_HINT: &str = "The link the sharing computer shows, like lan://192.168.1.20:60777";
const TOKEN_HINT: &str = "The token next to it";
const CONNECT_LABEL: &str = "Connect";

pub struct Phone {
    dirs: Vec<PathBuf>,
    panel: Entity<SessionPanel>,
    setup: Option<Setup>,
}

/// The form that stands in for a saved connection.
struct Setup {
    link: Entity<InputState>,
    token: Entity<InputState>,
    error: Option<SharedString>,
    _events: Vec<Subscription>,
}

impl Phone {
    pub fn new(window: &mut Window, cx: &mut Context<Self>, dirs: Vec<PathBuf>) -> Self {
        let panel = cx.new(|cx| SessionPanel::new(window, cx));
        let setup = match dirs.iter().find_map(|dir| Attach::read(dir)) {
            Some(attach) => {
                connect(panel.clone(), attach, cx);
                None
            }
            None => Some(Setup::new(window, cx)),
        };
        Self { dirs, panel, setup }
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let Some(setup) = &mut self.setup else {
            return;
        };
        let attach = Attach {
            link: setup.link.read(cx).value().trim().to_owned(),
            token: setup.token.read(cx).value().trim().to_owned(),
        };
        let Some(dir) = self.dirs.first() else {
            setup.error = Some("This app has no data directory to save in".into());
            cx.notify();
            return;
        };
        if let Err(error) = attach.save(dir) {
            setup.error = Some(error.into());
            cx.notify();
            return;
        }
        self.setup = None;
        connect(self.panel.clone(), attach, cx);
        cx.notify();
    }

    fn setup_view(&self, setup: &Setup, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .justify_center()
            .gap_3()
            .px_6()
            .child(div().text_2xl().text_color(rgb(FOREGROUND)).child(TITLE))
            .child(div().text_sm().text_color(rgb(MUTED)).child(SUBTITLE))
            .child(
                div()
                    .w_full()
                    .mt_6()
                    .child(Input::new(&setup.link).w_full()),
            )
            .child(div().text_xs().text_color(rgb(MUTED)).child(LINK_HINT))
            .child(div().w_full().child(Input::new(&setup.token).w_full()))
            .child(div().text_xs().text_color(rgb(MUTED)).child(TOKEN_HINT))
            .when_some(setup.error.clone(), |this, error| {
                this.child(div().text_xs().text_color(rgb(FOREGROUND)).child(error))
            })
            .child(
                h_flex().w_full().justify_end().child(
                    Button::new("connect")
                        .label(CONNECT_LABEL)
                        .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                ),
            )
    }
}

/// Opens the session in the background, so a slow connect never blocks a frame.
fn connect(panel: Entity<SessionPanel>, attach: Attach, cx: &mut Context<Phone>) {
    cx.spawn(async move |_, cx| {
        let started = cx
            .background_executor()
            .spawn(async move { attach.target().and_then(Backend::start) })
            .await;
        panel.update(cx, |panel, cx| panel.attach(started, true, cx));
    })
    .detach();
}

impl Setup {
    fn new(window: &mut Window, cx: &mut Context<Phone>) -> Self {
        let link = cx.new(|cx| InputState::new(window, cx).placeholder(LINK_HINT));
        let token = cx.new(|cx| InputState::new(window, cx).placeholder(TOKEN_HINT));
        let mut events = Vec::with_capacity(2);
        for input in [&link, &token] {
            events.push(cx.subscribe_in(
                input,
                window,
                |this: &mut Phone, _, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.submit(cx);
                    }
                },
            ));
        }
        link.read(cx).focus_handle(cx).focus(window, cx);
        Self {
            link,
            token,
            error: None,
            _events: events,
        }
    }
}

impl Render for Phone {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let safe = window.insets().effective();
        div()
            .size_full()
            .bg(rgb(BACKGROUND))
            .pt(safe.top)
            .pb(safe.bottom)
            .pl(safe.left)
            .pr(safe.right)
            .when_some(self.setup.as_ref(), |this, setup| {
                this.child(self.setup_view(setup, cx))
            })
            .when(self.setup.is_none(), |this| this.child(self.panel.clone()))
    }
}
