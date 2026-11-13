//! The phone's window: the chat panel, or what it takes to find one.
//!
//! The desktop wraps `SessionPanel` in tabs and a sidebar; a phone has room for
//! the panel alone, so this is the panel plus the connection it was given. With
//! no connection saved yet it asks for the link and token the sharing machine
//! shows, and keeps them for the next start.

use std::path::PathBuf;

use gpui::{
    App, AppContext, Context, Entity, Focusable as _, IntoElement, ParentElement, Render,
    SharedString, Styled, Subscription, Window, div, rgb,
};
use gpui_kit::component::{
    button::Button,
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use makima_chat::SessionPanel;
use makima_chat::backend::Backend;

use crate::attach::Attach;
use crate::{BACKGROUND, FOREGROUND, MUTED};

const TITLE: &str = "makima";
const SUBTITLE: &str = "A coding agent, in your pocket";
const LINK_HINT: &str = "The link the sharing computer shows, like lan://192.168.1.20:60777";
const TOKEN_HINT: &str = "The token next to it";
const CONNECT_LABEL: &str = "Connect";
const NO_DATA_DIR: &str = "This app has no data directory to save in";

pub struct Phone {
    dirs: Vec<PathBuf>,
    panel: Entity<SessionPanel>,
    setup: Setup,
    showing_setup: bool,
    /// The connection the last attempt used, to prefill the form with it.
    saved: Option<Attach>,
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
        let saved = dirs.iter().find_map(|dir| Attach::read(dir));
        let mut phone = Self {
            dirs,
            panel,
            setup: Setup::new(window, cx, saved.clone()),
            showing_setup: saved.is_none(),
            saved: saved.clone(),
        };
        match saved {
            Some(attach) => phone.connect(attach, cx),
            None => phone.setup.focus(window, cx),
        }
        phone
    }

    /// Opens the session in the background, so a slow connect never blocks a
    /// frame, and asks for the connection again when it cannot.
    fn connect(&mut self, attach: Attach, cx: &mut Context<Self>) {
        let panel = self.panel.clone();
        cx.spawn(async move |this, cx| {
            let started = cx
                .background_executor()
                .spawn(async move { attach.target().and_then(Backend::start) })
                .await;
            let error = started.as_ref().err().cloned();
            panel.update(cx, |panel, cx| panel.attach(started, true, cx));
            if let Some(error) = error {
                this.update(cx, |phone, cx| phone.ask_again(error, cx)).ok();
            }
        })
        .detach();
    }

    fn ask_again(&mut self, error: String, cx: &mut Context<Self>) {
        self.showing_setup = true;
        self.setup.error = Some(error.into());
        let handle = self.setup.link.read(cx).focus_handle(cx);
        if let Some(window) = cx.active_window() {
            _ = cx.update_window(window, |_, window, cx| handle.focus(window, cx));
        }
        cx.notify();
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let setup = &mut self.setup;
        let attach = Attach {
            link: setup.link.read(cx).value().trim().to_owned(),
            token: setup.token.read(cx).value().trim().to_owned(),
        };
        let Some(dir) = self.dirs.first() else {
            setup.error = Some(NO_DATA_DIR.into());
            cx.notify();
            return;
        };
        if let Err(error) = attach.save(dir) {
            setup.error = Some(error.into());
            cx.notify();
            return;
        }
        self.showing_setup = false;
        self.saved = Some(attach.clone());
        self.connect(attach, cx);
        cx.notify();
    }

    fn setup_view(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let setup = &self.setup;
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

impl Setup {
    fn new(window: &mut Window, cx: &mut Context<Phone>, prefill: Option<Attach>) -> Self {
        let (link, token) = prefill.map(|attach| (attach.link, attach.token)).unzip();
        let link = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(LINK_HINT)
                .default_value(link.unwrap_or_default())
        });
        let token = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(TOKEN_HINT)
                .default_value(token.unwrap_or_default())
        });
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
        Self {
            link,
            token,
            error: None,
            _events: events,
        }
    }

    fn focus(&self, window: &mut Window, cx: &mut App) {
        self.link.read(cx).focus_handle(cx).focus(window, cx);
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
            .when(self.showing_setup, |this| this.child(self.setup_view(cx)))
            .when(!self.showing_setup, |this| this.child(self.panel.clone()))
    }
}
