//! The session this window watches.
//!
//! One ACP connection, on its own task: the phone shows what the machine that
//! runs the session sends and, for now, sends nothing back.

use std::sync::Arc;

use gpui::{
    App, AppContext, Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled,
    WeakEntity, Window, div, px, rgb,
};
use maki_acp_client::{Client, Connection, ContentBlock, Event, SessionUpdate, short_session};

use crate::attach::Attach;
use crate::{BACKGROUND, FOREGROUND, MUTED, NOTICE};

const USER_MARK: &str = "> ";
const TOOL_MARK: &str = "· ";
const CONNECTING: &str = "connecting...";
const ATTACHED: &str = "watching this session read-only";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    User,
    Assistant,
    Tool,
    Notice,
}

struct Row {
    role: Role,
    text: String,
}

impl Row {
    fn new(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            text: text.into(),
        }
    }
}

enum Status {
    Connecting,
    Watching { model: String },
    Failed(String),
}

pub struct Session {
    status: Status,
    rows: Vec<Row>,
    client: Option<Arc<Client>>,
    context: Option<(u64, u64)>,
}

impl Session {
    /// Opens the connection off the UI thread, then follows it for the life of
    /// the window.
    pub fn open(cx: &mut App, attach: Attach) -> Entity<Self> {
        cx.new(|cx| {
            cx.spawn(async move |this: WeakEntity<Self>, cx| {
                let connected = cx
                    .background_executor()
                    .spawn(async move {
                        attach
                            .target()
                            .and_then(|target| maki_acp_client::connect(&target))
                    })
                    .await;
                let connection = match connected {
                    Ok(connection) => connection,
                    Err(error) => {
                        this.update(cx, |session, cx| {
                            session.status = Status::Failed(error);
                            cx.notify();
                        })
                        .ok();
                        return;
                    }
                };
                let events = connection.events.clone();
                this.update(cx, |session, cx| {
                    session.attach_to(&connection);
                    cx.notify();
                })
                .ok();
                while let Ok(event) = events.recv_async().await {
                    let alive = this
                        .update(cx, |session, cx| {
                            session.apply(event);
                            cx.notify();
                        })
                        .is_ok();
                    if !alive {
                        break;
                    }
                }
            })
            .detach();
            Self {
                status: Status::Connecting,
                rows: Vec::new(),
                client: None,
                context: None,
            }
        })
    }

    fn attach_to(&mut self, connection: &Connection) {
        self.client = Some(Arc::clone(&connection.client));
        self.status = Status::Watching {
            model: connection.model.clone(),
        };
        self.rows.push(Row::new(
            Role::Notice,
            format!("{} · {}", short_session(&connection.session), ATTACHED),
        ));
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::Update(update) => self.update(*update),
            Event::Closed(reason) => self.status = Status::Failed(reason),
            _ => {}
        }
    }

    fn update(&mut self, update: SessionUpdate) {
        match update {
            SessionUpdate::UserMessageChunk(chunk) => {
                if let ContentBlock::Text(text) = chunk.content {
                    self.push_text(Role::User, text.text);
                }
            }
            SessionUpdate::AgentMessageChunk(chunk) => {
                if let ContentBlock::Text(text) = chunk.content {
                    self.push_text(Role::Assistant, text.text);
                }
            }
            SessionUpdate::ToolCall(call) => {
                self.rows.push(Row::new(Role::Tool, call.title));
            }
            SessionUpdate::UsageUpdate(usage) => self.context = Some((usage.used, usage.size)),
            _ => {}
        }
    }

    /// A chunk continues the row it belongs to, so one message is one row and
    /// the text wraps instead of stacking.
    fn push_text(&mut self, role: Role, text: String) {
        match self.rows.last_mut() {
            Some(row) if row.role == role => row.text.push_str(&text),
            _ => self.rows.push(Row::new(role, text)),
        }
    }

    fn header(&self) -> impl IntoElement {
        let (line, detail) = match &self.status {
            Status::Connecting => (CONNECTING.to_owned(), None),
            Status::Watching { model } => (model.clone(), self.context.map(percent)),
            Status::Failed(error) => (error.clone(), None),
        };
        div()
            .w_full()
            .px_4()
            .py_3()
            .flex()
            .flex_col()
            .gap_1()
            .border_b(px(1.))
            .border_color(rgb(NOTICE))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(FOREGROUND))
                    .child(SharedString::from(line)),
            )
            .children(detail.map(|detail| {
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(SharedString::from(detail))
            }))
    }

    fn body(&self) -> impl IntoElement {
        let rows: Vec<_> = self.rows.iter().map(render_row).collect();
        div()
            .flex_1()
            .w_full()
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap_4()
            .px_4()
            .py_3()
            .children(rows)
    }
}

fn percent((used, size): (u64, u64)) -> String {
    let percent = if size == 0 { 0 } else { used * 100 / size };
    format!("{percent}% of {size} tokens")
}

fn render_row(row: &Row) -> impl IntoElement {
    let (color, text) = match row.role {
        Role::User => (FOREGROUND, format!("{USER_MARK}{}", row.text)),
        Role::Assistant => (FOREGROUND, row.text.clone()),
        Role::Tool => (MUTED, format!("{TOOL_MARK}{}", row.text)),
        Role::Notice => (MUTED, row.text.clone()),
    };
    div()
        .w_full()
        .text_sm()
        .text_color(rgb(color))
        .child(SharedString::from(text))
}

impl Render for Session {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BACKGROUND))
            .child(self.header())
            .child(self.body())
    }
}
