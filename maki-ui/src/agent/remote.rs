//! The TUI's turns on an agent another machine runs.
//!
//! [`AcpBackend`] is the local actor's execution half: the actor keeps owning
//! history, queueing, cancellation and outcomes, while each turn is a
//! `session/prompt` over ACP. Its `session/update` stream is turned back into
//! [`AgentEvent`]s, so the chat renderer, the permission prompt and the turn
//! outcome path all stay exactly what they are for a local session.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use agent_client_protocol_schema::{
    ContentBlock, ImageContent, PermissionOption, PermissionOptionKind, SessionUpdate, StopReason,
    TextContent, ToolCall, ToolCallContent, ToolCallUpdate, UsageUpdate,
};
use flume::Receiver;
use futures_lite::future::or;
use maki_acp_client::{Client, Connection, PermissionPrompt};
use maki_agent::actor::{ActorBackend, BackendResult, ControlWork, TurnContext, WorkKind};
use maki_agent::permissions::{PermissionAnswer, TaggedAnswer};
use maki_agent::{
    ActorWorkIdentity, ActorWorkKind, AgentEvent, AgentId, AgentInput, DoneReason, Envelope,
    EventSender, History, ToolDoneEvent, ToolOutput, ToolStartEvent, TurnCancellationReason,
    TurnFailure, TurnFailureKind, TurnId, TurnOutcome,
};
use maki_config::ToolKey;
use maki_domain::{Post, PostLevel};
use maki_providers::{Message, TokenUsage};
use serde_json::Value;
use tracing::warn;

use crate::app::MIRROR_PRESENTATION;

const TOOL_UNAVAILABLE: &str = "This command only works on a session this window runs";
const PERMISSION_PREFIX: &str = "remote-";
const REMOTE_TOOL: &str = "unknown";
const MCP_PROMPT_UNAVAILABLE: &str = "MCP prompts run where the session lives, not in this window";
const MIRROR_LABEL: &str = "This turn is running in another window";

pub(crate) struct AcpBackend {
    client: Arc<Client>,
    /// The turn this window runs, if any. The reader hands it every event that
    /// streams in and paints the rest into the transcript.
    turn: SharedTurn,
    /// Asks the reader to drain what already arrived, so a turn does not settle
    /// before the last of its updates is rendered.
    settle: flume::Sender<flume::Sender<()>>,
    agent_tx: flume::Sender<Envelope>,
    model: String,
}

impl AcpBackend {
    pub(crate) fn new(
        connection: &Connection,
        agent_tx: flume::Sender<Envelope>,
        answers: Receiver<String>,
    ) -> Self {
        let turn = SharedTurn::default();
        let permissions = Permissions::default();
        let (settle, settled) = flume::unbounded();
        let reader = Reader {
            events: connection.events.clone(),
            settled,
            turn: turn.clone(),
            mirror: Mirror::new(agent_tx.clone(), connection.model.clone()),
            permissions: permissions.clone(),
        };
        smol::spawn(reader.run()).detach();
        let client = Arc::clone(&connection.client);
        smol::spawn(async move {
            while let Ok(raw) = answers.recv_async().await {
                permissions.answer(&client, &raw);
            }
        })
        .detach();
        Self {
            client: Arc::clone(&connection.client),
            turn,
            settle,
            agent_tx,
            model: connection.model.clone(),
        }
    }
}

impl ActorBackend for AcpBackend {
    fn run_turn<'a>(
        &'a mut self,
        _history: &'a mut History,
        context: TurnContext,
        input: AgentInput,
        _work: WorkKind,
    ) -> Pin<Box<dyn Future<Output = BackendResult> + Send + 'a>> {
        Box::pin(async move {
            let turn_id = context.turn_id.unwrap_or_else(TurnId::generate);
            let sender = EventSender::with_identity(self.agent_tx.clone(), Some(context.work));
            context.output.set_terminal_sender(sender.clone());
            if input.prompt.is_some() {
                return BackendResult::EnteredRun(TurnOutcome::failed(
                    context.agent_id,
                    turn_id,
                    TokenUsage::default(),
                    0,
                    TurnFailure {
                        kind: TurnFailureKind::Internal,
                        diagnostic: MCP_PROMPT_UNAVAILABLE.to_owned(),
                        user_message: MCP_PROMPT_UNAVAILABLE.to_owned(),
                        retryable: false,
                    },
                ));
            }
            let blocks = blocks(&input);
            self.turn.start(Translator {
                sender,
                model: self.model.clone(),
                tools: HashMap::new(),
                usage: TokenUsage::default(),
                context_window: 0,
            });
            let cancel = {
                let client = Arc::clone(&self.client);
                let cancel = context.cancel.clone();
                smol::spawn(async move {
                    cancel.cancelled().await;
                    client.cancel();
                })
            };
            let client = Arc::clone(&self.client);
            let stop = smol::unblock(move || client.prompt_blocks(blocks)).await;
            // Everything the agent said about this turn arrived before its
            // response, so the reader drains the rest before the turn settles.
            let (ack_tx, ack_rx) = flume::bounded(1);
            let _ = self.settle.send(ack_tx);
            let _ = ack_rx.recv_async().await;
            self.turn.finish();
            cancel.cancel().await;
            let outcome = match stop {
                Ok(StopReason::Cancelled) => TurnOutcome::cancelled(
                    context.agent_id,
                    turn_id,
                    TokenUsage::default(),
                    1,
                    TurnCancellationReason::User,
                ),
                Ok(reason) => TurnOutcome::completed(
                    context.agent_id,
                    turn_id,
                    TokenUsage::default(),
                    1,
                    done_reason(reason),
                ),
                Err(error) => {
                    warn!(%turn_id, %error, "remote turn failed");
                    TurnOutcome::failed(
                        context.agent_id,
                        turn_id,
                        TokenUsage::default(),
                        0,
                        TurnFailure {
                            kind: TurnFailureKind::Internal,
                            diagnostic: error.clone(),
                            user_message: error,
                            retryable: true,
                        },
                    )
                }
            };
            BackendResult::EnteredRun(outcome)
        })
    }

    fn run_control<'a>(
        &'a mut self,
        _history: &'a mut History,
        context: TurnContext,
        _control: &'a ControlWork,
    ) -> Pin<Box<dyn Future<Output = BackendResult> + Send + 'a>> {
        Box::pin(async move {
            EventSender::with_identity(self.agent_tx.clone(), Some(context.work)).try_send(
                AgentEvent::ControlError {
                    message: TOOL_UNAVAILABLE.to_owned(),
                },
            );
            BackendResult::ControlFailed
        })
    }

    fn run_compact<'a>(
        &'a mut self,
        _history: &'a mut History,
        context: TurnContext,
        instructions: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = BackendResult> + Send + 'a>> {
        Box::pin(async move {
            // The transcript this window copies gets replaced once the agent
            // announces what it committed, so the compaction itself runs
            // where the session lives and this side only watches it.
            let sender = EventSender::with_identity(self.agent_tx.clone(), Some(context.work));
            self.turn.start(Translator {
                sender: sender.clone(),
                model: self.model.clone(),
                tools: HashMap::new(),
                usage: TokenUsage::default(),
                context_window: 0,
            });
            let cancel = {
                let client = Arc::clone(&self.client);
                let cancel = context.cancel.clone();
                smol::spawn(async move {
                    cancel.cancelled().await;
                    client.cancel();
                })
            };
            let client = Arc::clone(&self.client);
            let instructions = instructions.map(str::to_owned);
            let stop = smol::unblock(move || client.compact(instructions.as_deref())).await;
            let (ack_tx, ack_rx) = flume::bounded(1);
            let _ = self.settle.send(ack_tx);
            let _ = ack_rx.recv_async().await;
            self.turn.finish();
            cancel.cancel().await;
            match stop {
                Ok(StopReason::EndTurn) => {
                    sender.try_send(AgentEvent::ControlComplete {
                        usage: TokenUsage::default(),
                    });
                    BackendResult::CompactDone
                }
                Ok(_) => BackendResult::ControlFailed,
                Err(error) => {
                    warn!(%error, "remote compaction failed");
                    sender.try_send(AgentEvent::ControlError { message: error });
                    BackendResult::ControlFailed
                }
            }
        })
    }
}

fn done_reason(stop: StopReason) -> DoneReason {
    match stop {
        StopReason::MaxTokens => DoneReason::MaxTokens,
        StopReason::MaxTurnRequests => DoneReason::MaxTurns,
        _ => DoneReason::EndTurn,
    }
}

fn blocks(input: &AgentInput) -> Vec<ContentBlock> {
    let mut blocks = Vec::with_capacity(1 + input.images.len());
    blocks.push(ContentBlock::Text(TextContent::new(input.message.clone())));
    for image in &input.images {
        blocks.push(ContentBlock::Image(ImageContent::new(
            image.data.to_string(),
            image.media_type.mime(),
        )));
    }
    blocks
}

/// The connection's one reader. It feeds the turn this window runs and, between
/// turns, paints what another window's turn streams in, so an origin does not
/// look idle while the session talks.
struct Reader {
    events: Receiver<maki_acp_client::Event>,
    settled: Receiver<flume::Sender<()>>,
    turn: SharedTurn,
    mirror: Mirror,
    permissions: Permissions,
}

impl Reader {
    async fn run(mut self) {
        loop {
            let next = or(
                async { self.events.recv_async().await.ok().map(Next::Event) },
                async { self.settled.recv_async().await.ok().map(Next::Settle) },
            )
            .await;
            match next {
                Some(Next::Event(event)) => self.event(event),
                Some(Next::Settle(ack)) => {
                    while let Ok(event) = self.events.try_recv() {
                        self.event(event);
                    }
                    let _ = ack.send(());
                }
                None => return,
            }
        }
    }

    fn event(&mut self, event: maki_acp_client::Event) {
        match event {
            maki_acp_client::Event::Update(update) => {
                if let Some(update) = self.turn.route(update) {
                    self.mirror.update(*update);
                }
            }
            maki_acp_client::Event::Permission(prompt) => {
                // A question another window's turn asked is still shown here,
                // so whichever window sees it can answer it.
                let sender = self.turn.sender().unwrap_or_else(|| self.mirror.sender());
                self.permissions.ask(&sender, prompt);
            }
            maki_acp_client::Event::PermissionDone { request } => {
                if let Some(id) = self.permissions.dismiss(&request) {
                    self.mirror.dismissed(id);
                }
            }
            maki_acp_client::Event::Turn(running) => {
                if self.turn.is_running() {
                    return;
                }
                if running {
                    self.mirror.start();
                } else {
                    self.mirror.finish();
                }
            }
            // A watcher attaching mid-turn follows the turn already running.
            maki_acp_client::Event::Attached { busy: true } => self.mirror.start(),
            // The session committed history. The rows are already on screen;
            // this is what gives the local actor the same transcript to
            // resume, rewind or compact from.
            maki_acp_client::Event::Committed {
                messages,
                start,
                replace,
                compacted,
            } => self.mirror.committed(messages, start, replace, compacted),
            _ => {}
        }
    }
}

enum Next {
    Event(maki_acp_client::Event),
    Settle(flume::Sender<()>),
}

/// The turn this window runs, if any.
#[derive(Clone, Default)]
struct SharedTurn(Arc<Mutex<Option<Translator>>>);

impl SharedTurn {
    fn start(&self, translator: Translator) {
        *self.lock() = Some(translator);
    }

    fn finish(&self) {
        *self.lock() = None;
    }

    fn is_running(&self) -> bool {
        self.lock().is_some()
    }

    /// Hands the update to the turn running here, or gives it back.
    fn route(&self, update: Box<SessionUpdate>) -> Option<Box<SessionUpdate>> {
        match self.lock().as_mut() {
            Some(translator) => {
                translator.update(*update);
                None
            }
            None => Some(update),
        }
    }

    fn sender(&self) -> Option<EventSender> {
        self.lock()
            .as_ref()
            .map(|translator| translator.sender.clone())
    }

    fn lock(&self) -> MutexGuard<'_, Option<Translator>> {
        self.0.lock().unwrap_or_else(|error| error.into_inner())
    }
}

/// Another window's turn, painted into the transcript this window shows. It
/// joins no history here: the turn ran against the history on the other side,
/// which is why every row it paints is display only.
struct Mirror {
    translator: Translator,
    agent_id: AgentId,
    running: bool,
}

impl Mirror {
    fn new(agent_tx: flume::Sender<Envelope>, model: String) -> Self {
        let agent_id = AgentId::generate();
        let sender = EventSender::with_identity(
            agent_tx,
            Some(ActorWorkIdentity::new(
                agent_id,
                ActorWorkKind::Control { operation_id: 0 },
                MIRROR_PRESENTATION,
            )),
        );
        Self {
            translator: Translator {
                sender,
                model,
                tools: HashMap::new(),
                usage: TokenUsage::default(),
                context_window: 0,
            },
            agent_id,
            running: false,
        }
    }

    fn sender(&self) -> EventSender {
        self.translator.sender.clone()
    }

    fn start(&mut self) {
        if self.running {
            return;
        }
        self.running = true;
        self.translator.tools.clear();
        self.translator.usage = TokenUsage::default();
        self.translator.context_window = 0;
        self.translator.sender.try_send(AgentEvent::Post(Post {
            text: MIRROR_LABEL.to_owned(),
            level: PostLevel::Info,
            agent: None,
        }));
    }

    fn finish(&mut self) {
        if !self.running {
            return;
        }
        self.running = false;
        self.translator
            .sender
            .try_send(AgentEvent::TurnOutcome(TurnOutcome::completed(
                self.agent_id,
                TurnId::generate(),
                TokenUsage::default(),
                0,
                DoneReason::EndTurn,
            )));
    }

    fn update(&mut self, update: SessionUpdate) {
        if self.running {
            self.translator.update(update);
        }
    }

    fn dismissed(&self, id: String) {
        self.translator
            .sender
            .try_send(AgentEvent::PermissionDismissed { id });
    }

    fn committed(&self, messages: Vec<Value>, start: usize, replace: bool, compacted: bool) {
        let Ok(messages) = serde_json::from_value::<Vec<Message>>(Value::Array(messages)) else {
            warn!("the session announced a history this window could not read");
            return;
        };
        self.translator
            .sender
            .try_send(AgentEvent::HistoryCommitted {
                messages: Arc::new(messages),
                start,
                replace,
                compacted,
            });
    }
}

/// The questions this window was asked and has not answered yet.
#[derive(Clone, Default)]
struct Permissions {
    state: Arc<Mutex<PendingState>>,
}

#[derive(Default)]
struct PendingState {
    next: u32,
    requests: HashMap<String, Pending>,
}

impl Permissions {
    fn ask(&self, sender: &EventSender, prompt: PermissionPrompt) {
        let (tool, scopes) = permission_key(&prompt.title);
        let id = {
            let mut state = self.lock();
            let id = format!("{PERMISSION_PREFIX}{}", state.next);
            state.next += 1;
            state.requests.insert(
                id.clone(),
                Pending {
                    request: prompt.id,
                    options: prompt.options,
                },
            );
            id
        };
        if sender
            .send(AgentEvent::PermissionRequest {
                id: id.clone(),
                tool,
                scopes,
            })
            .is_err()
        {
            self.lock().requests.remove(&id);
        }
    }

    /// Removes the question another window answered, returning this window's
    /// id for it so the prompt closes.
    fn dismiss(&self, request: &Value) -> Option<String> {
        let mut state = self.lock();
        let id = state
            .requests
            .iter()
            .find(|(_, pending)| pending.request == *request)
            .map(|(id, _)| id.clone())?;
        state.requests.remove(&id);
        Some(id)
    }

    fn answer(&self, client: &Client, raw: &str) {
        let Some(TaggedAnswer { request_id, answer }) = TaggedAnswer::decode(raw) else {
            return;
        };
        let Some(pending) = self.lock().requests.remove(&request_id) else {
            return;
        };
        client.answer(pending.request, option_for(&answer, &pending.options));
    }

    fn lock(&self) -> MutexGuard<'_, PendingState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }
}

/// Turns session updates into the events the chat renders. A tool call's name
/// arrives once, in its first update, so later updates read it back from here.
struct Translator {
    sender: EventSender,
    model: String,
    tools: HashMap<String, Arc<str>>,
    usage: TokenUsage,
    context_window: u32,
}

struct Pending {
    request: serde_json::Value,
    options: Vec<PermissionOption>,
}

impl Translator {
    fn update(&mut self, update: SessionUpdate) {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                if let ContentBlock::Text(text) = chunk.content {
                    self.sender
                        .try_send(AgentEvent::TextDelta { text: text.text });
                }
            }
            SessionUpdate::UserMessageChunk(chunk) => {
                if let ContentBlock::Text(text) = chunk.content {
                    self.sender
                        .try_send(AgentEvent::MirroredPrompt { text: text.text });
                }
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                if let ContentBlock::Text(text) = chunk.content {
                    self.sender
                        .try_send(AgentEvent::ThinkingDelta { text: text.text });
                }
            }
            SessionUpdate::ToolCall(call) => self.tool_call(call),
            SessionUpdate::ToolCallUpdate(update) => self.tool_update(update),
            SessionUpdate::UsageUpdate(usage) => self.usage(usage),
            _ => {}
        }
    }

    fn tool_call(&mut self, call: ToolCall) {
        let id = call.tool_call_id.0.to_string();
        let name: Arc<str> = Arc::from(call.title);
        self.tools.insert(id.clone(), Arc::clone(&name));
        self.sender.try_send(AgentEvent::ToolPending {
            id,
            name: name.to_string(),
        });
    }

    fn tool_update(&mut self, update: ToolCallUpdate) {
        use agent_client_protocol_schema::ToolCallStatus as Status;

        let id = update.tool_call_id.0.to_string();
        let tool = self
            .tools
            .get(&id)
            .cloned()
            .unwrap_or_else(|| Arc::from(REMOTE_TOOL));
        let fields = update.fields;
        let text = content_text(fields.content.as_deref());
        match fields.status {
            Some(Status::Pending) => {
                let summary = fields.title.unwrap_or_else(|| tool.to_string());
                self.sender
                    .try_send(AgentEvent::ToolStart(Box::new(ToolStartEvent {
                        id,
                        tool,
                        summary,
                        render_header: None,
                        annotation: None,
                        input: None,
                        raw_input: fields.raw_input,
                        output: None,
                    })));
            }
            Some(Status::InProgress) => match text {
                Some(text) => self
                    .sender
                    .try_send(AgentEvent::ToolOutput { id, content: text }),
                None => self.sender.try_send(AgentEvent::ToolExecutionStart { id }),
            },
            Some(Status::Completed | Status::Failed) => {
                let is_error = fields.status == Some(Status::Failed);
                self.sender
                    .try_send(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
                        id,
                        tool,
                        output: ToolOutput::Plain(text.unwrap_or_default().into()),
                        is_error,
                        annotation: None,
                        written_path: None,
                    })));
            }
            _ => {}
        }
    }

    /// The agent reports the context gauge cumulatively, so this is the one
    /// number a turn's usage and its gauge are both built from.
    fn usage(&mut self, usage: UsageUpdate) {
        let used = u32::try_from(usage.used).unwrap_or(u32::MAX);
        self.context_window = u32::try_from(usage.size).unwrap_or(u32::MAX);
        self.usage = TokenUsage {
            input: used,
            ..self.usage
        };
        self.sender.try_send(AgentEvent::TurnComplete(Box::new(
            maki_agent::TurnCompleteEvent {
                message: Message::default(),
                usage: self.usage,
                model: self.model.clone(),
                cost: usage.cost.map(|cost| cost.amount),
                context_size: Some(used),
                context_window: self.context_window,
            },
        )));
    }
}

fn content_text(content: Option<&[ToolCallContent]>) -> Option<String> {
    let content = content?;
    let mut out = String::new();
    for block in content {
        let text = match block {
            ToolCallContent::Content(content) => match &content.content {
                ContentBlock::Text(text) => text.text.clone(),
                _ => continue,
            },
            _ => continue,
        };
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&text);
    }
    (!out.is_empty()).then_some(out)
}

/// The server titles a permission request `{tool}: {scopes}`; the prompt wants
/// the parts back so it can name the rule each answer would write.
fn permission_key(title: &str) -> (ToolKey, Vec<String>) {
    let (tool, scopes) = title.split_once(": ").unwrap_or((title, ""));
    let scopes = scopes
        .split(", ")
        .filter(|scope| !scope.is_empty())
        .map(str::to_owned)
        .collect();
    (
        ToolKey::parse(tool).unwrap_or_else(|_| ToolKey::Native(Arc::from(tool))),
        scopes,
    )
}

fn option_for<'a>(answer: &PermissionAnswer, options: &'a [PermissionOption]) -> Option<&'a str> {
    let kinds: &[PermissionOptionKind] = if answer.is_allow() {
        match answer {
            PermissionAnswer::AllowOnce => &[PermissionOptionKind::AllowOnce],
            _ => &[
                PermissionOptionKind::AllowAlways,
                PermissionOptionKind::AllowOnce,
            ],
        }
    } else {
        match answer {
            PermissionAnswer::Deny => &[PermissionOptionKind::RejectOnce],
            _ => &[
                PermissionOptionKind::RejectAlways,
                PermissionOptionKind::RejectOnce,
            ],
        }
    };
    kinds.iter().find_map(|kind| {
        options
            .iter()
            .find(|option| option.kind == *kind)
            .map(|option| option.option_id.0.as_ref())
    })
}

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::{
        ContentChunk, PermissionOptionId, ToolCallStatus, ToolCallUpdateFields, ToolKind,
    };
    use std::io::BufRead;
    use std::io::Write;
    use std::path::Path;

    use maki_agent::actor::TurnContext;
    use maki_agent::{
        ActorWorkIdentity, ActorWorkKind, AgentEvent, CancelToken, CancelTrigger, History,
        ReasonedCancelToken, ReasonedCancelTrigger, TurnId, WorkKind,
    };
    use test_case::test_case;

    use super::*;

    const MODEL: &str = "gateway/model";

    fn translator() -> (Translator, flume::Receiver<Envelope>) {
        let (tx, rx) = flume::unbounded();
        (
            Translator {
                sender: EventSender::new(tx),
                model: MODEL.to_owned(),
                tools: HashMap::new(),
                usage: TokenUsage::default(),
                context_window: 0,
            },
            rx,
        )
    }

    fn drain(rx: &flume::Receiver<Envelope>) -> Vec<AgentEvent> {
        rx.try_iter().map(|envelope| envelope.event).collect()
    }

    #[test]
    fn a_turn_announced_elsewhere_is_painted_and_a_replay_is_not() {
        let (tx, rx) = flume::unbounded();
        let mut mirror = Mirror::new(tx, MODEL.to_owned());

        mirror.update(chunk("replay"));
        assert!(rx.is_empty(), "a replayed transcript joins no turn");

        mirror.start();
        mirror.update(chunk("hello"));
        mirror.finish();

        let painted: Vec<_> = rx.try_iter().collect();
        assert!(
            painted
                .iter()
                .all(|envelope| envelope.presentation() == Some(MIRROR_PRESENTATION)),
            "mirrored rows must bypass the run fence"
        );
        match painted
            .iter()
            .map(|envelope| &envelope.event)
            .collect::<Vec<_>>()
            .as_slice()
        {
            [
                AgentEvent::Post(label),
                AgentEvent::TextDelta { text },
                AgentEvent::TurnOutcome(_),
            ] => {
                assert_eq!(label.text, MIRROR_LABEL);
                assert_eq!(text, "hello");
            }
            other => panic!("unexpected events: {other:?}"),
        }
    }

    fn chunk(text: &str) -> SessionUpdate {
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
            text,
        ))))
    }

    #[test]
    fn a_committed_history_reaches_the_local_actor() {
        let (tx, rx) = flume::unbounded();
        let mirror = Mirror::new(tx, MODEL.to_owned());

        mirror.committed(
            vec![serde_json::to_value(Message::user("one".into())).unwrap()],
            0,
            true,
            false,
        );

        match drain(&rx).as_slice() {
            [
                AgentEvent::HistoryCommitted {
                    messages,
                    start,
                    replace,
                    compacted,
                },
            ] => {
                assert_eq!(messages.len(), 1);
                assert_eq!(*start, 0);
                assert!(replace);
                assert!(!compacted);
            }
            other => panic!("unexpected events: {other:?}"),
        }
    }

    #[test]
    fn a_prompt_another_window_sent_becomes_a_user_bubble() {
        let (mut translator, rx) = translator();

        translator.update(SessionUpdate::UserMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new("why does it hang")),
        )));

        match drain(&rx).as_slice() {
            [AgentEvent::MirroredPrompt { text }] => assert_eq!(text, "why does it hang"),
            other => panic!("unexpected events: {other:?}"),
        }
    }

    #[test]
    fn message_and_thought_chunks_become_deltas() {
        let (mut translator, rx) = translator();

        translator.update(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new("hello")),
        )));
        translator.update(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new("thinking")),
        )));

        match drain(&rx).as_slice() {
            [
                AgentEvent::TextDelta { text },
                AgentEvent::ThinkingDelta { text: thought },
            ] => {
                assert_eq!(text, "hello");
                assert_eq!(thought, "thinking");
            }
            other => panic!("unexpected events: {other:?}"),
        }
    }

    #[test]
    fn a_tool_call_keeps_its_name_across_updates() {
        let (mut translator, rx) = translator();
        let mut fields = ToolCallUpdateFields::new().status(ToolCallStatus::Pending);
        fields.title = Some("src/main.rs".to_owned());
        fields.raw_input = Some(serde_json::json!({ "path": "src/main.rs" }));

        translator.update(SessionUpdate::ToolCall(
            ToolCall::new("t1", "read").kind(ToolKind::Read),
        ));
        translator.update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "t1", fields,
        )));

        match drain(&rx).as_slice() {
            [
                AgentEvent::ToolPending { id, name },
                AgentEvent::ToolStart(start),
            ] => {
                assert_eq!(id, "t1");
                assert_eq!(name, "read");
                assert_eq!(start.id, "t1");
                assert_eq!(&*start.tool, "read");
                assert_eq!(start.summary, "src/main.rs");
                assert!(start.raw_input.is_some());
            }
            other => panic!("unexpected events: {other:?}"),
        }
    }

    #[test]
    fn tool_output_arrives_whole_and_finishing_reports_an_error() {
        let (mut translator, rx) = translator();
        let output = ToolCallUpdateFields::new()
            .status(ToolCallStatus::InProgress)
            .content(vec![ToolCallContent::Content(
                agent_client_protocol_schema::Content::new(ContentBlock::Text(TextContent::new(
                    "line one",
                ))),
            )]);
        let failed = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Failed)
            .content(vec![ToolCallContent::Content(
                agent_client_protocol_schema::Content::new(ContentBlock::Text(TextContent::new(
                    "boom",
                ))),
            )]);

        translator.update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "t9", output,
        )));
        translator.update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "t9", failed,
        )));

        match drain(&rx).as_slice() {
            [
                AgentEvent::ToolOutput { id, content },
                AgentEvent::ToolDone(done),
            ] => {
                assert_eq!(id, "t9");
                assert_eq!(content, "line one");
                assert_eq!(done.id, "t9");
                assert_eq!(&*done.tool, REMOTE_TOOL);
                assert!(done.is_error);
                assert_eq!(done.output.as_text(), "boom");
            }
            other => panic!("unexpected events: {other:?}"),
        }
    }

    #[test]
    fn usage_update_closes_the_turn_with_the_context_gauge() {
        let (mut translator, rx) = translator();

        translator.update(SessionUpdate::UsageUpdate(UsageUpdate::new(1_500, 20_000)));

        match drain(&rx).as_slice() {
            [AgentEvent::TurnComplete(turn)] => {
                assert_eq!(turn.model, MODEL);
                assert_eq!(turn.usage.input, 1_500);
                assert_eq!(turn.context_size, Some(1_500));
                assert_eq!(turn.context_window, 20_000);
            }
            other => panic!("unexpected events: {other:?}"),
        }
    }

    #[test]
    fn an_update_before_its_tool_call_falls_back_to_an_unknown_tool() {
        let (mut translator, rx) = translator();
        let fields = ToolCallUpdateFields::new().status(ToolCallStatus::Completed);

        translator.update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "t1", fields,
        )));

        match drain(&rx).as_slice() {
            [AgentEvent::ToolDone(done)] => assert_eq!(&*done.tool, REMOTE_TOOL),
            other => panic!("unexpected events: {other:?}"),
        }
    }

    #[test_case("read: src/a.rs, src/b.rs", "read", vec!["src/a.rs".to_owned(), "src/b.rs".to_owned()] ; "scopes follow the tool")]
    #[test_case("bash", "bash", Vec::new() ; "no scopes")]
    fn permission_key_splits_the_server_title(title: &str, tool: &str, scopes: Vec<String>) {
        let (key, parsed) = permission_key(title);
        assert_eq!(key.to_string(), tool);
        assert_eq!(parsed, scopes);
    }

    fn option(id: &str, kind: PermissionOptionKind) -> PermissionOption {
        PermissionOption::new(PermissionOptionId::from(id.to_owned()), id.to_owned(), kind)
    }

    #[test]
    fn permission_answers_pick_the_matching_option() {
        let options = vec![
            option("allow_once", PermissionOptionKind::AllowOnce),
            option("allow_always", PermissionOptionKind::AllowAlways),
            option("reject_once", PermissionOptionKind::RejectOnce),
            option("reject_always", PermissionOptionKind::RejectAlways),
        ];

        assert_eq!(
            option_for(&PermissionAnswer::AllowOnce, &options),
            Some("allow_once")
        );
        assert_eq!(
            option_for(&PermissionAnswer::AllowSession, &options),
            Some("allow_always")
        );
        assert_eq!(
            option_for(&PermissionAnswer::Deny, &options),
            Some("reject_once")
        );
        assert_eq!(
            option_for(&PermissionAnswer::DenyAlwaysProject, &options),
            Some("reject_always")
        );
    }

    const SESSION_ID: &str = "s1";

    /// One scripted agent turn: what it answers, and what it checks the
    /// client sent.
    struct Script {
        lines: std::io::Lines<std::io::BufReader<std::net::TcpStream>>,
        out: std::net::TcpStream,
    }

    impl Script {
        fn recv(&mut self) -> serde_json::Value {
            let line = self.lines.next().expect("a request").expect("readable");
            serde_json::from_str(&line).expect("json")
        }

        fn send(&mut self, message: serde_json::Value) {
            let mut text = message.to_string();
            text.push('\n');
            self.out
                .write_all(text.as_bytes())
                .and_then(|()| self.out.flush())
                .expect("write");
        }

        fn respond(&mut self, id: &serde_json::Value, result: serde_json::Value) {
            self.send(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result,
            }));
        }

        fn update(&mut self, update: serde_json::Value) {
            self.send(serde_json::json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": { "sessionId": SESSION_ID, "update": update },
            }));
        }

        fn handshake(&mut self) {
            let initialize = self.recv();
            self.respond(
                &initialize["id"],
                serde_json::json!({ "protocolVersion": 1, "agentCapabilities": {} }),
            );
            let new = self.recv();
            self.respond(&new["id"], serde_json::json!({ "sessionId": SESSION_ID }));
        }
    }

    /// A client and the agent at its other end, over a loopback socket pair.
    fn connected() -> (
        std::thread::JoinHandle<serde_json::Value>,
        maki_acp_client::Connection,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let client = std::net::TcpStream::connect(address).expect("connect");
        let (agent, _) = listener.accept().expect("accept");
        let script = std::thread::spawn(move || {
            let reading = agent.try_clone().expect("clone");
            let mut script = Script {
                lines: std::io::BufReader::new(reading).lines(),
                out: agent,
            };
            script.handshake();
            let prompt = script.recv();
            script.update(serde_json::json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "hello" },
            }));
            script.respond(
                &prompt["id"],
                serde_json::json!({ "stopReason": "end_turn" }),
            );
            prompt
        });
        let split = client.try_clone().expect("clone");
        let connection = maki_acp_client::connect_streams(split, client, Path::new("/tmp"))
            .expect("connect streams");
        (script, connection)
    }

    /// The triggers must outlive the turn: dropping one cancels it.
    fn turn_context() -> (CancelTrigger, ReasonedCancelTrigger, TurnContext) {
        let turn_id = TurnId::generate();
        let (trigger, cancel) = CancelToken::new();
        let (reason_trigger, cancel_reason) = ReasonedCancelToken::new();
        let context = TurnContext {
            agent_id: maki_agent::AgentId::generate(),
            turn_id: Some(turn_id),
            work: ActorWorkIdentity::new(
                maki_agent::AgentId::generate(),
                ActorWorkKind::Turn {
                    operation_id: 0,
                    turn_id,
                },
                0,
            ),
            output: Default::default(),
            provenance: Default::default(),
            cancel,
            cancel_reason,
            generation: 0,
            policy: None,
            interrupt: None,
            inbox: None,
            managed_turn: None,
            admission: None,
        };
        (trigger, reason_trigger, context)
    }

    #[test]
    fn a_turn_prompts_the_agent_and_reports_its_outcome() {
        let (script, connection) = connected();
        let (tx, rx) = flume::unbounded();
        let (_answer_tx, answer_rx) = flume::unbounded();
        let mut backend = AcpBackend::new(&connection, tx, answer_rx);
        let mut history = History::new(Vec::new());
        let (_trigger, _reason, context) = turn_context();

        let result = smol::block_on(backend.run_turn(
            &mut history,
            context,
            AgentInput::new("hi".to_owned(), Vec::new()),
            WorkKind::Turn,
        ));

        let prompt = script.join().expect("script");
        assert_eq!(
            prompt["params"]["prompt"][0]["text"], "hi",
            "the prompt reached the agent"
        );
        match result {
            BackendResult::EnteredRun(TurnOutcome::Completed { reason, .. }) => {
                assert_eq!(reason, DoneReason::EndTurn);
            }
            other => panic!("unexpected result: {other:?}"),
        }
        match drain(&rx).as_slice() {
            [AgentEvent::TextDelta { text }] => assert_eq!(text, "hello"),
            other => panic!("unexpected events: {other:?}"),
        }
    }

    #[test]
    fn a_permission_request_becomes_a_prompt_the_answer_resolves() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let client = std::net::TcpStream::connect(address).expect("connect");
        let (agent, _) = listener.accept().expect("accept");
        let script = std::thread::spawn(move || {
            let reading = agent.try_clone().expect("clone");
            let mut script = Script {
                lines: std::io::BufReader::new(reading).lines(),
                out: agent,
            };
            script.handshake();
            let prompt = script.recv();
            script.send(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 99,
                "method": "session/request_permission",
                "params": {
                    "sessionId": SESSION_ID,
                    "toolCall": {
                        "toolCallId": "t1",
                        "title": "read: src/a.rs",
                        "status": "pending",
                    },
                    "options": [
                        { "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" },
                        { "optionId": "reject_once", "name": "Reject once", "kind": "reject_once" },
                    ],
                },
            }));
            let answer = script.recv();
            script.respond(
                &prompt["id"],
                serde_json::json!({ "stopReason": "end_turn" }),
            );
            answer
        });
        let split = client.try_clone().expect("clone");
        let connection = maki_acp_client::connect_streams(split, client, Path::new("/tmp"))
            .expect("connect streams");
        let (tx, rx) = flume::unbounded();
        let (answer_tx, answer_rx) = flume::unbounded();
        let (_trigger, _reason, context) = turn_context();

        let turn = std::thread::spawn(move || {
            let mut backend = AcpBackend::new(&connection, tx, answer_rx);
            let mut history = History::new(Vec::new());
            smol::block_on(backend.run_turn(
                &mut history,
                context,
                AgentInput::new("hi".to_owned(), Vec::new()),
                WorkKind::Turn,
            ))
        });

        let asked = loop {
            match rx.recv_timeout(std::time::Duration::from_secs(5)) {
                Ok(envelope) => {
                    if let AgentEvent::PermissionRequest { id, tool, scopes } = envelope.event {
                        break (id, tool, scopes);
                    }
                }
                Err(error) => panic!("no permission request: {error}"),
            }
        };
        assert_eq!(asked.0, format!("{PERMISSION_PREFIX}0"));
        assert_eq!(asked.1.to_string(), "read");
        assert_eq!(asked.2, vec!["src/a.rs".to_owned()]);
        answer_tx
            .send(TaggedAnswer::new(&asked.0, PermissionAnswer::AllowOnce).encode())
            .expect("answer");

        let result = turn.join().expect("turn");
        let answer = script.join().expect("script");
        assert_eq!(answer["result"]["outcome"]["optionId"], "allow_once");
        assert!(matches!(
            result,
            BackendResult::EnteredRun(TurnOutcome::Completed { .. })
        ));
    }

    #[test]
    fn a_refused_prompt_fails_the_turn_with_the_agents_words() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let client = std::net::TcpStream::connect(address).expect("connect");
        let (agent, _) = listener.accept().expect("accept");
        let script = std::thread::spawn(move || {
            let reading = agent.try_clone().expect("clone");
            let mut script = Script {
                lines: std::io::BufReader::new(reading).lines(),
                out: agent,
            };
            script.handshake();
            let prompt = script.recv();
            script.send(serde_json::json!({
                "jsonrpc": "2.0",
                "id": prompt["id"],
                "error": { "code": -32010, "message": "another window controls this session" },
            }));
        });
        let split = client.try_clone().expect("clone");
        let connection = maki_acp_client::connect_streams(split, client, Path::new("/tmp"))
            .expect("connect streams");
        let (tx, _rx) = flume::unbounded();
        let (_answer_tx, answer_rx) = flume::unbounded();
        let mut backend = AcpBackend::new(&connection, tx, answer_rx);
        let mut history = History::new(Vec::new());
        let (_trigger, _reason, context) = turn_context();

        let result = smol::block_on(backend.run_turn(
            &mut history,
            context,
            AgentInput::new("hi".to_owned(), Vec::new()),
            WorkKind::Turn,
        ));

        script.join().expect("script");
        match result {
            BackendResult::EnteredRun(TurnOutcome::Failed { failure, .. }) => {
                assert!(failure.user_message.contains("another window"));
            }
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[test]
    fn compacting_a_shared_session_asks_where_the_session_lives() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let client = std::net::TcpStream::connect(address).expect("connect");
        let (agent, _) = listener.accept().expect("accept");
        let script = std::thread::spawn(move || {
            let reading = agent.try_clone().expect("clone");
            let mut script = Script {
                lines: std::io::BufReader::new(reading).lines(),
                out: agent,
            };
            script.handshake();
            let compact = script.recv();
            script.update(serde_json::json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "t1",
                "title": "Compact context",
                "status": "pending",
            }));
            script.respond(
                &compact["id"],
                serde_json::json!({ "stopReason": "end_turn" }),
            );
            compact
        });
        let split = client.try_clone().expect("clone");
        let connection = maki_acp_client::connect_streams(split, client, Path::new("/tmp"))
            .expect("connect streams");
        let (tx, rx) = flume::unbounded();
        let (_answer_tx, answer_rx) = flume::unbounded();
        let mut backend = AcpBackend::new(&connection, tx, answer_rx);
        let mut history = History::new(Vec::new());
        let (_trigger, _reason, context) = turn_context();

        let result = smol::block_on(backend.run_compact(&mut history, context, Some("keep it")));

        let compact = script.join().expect("script");
        assert_eq!(compact["method"], "maki/compact");
        assert_eq!(compact["params"]["sessionId"], SESSION_ID);
        assert_eq!(compact["params"]["instructions"], "keep it");
        assert!(matches!(result, BackendResult::CompactDone));
        let events = drain(&rx);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::ToolPending { name, .. } if name == "Compact context")),
            "the row the agent opened is painted here: {events:?}"
        );
    }

    #[test]
    fn a_missing_option_kind_falls_back_to_a_once_answer() {
        let options = vec![option("yes", PermissionOptionKind::AllowOnce)];

        assert_eq!(
            option_for(&PermissionAnswer::AllowAlwaysGlobal, &options),
            Some("yes")
        );
        assert_eq!(option_for(&PermissionAnswer::Deny, &options), None);
    }
}
