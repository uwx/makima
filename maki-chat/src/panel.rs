use std::path::PathBuf;
use std::time::{Duration, Instant};

use agent_client_protocol_schema::{
    AvailableCommand, ContentBlock, PermissionOption, SessionUpdate, ToolCallContent,
    ToolCallStatus,
};
use gpui_kit::component::{
    ActiveTheme, Icon, IconName, Sizable, Theme, ThemeMode,
    button::Button,
    h_flex,
    input::{InputEvent, Textarea, TextareaState},
    scroll::ScrollableElement,
    text::{TextView, TextViewState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde_json::Value;

use crate::backend::{Backend, Command, Started};
use maki_acp_client as acp;
use maki_acp_client::{
    ArgItem, Event, PickItem, PickOutcome, RewindTurn, SessionEntry, ShareInfo, Target,
};

const SEND_LABEL: &str = "Send";
const STOP_LABEL: &str = "Stop";
const COMPOSER_PLACEHOLDER: &str = "Ask makima to explore, explain, or change the code";
const COMPOSER_HINT: &str = "Enter sends, Shift+Enter adds a line, / opens commands";
const TRANSCRIPT_MAX_WIDTH: f32 = 860.;
/// Below this the composer and status bar drop what a phone has no room for.
const NARROW_WIDTH: f32 = 600.;
const PICK_WIDTH: f32 = 560.;
const PICK_MAX_HEIGHT: f32 = 480.;
const MAX_SLASH_ROWS: usize = 8;
const LIGHT_ACCENT: f32 = 0.6;
const ON_LIGHT_ACCENT: [u8; 3] = [0x1a, 0x1a, 0x1a];
const ON_DARK_ACCENT: [u8; 3] = [0xf5, 0xf5, 0xf5];
const ACTIVE_DESCRIPTION_ALPHA: f32 = 0.75;
const THEME_COMMAND: &str = "theme";
const THEME_DESCRIPTION: &str = "Switch the color theme";
const THEME_PICKER_TITLE: &str = "Theme";
const THEMES_UNAVAILABLE: &str = "This agent does not provide themes";
const SESSIONS_COMMAND: &str = "sessions";
const SESSIONS_DESCRIPTION: &str = "Browse and switch sessions";
const SESSION_PICKER_TITLE: &str = "Sessions";
const SESSIONS_EMPTY: &str = "No stored sessions in this folder";
const SESSION_SHORT_CHARS: usize = 8;
const SESSION_CLOCK_FORMAT: &str = "%b %d %H:%M";
const OPEN_ELSEWHERE: &str = " Â· open elsewhere";
const SESSION_PICK_HINT_ACTION: &str = "open";
const REWIND_TITLE: &str = "Rewind";
const REWIND_EMPTY: &str = "No user turns to rewind to";
const REWIND_PICK_HINT_ACTION: &str = "rewind";
const ESC_REWIND_HINT: &str = "Press Esc again to rewind...";
const ESC_CANCEL_HINT: &str = "Press Esc again to stop...";
const ESCAPE_KEY: &str = "escape";
const DOUBLE_TAP: Duration = Duration::from_millis(1500);
const PICK_HINT: &str = "Enter";
const YOLO_BADGE: &str = "[yolo]";
const NORMAL_BADGE: &str = "[normal]";
const SHARE_LABEL: &str = "Share";
const SHARE_STOP_LABEL: &str = "Stop sharing";
const SHARE_LINK_LABEL: &str = "Copy link";
const SHARE_TOKEN_LABEL: &str = "Copy token";
const SHARE_BADGE: &str = "[shared]";
const SHARED_NOTICE: &str = "Sharing this session";
const SHARE_STOPPED: &str = "This session is no longer shared";
const LINK_COPIED: &str = "Link copied";
const TOKEN_COPIED: &str = "Token copied";
pub const CONNECT_COMMAND: &str = "connect";
const CONNECT_USAGE: &str = "Usage: /connect lan://host:port <token>";
pub const REMOTE_GROUP: &str = "Remote";
pub const REMOTE_TOKEN_HINT: &str = "Type the token the sharing window shows, then press Enter";
pub const DISCOVERY_WAIT: Duration = Duration::from_secs(2);
const THINKING_LABEL: &str = "thinking";
const CLOCK_FORMAT: &str = "%H:%M";
const THOUSAND: f64 = 1_000.0;
const MILLION: f64 = 1_000_000.0;
const PICK_HINT_ACTION: &str = "apply";
const ARGUMENT_MAX_HEIGHT: f32 = 320.;
pub const SLASH_PREFIX: char = '/';
const DISCONNECTED: &str = "Agent is not connected";
pub const NEW_TAB_KEY: &str = "t";
pub const CLOSE_TAB_KEY: &str = "w";
pub const NEW_TAB_LABEL: &str = "New session";
pub const SESSIONS_GROUP: &str = "Sessions";
const CONNECTING_LABEL: &str = "Connecting...";
const SESSION_ENDED_LABEL: &str = "Session ended";
const START_FAILED_LABEL: &str = "Start failed";
const MAX_DIFF_LINES: usize = 400;
const DIFF_TRUNCATED: &str = "... diff truncated";

enum ChatView {
    User {
        text: SharedString,
        at: SharedString,
    },
    Assistant {
        state: Entity<TextViewState>,
        at: SharedString,
    },
    Thinking {
        text: String,
        at: SharedString,
    },
    Tool(ToolCard),
    Notice(SharedString),
}

struct ToolCard {
    id: String,
    name: SharedString,
    summary: SharedString,
    output: SharedString,
    diff: SharedString,
    done: bool,
    failed: bool,
}

#[derive(Default)]
struct Status {
    model: SharedString,
    context: Option<u64>,
    context_window: u64,
    cost: Option<String>,
    thinking: Option<SharedString>,
    yolo: bool,
}

struct PendingPermission {
    request: Value,
    title: String,
    options: Vec<PermissionOption>,
    diff: Option<String>,
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
    target: PickTarget,
    title: String,
    footer: Vec<(String, String)>,
    items: Vec<PickItem>,
    active: usize,
}

enum PickTarget {
    Agent(Value),
    Themes,
    Sessions(Vec<String>),
    Rewind(Vec<usize>),
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

/// What a panel asks of the window, which alone can open a tab for another
/// agent.
pub enum PanelEvent {
    /// Attach to the agent at this link, with the token it expects.
    Connect { link: String, token: String },
}

impl EventEmitter<PanelEvent> for SessionPanel {}

/// One agent session: its connection, transcript, composer, and overlays.
pub struct SessionPanel {
    backend: Option<Backend>,
    closed: bool,
    failed: bool,
    input: Entity<TextareaState>,
    views: Vec<ChatView>,
    scroll: ScrollHandle,
    busy: bool,
    /// Set while this window shares the session with other machines.
    share: Option<ShareInfo>,
    pending: Option<PendingPermission>,
    pick: Option<PendingPick>,
    status: Status,
    commands: Vec<AvailableCommand>,
    themes: Vec<String>,
    theme: Option<String>,
    slash: Option<SlashMenu>,
    slash_dismissed: bool,
    arguments: Option<ArgumentMenu>,
    arguments_dismissed: bool,
    last_escape: Option<Instant>,
    _subscriptions: Vec<Subscription>,
    _events: Option<Task<()>>,
}

impl SessionPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(COMPOSER_PLACEHOLDER)
                .submit_on_enter(true)
                .auto_grow(1, 8)
        });
        let subscriptions = vec![cx.subscribe_in(
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
        )];
        Self {
            backend: None,
            closed: false,
            failed: false,
            input,
            views: Vec::new(),
            scroll: ScrollHandle::new(),
            busy: false,
            share: None,
            pending: None,
            pick: None,
            status: Status::default(),
            commands: Vec::new(),
            themes: Vec::new(),
            theme: None,
            slash: None,
            slash_dismissed: false,
            arguments: None,
            arguments_dismissed: false,
            last_escape: None,
            _subscriptions: subscriptions,
            _events: None,
        }
    }

    /// A panel starts empty and gets its agent once the connection is up, so
    /// opening a tab never blocks the window.
    pub fn attach(
        &mut self,
        started: Result<Started, String>,
        apply_theme: bool,
        cx: &mut Context<Self>,
    ) {
        match started {
            Ok(Started { backend, events }) => {
                // A panel that failed once gets a second chance, so the old
                // error and its transcript must not linger under the replay.
                self.failed = false;
                self.closed = false;
                self.busy = false;
                self.pending = None;
                self.views.clear();
                self.status = session_status(&backend);
                self.themes = backend.themes.names.clone();
                self.theme = backend
                    .themes
                    .current
                    .as_ref()
                    .map(|theme| theme.name.clone());
                if apply_theme && let Some(theme) = backend.themes.current.clone() {
                    apply_palette(&theme, cx);
                }
                self.backend = Some(backend);
                self._events = Some(cx.spawn(async move |this, cx| {
                    while let Ok(event) = events.recv_async().await {
                        if this
                            .update(cx, |panel, cx| panel.on_event(event, cx))
                            .is_err()
                        {
                            break;
                        }
                    }
                }));
            }
            Err(error) => {
                self.failed = true;
                self.notice(format!("Could not start a session: {error}"));
            }
        }
        cx.notify();
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.input.read(cx).focus_handle(cx).focus(window, cx);
    }

    pub fn title(&self) -> String {
        match &self.backend {
            Some(backend) => format!("Session {}", backend.session),
            None if self.failed => START_FAILED_LABEL.to_owned(),
            None if self.closed => SESSION_ENDED_LABEL.to_owned(),
            None => CONNECTING_LABEL.to_owned(),
        }
    }

    pub fn location(&self) -> String {
        self.backend
            .as_ref()
            .map(|backend| format!("{} Â· {}", backend.target, backend.cwd.display()))
            .unwrap_or_default()
    }

    /// A window that opens a new tab inherits the theme already in use, so a
    /// live switch does not reset when another tab appears.
    pub fn inherit_theme(&mut self, theme: Option<String>) {
        if theme.is_some() {
            self.theme = theme;
        }
    }

    pub fn theme_name(&self) -> Option<String> {
        self.theme.clone()
    }

    pub fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_owned();
        if text.is_empty() {
            return;
        }
        if self.run_local_command(&text, cx) {
            self.input.update(cx, |state, cx| state.clean(window, cx));
            self.slash = None;
            self.arguments = None;
            cx.notify();
            return;
        }
        if self.busy {
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
        self.views.push(ChatView::User {
            text: text.into(),
            at: clock(),
        });
        self.views.push(ChatView::Assistant {
            state: cx.new(|cx| TextViewState::markdown("", cx)),
            at: clock(),
        });
        self.busy = true;
        self.pending = None;
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    fn run_local_command(&mut self, text: &str, cx: &mut Context<Self>) -> bool {
        match local_command(text) {
            Some(LocalCommand::ThemePicker) => self.open_theme_picker(),
            Some(LocalCommand::Theme(name)) => self.switch_theme(name),
            Some(LocalCommand::Sessions(query)) => self.open_sessions(query),
            Some(LocalCommand::Connect {
                link,
                token: Some(token),
            }) => self.connect(link, token, cx),
            Some(LocalCommand::Connect { token: None, .. }) => {
                self.notice(CONNECT_USAGE.to_owned())
            }
            None => return false,
        }
        true
    }

    /// Asks the window for a tab on the agent at {link}.
    fn connect(&mut self, link: String, token: String, cx: &mut Context<Self>) {
        if let Err(error) = Target::lan(&link, Some(&token)) {
            self.notice(error);
            cx.notify();
            return;
        }
        cx.emit(PanelEvent::Connect { link, token });
    }

    /// Puts the start of a connect command in the composer for a window that
    /// still has to type the token.
    pub fn prefill(
        &mut self,
        text: String,
        notice: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.input
            .update(cx, |state, cx| state.set_value(text, window, cx));
        self.notice(notice);
        self.focus(window, cx);
        cx.notify();
    }

    fn start_share(&mut self, cx: &mut Context<Self>) {
        let Some(backend) = &self.backend else {
            return;
        };
        if let Err(error) = backend.share() {
            self.notice(error);
        }
        cx.notify();
    }

    fn stop_share(&mut self, cx: &mut Context<Self>) {
        let Some(backend) = &self.backend else {
            return;
        };
        if let Err(error) = backend.unshare() {
            self.notice(error);
        }
        cx.notify();
    }

    fn copy(&mut self, text: String, copied: &str, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.notice(copied.to_owned());
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
                    diff: prompt.diff,
                });
            }
            Event::Flash(text) => self.notice(text),
            Event::Share(result) => {
                match result {
                    Ok(share) => {
                        self.notice(format!(
                            "{SHARED_NOTICE} at {} with token {}",
                            share.link, share.token
                        ));
                        self.share = Some(share);
                    }
                    Err(error) => self.notice(format!("Could not share this session: {error}")),
                }
                cx.notify();
            }
            Event::Unshare(result) => {
                match result {
                    Ok(()) => {
                        self.share = None;
                        self.notice(SHARE_STOPPED.to_owned());
                    }
                    Err(error) => self.notice(format!("Could not stop sharing: {error}")),
                }
                cx.notify();
            }
            Event::Pick(prompt) => self.open_pick(prompt),
            Event::Completion { text, items } => {
                self.apply_arguments(text, items, cx);
                return;
            }
            Event::Theme(result) => match result {
                Ok(palette) => {
                    apply_palette(&palette, cx);
                    self.theme = Some(palette.name.clone());
                    self.notice(format!("Theme: {}", palette.name));
                }
                Err(error) => self.notice(format!("Theme failed: {error}")),
            },
            Event::Sessions(result) => match result {
                Ok(entries) => self.open_session_picker(entries),
                Err(error) => self.notice(format!("Sessions unavailable: {error}")),
            },
            Event::Loaded(result) => {
                self.busy = false;
                match result {
                    Ok(loaded) => {
                        self.status.model = loaded.model.into();
                        self.status.thinking = loaded.options.thinking.map(SharedString::from);
                        self.status.yolo = loaded.options.yolo;
                        if let Some(backend) = &mut self.backend {
                            backend.session = acp::short_session(&loaded.session);
                        }
                        self.notice(format!(
                            "Loaded session {}",
                            acp::short_session(&loaded.session)
                        ));
                    }
                    Err(error) => self.notice(format!("Session not opened: {error}")),
                }
            }
            Event::RewindTurns(result) => match result {
                Ok(turns) => self.open_rewind_picker(turns),
                Err(error) => self.notice(format!("Rewind unavailable: {error}")),
            },
            // The panel draws the transcript as it streams; the committed
            // copy is for a client that runs its own agent, like the TUI.
            Event::Committed { .. } => {}
            Event::Rewound => {
                self.views.clear();
                self.pending = None;
                self.arguments = None;
                self.slash = None;
                self.busy = false;
                self.status.context = None;
                self.scroll.scroll_to_bottom();
            }
            Event::RewindDone(result) => {
                self.busy = false;
                match result {
                    Ok(draft) => self.restore_draft(draft, cx),
                    Err(error) => self.notice(format!("Rewind failed: {error}")),
                }
            }
            Event::PromptDone(result) => {
                self.busy = false;
                self.pending = None;
                if let Err(error) = result {
                    self.notice(error);
                }
            }
            Event::Attached { busy } => self.busy = busy,
            Event::Turn(running) => self.busy = running,
            Event::Closed(reason) => {
                self.backend = None;
                self.closed = true;
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
            target: PickTarget::Agent(prompt.id),
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
        match &pick.target {
            PickTarget::Agent(request) => {
                let outcome = match end {
                    PickEnd::Choose => PickOutcome::Choice(pick.active),
                    PickEnd::Delete => PickOutcome::Delete(pick.active),
                    PickEnd::Close => PickOutcome::Close,
                };
                if let Some(backend) = &self.backend {
                    backend.answer_pick(request, outcome);
                }
            }
            PickTarget::Themes => {
                if let (PickEnd::Choose, Some(name)) = (end, self.themes.get(pick.active)) {
                    self.switch_theme(name.clone());
                }
            }
            PickTarget::Sessions(ids) => {
                if let (PickEnd::Choose, Some(id)) = (end, ids.get(pick.active)) {
                    self.resume_session(id.clone());
                }
            }
            PickTarget::Rewind(offsets) => {
                if let (PickEnd::Choose, Some(offset)) = (end, offsets.get(pick.active)) {
                    self.rewind(*offset);
                }
            }
        }
        cx.notify();
    }

    fn open_theme_picker(&mut self) {
        let items = self
            .themes
            .iter()
            .map(|name| PickItem {
                label: name.clone(),
                detail: None,
                section: None,
            })
            .collect::<Vec<_>>();
        if items.is_empty() {
            self.notice(THEMES_UNAVAILABLE.to_owned());
            return;
        }
        let active = self
            .theme
            .as_ref()
            .and_then(|theme| self.themes.iter().position(|name| name == theme))
            .unwrap_or(0);
        self.pick = Some(PendingPick {
            target: PickTarget::Themes,
            title: THEME_PICKER_TITLE.to_owned(),
            footer: vec![(PICK_HINT.to_owned(), PICK_HINT_ACTION.to_owned())],
            items,
            active,
        });
    }

    fn switch_theme(&mut self, name: String) {
        let Some(backend) = &self.backend else {
            self.notice(DISCONNECTED.to_owned());
            return;
        };
        if let Err(error) = backend.set_theme(name) {
            self.notice(error);
        }
    }

    fn open_sessions(&mut self, query: String) {
        let Some(backend) = &self.backend else {
            self.notice(DISCONNECTED.to_owned());
            return;
        };
        if let Err(error) = backend.list_sessions(query) {
            self.notice(error);
        }
    }

    fn open_session_picker(&mut self, entries: Vec<SessionEntry>) {
        let entries = entries
            .into_iter()
            .filter(|entry| !entry.current)
            .collect::<Vec<_>>();
        if entries.is_empty() {
            self.notice(SESSIONS_EMPTY.to_owned());
            return;
        }
        let items = entries
            .iter()
            .map(|entry| PickItem {
                label: entry.title.clone(),
                detail: Some(session_detail(entry)),
                section: None,
            })
            .collect();
        let ids = entries.into_iter().map(|entry| entry.id).collect();
        self.pick = Some(PendingPick {
            target: PickTarget::Sessions(ids),
            title: SESSION_PICKER_TITLE.to_owned(),
            footer: vec![(PICK_HINT.to_owned(), SESSION_PICK_HINT_ACTION.to_owned())],
            items,
            active: 0,
        });
    }

    /// The agent replays the stored transcript as session updates, so the
    /// transcript is emptied first and the replay fills it back.
    fn resume_session(&mut self, id: String) {
        let Some(backend) = &self.backend else {
            self.notice(DISCONNECTED.to_owned());
            return;
        };
        if let Err(error) = backend.load_session(id) {
            self.notice(error);
            return;
        }
        self.views.clear();
        self.pending = None;
        self.arguments = None;
        self.slash = None;
        self.busy = true;
        self.scroll.scroll_to_bottom();
    }

    fn open_rewind(&mut self) {
        let Some(backend) = &self.backend else {
            self.notice(DISCONNECTED.to_owned());
            return;
        };
        if let Err(error) = backend.list_rewind_turns() {
            self.notice(error);
        }
    }

    fn open_rewind_picker(&mut self, turns: Vec<RewindTurn>) {
        if turns.is_empty() {
            self.notice(REWIND_EMPTY.to_owned());
            return;
        }
        let (items, offsets) = rewind_rows(turns);
        self.pick = Some(PendingPick {
            target: PickTarget::Rewind(offsets),
            title: REWIND_TITLE.to_owned(),
            footer: vec![(PICK_HINT.to_owned(), REWIND_PICK_HINT_ACTION.to_owned())],
            items,
            active: 0,
        });
    }

    fn rewind(&mut self, offset: usize) {
        let Some(backend) = &self.backend else {
            self.notice(DISCONNECTED.to_owned());
            return;
        };
        if let Err(error) = backend.rewind(offset) {
            self.notice(error);
            return;
        }
        self.busy = true;
    }

    fn restore_draft(&mut self, draft: String, cx: &mut Context<Self>) {
        if draft.is_empty() {
            return;
        }
        let Some(window) = cx.active_window() else {
            return;
        };
        _ = cx.update_window(window, |_, window, cx| {
            self.input
                .update(cx, |state, cx| state.set_value(draft, window, cx));
            self.input.read(cx).focus_handle(cx).focus(window, cx);
        });
    }

    fn on_update(&mut self, update: SessionUpdate, cx: &mut Context<Self>) {
        match update {
            // Only replayed history carries user chunks; a live prompt is
            // already on screen by the time the agent answers.
            SessionUpdate::UserMessageChunk(chunk) => {
                if let ContentBlock::Text(block) = chunk.content {
                    self.views.push(ChatView::User {
                        text: block.text.into(),
                        at: clock(),
                    });
                }
            }
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
                let diff = fields.content.as_deref().and_then(acp::diff_text);
                self.upsert_tool(&id, "", &summary);
                if let Some(output) = output {
                    self.set_tool_output(&id, output);
                }
                if let Some(diff) = diff {
                    self.set_tool_diff(&id, diff);
                }
                if let Some(status) = fields.status {
                    self.apply_tool_status(&id, status);
                }
            }
            SessionUpdate::AvailableCommandsUpdate(update) => {
                self.commands = update.available_commands;
                for (name, description) in [
                    (THEME_COMMAND, THEME_DESCRIPTION),
                    (SESSIONS_COMMAND, SESSIONS_DESCRIPTION),
                ] {
                    if !self.commands.iter().any(|command| command.name == name) {
                        self.commands.push(AvailableCommand::new(name, description));
                    }
                }
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
                let view = acp::session_view(&update.config_options);
                if let Some(model) = view.model {
                    self.status.model = model.into();
                }
                self.status.thinking = view.thinking.map(SharedString::from);
                self.status.yolo = view.yolo;
            }
            _ => {}
        }
    }

    fn push_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let trailing = self
            .views
            .len()
            .checked_sub(1)
            .filter(|index| matches!(&self.views[*index], ChatView::Assistant { .. }));
        if let Some(index) = trailing {
            if let ChatView::Assistant { state, .. } = &self.views[index] {
                state.update(cx, |state, cx| state.push_str(text, cx));
            }
            return;
        }
        let state = cx.new(|cx| TextViewState::markdown(text, cx));
        self.views.push(ChatView::Assistant { state, at: clock() });
    }

    fn push_thinking(&mut self, text: &str) {
        if let Some(ChatView::Thinking { text: buffer, .. }) = self.views.last_mut() {
            buffer.push_str(text);
            return;
        }
        self.views.push(ChatView::Thinking {
            text: text.to_owned(),
            at: clock(),
        });
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
            diff: SharedString::default(),
            done: false,
            failed: false,
        }));
    }

    fn set_tool_output(&mut self, id: &str, output: String) {
        if let Some(card) = self.tool_mut(id) {
            card.output = output.into();
        }
    }

    fn set_tool_diff(&mut self, id: &str, diff: String) {
        if let Some(card) = self.tool_mut(id) {
            card.diff = diff.into();
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
        let insertion = argument_insertion(&item.insertion, slot.braced);
        self.input.update(cx, |state, cx| {
            state.set_value(head, window, cx);
            state.insert(insertion, window, cx);
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

    pub fn on_key(&mut self, key: &Keystroke, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if key.key != ESCAPE_KEY {
            self.last_escape = None;
        }
        if self.pick.is_some() {
            return self.on_pick_key(key, cx);
        }
        if self.on_slash_key(key, window, cx) {
            return true;
        }
        if self.on_argument_key(key, window, cx) {
            return true;
        }
        if key.key == ESCAPE_KEY
            && !key.modifiers.control
            && !key.modifiers.alt
            && !key.modifiers.platform
        {
            return self.on_escape(cx);
        }
        false
    }

    /// Double Esc cancels a running turn or opens the rewind picker, like the
    /// TUI; one Esc only arms the second press and says what it would do.
    fn on_escape(&mut self, cx: &mut Context<Self>) -> bool {
        let repeated = self
            .last_escape
            .take()
            .is_some_and(|at| at.elapsed() < DOUBLE_TAP);
        if !repeated {
            self.last_escape = Some(Instant::now());
            self.notice(
                if self.busy {
                    ESC_CANCEL_HINT
                } else {
                    ESC_REWIND_HINT
                }
                .to_owned(),
            );
            cx.notify();
            return true;
        }
        if self.busy {
            self.cancel(cx);
        } else {
            self.open_rewind();
        }
        true
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
            ChatView::User { text, at } => v_flex()
                .w_full()
                .items_end()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(at.clone()),
                )
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
            ChatView::Assistant { state, at } => v_flex()
                .w_full()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(Icon::new(IconName::Bot))
                        .child("Assistant")
                        .child(div().flex_1())
                        .child(at.clone()),
                )
                .child(TextView::new(state).selectable(true))
                .into_any_element(),
            ChatView::Thinking { text, at } => v_flex()
                .w_full()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(Icon::new(IconName::Cpu))
                        .child("Thinking")
                        .child(div().flex_1())
                        .child(at.clone()),
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
            .when(!card.diff.is_empty(), |this| {
                this.child(diff_block(&card.diff, cx))
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
            .when_some(pending.diff.as_deref(), |this, diff| {
                this.child(diff_block(diff, cx))
            })
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
                        meta.push_str(" Â· ");
                    }
                    meta.push_str(detail);
                }
                let (label, description) = row_colors(active, cx);
                v_flex()
                    .id(("pick", row))
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .when(active, |this| this.bg(cx.theme().accent))
                    .child(div().text_sm().text_color(label).child(item.label.clone()))
                    .when(!meta.is_empty(), |this| {
                        this.child(div().text_xs().text_color(description).child(meta))
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
                let (label, description) = row_colors(active, cx);
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
                            .text_color(label)
                            .child(format!("{SLASH_PREFIX}{}", command.name)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .overflow_hidden()
                            .text_color(description)
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
                let (label, detail) = row_colors(active, cx);
                h_flex()
                    .id(("argument", row))
                    .w_full()
                    .gap_3()
                    .items_center()
                    .px_3()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .when(active, |this| this.bg(cx.theme().accent))
                    .child(div().text_sm().text_color(label).child(item.label.clone()))
                    .when_some(item.description.clone(), |this, description| {
                        this.child(
                            div()
                                .flex_1()
                                .text_xs()
                                .overflow_hidden()
                                .text_color(detail)
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

    fn composer(&self, narrow: bool, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w_full()
            .border_t_1()
            .border_color(cx.theme().border)
            // A finger is less precise than a pointer, so the whole row focuses
            // the field rather than only the text.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    let handle = this.input.read(cx).focus_handle(cx);
                    handle.focus(window, cx);
                }),
            )
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
                                    .when(!narrow, |this| this.child(COMPOSER_HINT)),
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

    fn status_bar(&self, narrow: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let context = match self.status.context {
            Some(used) if self.status.context_window > 0 => {
                let percent = used.saturating_mul(100) / self.status.context_window;
                let window = compact_tokens(self.status.context_window);
                format!("{}/{window} ({percent}%)", compact_tokens(used))
            }
            _ => String::new(),
        };
        let thinking = self
            .status
            .thinking
            .as_ref()
            .map(|thinking| format!("{THINKING_LABEL}: {thinking}"));
        let permission = if self.status.yolo {
            YOLO_BADGE
        } else {
            NORMAL_BADGE
        };
        let can_share = self
            .backend
            .as_ref()
            .is_some_and(|backend| backend.can_share);

        h_flex()
            .w_full()
            .items_center()
            .flex_wrap()
            .gap_3()
            .px_4()
            .py_1()
            .text_xs()
            .border_t_1()
            .border_color(cx.theme().border)
            .text_color(cx.theme().muted_foreground)
            .child(self.status.model.clone())
            .child(div().flex_1())
            .children(thinking.filter(|_| !narrow))
            .when(!context.is_empty(), |this| this.child(context))
            .children(self.status.cost.clone())
            .when_some(self.share.clone(), |this, share| {
                let (link, token) = (share.link.clone(), share.token.clone());
                this.child(
                    div()
                        .text_xs()
                        .child(format!("{SHARE_BADGE} {}", share.address)),
                )
                .child(
                    Button::new("share-link")
                        .label(SHARE_LINK_LABEL)
                        .small()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.copy(link.clone(), LINK_COPIED, cx)
                        })),
                )
                .child(
                    Button::new("share-token")
                        .label(SHARE_TOKEN_LABEL)
                        .small()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.copy(token.clone(), TOKEN_COPIED, cx)
                        })),
                )
                .child(
                    Button::new("share-stop")
                        .label(SHARE_STOP_LABEL)
                        .small()
                        .on_click(cx.listener(|this, _, _, cx| this.stop_share(cx))),
                )
            })
            .when(self.share.is_none() && can_share, |this| {
                this.child(
                    Button::new("share")
                        .label(SHARE_LABEL)
                        .small()
                        .on_click(cx.listener(|this, _, _, cx| this.start_share(cx))),
                )
            })
            .child(permission)
    }
}

impl SessionPanel {
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

#[derive(Debug, PartialEq, Eq)]
enum LocalCommand {
    ThemePicker,
    Theme(String),
    Sessions(String),
    /// A LAN agent to attach to, and the token it expects when it was given.
    Connect {
        link: String,
        token: Option<String>,
    },
}

fn local_command(text: &str) -> Option<LocalCommand> {
    let rest = text.strip_prefix(SLASH_PREFIX)?;
    if let Some(name) = rest.strip_prefix(THEME_COMMAND) {
        return Some(match name.trim() {
            "" => LocalCommand::ThemePicker,
            name => LocalCommand::Theme(name.to_owned()),
        });
    }
    if let Some(arguments) = rest.strip_prefix(CONNECT_COMMAND) {
        let mut arguments = arguments.split_whitespace();
        return Some(LocalCommand::Connect {
            link: arguments.next().unwrap_or_default().to_owned(),
            token: arguments.next().map(str::to_owned),
        });
    }
    Some(LocalCommand::Sessions(
        rest.strip_prefix(SESSIONS_COMMAND)?.trim().to_owned(),
    ))
}

/// The token being typed after the command name, where it starts, and whether
/// it sits inside a `{..}` field list that a space does not close.
struct ArgumentSlot {
    start: usize,
    query: String,
    braced: bool,
}

const FIELD_OPEN: char = '{';
const FIELD_CLOSE: char = '}';
const FIELD_SEPARATOR: char = ',';

fn argument_slot(value: &str) -> Option<ArgumentSlot> {
    let command_end = value.find(char::is_whitespace)?;
    if !value.starts_with(SLASH_PREFIX) {
        return None;
    }
    let arguments = &value[command_end..];
    if let Some(open) = arguments
        .rfind(FIELD_OPEN)
        .filter(|open| !arguments[*open..].contains(FIELD_CLOSE))
    {
        let inner = &arguments[open + 1..];
        let field_start = inner
            .rfind(FIELD_SEPARATOR)
            .map_or(0, |separator| separator + 1);
        let after_separator = &inner[field_start..];
        let field = after_separator.trim_start();
        return Some(ArgumentSlot {
            start: command_end + open + 1 + field_start + after_separator.len() - field.len(),
            query: field.to_lowercase(),
            braced: true,
        });
    }
    if value.ends_with(char::is_whitespace) {
        return Some(ArgumentSlot {
            start: value.len(),
            query: String::new(),
            braced: false,
        });
    }
    let token = arguments.trim_start().rsplit(char::is_whitespace).next()?;
    Some(ArgumentSlot {
        start: value.len() - token.len(),
        query: token.to_lowercase(),
        braced: false,
    })
}

/// A directory insertion leaves the token open so the next level lists, and
/// everything else gets the separating space done.
/// Paints the window with the CLI theme the agent reported: the roles it
/// knows are applied, everything else keeps the desktop default.
fn apply_palette(palette: &acp::ThemePalette, cx: &mut App) {
    let mode = if palette.is_light() {
        ThemeMode::Light
    } else {
        ThemeMode::Dark
    };
    Theme::change(mode, None, cx);
    let colors = &mut Theme::global_mut(cx).colors;
    let paint = |role: &str, slot: &mut Hsla| {
        if let Some(color) = palette.rgb(role) {
            *slot = hsla(color);
        }
    };
    paint("background", &mut colors.background);
    paint("foreground", &mut colors.foreground);
    paint("foreground", &mut colors.popover_foreground);
    paint("muted", &mut colors.muted_foreground);
    paint("border", &mut colors.border);
    paint("border", &mut colors.sidebar_border);
    paint("input_border", &mut colors.input);
    paint("selection", &mut colors.selection);
    paint("selection", &mut colors.list_active);
    if let Some(accent) = palette.rgb("accent") {
        // The fills and their text move together, so buttons and rings stay
        // readable on light accents too.
        let fill = hsla(accent);
        colors.accent = fill;
        colors.primary = fill;
        colors.ring = fill;
        colors.caret = fill;
        colors.accent_foreground = accent_text(fill);
        colors.primary_foreground = accent_text(fill);
    }
    paint("tool_background", &mut colors.popover);
    paint("tool_background", &mut colors.sidebar);
    paint("tool_background", &mut colors.muted);
    paint("tool", &mut colors.sidebar_foreground);
    paint("selection", &mut colors.sidebar_accent);
    paint("accent", &mut colors.sidebar_accent_foreground);
    paint("user", &mut colors.sidebar_primary);
    paint("error", &mut colors.danger);
    paint("error", &mut colors.red);
    paint("success", &mut colors.success);
    paint("success", &mut colors.green);
    paint("warning", &mut colors.warning);
    paint("warning", &mut colors.yellow);
}

/// `500000` reads as `500.0k`, matching the CLI status bar.
fn compact_tokens(tokens: u64) -> String {
    match tokens {
        0..1_000 => tokens.to_string(),
        1_000..1_000_000 => format!("{:.1}k", tokens as f64 / THOUSAND),
        _ => format!("{:.1}m", tokens as f64 / MILLION),
    }
}

/// The local time a message was created, e.g. `14:05`.
fn clock() -> SharedString {
    Timestamp::now()
        .to_zoned(TimeZone::system())
        .strftime(CLOCK_FORMAT)
        .to_string()
        .into()
}

/// The status bar state a freshly connected session starts with.
fn session_status(backend: &Backend) -> Status {
    Status {
        model: SharedString::from(backend.model.clone()),
        thinking: backend.options.thinking.clone().map(SharedString::from),
        yolo: backend.options.yolo,
        ..Status::default()
    }
}

/// One picker row's second line: id, when it was last saved, and size.
fn session_detail(entry: &SessionEntry) -> String {
    let short = entry.id.get(..SESSION_SHORT_CHARS).unwrap_or(&entry.id);
    let when = Timestamp::from_second(i64::try_from(entry.updated_at).unwrap_or_default())
        .map(|stamp| {
            stamp
                .to_zoned(TimeZone::system())
                .strftime(SESSION_CLOCK_FORMAT)
                .to_string()
        })
        .unwrap_or_default();
    let mut detail = format!("{short} Â· {when} Â· {} messages", entry.messages);
    if entry.open_elsewhere {
        detail.push_str(OPEN_ELSEWHERE);
    }
    detail
}

/// Newest turn first, like the TUI picker, with the offsets kept in the same
/// order as the rows they label.
fn rewind_rows(turns: Vec<RewindTurn>) -> (Vec<PickItem>, Vec<usize>) {
    let mut items = Vec::with_capacity(turns.len());
    let mut offsets = Vec::with_capacity(turns.len());
    for turn in turns.into_iter().rev() {
        items.push(PickItem {
            label: format!("{}: {}", turn.turn, turn.preview),
            detail: None,
            section: None,
        });
        offsets.push(turn.offset);
    }
    (items, offsets)
}

/// Label and description colors for a row, readable on its own background:
/// the accent fill needs its own foreground, a plain row keeps the theme's.
/// A unified diff in the theme's mono font, added lines in the success color
/// and removed ones in the danger color.
fn diff_block(text: &str, cx: &App) -> AnyElement {
    let mut lines = text.lines().take(MAX_DIFF_LINES + 1).collect::<Vec<_>>();
    let truncated = lines.len() > MAX_DIFF_LINES;
    lines.truncate(MAX_DIFF_LINES);
    v_flex()
        .w_full()
        .px_3()
        .py_2()
        .gap_0()
        .text_xs()
        .font_family(cx.theme().mono_font_family.clone())
        .children(lines.into_iter().map(|line| {
            let color = match line.chars().next() {
                Some('+') => cx.theme().success,
                Some('-') => cx.theme().danger,
                _ => cx.theme().muted_foreground,
            };
            div().text_color(color).child(line.to_owned())
        }))
        .when(truncated, |this| {
            this.child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(DIFF_TRUNCATED),
            )
        })
        .into_any_element()
}

fn row_colors(active: bool, cx: &App) -> (Hsla, Hsla) {
    if !active {
        return (cx.theme().foreground, cx.theme().muted_foreground);
    }
    let label = accent_text(cx.theme().accent);
    (label, label.opacity(ACTIVE_DESCRIPTION_ALPHA))
}

/// Accent fills are light in some themes and dark in others, so the text on
/// them flips with the fill's lightness.
fn accent_text(accent: Hsla) -> Hsla {
    hsla(if accent.l > LIGHT_ACCENT {
        ON_LIGHT_ACCENT
    } else {
        ON_DARK_ACCENT
    })
}

fn hsla([red, green, blue]: [u8; 3]) -> Hsla {
    Hsla::from(rgb(u32::from(red) << 16
        | u32::from(green) << 8
        | u32::from(blue)))
}

fn argument_insertion(insertion: &str, braced: bool) -> String {
    if insertion.ends_with(std::path::is_separator) {
        return insertion.to_owned();
    }
    if braced {
        return format!("{},", quote_argument(insertion));
    }
    format!("{} ", quote_argument(insertion))
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
            _ => continue,
        };
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&piece);
    }
    (!text.is_empty()).then_some(text)
}

impl Render for SessionPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let narrow = window.viewport_size().width < px(NARROW_WIDTH);
        div()
            .relative()
            .size_full()
            .child(
                v_flex()
                    .size_full()
                    .min_w_0()
                    .child(self.header(cx))
                    .child(self.transcript(cx))
                    .children(
                        self.pending
                            .as_ref()
                            .map(|pending| self.permission_card(pending, cx)),
                    )
                    .child(self.composer(narrow, cx))
                    .child(self.status_bar(narrow, cx)),
            )
            .children(self.pick.as_ref().map(|pick| self.pick_modal(pick, cx)))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ArgumentMenu, FIELD_CLOSE, LocalCommand, SLASH_PREFIX, accent_text, argument_insertion,
        argument_slot, compact_tokens, local_command, quote_argument, rewind_rows, slash_query,
    };
    use gpui_kit::{Hsla, rgb};
    use maki_acp_client::{ArgItem, RewindTurn};
    use test_case::test_case;

    #[test]
    fn slash_query_accepts_the_bare_prefix() {
        assert_eq!(slash_query(&SLASH_PREFIX.to_string()), Some(String::new()));
    }

    #[test]
    fn rewind_rows_newest_first_keeps_offsets_aligned() {
        let turns = vec![
            RewindTurn {
                offset: 0,
                turn: 1,
                preview: "first".into(),
            },
            RewindTurn {
                offset: 12,
                turn: 2,
                preview: "second".into(),
            },
        ];
        let (items, offsets) = rewind_rows(turns);
        assert_eq!(items[0].label, "2: second");
        assert_eq!(items[1].label, "1: first");
        assert_eq!(offsets, vec![12, 0]);
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

    #[test_case("/theme", Some(LocalCommand::ThemePicker) ; "bare theme opens the picker")]
    #[test_case("/theme dracula", Some(LocalCommand::Theme("dracula".to_owned())) ; "named theme")]
    #[test_case("/theme  dracula ", Some(LocalCommand::Theme("dracula".to_owned())) ; "trimmed")]
    #[test_case("/themeology", Some(LocalCommand::Theme("ology".to_owned())) ; "prefix match keeps the word")]
    #[test_case("/theming", None ; "other command")]
    #[test_case("/sessions", Some(LocalCommand::Sessions(String::new())) ; "bare sessions opens the picker")]
    #[test_case("/sessions auth", Some(LocalCommand::Sessions("auth".to_owned())) ; "session query")]
    #[test_case(
        "/connect lan://box:7331",
        Some(LocalCommand::Connect { link: "lan://box:7331".to_owned(), token: None })
        ; "a link still waiting for its token"
    )]
    #[test_case(
        "/connect lan://box:7331 abc123",
        Some(LocalCommand::Connect {
            link: "lan://box:7331".to_owned(),
            token: Some("abc123".to_owned()),
        })
        ; "a link with the token it expects"
    )]
    #[test_case(
        "/connect",
        Some(LocalCommand::Connect { link: String::new(), token: None })
        ; "bare connect asks how to use it"
    )]
    #[test_case("hello", None ; "plain text")]
    fn local_command_recognizes_local_commands(text: &str, expected: Option<LocalCommand>) {
        assert_eq!(local_command(text), expected);
    }

    #[test_case("/model {anthropic/opus", (8, "anthropic/opus") ; "first field")]
    #[test_case("/model {anthropic/opus,thin", (23, "thin") ; "next field")]
    #[test_case("/model {a/b, thinking=h", (13, "thinking=h") ; "field after a space")]
    #[test_case("/model {a/b} ", (13, "") ; "closed field list is an argument")]
    fn argument_slot_opens_field_lists(value: &str, expected: (usize, &str)) {
        let slot = argument_slot(value).unwrap();
        assert_eq!((slot.start, slot.query.as_str()), expected);
        assert_eq!(slot.braced, !value.contains(FIELD_CLOSE));
    }

    #[test_case("all", "all" ; "single word")]
    #[test_case("import mcp", "\"import mcp\"" ; "quoted when it has a space")]
    fn quote_argument_wraps_only_spaced_values(insertion: &str, expected: &str) {
        assert_eq!(quote_argument(insertion), expected);
    }

    #[test_case("all", false, "all " ; "command argument closes with a space")]
    #[test_case("import mcp", false, "\"import mcp\" " ; "spaced value is quoted")]
    #[test_case("src/", false, "src/" ; "directory stays open for the next level")]
    #[test_case("thinking=high", true, "thinking=high," ; "field list opens the next field")]
    fn argument_insertion_closes_or_keeps_the_token(insertion: &str, braced: bool, expected: &str) {
        assert_eq!(argument_insertion(insertion, braced), expected);
    }

    #[test_case(0, "0" ; "zero")]
    #[test_case(999, "999" ; "below a thousand")]
    #[test_case(500_000, "500.0k" ; "thousands")]
    #[test_case(1_200_000, "1.2m" ; "millions")]
    fn compact_tokens_matches_the_cli(tokens: u64, expected: &str) {
        assert_eq!(compact_tokens(tokens), expected);
    }

    #[test_case(0xe8dcc8, true ; "beige fill takes dark text")]
    #[test_case(0xc0e8e0, true ; "pale teal fill takes dark text")]
    #[test_case(0x203040, false ; "dark fill takes light text")]
    fn accent_text_flips_with_the_fill_lightness(fill: u32, dark_text: bool) {
        let text = accent_text(Hsla::from(rgb(fill)));
        assert_eq!(text.l < 0.2, dark_text);
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
