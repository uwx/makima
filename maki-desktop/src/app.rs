use std::path::PathBuf;

use agent_client_protocol_schema::{
    AvailableCommand, ContentBlock, PermissionOption, SessionUpdate, ToolCallContent,
    ToolCallStatus,
};
use gpui_kit::component::{
    ActiveTheme, Icon, IconName, Sizable,
    button::Button,
    h_flex,
    input::{InputEvent, Textarea, TextareaState},
    scroll::ScrollableElement,
    sidebar::{
        Sidebar, SidebarCollapsible, SidebarFooter, SidebarGroup, SidebarHeader, SidebarMenu,
        SidebarMenuItem,
    },
    text::{TextView, TextViewState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::Value;

use crate::acp::{self, ArgItem, Event, PickItem, PickOutcome, Target};
use crate::backend::{Backend, Command, Started};

const SEND_LABEL: &str = "Send";
const STOP_LABEL: &str = "Stop";
const COMPOSER_PLACEHOLDER: &str = "Ask makima to explore, explain, or change the code";
const COMPOSER_HINT: &str = "Enter sends, Shift+Enter adds a line, / opens commands";
const TRANSCRIPT_MAX_WIDTH: f32 = 860.;
const PICK_WIDTH: f32 = 560.;
const PICK_MAX_HEIGHT: f32 = 480.;
const MAX_SLASH_ROWS: usize = 8;
const ARGUMENT_MAX_HEIGHT: f32 = 320.;
const SLASH_PREFIX: char = '/';
const DISCONNECTED: &str = "Agent is not connected";

enum ChatView {
    User(SharedString),
    Assistant(Entity<TextViewState>),
    Thinking(String),
    Tool(ToolCard),
    Notice(SharedString),
}

struct ToolCard {
    id: String,
    name: SharedString,
    summary: SharedString,
    output: SharedString,
    done: bool,
    failed: bool,
}

#[derive(Default)]
struct Status {
    model: SharedString,
    context: Option<u64>,
    context_window: u64,
    cost: Option<String>,
}

struct PendingPermission {
    request: Value,
    title: String,
    options: Vec<PermissionOption>,
}

struct SlashMenu {
    matches: Vec<usize>,
    active: usize,
}

struct ArgumentMenu {
    items: Vec<ArgItem>,
    matches: Vec<usize>,
    active: usize,
}

struct PendingPick {
    request: Value,
    title: String,
    footer: Vec<(String, String)>,
    items: Vec<PickItem>,
    active: usize,
}

#[derive(Clone, Copy)]
enum PickEnd {
    Choose,
    Delete,
    Close,
}

impl SlashMenu {
    fn is_open(&self) -> bool {
        !self.matches.is_empty()
    }
}

impl ArgumentMenu {
    fn new(items: Vec<ArgItem>, query: &str) -> Self {
        let mut menu = Self {
            items,
            matches: Vec::new(),
            active: 0,
        };
        menu.filter(query);
        menu
    }

    fn filter(&mut self, query: &str) {
        self.matches = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.label.to_lowercase().starts_with(query))
            .map(|(index, _)| index)
            .take(MAX_SLASH_ROWS)
            .collect();
        self.active = 0;
    }

    fn is_open(&self) -> bool {
        !self.matches.is_empty()
    }

    fn selected(&self) -> Option<&ArgItem> {
        self.matches
            .get(self.active)
            .and_then(|index| self.items.get(*index))
    }
}

pub struct ChatApp {
    backend: Option<Backend>,
    input: Entity<TextareaState>,
    views: Vec<ChatView>,
    scroll: ScrollHandle,
    busy: bool,
    pending: Option<PendingPermission>,
    pick: Option<PendingPick>,
    status: Status,
    commands: Vec<AvailableCommand>,
    slash: Option<SlashMenu>,
    slash_dismissed: bool,
    arguments: Option<ArgumentMenu>,
    arguments_dismissed: bool,
    _subscriptions: Vec<Subscription>,
    _events: Task<()>,
}

impl ChatApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut views = Vec::new();
        let started = Target::from_env().and_then(Backend::start);
        let (backend, events) = match started {
            Ok(Started { backend, events }) => (Some(backend), Some(events)),
            Err(error) => {
                views.push(ChatView::Notice(
                    format!("Could not start a session: {error}").into(),
                ));
                (None, None)
            }
        };

        let status = Status {
            model: backend
                .as_ref()
                .map(|backend| SharedString::from(backend.model.clone()))
                .unwrap_or_default(),
            ..Status::default()
        };

        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(COMPOSER_PLACEHOLDER)
                .submit_on_enter(true)
                .auto_grow(1, 8)
        });

        let this = cx.entity().downgrade();
        let subscriptions = vec![
            cx.subscribe_in(
                &input,
                window,
                |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => {
                        this.slash_dismissed = false;
                        this.arguments_dismissed = false;
                        this.refresh_slash(cx);
                        this.refresh_arguments(cx);
                        cx.notify();
                    }
                    InputEvent::PressEnter { shift: false, .. } => this.send(window, cx),
                    _ => {}
                },
            ),
            cx.intercept_keystrokes({
                let this = this.clone();
                move |event, window, cx| {
                    _ = this.update(cx, |this, cx| {
                        if this.on_key(&event.keystroke, window, cx) {
                            cx.stop_propagation();
                        }
                    });
                }
            }),
        ];

        let events_task = cx.spawn(async move |this, cx| {
            let Some(events) = events else {
                return;
            };
            while let Ok(event) = events.recv_async().await {
                if this.update(cx, |app, cx| app.on_event(event, cx)).is_err() {
                    break;
                }
            }
        });

        Self {
            backend,
            input,
            views,
            scroll: ScrollHandle::new(),
            busy: false,
            pending: None,
            pick: None,
            status,
            commands: Vec::new(),
            slash: None,
            slash_dismissed: false,
            arguments: None,
            arguments_dismissed: false,
            _subscriptions: subscriptions,
            _events: events_task,
        }
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let text = self.input.read(cx).value().trim().to_owned();
        if text.is_empty() {
            return;
        }
        let Some(backend) = &self.backend else {
            self.notice(DISCONNECTED.to_owned());
            cx.notify();
            return;
        };
        if let Err(error) = backend.send(Command::Prompt(text.clone())) {
            self.notice(error);
            cx.notify();
            return;
        }

        self.input.update(cx, |state, cx| state.clean(window, cx));
        self.slash = None;
        self.views.push(ChatView::User(text.into()));
        self.views.push(ChatView::Assistant(
            cx.new(|cx| TextViewState::markdown("", cx)),
        ));
        self.busy = true;
        self.pending = None;
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(backend) = &self.backend {
            _ = backend.send(Command::Cancel);
        }
        self.busy = false;
        self.pending = None;
        self.notice("Interrupted".to_owned());
        cx.notify();
    }

    fn answer(&mut self, option: Option<&str>, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        if let Some(backend) = &self.backend {
            backend.answer(&pending.request, option);
        }
        cx.notify();
    }

    fn notice(&mut self, text: String) {
        self.views.push(ChatView::Notice(text.into()));
        self.scroll.scroll_to_bottom();
    }

    fn on_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Update(update) => self.on_update(*update, cx),
            Event::Permission(prompt) => {
                self.pending = Some(PendingPermission {
                    request: prompt.id,
                    title: prompt.title,
                    options: prompt.options,
                });
            }
            Event::Flash(text) => self.notice(text),
            Event::Pick(prompt) => self.open_pick(prompt),
            Event::Completion { text, items } => {
                self.apply_arguments(text, items, cx);
                return;
            }
            Event::PromptDone(result) => {
                self.busy = false;
                self.pending = None;
                if let Err(error) = result {
                    self.notice(error);
                }
            }
            Event::Closed(reason) => {
                self.backend = None;
                self.busy = false;
                self.pending = None;
                self.pick = None;
                self.notice(reason);
            }
        }
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    fn open_pick(&mut self, prompt: acp::PickPrompt) {
        let Some(active) = prompt
            .items
            .len()
            .checked_sub(1)
            .map(|last| prompt.cursor.min(last))
        else {
            if let Some(backend) = &self.backend {
                backend.answer_pick(&prompt.id, PickOutcome::Close);
            }
            return;
        };
        self.pick = Some(PendingPick {
            request: prompt.id,
            title: prompt.title,
            footer: prompt.footer,
            items: prompt.items,
            active,
        });
    }

    fn move_pick(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(pick) = &mut self.pick else {
            return;
        };
        let len = pick.items.len() as isize;
        if len > 0 {
            pick.active = (pick.active as isize + delta).rem_euclid(len) as usize;
        }
        cx.notify();
    }

    fn end_pick(&mut self, end: PickEnd, cx: &mut Context<Self>) {
        let Some(pick) = self.pick.take() else {
            return;
        };
        let outcome = match end {
            PickEnd::Choose => PickOutcome::Choice(pick.active),
            PickEnd::Delete => PickOutcome::Delete(pick.active),
            PickEnd::Close => PickOutcome::Close,
        };
        if let Some(backend) = &self.backend {
            backend.answer_pick(&pick.request, outcome);
        }
        cx.notify();
    }

    fn on_update(&mut self, update: SessionUpdate, cx: &mut Context<Self>) {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                if let ContentBlock::Text(block) = chunk.content {
                    self.push_text(&block.text, cx);
                    self.busy = true;
                }
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                if let ContentBlock::Text(block) = chunk.content {
                    self.push_thinking(&block.text);
                }
            }
            SessionUpdate::ToolCall(call) => {
                let id = call.tool_call_id.0.to_string();
                self.upsert_tool(&id, &call.title, "");
                self.apply_tool_status(&id, call.status);
            }
            SessionUpdate::ToolCallUpdate(update) => {
                let id = update.tool_call_id.0.to_string();
                let fields = update.fields;
                let summary = fields.title.unwrap_or_default();
                let output = fields
                    .raw_output
                    .map(|value| match value {
                        Value::String(text) => text,
                        other => other.to_string(),
                    })
                    .or_else(|| fields.content.as_deref().and_then(tool_text));
                self.upsert_tool(&id, "", &summary);
                if let Some(output) = output {
                    self.set_tool_output(&id, output);
                }
                if let Some(status) = fields.status {
                    self.apply_tool_status(&id, status);
                }
            }
            SessionUpdate::AvailableCommandsUpdate(update) => {
                self.commands = update.available_commands;
                self.refresh_slash(cx);
            }
            SessionUpdate::UsageUpdate(usage) => {
                self.status.context = Some(usage.used);
                self.status.context_window = usage.size;
                self.status.cost = usage
                    .cost
                    .map(|cost| format!("{:.4} {}", cost.amount, cost.currency));
            }
            SessionUpdate::ConfigOptionUpdate(update) => {
                if let Some(model) = acp::model_from(&update.config_options) {
                    self.status.model = model.into();
                }
            }
            _ => {}
        }
    }

    fn push_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let trailing = self
            .views
            .len()
            .checked_sub(1)
            .filter(|index| matches!(&self.views[*index], ChatView::Assistant(_)));
        if let Some(index) = trailing {
            if let ChatView::Assistant(state) = &self.views[index] {
                state.update(cx, |state, cx| state.push_str(text, cx));
            }
            return;
        }
        let state = cx.new(|cx| TextViewState::markdown(text, cx));
        self.views.push(ChatView::Assistant(state));
    }

    fn push_thinking(&mut self, text: &str) {
        if let Some(ChatView::Thinking(buffer)) = self.views.last_mut() {
            buffer.push_str(text);
            return;
        }
        self.views.push(ChatView::Thinking(text.into()));
    }

    fn tool_mut(&mut self, id: &str) -> Option<&mut ToolCard> {
        self.views.iter_mut().find_map(|view| match view {
            ChatView::Tool(card) if card.id == id => Some(card),
            _ => None,
        })
    }

    fn upsert_tool(&mut self, id: &str, name: &str, summary: &str) {
        if let Some(card) = self.tool_mut(id) {
            if !name.is_empty() {
                card.name = name.into();
            }
            if !summary.is_empty() {
                card.summary = summary.into();
            }
            return;
        }
        self.views.push(ChatView::Tool(ToolCard {
            id: id.to_owned(),
            name: name.into(),
            summary: summary.into(),
            output: SharedString::default(),
            done: false,
            failed: false,
        }));
    }

    fn set_tool_output(&mut self, id: &str, output: String) {
        if let Some(card) = self.tool_mut(id) {
            card.output = output.into();
        }
    }

    fn apply_tool_status(&mut self, id: &str, status: ToolCallStatus) {
        let (done, failed) = match status {
            ToolCallStatus::Completed => (true, false),
            ToolCallStatus::Failed => (true, true),
            ToolCallStatus::Pending | ToolCallStatus::InProgress => (false, false),
            _ => (false, false),
        };
        if let Some(card) = self.tool_mut(id) {
            card.done = done;
            card.failed = failed;
        }
    }

    fn refresh_slash(&mut self, cx: &mut Context<Self>) {
        let Some(query) = slash_query(&self.input.read(cx).value()) else {
            self.slash = None;
            return;
        };
        if self.slash_dismissed {
            self.slash = None;
            return;
        }
        let matches = self
            .commands
            .iter()
            .enumerate()
            .filter(|(_, command)| command.name.to_lowercase().starts_with(&query))
            .map(|(index, _)| index)
            .take(MAX_SLASH_ROWS)
            .collect::<Vec<_>>();
        self.slash = (!matches.is_empty()).then_some(SlashMenu { matches, active: 0 });
    }

    fn refresh_arguments(&mut self, cx: &mut Context<Self>) {
        let value = self.input.read(cx).value().to_string();
        let Some(slot) = argument_slot(&value) else {
            self.arguments = None;
            return;
        };
        if self.arguments_dismissed {
            self.arguments = None;
            return;
        }
        if let Some(backend) = &self.backend {
            backend.complete_arguments(value);
        }
        if let Some(menu) = &mut self.arguments {
            menu.filter(&slot.query);
        }
    }

    fn apply_arguments(&mut self, text: String, items: Vec<ArgItem>, cx: &mut Context<Self>) {
        let value = self.input.read(cx).value().to_string();
        if text != value || self.arguments_dismissed {
            return;
        }
        let Some(slot) = argument_slot(&value) else {
            self.arguments = None;
            cx.notify();
            return;
        };
        self.arguments = (!items.is_empty()).then(|| ArgumentMenu::new(items, &slot.query));
        cx.notify();
    }

    fn move_arguments(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(menu) = &mut self.arguments else {
            return;
        };
        let len = menu.matches.len() as isize;
        if len > 0 {
            menu.active = (menu.active as isize + delta).rem_euclid(len) as usize;
        }
        cx.notify();
    }

    fn accept_arguments(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self
            .arguments
            .take()
            .and_then(|menu| menu.selected().cloned())
        else {
            return;
        };
        let value = self.input.read(cx).value().to_string();
        let Some(slot) = argument_slot(&value) else {
            return;
        };
        let head = value[..slot.start].to_owned();
        let insertion = quote_argument(&item.insertion);
        self.input.update(cx, |state, cx| {
            state.set_value(head, window, cx);
            state.insert(format!("{insertion} "), window, cx);
        });
        cx.notify();
    }

    fn dismiss_arguments(&mut self, cx: &mut Context<Self>) {
        self.arguments = None;
        self.arguments_dismissed = true;
        cx.notify();
    }

    fn move_slash(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(menu) = &mut self.slash else {
            return;
        };
        let len = menu.matches.len() as isize;
        if len > 0 {
            menu.active = (menu.active as isize + delta).rem_euclid(len) as usize;
        }
        cx.notify();
    }

    fn select_slash(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(menu) = &mut self.slash {
            menu.active = row;
        }
        self.accept_slash(window, cx);
    }

    fn accept_slash(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.slash.take() else {
            return;
        };
        let Some(index) = menu.matches.get(menu.active).copied() else {
            return;
        };
        let name = self.commands[index].name.clone();
        self.input.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.insert(format!("{SLASH_PREFIX}{name} "), window, cx);
        });
        cx.notify();
    }

    fn on_key(&mut self, key: &Keystroke, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.pick.is_some() {
            return self.on_pick_key(key, cx);
        }
        if self.on_slash_key(key, window, cx) {
            return true;
        }
        self.on_argument_key(key, window, cx)
    }

    fn on_pick_key(&mut self, key: &Keystroke, cx: &mut Context<Self>) -> bool {
        if key.modifiers.alt || key.modifiers.platform {
            return false;
        }
        let control = key.modifiers.control;
        match key.key.as_str() {
            "d" if control => self.end_pick(PickEnd::Delete, cx),
            "down" if !control => self.move_pick(1, cx),
            "up" if !control => self.move_pick(-1, cx),
            "enter" if !control => self.end_pick(PickEnd::Choose, cx),
            "escape" if !control => self.end_pick(PickEnd::Close, cx),
            _ => return false,
        }
        true
    }

    fn on_slash_key(
        &mut self,
        key: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.slash.as_ref().is_some_and(SlashMenu::is_open)
            || !self.input.read(cx).focus_handle(cx).is_focused(window)
        {
            return false;
        }
        if key.modifiers.control || key.modifiers.alt || key.modifiers.platform {
            return false;
        }
        match key.key.as_str() {
            "down" => self.move_slash(1, cx),
            "up" => self.move_slash(-1, cx),
            "enter" | "tab" => self.accept_slash(window, cx),
            "escape" => {
                self.slash = None;
                self.slash_dismissed = true;
                cx.notify();
            }
            _ => return false,
        }
        true
    }

    fn on_argument_key(
        &mut self,
        key: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.arguments.as_ref().is_some_and(ArgumentMenu::is_open)
            || !self.input.read(cx).focus_handle(cx).is_focused(window)
        {
            return false;
        }
        if key.modifiers.control || key.modifiers.alt || key.modifiers.platform {
            return false;
        }
        match key.key.as_str() {
            "down" => self.move_arguments(1, cx),
            "up" => self.move_arguments(-1, cx),
            "enter" | "tab" => self.accept_arguments(window, cx),
            "escape" => self.dismiss_arguments(cx),
            _ => return false,
        }
        true
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let session_label = self
            .backend
            .as_ref()
            .map(|backend| format!("Session {}", backend.session))
            .unwrap_or_else(|| "No session".to_owned());
        let location = self
            .backend
            .as_ref()
            .map(|backend| format!("{} · {}", backend.target, backend.cwd.display()))
            .unwrap_or_default();

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
                SidebarGroup::new("Chat").child(
                    SidebarMenu::new().children([SidebarMenuItem::new(session_label)
                        .icon(IconName::Inbox)
                        .active(true)]),
                ),
            )
            .footer(
                SidebarFooter::new().child(
                    h_flex()
                        .gap_2()
                        .child(IconName::Folder)
                        .child(div().text_xs().overflow_hidden().child(location)),
                ),
            )
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .w_full()
            .items_center()
            .gap_2()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(div().text_sm().child("Agent"))
            .child(div().flex_1())
            .when(self.busy, |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .text_color(cx.theme().muted_foreground)
                        .child(Icon::new(IconName::LoaderCircle))
                        .child(div().text_xs().child("Working")),
                )
                .child(
                    Button::new("stop")
                        .label(STOP_LABEL)
                        .icon(IconName::Square)
                        .small()
                        .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                )
            })
    }

    fn transcript(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut items: Vec<AnyElement> = Vec::with_capacity(self.views.len());
        for view in &self.views {
            items.push(self.render_view(view, cx).into_any_element());
        }
        if self.views.is_empty() {
            items.push(
                v_flex()
                    .gap_2()
                    .items_center()
                    .py_24()
                    .child(Icon::new(IconName::Bot))
                    .child(div().text_lg().child("What are we building?"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("Working in {}", self.cwd_label())),
                    )
                    .into_any_element(),
            );
        }

        div()
            .id("transcript")
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(TRANSCRIPT_MAX_WIDTH))
                    .mx_auto()
                    .p_4()
                    .gap_4()
                    .children(items),
            )
    }

    fn render_view(&self, view: &ChatView, cx: &mut Context<Self>) -> AnyElement {
        match view {
            ChatView::User(text) => h_flex()
                .w_full()
                .justify_end()
                .child(
                    div()
                        .max_w(px(680.))
                        .p_3()
                        .rounded(cx.theme().radius)
                        .bg(cx.theme().secondary)
                        .text_color(cx.theme().secondary_foreground)
                        .child(text.clone()),
                )
                .into_any_element(),
            ChatView::Assistant(state) => v_flex()
                .w_full()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(Icon::new(IconName::Bot))
                        .child("Assistant"),
                )
                .child(TextView::new(state).selectable(true))
                .into_any_element(),
            ChatView::Thinking(text) => v_flex()
                .w_full()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(Icon::new(IconName::Cpu))
                        .child("Thinking"),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(text.clone()),
                )
                .into_any_element(),
            ChatView::Tool(card) => self.tool_card(card, cx),
            ChatView::Notice(text) => div()
                .w_full()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(text.clone())
                .into_any_element(),
        }
    }

    fn tool_card(&self, card: &ToolCard, cx: &mut Context<Self>) -> AnyElement {
        let icon = if card.failed {
            IconName::CircleAlert
        } else if card.done {
            IconName::CircleCheck
        } else {
            IconName::LoaderCircle
        };
        v_flex()
            .w_full()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .overflow_hidden()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .px_3()
                    .py_2()
                    .bg(cx.theme().muted)
                    .child(Icon::new(icon))
                    .child(div().text_sm().child(card.name.clone()))
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .overflow_hidden()
                            .text_color(cx.theme().muted_foreground)
                            .child(card.summary.clone()),
                    ),
            )
            .when(!card.output.is_empty(), |this| {
                this.child(
                    div()
                        .px_3()
                        .py_2()
                        .max_h(px(240.))
                        .overflow_hidden()
                        .text_xs()
                        .child(card.output.clone()),
                )
            })
            .into_any_element()
    }

    fn permission_card(
        &self,
        pending: &PendingPermission,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let title = if pending.title.is_empty() {
            "A tool needs permission".to_owned()
        } else {
            format!("{} needs permission", pending.title)
        };
        let mut actions: Vec<AnyElement> = Vec::with_capacity(pending.options.len());
        for option in &pending.options {
            let option_id = option.option_id.0.to_string();
            actions.push(
                Button::new(option_id.clone())
                    .label(option.name.clone())
                    .small()
                    .on_click(cx.listener(move |this, _, _, cx| this.answer(Some(&option_id), cx)))
                    .into_any_element(),
            );
        }
        if actions.is_empty() {
            actions.push(
                Button::new("deny")
                    .label("Deny")
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| this.answer(None, cx)))
                    .into_any_element(),
            );
        }

        v_flex()
            .mx_4()
            .mb_2()
            .p_3()
            .gap_2()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(Icon::new(IconName::TriangleAlert))
                    .child(div().text_sm().child(title)),
            )
            .child(h_flex().gap_2().justify_end().children(actions))
    }

    fn pick_modal(&self, pick: &PendingPick, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = pick
            .items
            .iter()
            .enumerate()
            .map(|(row, item)| {
                let active = row == pick.active;
                let mut meta = String::new();
                if let Some(section) = &item.section {
                    meta.push_str(section);
                }
                if let Some(detail) = &item.detail {
                    if !meta.is_empty() {
                        meta.push_str(" · ");
                    }
                    meta.push_str(detail);
                }
                v_flex()
                    .id(("pick", row))
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .when(active, |this| this.bg(cx.theme().accent))
                    .child(div().text_sm().child(item.label.clone()))
                    .when(!meta.is_empty(), |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(meta),
                        )
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(pick) = &mut this.pick {
                            pick.active = row;
                        }
                        this.end_pick(PickEnd::Choose, cx);
                    }))
            })
            .collect::<Vec<_>>();

        let footer = pick
            .footer
            .iter()
            .map(|(key, label)| {
                h_flex()
                    .gap_1()
                    .child(div().text_xs().child(key.clone()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(label.clone()),
                    )
            })
            .collect::<Vec<_>>();

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(cx.theme().overlay)
            .child(
                v_flex()
                    .w(px(PICK_WIDTH))
                    .max_h(px(PICK_MAX_HEIGHT))
                    .p_3()
                    .gap_2()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().popover)
                    .when(!pick.title.is_empty(), |this| {
                        this.child(div().text_sm().child(pick.title.clone()))
                    })
                    .child(v_flex().gap_1().overflow_y_scrollbar().children(rows))
                    .when(!footer.is_empty(), |this| {
                        this.child(h_flex().gap_3().children(footer))
                    }),
            )
    }

    fn slash_menu(&self, menu: &SlashMenu, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = menu
            .matches
            .iter()
            .enumerate()
            .map(|(row, index)| {
                let command = &self.commands[*index];
                let active = row == menu.active;
                h_flex()
                    .id(("slash", row))
                    .w_full()
                    .gap_3()
                    .items_center()
                    .px_3()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .when(active, |this| this.bg(cx.theme().accent))
                    .child(
                        div()
                            .text_sm()
                            .child(format!("{SLASH_PREFIX}{}", command.name)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .overflow_hidden()
                            .text_color(cx.theme().muted_foreground)
                            .child(command.description.clone()),
                    )
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.select_slash(row, window, cx)),
                    )
            })
            .collect::<Vec<_>>();

        v_flex()
            .w_full()
            .p_1()
            .gap_1()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .children(rows)
    }

    fn argument_menu(&self, menu: &ArgumentMenu, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = menu
            .matches
            .iter()
            .enumerate()
            .map(|(row, index)| {
                let item = &menu.items[*index];
                let active = row == menu.active;
                h_flex()
                    .id(("argument", row))
                    .w_full()
                    .gap_3()
                    .items_center()
                    .px_3()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .when(active, |this| this.bg(cx.theme().accent))
                    .child(div().text_sm().child(item.label.clone()))
                    .when_some(item.description.clone(), |this, description| {
                        this.child(
                            div()
                                .flex_1()
                                .text_xs()
                                .overflow_hidden()
                                .text_color(cx.theme().muted_foreground)
                                .child(description),
                        )
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(menu) = &mut this.arguments {
                            menu.active = row;
                        }
                        this.accept_arguments(window, cx);
                    }))
            })
            .collect::<Vec<_>>();
        div()
            .w_full()
            .max_h(px(ARGUMENT_MAX_HEIGHT))
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .p_1()
            .flex()
            .flex_col()
            .gap_1()
            .overflow_y_scrollbar()
            .children(rows)
    }

    fn composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w_full()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(TRANSCRIPT_MAX_WIDTH))
                    .mx_auto()
                    .px_4()
                    .pt_2()
                    .pb_3()
                    .gap_2()
                    .child(
                        div()
                            .relative()
                            .w_full()
                            .child(
                                div()
                                    .w_full()
                                    .rounded(cx.theme().radius)
                                    .border_1()
                                    .border_color(cx.theme().border)
                                    .bg(cx.theme().background)
                                    .child(Textarea::new(&self.input)),
                            )
                            .children(self.slash.as_ref().map(|menu| {
                                div()
                                    .absolute()
                                    .bottom(relative(1.))
                                    .left_0()
                                    .right_0()
                                    .pb_2()
                                    .child(self.slash_menu(menu, cx))
                            }))
                            .children(self.arguments.as_ref().filter(|menu| menu.is_open()).map(
                                |menu| {
                                    div()
                                        .absolute()
                                        .bottom(relative(1.))
                                        .left_0()
                                        .right_0()
                                        .pb_2()
                                        .child(self.argument_menu(menu, cx))
                                },
                            )),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(COMPOSER_HINT),
                            )
                            .when(!self.busy, |this| {
                                this.child(
                                    Button::new("send")
                                        .label(SEND_LABEL)
                                        .icon(IconName::ArrowUp)
                                        .small()
                                        .on_click(
                                            cx.listener(|this, _, window, cx| {
                                                this.send(window, cx)
                                            }),
                                        ),
                                )
                            })
                            .when(self.busy, |this| {
                                this.child(
                                    Button::new("stop-inline")
                                        .label(STOP_LABEL)
                                        .icon(IconName::Square)
                                        .small()
                                        .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                                )
                            }),
                    ),
            )
    }

    fn status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let context = match self.status.context {
            Some(used) if self.status.context_window > 0 => {
                format!("{used} / {} ctx", self.status.context_window)
            }
            _ => String::new(),
        };

        h_flex()
            .w_full()
            .items_center()
            .gap_3()
            .px_4()
            .py_1()
            .text_xs()
            .border_t_1()
            .border_color(cx.theme().border)
            .text_color(cx.theme().muted_foreground)
            .child(self.status.model.clone())
            .when(!context.is_empty(), |this| this.child(context))
            .children(self.status.cost.clone())
    }
}

impl ChatApp {
    fn cwd_label(&self) -> String {
        self.backend
            .as_ref()
            .map(|backend| backend.cwd.display().to_string())
            .unwrap_or_else(|| PathBuf::from(".").display().to_string())
    }
}

fn slash_query(value: &str) -> Option<String> {
    let rest = value.strip_prefix(SLASH_PREFIX)?;
    rest.chars()
        .all(|character| !character.is_whitespace())
        .then(|| rest.to_lowercase())
}

/// The token being typed after the command name, and where it starts.
struct ArgumentSlot {
    start: usize,
    query: String,
}

fn argument_slot(value: &str) -> Option<ArgumentSlot> {
    let command_end = value.find(char::is_whitespace)?;
    if !value.starts_with(SLASH_PREFIX) {
        return None;
    }
    if value.ends_with(char::is_whitespace) {
        return Some(ArgumentSlot {
            start: value.len(),
            query: String::new(),
        });
    }
    let token = value[command_end..]
        .trim_start()
        .rsplit(char::is_whitespace)
        .next()?;
    Some(ArgumentSlot {
        start: value.len() - token.len(),
        query: token.to_lowercase(),
    })
}

fn quote_argument(insertion: &str) -> String {
    if insertion.contains(char::is_whitespace) {
        format!("\"{insertion}\"")
    } else {
        insertion.to_owned()
    }
}

fn tool_text(content: &[ToolCallContent]) -> Option<String> {
    let mut text = String::new();
    for item in content {
        let piece = match item {
            ToolCallContent::Content(content) => match &content.content {
                ContentBlock::Text(block) => block.text.clone(),
                _ => continue,
            },
            ToolCallContent::Diff(diff) => diff.path.display().to_string(),
            _ => continue,
        };
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&piece);
    }
    (!text.is_empty()).then_some(text)
}

impl Render for ChatApp {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .relative()
            .size_full()
            .child(
                h_flex()
                    .size_full()
                    .bg(cx.theme().background)
                    .text_color(cx.theme().foreground)
                    .child(self.sidebar(cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .h_full()
                            .min_w_0()
                            .child(self.header(cx))
                            .child(self.transcript(cx))
                            .children(
                                self.pending
                                    .as_ref()
                                    .map(|pending| self.permission_card(pending, cx)),
                            )
                            .child(self.composer(cx))
                            .child(self.status_bar(cx)),
                    ),
            )
            .children(self.pick.as_ref().map(|pick| self.pick_modal(pick, cx)))
    }
}

#[cfg(test)]
mod tests {
    use super::{ArgumentMenu, SLASH_PREFIX, argument_slot, quote_argument, slash_query};
    use crate::acp::ArgItem;
    use test_case::test_case;

    #[test]
    fn slash_query_accepts_the_bare_prefix() {
        assert_eq!(slash_query(&SLASH_PREFIX.to_string()), Some(String::new()));
    }

    #[test_case("/inherit", None ; "command without arguments")]
    #[test_case("hello there", None ; "plain text")]
    #[test_case("/inherit ", Some((9, "")) ; "empty first argument")]
    #[test_case("/inherit a", Some((9, "a")) ; "first argument typed")]
    #[test_case("/inherit all c", Some((13, "c")) ; "later argument typed")]
    #[test_case("/inherit ALL ", Some((13, "")) ; "empty later argument")]
    fn argument_slot_finds_the_typed_token(value: &str, expected: Option<(usize, &str)>) {
        let slot = argument_slot(value).map(|slot| (slot.start, slot.query));
        assert_eq!(
            slot,
            expected.map(|(start, query)| (start, query.to_owned()))
        );
    }

    #[test_case("all", "all" ; "single word")]
    #[test_case("import mcp", "\"import mcp\"" ; "quoted when it has a space")]
    fn quote_argument_wraps_only_spaced_values(insertion: &str, expected: &str) {
        assert_eq!(quote_argument(insertion), expected);
    }

    #[test]
    fn argument_menu_filters_by_label_prefix() {
        let items = ["all", "conversations", "import-mcp"]
            .into_iter()
            .map(|label| ArgItem {
                label: label.to_owned(),
                insertion: label.to_owned(),
                description: None,
            })
            .collect();
        let mut menu = ArgumentMenu::new(items, "c");
        assert_eq!(menu.matches.len(), 1);
        assert_eq!(
            menu.selected().map(|item| item.label.as_str()),
            Some("conversations")
        );
        menu.filter("");
        assert!(menu.is_open());
        menu.filter("zzz");
        assert!(!menu.is_open());
    }

    #[test_case("/mod", Some("mod".to_owned()) ; "partial name")]
    #[test_case("/MOD", Some("mod".to_owned()) ; "lowercased")]
    #[test_case("/model opus", None ; "argument typed")]
    #[test_case(" /mod", None ; "not at start")]
    #[test_case("hello", None ; "plain text")]
    #[test_case("", None ; "empty")]
    fn slash_query_filters(value: &str, expected: Option<String>) {
        assert_eq!(slash_query(value), expected);
    }
}
