//! The desktop shell: a sidebar of live sessions and the active one.

use std::collections::HashMap;
use std::thread;

use gpui_kit::component::{
    ActiveTheme, Icon, IconName, h_flex,
    sidebar::{
        Sidebar, SidebarCollapsible, SidebarFooter, SidebarGroup, SidebarHeader, SidebarMenu,
        SidebarMenuItem,
    },
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use maki_acp_client::Target;
use maki_chat::panel::{
    CLOSE_TAB_KEY, CONNECT_COMMAND, DISCOVERY_WAIT, NEW_TAB_KEY, NEW_TAB_LABEL, PanelEvent,
    REMOTE_GROUP, REMOTE_TOKEN_HINT, SESSIONS_GROUP, SLASH_PREFIX,
};
use maki_chat::{Backend, SessionPanel};
use maki_lan::discovery::{Browser, Discovered, Found};
use maki_lan::link;

/// The desktop shell: a sidebar of live sessions and the active one.
pub struct ChatApp {
    panels: Vec<Entity<SessionPanel>>,
    active: usize,
    /// Sessions other machines announce on the local network.
    remotes: Vec<Found>,
    /// The token each link expects, once a window has been given one.
    tokens: HashMap<String, String>,
    _discovery: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl ChatApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let panel = cx.new(|cx| {
            let mut panel = SessionPanel::new(window, cx);
            panel.attach(Target::from_env().and_then(Backend::start), true, cx);
            panel
        });
        let this = cx.entity().downgrade();
        let subscriptions = vec![
            cx.observe(&panel, |_, _, cx| cx.notify()),
            cx.subscribe_in(&panel, window, |app: &mut ChatApp, _, event, window, cx| {
                let PanelEvent::Connect { link, token } = event;
                app.connect(link.clone(), token.clone(), window, cx);
            }),
            cx.intercept_keystrokes(move |event, window, cx| {
                _ = this.update(cx, |this, cx| {
                    if this.on_key(&event.keystroke, window, cx) {
                        cx.stop_propagation();
                    }
                });
            }),
        ];
        panel.update(cx, |panel, cx| panel.focus(window, cx));
        Self {
            panels: vec![panel],
            active: 0,
            remotes: Vec::new(),
            tokens: HashMap::new(),
            _discovery: watch_remote_sessions(cx),
            _subscriptions: subscriptions,
        }
    }

    /// Ctrl+T opens a tab with its own agent, Ctrl+W closes the active one.
    fn on_key(&mut self, key: &Keystroke, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if key.modifiers.control && !key.modifiers.alt && !key.modifiers.platform {
            match key.key.as_str() {
                NEW_TAB_KEY => return self.open_tab(window, cx),
                CLOSE_TAB_KEY => return self.close_tab(window, cx),
                _ => {}
            }
        }
        let Some(panel) = self.panels.get(self.active).cloned() else {
            return false;
        };
        panel.update(cx, |panel, cx| panel.on_key(key, window, cx))
    }

    fn open_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.open_panel(Target::from_env, window, cx)
    }

    /// Opens a tab whose agent is {target}.
    fn open_target(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.open_panel(move || Ok(target), window, cx)
    }

    fn open_panel(
        &mut self,
        target: impl FnOnce() -> Result<Target, String> + Send + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let panel = cx.new(|cx| SessionPanel::new(window, cx));
        let target_panel = panel.clone();
        let theme = self
            .panels
            .get(self.active)
            .and_then(|panel| panel.read(cx).theme_name());
        self._subscriptions
            .push(cx.observe(&panel, |_, _, cx| cx.notify()));
        self._subscriptions.push(cx.subscribe_in(
            &panel,
            window,
            |app: &mut ChatApp, _, event, window, cx| {
                let PanelEvent::Connect { link, token } = event;
                app.connect(link.clone(), token.clone(), window, cx);
            },
        ));
        self.panels.push(panel);
        self.active = self.panels.len() - 1;
        self.focus_active(window, cx);
        cx.spawn(async move |this, cx| {
            let started = cx
                .background_executor()
                .spawn(async move { target().and_then(Backend::start) })
                .await;
            _ = this.update(cx, |app, cx| {
                if app.panels.contains(&target_panel) {
                    target_panel.update(cx, |panel, cx| {
                        panel.attach(started, false, cx);
                        panel.inherit_theme(theme.clone());
                    });
                }
            });
        })
        .detach();
        cx.notify();
        true
    }

    /// Remembers the token for {link} and opens a tab that watches it.
    fn connect(
        &mut self,
        link: String,
        token: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.tokens.insert(link.clone(), token.clone());
        if let Ok(target) = Target::lan(&link, Some(&token)) {
            self.open_target(target, window, cx);
        }
    }

    /// Opens a tab on an announced session, asking for its token when this
    /// window has not been given one yet.
    fn open_remote(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(found) = self.remotes.get(index).cloned() else {
            return;
        };
        let link = link(found.address);
        match self.tokens.get(&link).cloned() {
            Some(token) => self.connect(link, token, window, cx),
            None => self.ask_for_token(&link, window, cx),
        }
    }

    fn ask_for_token(&mut self, link: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.panels.get(self.active).cloned() else {
            return;
        };
        panel.update(cx, |panel, cx| {
            panel.prefill(
                format!("{SLASH_PREFIX}{CONNECT_COMMAND} {link} "),
                REMOTE_TOKEN_HINT.to_owned(),
                window,
                cx,
            );
        });
        self.focus_active(window, cx);
    }

    fn on_discovered(&mut self, discovered: Discovered, cx: &mut Context<Self>) {
        match discovered {
            Discovered::Found(found) => {
                match self
                    .remotes
                    .iter_mut()
                    .find(|remote| remote.fullname == found.fullname)
                {
                    Some(remote) => *remote = found,
                    None => self.remotes.push(found),
                }
                self.remotes.sort_by(|a, b| a.name.cmp(&b.name));
            }
            Discovered::Gone(fullname) => {
                self.remotes.retain(|remote| remote.fullname != fullname);
            }
        }
        cx.notify();
    }

    fn close_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.panels.len() < 2 {
            return true;
        }
        self.panels.remove(self.active);
        self.active = self.active.min(self.panels.len() - 1);
        self.focus_active(window, cx);
        cx.notify();
        true
    }

    fn switch_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index == self.active || index >= self.panels.len() {
            return;
        }
        self.active = index;
        self.focus_active(window, cx);
        cx.notify();
    }

    fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = self.panels.get(self.active) {
            panel.update(cx, |panel, cx| panel.focus(window, cx));
        }
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self
            .panels
            .iter()
            .enumerate()
            .map(|(index, panel)| {
                let this = cx.entity();
                SidebarMenuItem::new(panel.read(cx).title())
                    .icon(IconName::Inbox)
                    .active(index == self.active)
                    .on_click(move |_, window, cx| {
                        this.update(cx, |app, cx| app.switch_tab(index, window, cx));
                    })
            })
            .collect::<Vec<_>>();
        let remote_rows = self
            .remotes
            .iter()
            .enumerate()
            .map(|(index, found)| {
                let this = cx.entity();
                let label = if found.project.is_empty() {
                    found.name.clone()
                } else {
                    found.project.clone()
                };
                let address = found.address.to_string();
                SidebarMenuItem::new(label)
                    .icon(IconName::Network)
                    .suffix(move |_, cx| {
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(address.clone())
                    })
                    .on_click(move |_, window, cx| {
                        this.update(cx, |app, cx| app.open_remote(index, window, cx));
                    })
            })
            .collect::<Vec<_>>();
        let new_row = {
            let this = cx.entity();
            SidebarMenuItem::new(NEW_TAB_LABEL)
                .icon(IconName::Plus)
                .on_click(move |_, window, cx| {
                    this.update(cx, |app, cx| {
                        app.open_tab(window, cx);
                    });
                })
        };
        let (location, theme) = self.panels.get(self.active).map_or_else(
            || (String::new(), None),
            |panel| {
                let panel = panel.read(cx);
                (panel.location(), panel.theme_name())
            },
        );

        Sidebar::new("sidebar")
            .w(px(240.))
            .collapsible(SidebarCollapsible::None)
            .header(
                SidebarHeader::new().child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_center()
                                .size_8()
                                .flex_shrink_0()
                                .rounded(cx.theme().radius)
                                .bg(cx.theme().sidebar_primary)
                                .text_color(cx.theme().sidebar_primary_foreground)
                                .child(Icon::new(IconName::Bot)),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .overflow_hidden()
                                .child("Makima")
                                .child(div().text_xs().child("desktop")),
                        ),
                ),
            )
            .child(
                SidebarGroup::new(SESSIONS_GROUP)
                    .child(SidebarMenu::new().children(rows).child(new_row)),
            )
            .when(!remote_rows.is_empty(), |this| {
                this.child(
                    SidebarGroup::new(REMOTE_GROUP).child(SidebarMenu::new().children(remote_rows)),
                )
            })
            .footer(
                SidebarFooter::new().child(
                    v_flex()
                        .w_full()
                        .min_w_0()
                        .gap_1()
                        .child(
                            h_flex()
                                .w_full()
                                .min_w_0()
                                .gap_2()
                                .child(IconName::Folder)
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_xs()
                                        .truncate()
                                        .child(location),
                                ),
                        )
                        .when_some(theme, |this, theme| {
                            this.child(
                                div()
                                    .w_full()
                                    .min_w_0()
                                    .text_xs()
                                    .truncate()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!("Theme Â· {theme}")),
                            )
                        }),
                ),
            )
    }
}

/// Lists the sessions other machines announce, for as long as the window lives.
fn watch_remote_sessions(cx: &mut Context<ChatApp>) -> Task<()> {
    let (found_tx, found_rx) = flume::unbounded();
    _ = thread::Builder::new()
        .name("maki-discovery".into())
        .spawn(move || {
            let Some(browser) = Browser::start() else {
                return;
            };
            while let Some(discovered) = browser.next(DISCOVERY_WAIT) {
                if found_tx.send(discovered).is_err() {
                    return;
                }
            }
        });
    cx.spawn(async move |this, cx| {
        while let Ok(discovered) = found_rx.recv_async().await {
            if this
                .update(cx, |app, cx| app.on_discovered(discovered, cx))
                .is_err()
            {
                break;
            }
        }
    })
}

impl Render for ChatApp {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.sidebar(cx))
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .children(self.panels.get(self.active).cloned()),
            )
    }
}
