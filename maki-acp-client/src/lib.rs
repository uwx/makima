//! The ACP side of a frontend: where an agent lives, the JSON-RPC client that
//! talks to it, and the shapes it answers with. A frontend renders what this
//! hands it and never speaks the protocol itself.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::net::TcpStream;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::Duration;

pub use agent_client_protocol_schema::{
    CancelNotification, ClientCapabilities, ContentBlock, FileSystemCapabilities,
    InitializeRequest, JsonRpcMessage, LoadSessionRequest, LoadSessionResponse, NewSessionRequest,
    NewSessionResponse, Notification, PermissionOption, PromptRequest, PromptResponse,
    ProtocolVersion, Request, RequestId, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionConfigKind, SessionConfigOption,
    SessionConfigOptionCategory, SessionId, SessionNotification, SessionUpdate, StopReason,
    TextContent, ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use flume::{Receiver, Sender};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};

const AGENT_ENV: &str = "MAKI_DESKTOP_AGENT";
const REMOTE_ENV: &str = "MAKI_DESKTOP_REMOTE";
const MODEL_ENV: &str = "MAKI_DESKTOP_MODEL";
const DEFAULT_AGENT: &str = "makima";
const LINK_SCHEME: &str = maki_lan::LINK_SCHEME;
const LAN_TOKEN_ENV: &str = "MAKI_DESKTOP_LAN_TOKEN";
const LAN_KIND: &str = "lan";
const LAN_VERSION: u64 = 1;
const LAN_TIMEOUT: Duration = Duration::from_secs(10);
const ACP_SUBCOMMAND: &str = "acp";
const MODEL_FLAG: &str = "--model";
const SSH_PROGRAM: &str = "ssh";
const AGENT_GONE: &str = "agent is not running";
const AGENT_CLOSED: &str = "agent closed the connection";
const SESSION_ENDED: &str = "the shared session ended while this window was disconnected";
const RECONNECT_INITIAL: Duration = Duration::from_millis(250);
const RECONNECT_MAX: Duration = Duration::from_secs(5);
const JSONRPC_VERSION: &str = "2.0";
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(30);
const SESSION_TIMEOUT: Duration = Duration::from_secs(120);
const SESSION_PROMPT_METHOD: &str = "session/prompt";
const SESSION_CANCEL_METHOD: &str = "session/cancel";
const SESSION_NEW_METHOD: &str = "session/new";
const INITIALIZE_METHOD: &str = "initialize";
const SESSION_UPDATE_METHOD: &str = "session/update";
const PERMISSION_METHOD: &str = "session/request_permission";
const UI_CAPABILITY_KEY: &str = "maki-ui";
const YOLO_OPTION_ID: &str = "yolo";
const ENABLED_VALUE: &str = "enabled";
const FLASH_METHOD: &str = "maki/ui/flash";
const PICK_METHOD: &str = "maki/ui/pick";
const COMPLETE_METHOD: &str = "maki/complete";
const THEME_METHOD: &str = "maki/theme";
const SESSIONS_METHOD: &str = "maki/sessions";
const SESSION_LOAD_METHOD: &str = "session/load";
const LIVE_METHOD: &str = "maki/live";
const ATTACH_METHOD: &str = "maki/attach";
const SHARE_METHOD: &str = "maki/share";
const UNSHARE_METHOD: &str = "maki/unshare";
const TURN_METHOD: &str = "maki/ui/turn";
const COMMITTED_METHOD: &str = "maki/ui/committed";
const PERMISSION_DONE_METHOD: &str = "maki/ui/permission_done";
const REWIND_METHOD: &str = "maki/rewind";
const REWOUND_METHOD: &str = "maki/ui/rewind";
const SHORT_SESSION_CHARS: usize = 8;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const COMPLETE_TIMEOUT: Duration = Duration::from_secs(5);
const THEME_TIMEOUT: Duration = Duration::from_secs(30);
const DIFF_CONTEXT_LINES: usize = 3;
const TRUNCATED_HISTORY: &str =
    "This shared session is long; its oldest updates were not replayed.";

/// An inbound event from the agent, already narrowed to what the desktop renders.
pub enum Event {
    Update(Box<SessionUpdate>),
    Permission(PermissionPrompt),
    /// Another window answered a permission question this one also shows.
    PermissionDone {
        request: Value,
    },
    Flash(String),
    Pick(PickPrompt),
    /// Argument candidates for `text`, the composer content that asked.
    Completion {
        text: String,
        items: Vec<ArgItem>,
    },
    Theme(Result<ThemePalette, String>),
    Sessions(Result<Vec<SessionEntry>, String>),
    Loaded(Result<LoadedSession, String>),
    RewindTurns(Result<Vec<RewindTurn>, String>),
    /// The agent rewound its history; the replay follows as updates.
    Rewound,
    /// The agent started sharing this session, or said why it could not.
    Share(Result<ShareInfo, String>),
    /// The agent stopped sharing this session, or said why it could not.
    Unshare(Result<(), String>),
    RewindDone(Result<String, String>),
    PromptDone(Result<StopReason, String>),
    /// A shared session finished replaying its transcript; from here on its
    /// updates are live, and `busy` says whether a turn is already running.
    Attached {
        busy: bool,
    },
    /// The session finished a turn and committed its history: append these
    /// messages, or replace the transcript when `replace` says a rewrite
    /// happened (a rewind or a compaction).
    Committed {
        messages: Vec<Value>,
        start: usize,
        replace: bool,
        compacted: bool,
    },
    /// The session started (true) or finished (false) a turn.
    Turn(bool),
    /// The link to the agent dropped; a reconnect is running in the
    /// background and the session replays once it is back.
    Reconnecting,
    /// The link is back and the session this window watches was attached
    /// again, so its transcript replays from here.
    Reconnected,
    Closed(String),
}

/// Where another machine reaches a shared session, and the token it presents.
#[derive(Debug, Clone, Deserialize)]
pub struct ShareInfo {
    pub address: String,
    pub link: String,
    pub token: String,
}

/// A stored session the agent can reopen.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEntry {
    pub id: String,
    pub title: String,
    pub updated_at: u64,
    pub messages: usize,
    pub open_elsewhere: bool,
    pub current: bool,
}

#[derive(Deserialize)]
struct SessionsResponse {
    sessions: Vec<SessionEntry>,
}

#[derive(Deserialize)]
struct LiveSessions {
    sessions: Vec<LiveSession>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LiveSession {
    session_id: String,
}

/// One user turn a rewind can return to.
#[derive(Debug, Clone, Deserialize)]
pub struct RewindTurn {
    pub offset: usize,
    pub turn: usize,
    pub preview: String,
}

#[derive(Deserialize)]
struct RewindTurns {
    turns: Vec<RewindTurn>,
}

#[derive(Deserialize)]
struct RewindResult {
    draft: String,
}

/// The session the agent switched to, with the config the status bar shows.
pub struct LoadedSession {
    pub session: SessionId,
    pub model: String,
    pub options: SessionView,
}

/// A permission request waiting for a click.
pub struct PermissionPrompt {
    pub id: Value,
    pub title: String,
    pub options: Vec<PermissionOption>,
    pub diff: Option<String>,
}

/// A plugin's list picker waiting for a choice.
pub struct PickPrompt {
    pub id: Value,
    pub title: String,
    pub footer: Vec<(String, String)>,
    pub items: Vec<PickItem>,
    pub cursor: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PickItem {
    pub label: String,
    pub detail: Option<String>,
    pub section: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickOutcome {
    Choice(usize),
    Delete(usize),
    Close,
}

/// The themes the agent knows, and the one it paints with now.
#[derive(Debug, Clone, Default)]
pub struct ThemeCatalog {
    pub current: Option<ThemePalette>,
    pub names: Vec<String>,
}

/// The CLI theme the agent says to paint with, as `#rrggbb` roles.
#[derive(Debug, Clone, Deserialize)]
pub struct ThemePalette {
    pub name: String,
    #[serde(default)]
    colors: HashMap<String, String>,
}

impl ThemePalette {
    pub fn rgb(&self, role: &str) -> Option<[u8; 3]> {
        let hex = self.colors.get(role)?.strip_prefix('#')?;
        let channel = |at: usize| u8::from_str_radix(hex.get(at..at + 2)?, 16).ok();
        Some([channel(0)?, channel(2)?, channel(4)?])
    }

    pub fn is_light(&self) -> bool {
        self.rgb("background").is_some_and(|[red, green, blue]| {
            u32::from(red) + u32::from(green) + u32::from(blue) > 3 * 128
        })
    }
}

/// One argument candidate for the command being typed.
#[derive(Debug, Clone, Deserialize)]
pub struct ArgItem {
    pub label: String,
    pub insertion: String,
    pub description: Option<String>,
}

#[derive(Deserialize)]
struct CompleteResponse {
    items: Vec<ArgItem>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PickParams {
    title: Option<String>,
    footer: Option<Vec<(String, String)>>,
    cursor: Option<usize>,
    items: Vec<PickItem>,
}

fn themes_from(response: &Value) -> ThemeCatalog {
    let Some(maki) = response.pointer("/agentCapabilities/_meta/maki") else {
        return ThemeCatalog::default();
    };
    ThemeCatalog {
        current: maki
            .get("theme")
            .and_then(|theme| serde_json::from_value(theme.clone()).ok()),
        names: maki
            .get("themes")
            .and_then(|names| serde_json::from_value(names.clone()).ok())
            .unwrap_or_default(),
    }
}

/// Turns the file diffs an agent attaches to a tool call into `+`/`-`/` `
/// lines under each path, so the client can show what a tool changed.
pub fn diff_text(content: &[ToolCallContent]) -> Option<String> {
    let mut out = String::new();
    for item in content {
        let ToolCallContent::Diff(diff) = item else {
            continue;
        };
        let before = diff.old_text.as_deref().unwrap_or_default();
        let changes = TextDiff::from_lines(before, &diff.new_text);
        let mut lines = String::new();
        for group in changes.grouped_ops(DIFF_CONTEXT_LINES) {
            for op in group {
                for change in changes.iter_changes(&op) {
                    lines.push(match change.tag() {
                        ChangeTag::Delete => '-',
                        ChangeTag::Insert => '+',
                        ChangeTag::Equal => ' ',
                    });
                    lines.push(' ');
                    lines.push_str(change.value().trim_end_matches('\n'));
                    lines.push('\n');
                }
            }
        }
        if lines.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&diff.path.display().to_string());
        out.push('\n');
        out.push_str(&lines);
    }
    (!out.is_empty()).then_some(out)
}

/// Where the ACP agent lives: a local process, an SSH connection, or an agent
/// another machine shares over the LAN.
#[derive(Clone)]
pub struct Target {
    label: String,
    program: String,
    args: Vec<String>,
    process_cwd: Option<PathBuf>,
    session_cwd: PathBuf,
    lan: Option<Lan>,
    /// Whether the session must already exist there, as a copied link implies.
    attach_only: bool,
    /// A session the agent should open instead of starting a new one.
    resume: Option<String>,
    /// Whether the agent's stderr is dropped instead of shared with us, for a
    /// frontend that owns the terminal.
    quiet: bool,
}

#[derive(Clone)]
struct Lan {
    address: String,
    token: String,
}

impl Target {
    pub fn from_env() -> Result<Self, String> {
        let cwd = std::env::current_dir()
            .map_err(|error| format!("resolve current directory: {error}"))?;
        let agent = std::env::var(AGENT_ENV).unwrap_or_else(|_| DEFAULT_AGENT.to_owned());
        let model = std::env::var(MODEL_ENV).ok();
        match std::env::var(REMOTE_ENV) {
            Ok(remote) if remote.trim().starts_with(LINK_SCHEME) => {
                let token = std::env::var(LAN_TOKEN_ENV).ok();
                Self::lan(remote.trim(), token.as_deref())
            }
            Ok(remote) if !remote.trim().is_empty() => {
                Self::remote(remote.trim(), &agent, model.as_deref())
            }
            _ => Ok(Self::local(cwd, &agent, model.as_deref())),
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// Whether the agent may already serve a session: a LAN listener does, a
    /// spawned process never does.
    fn shares_a_session(&self) -> bool {
        self.lan.is_some()
    }

    /// Whether the session must already exist where the agent runs, which is
    /// what a link without a directory says.
    pub fn attach_only(&self) -> bool {
        self.attach_only
    }

    /// Whether this window runs the agent itself, so it can share it.
    pub fn is_local(&self) -> bool {
        self.lan.is_none() && self.process_cwd.is_some()
    }

    pub fn session_cwd(&self) -> &Path {
        &self.session_cwd
    }

    fn local(cwd: PathBuf, agent: &str, model: Option<&str>) -> Self {
        let mut args = Vec::new();
        push_model(&mut args, model);
        Self::local_with(cwd, agent, args)
    }

    /// The agent this machine runs, with extra arguments for its `acp` command,
    /// like `--share`.
    pub fn local_with(cwd: PathBuf, agent: &str, extra: Vec<String>) -> Self {
        let mut args = vec![ACP_SUBCOMMAND.to_owned()];
        args.extend(extra);
        Self {
            label: "local".to_owned(),
            program: agent.to_owned(),
            args,
            process_cwd: Some(cwd.clone()),
            session_cwd: cwd,
            lan: None,
            attach_only: false,
            resume: None,
            quiet: false,
        }
    }

    /// Opens {id} on the agent instead of starting a new session.
    pub fn resuming(mut self, id: impl Into<String>) -> Self {
        self.resume = Some(id.into());
        self
    }

    /// Keeps the agent's stderr out of ours, for a frontend drawing the terminal.
    pub fn quiet(mut self) -> Self {
        self.quiet = true;
        self
    }

    /// `lan://host:port:/absolute/path`: the session runs on the machine that
    /// shares it, so the path is that machine's. A bare `lan://host:port`
    /// watches the session that machine already serves.
    pub fn lan(spec: &str, token: Option<&str>) -> Result<Self, String> {
        let rest = spec
            .strip_prefix(LINK_SCHEME)
            .ok_or_else(|| format!("{LINK_SCHEME} expected, got {spec:?}"))?;
        let directory = rest
            .split_once(":/")
            .filter(|(address, path)| !address.is_empty() && !path.is_empty())
            .map(|(address, path)| (address, PathBuf::from(format!("/{path}"))));
        let attach_only = directory.is_none();
        let (address, path) = match directory {
            Some((address, path)) => (address, path),
            None if !rest.is_empty() && !rest.contains(":/") => (rest, PathBuf::from("/")),
            None => {
                return Err(format!(
                    "{REMOTE_ENV} must be {LINK_SCHEME}host:port:/absolute/path, got {spec:?}"
                ));
            }
        };
        let token = token
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| format!("{LAN_TOKEN_ENV} must hold the token the listener printed"))?;
        Ok(Self {
            label: spec.to_owned(),
            program: String::new(),
            args: Vec::new(),
            process_cwd: None,
            attach_only,
            resume: None,
            quiet: false,
            session_cwd: path,
            lan: Some(Lan {
                address: address.to_owned(),
                token: token.to_owned(),
            }),
        })
    }

    fn remote(spec: &str, agent: &str, model: Option<&str>) -> Result<Self, String> {
        let (host, path) = spec
            .split_once(':')
            .filter(|(_, path)| path.starts_with('/'))
            .ok_or_else(|| format!("{REMOTE_ENV} must be host:/absolute/path, got {spec:?}"))?;
        let path = PathBuf::from(path);
        let mut remote_command = format!(
            "cd {} && {} {ACP_SUBCOMMAND}",
            shell_quote(&path.to_string_lossy()),
            shell_quote(agent)
        );
        if let Some(model) = model {
            remote_command.push(' ');
            remote_command.push_str(MODEL_FLAG);
            remote_command.push(' ');
            remote_command.push_str(&shell_quote(model));
        }
        Ok(Self {
            label: spec.to_owned(),
            program: SSH_PROGRAM.to_owned(),
            args: vec![host.to_owned(), remote_command],
            process_cwd: None,
            session_cwd: path,
            lan: None,
            attach_only: false,
            resume: None,
            quiet: false,
        })
    }

    /// The agent's input and output, plus the child to reap when there is one.
    fn open(&self) -> Result<(Reader, Writer, Option<Child>), String> {
        if let Some(lan) = &self.lan {
            let mut stream = TcpStream::connect(&lan.address)
                .map_err(|error| format!("connect to {} over the LAN: {error}", lan.address))?;
            stream
                .set_read_timeout(Some(LAN_TIMEOUT))
                .map_err(|error| {
                    format!("set the LAN connect timeout for {}: {error}", lan.address)
                })?;
            let _ = stream.set_nodelay(true);
            handshake(&mut stream, &lan.token)?;
            stream
                .set_read_timeout(None)
                .map_err(|error| format!("clear the LAN connect timeout: {error}"))?;
            let reader = stream
                .try_clone()
                .map_err(|error| format!("split the LAN stream: {error}"))?;
            return Ok((Box::new(reader), Box::new(stream), None));
        }
        let mut child = self.spawn()?;
        let writer = child
            .stdin
            .take()
            .ok_or_else(|| "agent has no stdin".to_owned())?;
        let reader = child
            .stdout
            .take()
            .ok_or_else(|| "agent has no stdout".to_owned())?;
        Ok((Box::new(reader), Box::new(writer), Some(child)))
    }

    fn spawn(&self) -> Result<Child, String> {
        let mut command = Process::new(&self.program);
        command
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if self.quiet {
                Stdio::null()
            } else {
                Stdio::inherit()
            });
        if let Some(cwd) = &self.process_cwd {
            command.current_dir(cwd);
        }
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);
        command
            .spawn()
            .map_err(|error| format!("start `{}`: {error}", self.program))
    }
}

type Reader = Box<dyn Read + Send>;
type Writer = Box<dyn Write + Send>;

pub struct Connection {
    pub client: Arc<Client>,
    pub options: SessionView,
    pub events: Receiver<Event>,
    pub session: SessionId,
    pub model: String,
    pub themes: ThemeCatalog,
}

pub fn connect(target: &Target) -> Result<Connection, String> {
    let (reader, writer, child) = target.open()?;
    let reconnect = target.lan.as_ref().map(|_| target.clone());
    let (client, events) = spawn_agent(reader, writer, child, reconnect)?;
    open_session(
        client,
        events,
        target.session_cwd(),
        target.shares_a_session(),
        target.attach_only(),
        target.resume.as_deref(),
        target.label(),
    )
}

/// Talks to an agent that is already reachable, without spawning one: the
/// caller owns both ends, so tests can script the agent side of a pipe.
#[cfg(feature = "test-util")]
pub fn connect_streams(
    reader: impl Read + Send + 'static,
    writer: impl Write + Send + 'static,
    cwd: &Path,
) -> Result<Connection, String> {
    let (client, events) = spawn_agent(reader, writer, None, None)?;
    open_session(client, events, cwd, false, false, None, "streams")
}

fn spawn_agent(
    reader: impl Read + Send + 'static,
    writer: impl Write + Send + 'static,
    child: Option<Child>,
    target: Option<Target>,
) -> Result<(Arc<Client>, Receiver<Event>), String> {
    let (out_tx, out_rx) = flume::unbounded::<String>();
    let (events_tx, events_rx) = flume::unbounded::<Event>();
    let client = Arc::new(Client {
        next_id: AtomicI64::new(0),
        link: Mutex::new(Link {
            generation: 0,
            out: Some(out_tx),
            target,
        }),
        pending: Mutex::new(HashMap::new()),
        events: events_tx,
        session: Mutex::new(None),
        reconnecting: AtomicBool::new(false),
    });

    spawn_writer(writer, out_rx)?;
    spawn_reader(reader, 0, Arc::downgrade(&client))?;
    if let Some(mut child) = child {
        thread::Builder::new()
            .name("acp-wait".into())
            .spawn(move || {
                let _ = child.wait();
            })
            .map_err(|error| format!("start agent waiter: {error}"))?;
    }

    Ok((client, events_rx))
}

fn open_session(
    client: Arc<Client>,
    events: Receiver<Event>,
    cwd: &Path,
    shares_a_session: bool,
    attach_only: bool,
    resume: Option<&str>,
    label: &str,
) -> Result<Connection, String> {
    let themes = client.initialize()?;
    if let Some(id) = resume {
        let loaded = client.load_session(id, cwd)?;
        return Ok(Connection {
            client,
            events,
            session: loaded.session,
            model: loaded.model,
            options: loaded.options,
            themes,
        });
    }
    let live = if shares_a_session {
        client.live_sessions()?.into_iter().next()
    } else {
        None
    };
    let (session, model, options) = match live {
        Some(id) => {
            let session = SessionId::new(id);
            let reply = client.attach(&session)?;
            if reply.get("truncated").and_then(Value::as_bool) == Some(true) {
                let _ = client.emit(Event::Flash(TRUNCATED_HISTORY.to_owned()));
            }
            // Queued behind the replay, so a watcher knows where history ends.
            let busy = reply.get("busy").and_then(Value::as_bool) == Some(true);
            let _ = client.emit(Event::Attached { busy });
            let options = session_options(&reply);
            let view = session_view(&options);
            let model = view.model.clone().unwrap_or_default();
            (session, model, view)
        }
        None if attach_only => {
            return Err(format!(
                "{label} shares no session right now; add a directory to start one there"
            ));
        }
        None => {
            let (session, model, options) = client.new_session(cwd)?;
            (session, model, options)
        }
    };
    Ok(Connection {
        client,
        events,
        session,
        model,
        options,
        themes,
    })
}

/// Opens a shared session: the listener answers with `ok`, and anything else
/// means it will not serve this token.
fn handshake(stream: &mut TcpStream, token: &str) -> Result<(), String> {
    let mut line = json!({ "maki": LAN_KIND, "version": LAN_VERSION, "token": token }).to_string();
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .and_then(|()| stream.flush())
        .map_err(|error| format!("send the LAN handshake: {error}"))?;
    let reply = read_line(stream)?;
    let reply: Value = serde_json::from_str(&reply)
        .map_err(|error| format!("decode the LAN handshake: {error}"))?;
    if reply["ok"] != true {
        let reason = reply["error"].as_str().unwrap_or("no reason given");
        return Err(format!("LAN handshake refused: {reason}"));
    }
    Ok(())
}

/// Reads one line without buffering past it, so the same stream can be handed
/// to the reader thread afterwards.
/// The message an ACP error carries, without the JSON around it.
fn error_message(error: &Value) -> String {
    match error.get("message").and_then(Value::as_str) {
        Some(message) => message.to_owned(),
        None => error.to_string(),
    }
}

fn permission_done_event(message: &Value) -> Option<Event> {
    let request = message.pointer("/params/requestId")?.clone();
    Some(Event::PermissionDone { request })
}

fn committed_event(message: &Value) -> Option<Event> {
    let params = message.get("params")?;
    let messages = params.get("messages")?.as_array()?.clone();
    Some(Event::Committed {
        messages,
        start: params.get("start").and_then(Value::as_u64).unwrap_or(0) as usize,
        replace: params
            .get("replace")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        compacted: params
            .get("compacted")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn read_line(stream: &mut TcpStream) -> Result<String, String> {
    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => line.push(byte[0]),
            Err(error) => return Err(format!("read the LAN handshake: {error}")),
        }
    }
    String::from_utf8(line).map_err(|error| format!("decode the LAN handshake: {error}"))
}

fn spawn_writer(
    stdin: impl Write + Send + 'static,
    out_rx: Receiver<String>,
) -> Result<(), String> {
    thread::Builder::new()
        .name("acp-writer".into())
        .spawn(move || {
            let mut writer = BufWriter::new(stdin);
            while let Ok(line) = out_rx.recv() {
                if writer.write_all(line.as_bytes()).is_err()
                    || writer.write_all(b"\n").is_err()
                    || writer.flush().is_err()
                {
                    return;
                }
            }
        })
        .map_err(|error| format!("start agent writer: {error}"))?;
    Ok(())
}

/// Holds the client weakly: a strong handle here would keep the write end of
/// the agent's stdin open after its panel is gone, so neither side could ever
/// see EOF and the agent would outlive the desktop.
fn spawn_reader(
    stdout: impl std::io::Read + Send + 'static,
    generation: u64,
    client: Weak<Client>,
) -> Result<(), String> {
    thread::Builder::new()
        .name("acp-reader".into())
        .spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let Some(client) = client.upgrade() else {
                    break;
                };
                client.dispatch(message);
            }
            if let Some(client) = client.upgrade() {
                client.link_lost(generation);
            }
        })
        .map_err(|error| format!("start agent reader: {error}"))?;
    Ok(())
}

/// Why a reconnect attempt did not restore the link. A link failure is worth
/// retrying; a session the agent no longer serves is not.
enum ReopenError {
    Link,
    SessionEnded,
}

/// The live connection to the agent. A LAN link can come back, so the channel
/// to the writer and a generation that tells stale readers apart live here.
struct Link {
    generation: u64,
    out: Option<Sender<String>>,
    target: Option<Target>,
}

pub struct Client {
    next_id: AtomicI64,
    link: Mutex<Link>,
    pending: Mutex<HashMap<i64, Sender<Result<Value, String>>>>,
    events: Sender<Event>,
    session: Mutex<Option<SessionId>>,
    reconnecting: AtomicBool,
}

impl Client {
    fn abort_pending(&self) {
        for (_, sender) in self.pending.lock().unwrap().drain() {
            let _ = sender.send(Err(AGENT_GONE.to_owned()));
        }
    }

    fn send_line(&self, text: String) -> bool {
        let link = self.link.lock().unwrap();
        link.out.as_ref().is_some_and(|out| out.send(text).is_ok())
    }

    /// The reader saw its stream end. A LAN link retries with backoff; a
    /// spawned or SSH agent is gone for good.
    fn link_lost(self: &Arc<Self>, generation: u64) {
        let target = {
            let mut link = self.link.lock().unwrap();
            if link.generation != generation {
                return;
            }
            link.out = None;
            link.target.clone()
        };
        self.abort_pending();
        let Some(target) = target else {
            let _ = self.emit(Event::Closed(AGENT_CLOSED.to_owned()));
            return;
        };
        if self.reconnecting.swap(true, Ordering::SeqCst) {
            return;
        }
        if self.emit(Event::Reconnecting).is_err() {
            self.reconnecting.store(false, Ordering::SeqCst);
            return;
        }
        let client = Arc::clone(self);
        let spawned = thread::Builder::new()
            .name("acp-reconnect".into())
            .spawn(move || client.reconnect(target));
        if let Err(error) = spawned {
            let _ = self.emit(Event::Closed(format!("start reconnecting: {error}")));
        }
    }

    fn drop_link(&self, generation: u64) {
        let mut link = self.link.lock().unwrap();
        if link.generation == generation {
            link.out = None;
        }
    }

    /// Reopens {target} and attaches the session again, until it works or the
    /// session is no longer there. The replay that follows the attach repaints
    /// the transcript, so nothing the window missed is lost.
    fn reconnect(self: Arc<Self>, target: Target) {
        let mut delay = RECONNECT_INITIAL;
        loop {
            thread::sleep(delay);
            delay = (delay * 2).min(RECONNECT_MAX);
            if !self.reconnecting.load(Ordering::SeqCst) || self.events.is_disconnected() {
                return;
            }
            match self.reopen(&target) {
                Ok(()) => {
                    self.reconnecting.store(false, Ordering::SeqCst);
                    let _ = self.emit(Event::Reconnected);
                    return;
                }
                Err(ReopenError::SessionEnded) => {
                    self.reconnecting.store(false, Ordering::SeqCst);
                    let _ = self.emit(Event::Closed(SESSION_ENDED.to_owned()));
                    return;
                }
                Err(ReopenError::Link) => {}
            }
        }
    }

    fn reopen(self: &Arc<Self>, target: &Target) -> Result<(), ReopenError> {
        let (reader, writer, _child) = target.open().map_err(|_| ReopenError::Link)?;
        let (out_tx, out_rx) = flume::unbounded::<String>();
        spawn_writer(writer, out_rx).map_err(|_| ReopenError::Link)?;
        let generation = {
            let mut link = self.link.lock().unwrap();
            link.generation += 1;
            link.out = Some(out_tx);
            link.generation
        };
        if spawn_reader(reader, generation, Arc::downgrade(self)).is_err() {
            self.drop_link(generation);
            return Err(ReopenError::Link);
        }
        let live = self
            .initialize()
            .and_then(|_| self.live_sessions())
            .map_err(|_| ReopenError::Link);
        let live = match live {
            Ok(live) => live,
            Err(error) => {
                self.drop_link(generation);
                return Err(error);
            }
        };
        let Some(session) = self.session.lock().unwrap().clone() else {
            self.drop_link(generation);
            return Err(ReopenError::Link);
        };
        if !live.iter().any(|id| id.as_str() == &*session.0) {
            self.drop_link(generation);
            return Err(ReopenError::SessionEnded);
        }
        let _ = self.emit(Event::Rewound);
        if self.attach(&session).is_err() {
            self.drop_link(generation);
            return Err(ReopenError::Link);
        }
        Ok(())
    }

    pub fn emit(&self, event: Event) -> Result<(), String> {
        self.events.send(event).map_err(|_| "ui closed".to_owned())
    }

    pub fn initialize(&self) -> Result<ThemeCatalog, String> {
        let mut meta = serde_json::Map::new();
        meta.insert(UI_CAPABILITY_KEY.to_owned(), Value::Bool(true));
        let request = InitializeRequest::new(ProtocolVersion::LATEST).client_capabilities(
            ClientCapabilities::new()
                .fs(FileSystemCapabilities::new())
                .meta(meta),
        );
        let response: Value = self.request(INITIALIZE_METHOD, request, Some(INITIALIZE_TIMEOUT))?;
        Ok(themes_from(&response))
    }

    pub fn set_theme(&self, name: &str) -> Result<ThemePalette, String> {
        let response: Value =
            self.request(THEME_METHOD, json!({ "name": name }), Some(THEME_TIMEOUT))?;
        serde_json::from_value(response).map_err(|error| format!("decode theme: {error}"))
    }

    pub fn list_sessions(&self, query: &str) -> Result<Vec<SessionEntry>, String> {
        let response: SessionsResponse =
            self.request(SESSIONS_METHOD, json!({}), Some(SESSION_TIMEOUT))?;
        let needle = query.to_lowercase();
        Ok(response
            .sessions
            .into_iter()
            .filter(|entry| needle.is_empty() || entry.title.to_lowercase().contains(&needle))
            .collect())
    }

    pub fn load_session(&self, id: &str, cwd: &Path) -> Result<LoadedSession, String> {
        let request = LoadSessionRequest::new(SessionId::new(id), cwd);
        let response: LoadSessionResponse =
            self.request(SESSION_LOAD_METHOD, request, Some(SESSION_TIMEOUT))?;
        let options = session_view(response.config_options.as_deref().unwrap_or_default());
        let session = SessionId::new(id);
        *self.session.lock().unwrap() = Some(session.clone());
        Ok(LoadedSession {
            model: options.model.clone().unwrap_or_default(),
            session,
            options,
        })
    }

    pub fn rewind_turns(&self) -> Result<Vec<RewindTurn>, String> {
        let response: RewindTurns =
            self.request(REWIND_METHOD, json!({}), Some(SESSION_TIMEOUT))?;
        Ok(response.turns)
    }

    pub fn rewind(&self, offset: usize) -> Result<String, String> {
        let response: RewindResult = self.request(
            REWIND_METHOD,
            json!({ "offset": offset }),
            Some(SESSION_TIMEOUT),
        )?;
        Ok(response.draft)
    }

    pub fn complete_arguments(&self, text: &str) -> Result<Vec<ArgItem>, String> {
        let response: CompleteResponse = self.request(
            COMPLETE_METHOD,
            json!({ "text": text }),
            Some(COMPLETE_TIMEOUT),
        )?;
        Ok(response.items)
    }

    pub fn new_session(&self, cwd: &Path) -> Result<(SessionId, String, SessionView), String> {
        let response: NewSessionResponse = self.request(
            SESSION_NEW_METHOD,
            NewSessionRequest::new(cwd),
            Some(SESSION_TIMEOUT),
        )?;
        let options = session_view(response.config_options.as_deref().unwrap_or_default());
        let model = options.model.clone().unwrap_or_default();
        *self.session.lock().unwrap() = Some(response.session_id.clone());
        Ok((response.session_id, model, options))
    }

    /// The sessions the agent serves right now. Empty for a spawned process.
    fn live_sessions(&self) -> Result<Vec<String>, String> {
        let response: LiveSessions = self.request(LIVE_METHOD, json!({}), Some(SESSION_TIMEOUT))?;
        Ok(response
            .sessions
            .into_iter()
            .map(|session| session.session_id)
            .collect())
    }

    /// Watches {session}: its history replays as updates, then new ones follow.
    fn attach(&self, session: &SessionId) -> Result<Value, String> {
        let reply: Value = self.request(
            ATTACH_METHOD,
            json!({ "sessionId": session.0 }),
            Some(SESSION_TIMEOUT),
        )?;
        *self.session.lock().unwrap() = Some(session.clone());
        Ok(reply)
    }

    /// Opens a listener other machines can watch this session through.
    pub fn share(&self) -> Result<ShareInfo, String> {
        self.request(SHARE_METHOD, json!({}), Some(SESSION_TIMEOUT))
    }

    /// Stops the listener this connection opened.
    pub fn unshare(&self) -> Result<(), String> {
        let _: Value = self.request(UNSHARE_METHOD, json!({}), Some(SESSION_TIMEOUT))?;
        Ok(())
    }

    pub fn prompt(&self, text: &str) -> Result<StopReason, String> {
        self.prompt_blocks(vec![ContentBlock::Text(TextContent::new(text.to_owned()))])
    }

    pub fn prompt_blocks(&self, blocks: Vec<ContentBlock>) -> Result<StopReason, String> {
        let session = self
            .session
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| "no active session".to_owned())?;
        let request = PromptRequest::new(session, blocks);
        let response: PromptResponse = self.request(SESSION_PROMPT_METHOD, request, None)?;
        Ok(response.stop_reason)
    }

    pub fn cancel(&self) {
        let Some(session) = self.session.lock().unwrap().clone() else {
            return;
        };
        let notification = JsonRpcMessage::wrap(Notification {
            method: SESSION_CANCEL_METHOD.into(),
            params: Some(CancelNotification::new(session)),
        });
        self.send_value(notification);
    }

    pub fn answer(&self, id: Value, option: Option<&str>) {
        let outcome = match option {
            Some(option) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                option.to_owned(),
            )),
            None => RequestPermissionOutcome::Cancelled,
        };
        let response = RequestPermissionResponse::new(outcome);
        self.send_value(json!({
            "jsonrpc": JSONRPC_VERSION,
            "id": id,
            "result": response,
        }));
    }

    fn request<P, R>(&self, method: &str, params: P, timeout: Option<Duration>) -> Result<R, String>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = flume::bounded(1);
        self.pending.lock().unwrap().insert(id, tx);
        let message = JsonRpcMessage::wrap(Request {
            id: RequestId::Number(id),
            method: method.into(),
            params: Some(params),
        });
        let text =
            serde_json::to_string(&message).map_err(|error| format!("encode {method}: {error}"))?;
        if !self.send_line(text) {
            self.pending.lock().unwrap().remove(&id);
            return Err(AGENT_GONE.to_owned());
        }
        let received = match timeout {
            Some(timeout) => rx.recv_timeout(timeout),
            None => rx.recv().map_err(|_| flume::RecvTimeoutError::Disconnected),
        };
        let value = match received {
            Ok(value) => value,
            Err(error) => {
                self.pending.lock().unwrap().remove(&id);
                return Err(match error {
                    flume::RecvTimeoutError::Timeout => format!("{method} timed out"),
                    flume::RecvTimeoutError::Disconnected => AGENT_GONE.to_owned(),
                });
            }
        }?;
        serde_json::from_value(value).map_err(|error| format!("decode {method}: {error}"))
    }

    fn dispatch(&self, message: Value) {
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let id = message.get("id").cloned();
        match (method, id) {
            (Some(method), Some(id)) => self.handle_request(id, &method, &message),
            (Some(method), None) => self.handle_notification(&method, &message),
            (None, _) => self.resolve_response(&message),
        }
    }

    fn resolve_response(&self, message: &Value) {
        let Some(id) = message.get("id").and_then(Value::as_i64) else {
            return;
        };
        let Some(sender) = self.pending.lock().unwrap().remove(&id) else {
            return;
        };
        let result = match message.get("error") {
            Some(error) => Err(error_message(error)),
            None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
        };
        let _ = sender.send(result);
    }

    fn handle_notification(&self, method: &str, message: &Value) {
        match method {
            SESSION_UPDATE_METHOD => self.handle_session_update(message),
            FLASH_METHOD => {
                if let Some(text) = message.pointer("/params/text").and_then(Value::as_str) {
                    let _ = self.emit(Event::Flash(text.to_owned()));
                }
            }
            REWOUND_METHOD => {
                let _ = self.emit(Event::Rewound);
            }
            PERMISSION_DONE_METHOD => {
                if let Some(event) = permission_done_event(message) {
                    let _ = self.emit(event);
                }
            }
            TURN_METHOD => {
                if let Some(running) = message.pointer("/params/running").and_then(Value::as_bool) {
                    let _ = self.emit(Event::Turn(running));
                }
            }
            COMMITTED_METHOD => {
                if let Some(event) = committed_event(message) {
                    let _ = self.emit(event);
                }
            }
            _ => {}
        }
    }

    fn handle_session_update(&self, message: &Value) {
        let Some(params) = message.get("params") else {
            return;
        };
        let Ok(notification) = serde_json::from_value::<SessionNotification>(params.clone()) else {
            return;
        };
        let _ = self.emit(Event::Update(Box::new(notification.update)));
    }

    fn handle_request(&self, id: Value, method: &str, message: &Value) {
        match method {
            PERMISSION_METHOD => self.handle_permission(id, message),
            PICK_METHOD => self.handle_pick(id, message),
            _ => self.answer(id, None),
        }
    }

    fn handle_permission(&self, id: Value, message: &Value) {
        let request = message
            .get("params")
            .cloned()
            .and_then(|params| serde_json::from_value::<RequestPermissionRequest>(params).ok());
        let Some(request) = request else {
            self.answer(id, None);
            return;
        };
        let prompt = PermissionPrompt {
            id,
            title: request.tool_call.fields.title.unwrap_or_default(),
            options: request.options,
            diff: request
                .tool_call
                .fields
                .content
                .as_deref()
                .and_then(diff_text),
        };
        let _ = self.emit(Event::Permission(prompt));
    }

    fn handle_pick(&self, id: Value, message: &Value) {
        let params = message
            .get("params")
            .cloned()
            .and_then(|params| serde_json::from_value::<PickParams>(params).ok());
        let Some(params) = params else {
            self.answer_pick(id, PickOutcome::Close);
            return;
        };
        let request = id.clone();
        let prompt = PickPrompt {
            id,
            title: params.title.unwrap_or_default(),
            footer: params.footer.unwrap_or_default(),
            cursor: params.cursor.unwrap_or_default(),
            items: params.items,
        };
        if self.emit(Event::Pick(prompt)).is_err() {
            self.answer_pick(request, PickOutcome::Close);
        }
    }

    pub fn answer_pick(&self, id: Value, outcome: PickOutcome) {
        let result = match outcome {
            PickOutcome::Choice(index) => json!({ "choice": index }),
            PickOutcome::Delete(index) => json!({ "delete": index }),
            PickOutcome::Close => json!({ "close": true }),
        };
        self.send_value(json!({
            "jsonrpc": JSONRPC_VERSION,
            "id": id,
            "result": result,
        }));
    }

    fn send_value(&self, message: impl Serialize) {
        if let Ok(text) = serde_json::to_string(&message) {
            self.send_line(text);
        }
    }
}

/// The parts of the session config the status bar shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionView {
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub yolo: bool,
}

pub fn session_view(options: &[SessionConfigOption]) -> SessionView {
    let mut view = SessionView::default();
    for option in options {
        let SessionConfigKind::Select(select) = &option.kind else {
            continue;
        };
        let value = select.current_value.0.to_string();
        if matches!(option.category, Some(SessionConfigOptionCategory::Model)) {
            view.model = Some(value);
        } else if option.category == Some(SessionConfigOptionCategory::ThoughtLevel) {
            view.thinking = Some(value);
        } else if option.id.0.as_ref() == YOLO_OPTION_ID {
            view.yolo = value == ENABLED_VALUE;
        }
    }
    view
}

/// The config options a shared session reported, which is what lets an
/// attached window paint its status bar.
fn session_options(reply: &Value) -> Vec<SessionConfigOption> {
    reply
        .get("configOptions")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(|value| serde_json::from_value(value.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn push_model(args: &mut Vec<String>, model: Option<&str>) {
    if let Some(model) = model {
        args.push(MODEL_FLAG.to_owned());
        args.push(model.to_owned());
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

pub fn short_session(session: &SessionId) -> String {
    session.0.chars().take(SHORT_SESSION_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use agent_client_protocol_schema::{Diff, ToolCallContent};
    use serde_json::Value;

    use std::time::{Duration, Instant};

    use super::{
        ACP_SUBCOMMAND, Event, MODEL_FLAG, SESSION_ENDED, SSH_PROGRAM, Target, ThemePalette,
        diff_text, error_message, themes_from,
    };
    use test_case::test_case;

    #[test]
    fn diff_text_renders_changes_under_the_path() {
        let content = vec![ToolCallContent::Diff(
            Diff::new(
                "/p/a.rs",
                "keep
new
",
            )
            .old_text(
                "keep
old
",
            ),
        )];
        let text = diff_text(&content).unwrap();
        assert!(text.starts_with(
            "/p/a.rs
"
        ));
        assert!(text.contains("- old"));
        assert!(text.contains("+ new"));
        assert!(text.contains("  keep"));
    }

    #[test]
    fn diff_text_ignores_content_without_changes() {
        let content = vec![ToolCallContent::Diff(
            Diff::new(
                "/p/a.rs", "same
",
            )
            .old_text(
                "same
",
            ),
        )];
        assert_eq!(diff_text(&content), None);
    }

    #[test]
    fn committed_notifications_carry_messages_and_shape() {
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "method": super::COMMITTED_METHOD,
            "params": {
                "messages": [{ "role": "user" }],
                "start": 3,
                "replace": true,
                "compacted": false,
            },
        });
        match super::committed_event(&message) {
            Some(super::Event::Committed {
                messages,
                start,
                replace,
                compacted,
            }) => {
                assert_eq!(messages.len(), 1);
                assert_eq!(start, 3);
                assert!(replace);
                assert!(!compacted);
            }
            _ => panic!("no committed event"),
        }
        assert!(super::committed_event(&serde_json::json!({ "params": {} })).is_none());
    }

    #[test]
    fn a_permission_done_names_the_question_it_ends() {
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "method": super::PERMISSION_DONE_METHOD,
            "params": { "requestId": 41 },
        });
        match super::permission_done_event(&message) {
            Some(super::Event::PermissionDone { request }) => {
                assert_eq!(request, serde_json::json!(41));
            }
            _ => panic!("no permission done event"),
        }
        assert!(super::permission_done_event(&serde_json::json!({ "params": {} })).is_none());
    }

    const HOST: &str = "buildbox";
    const REMOTE_PATH: &str = "/srv/app";
    const QUOTED_PATH: &str = "/srv/it's";
    const MODEL: &str = "claude-sonnet-4";

    fn palette(colors: &[(&str, &str)]) -> ThemePalette {
        let colors = colors
            .iter()
            .map(|(role, hex)| ((*role).to_owned(), (*hex).to_owned()))
            .collect();
        ThemePalette {
            name: "makima".to_owned(),
            colors,
        }
    }

    #[test_case("#120c0c", Some([0x12, 0x0c, 0x0c]) ; "hex triplet")]
    #[test_case("#FFB08A", Some([0xff, 0xb0, 0x8a]) ; "uppercase")]
    #[test_case("120c0c", None ; "missing hash")]
    #[test_case("#12c", None ; "short form")]
    #[test_case("#zzzzzz", None ; "not hex")]
    fn rgb_reads_hex_role(hex: &str, expected: Option<[u8; 3]>) {
        assert_eq!(palette(&[("background", hex)]).rgb("background"), expected);
    }

    #[test]
    fn session_view_reads_model_thinking_and_yolo() {
        use agent_client_protocol_schema::{
            SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
        };

        fn option(
            id: &str,
            category: SessionConfigOptionCategory,
            current: &str,
        ) -> SessionConfigOption {
            SessionConfigOption::select(
                id.to_owned(),
                id.to_owned(),
                current.to_owned(),
                vec![SessionConfigSelectOption::new(
                    current.to_owned(),
                    current.to_owned(),
                )],
            )
            .category(category)
        }
        let options = [
            option("model", SessionConfigOptionCategory::Model, "openai/gpt-5"),
            option(
                "thinking",
                SessionConfigOptionCategory::ThoughtLevel,
                "high",
            ),
            option(
                "yolo",
                SessionConfigOptionCategory::Other("other".into()),
                "enabled",
            ),
        ];

        let view = super::session_view(&options);

        assert_eq!(view.model.as_deref(), Some("openai/gpt-5"));
        assert_eq!(view.thinking.as_deref(), Some("high"));
        assert!(view.yolo);
    }

    #[test]
    fn initialize_theme_is_read_from_the_agent_meta() {
        let response = serde_json::json!({
            "agentCapabilities": {
                "_meta": {
                    "maki": {
                        "theme": { "name": "dracula", "colors": { "background": "#282a36" } }
                    }
                }
            }
        });
        let catalog = themes_from(&response);
        let theme = catalog.current.expect("theme");
        assert_eq!(theme.name, "dracula");
        assert_eq!(theme.rgb("background"), Some([0x28, 0x2a, 0x36]));
        assert!(catalog.names.is_empty());
    }

    #[test]
    fn initialize_without_theme_meta_yields_none() {
        let response = serde_json::json!({ "agentCapabilities": {} });
        assert!(themes_from(&response).current.is_none());
    }

    #[test]
    fn unknown_role_has_no_color() {
        assert_eq!(palette(&[]).rgb("background"), None);
    }

    #[test_case("#120c0c", false ; "dark background")]
    #[test_case("#f5f5f5", true ; "light background")]
    fn is_light_follows_the_background(hex: &str, light: bool) {
        assert_eq!(palette(&[("background", hex)]).is_light(), light);
    }

    #[test]
    fn remote_builds_ssh_argv() {
        let spec = format!("{HOST}:{REMOTE_PATH}");
        let target = Target::remote(&spec, "makima", None).unwrap();
        assert_eq!(target.program, SSH_PROGRAM);
        assert_eq!(
            target.args,
            [
                HOST.to_owned(),
                format!("cd '{REMOTE_PATH}' && 'makima' {ACP_SUBCOMMAND}"),
            ]
        );
        assert_eq!(target.session_cwd.as_path(), Path::new(REMOTE_PATH));
        assert!(target.process_cwd.is_none());
    }

    #[test]
    fn remote_quotes_single_quotes_in_path() {
        let spec = format!("{HOST}:{QUOTED_PATH}");
        let target = Target::remote(&spec, "makima", None).unwrap();
        assert!(target.args[1].contains(r"'/srv/it'\''s'"));
    }

    #[test]
    fn remote_appends_model_flag() {
        let spec = format!("{HOST}:{REMOTE_PATH}");
        let target = Target::remote(&spec, "makima", Some(MODEL)).unwrap();
        assert!(target.args[1].ends_with(&format!(" {MODEL_FLAG} '{MODEL}'")));
    }

    #[test_case(HOST ; "missing path")]
    #[test_case("buildbox:relative/path" ; "path not absolute")]
    fn remote_rejects_malformed_spec(spec: &str) {
        assert!(Target::remote(spec, "makima", None).is_err());
    }

    #[test]
    fn lan_target_keeps_the_address_and_the_remote_directory() {
        let target = Target::lan("lan://buildbox:7331:/srv/app", Some("token")).unwrap();
        assert_eq!(target.label(), "lan://buildbox:7331:/srv/app");
        assert_eq!(target.session_cwd(), Path::new("/srv/app"));
        assert!(target.lan.is_some());
    }

    #[test]
    fn a_copied_lan_link_attaches_without_a_directory() {
        let target = Target::lan("lan://buildbox:7331", Some("token")).unwrap();
        assert_eq!(target.label(), "lan://buildbox:7331");
        assert_eq!(target.session_cwd(), Path::new("/"));
        assert!(target.attach_only());
        assert!(!target.is_local());
    }

    #[test]
    fn a_lan_link_with_a_directory_may_start_a_session_there() {
        let target = Target::lan("lan://buildbox:7331:/srv/app", Some("token")).unwrap();
        assert!(!target.attach_only());
    }

    #[test_case("lan://:/srv/app" ; "missing address")]
    #[test_case("lan://" ; "missing host and directory")]
    fn lan_rejects_malformed_spec(spec: &str) {
        assert!(Target::lan(spec, Some("token")).is_err());
    }

    #[test]
    fn lan_requires_a_token() {
        assert!(Target::lan("lan://buildbox:7331:/srv/app", None).is_err());
        assert!(Target::lan("lan://buildbox:7331:/srv/app", Some(" ")).is_err());
    }

    /// A LAN agent that answers {replies} requests, then closes. Yields the
    /// methods it saw.
    fn fake_agent(
        replies: usize,
        reply: impl Fn(&str, &Value) -> Option<Value> + Send + 'static,
    ) -> (std::net::SocketAddr, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        use serde_json::{Value, json};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut methods = Vec::new();
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let handshake: Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(handshake["token"], "token");
            stream
                .write_all(b"{\"maki\":\"lan\",\"version\":1,\"ok\":true}\n")
                .unwrap();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let request: Value = serde_json::from_str(line.trim()).unwrap();
                let method = request["method"].as_str().unwrap_or_default().to_owned();
                methods.push(method.clone());
                let Some(result) = reply(&method, &request) else {
                    break;
                };
                let message = json!({ "jsonrpc": "2.0", "id": request["id"], "result": result });
                stream.write_all(message.to_string().as_bytes()).unwrap();
                stream.write_all(b"\n").unwrap();
                if methods.len() == replies {
                    // Let the client see that no more replies follow, while
                    // still reading its writes until it hangs up.
                    let _ = stream.shutdown(std::net::Shutdown::Write);
                }
            }
            methods
        });
        (address, server)
    }

    fn initialized(method: &str) -> Option<Value> {
        (method == "initialize").then(|| serde_json::json!({ "protocolVersion": 1 }))
    }

    #[test]
    fn lan_connect_handshakes_and_opens_a_session() {
        use agent_client_protocol_schema::{NewSessionResponse, SessionId};

        let (address, server) = fake_agent(3, |method, _| match method {
            "initialize" => initialized(method),
            "maki/live" => Some(serde_json::json!({ "sessions": [] })),
            "session/new" => Some(
                serde_json::to_value(NewSessionResponse::new(SessionId::new("sess-lan"))).unwrap(),
            ),
            _ => None,
        });
        let target = Target::lan(&format!("lan://{address}:/srv/app"), Some("token")).unwrap();
        let connection = super::connect(&target).unwrap();
        assert_eq!(&*connection.session.0, "sess-lan");
        drop(connection);
        assert_eq!(
            server.join().unwrap(),
            ["initialize", "maki/live", "session/new"]
        );
    }

    #[test]
    fn lan_connect_attaches_to_a_live_session() {
        use agent_client_protocol_schema::{
            SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
        };

        let model = SessionConfigOption::select(
            "model".to_owned(),
            "Model".to_owned(),
            "openai/gpt-5".to_owned(),
            vec![SessionConfigSelectOption::new(
                "openai/gpt-5".to_owned(),
                "openai/gpt-5".to_owned(),
            )],
        )
        .category(SessionConfigOptionCategory::Model);

        let (address, server) = fake_agent(3, move |method, _| match method {
            "initialize" => initialized(method),
            "maki/live" => Some(serde_json::json!({
                "sessions": [{ "sessionId": "sess-live", "watchers": 1 }],
            })),
            "maki/attach" => Some(serde_json::json!({
                "sessionId": "sess-live",
                "readOnly": true,
                "replayed": 2,
                "truncated": false,
                "configOptions": serde_json::to_value(vec![model.clone()]).unwrap(),
            })),
            _ => None,
        });
        let target = Target::lan(&format!("lan://{address}:/srv/app"), Some("token")).unwrap();
        let connection = super::connect(&target).unwrap();
        assert_eq!(&*connection.session.0, "sess-live");
        assert_eq!(connection.model, "openai/gpt-5");
        drop(connection);
        assert_eq!(
            server.join().unwrap(),
            ["initialize", "maki/live", "maki/attach"]
        );
    }

    #[test]
    fn resuming_loads_the_named_session_instead_of_starting_one() {
        use agent_client_protocol_schema::{
            SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
        };

        let model = SessionConfigOption::select(
            "model".to_owned(),
            "Model".to_owned(),
            "openai/gpt-5".to_owned(),
            vec![SessionConfigSelectOption::new(
                "openai/gpt-5".to_owned(),
                "openai/gpt-5".to_owned(),
            )],
        )
        .category(SessionConfigOptionCategory::Model);

        let (address, server) = fake_agent(2, move |method, request| match method {
            "initialize" => initialized(method),
            "session/load" => {
                assert_eq!(request["params"]["sessionId"], "sess-old");
                assert_eq!(request["params"]["cwd"], "/srv/app");
                Some(serde_json::json!({
                    "configOptions": serde_json::to_value(vec![model.clone()]).unwrap(),
                }))
            }
            _ => None,
        });
        let target = Target::lan(&format!("lan://{address}:/srv/app"), Some("token"))
            .unwrap()
            .resuming("sess-old");
        let connection = super::connect(&target).unwrap();
        assert_eq!(&*connection.session.0, "sess-old");
        assert_eq!(connection.model, "openai/gpt-5");
        drop(connection);
        assert_eq!(server.join().unwrap(), ["initialize", "session/load"]);
    }

    /// One LAN life: consume the handshake and keep the reader for the rest.
    fn accept_lan(
        listener: &std::net::TcpListener,
    ) -> (std::net::TcpStream, std::io::BufReader<std::net::TcpStream>) {
        use std::io::{BufRead, BufReader, Write};

        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        stream
            .write_all(b"{\"maki\":\"lan\",\"version\":1,\"ok\":true}\n")
            .unwrap();
        (stream, reader)
    }

    fn read_request(reader: &mut impl std::io::BufRead) -> Value {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0, "client hung up");
        serde_json::from_str(line.trim()).unwrap()
    }

    fn write_message(stream: &mut std::net::TcpStream, message: Value) {
        use std::io::Write;

        stream.write_all(message.to_string().as_bytes()).unwrap();
        stream.write_all(b"\n").unwrap();
    }

    fn answer(stream: &mut std::net::TcpStream, request: &Value, result: Value) {
        write_message(
            stream,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": result,
            }),
        );
    }

    /// A LAN agent in two lives: the first opens a session and hangs up, the
    /// second serves `live` and, when it still lists the session, replays one
    /// update and one commit before answering the attach.
    fn reconnect_agent(
        live: Value,
    ) -> (std::net::SocketAddr, std::thread::JoinHandle<Vec<String>>) {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            {
                let (mut stream, mut reader) = accept_lan(&listener);
                for (method, result) in [
                    ("initialize", serde_json::json!({ "protocolVersion": 1 })),
                    ("maki/live", serde_json::json!({ "sessions": [] })),
                    (
                        "session/new",
                        serde_json::json!({ "sessionId": "sess-1", "configOptions": [] }),
                    ),
                ] {
                    let request = read_request(&mut reader);
                    assert_eq!(request["method"], method);
                    seen.push(method.to_owned());
                    answer(&mut stream, &request, result);
                }
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
            let (mut stream, mut reader) = accept_lan(&listener);
            let request = read_request(&mut reader);
            assert_eq!(request["method"], "initialize");
            seen.push("initialize".to_owned());
            answer(
                &mut stream,
                &request,
                serde_json::json!({ "protocolVersion": 1 }),
            );
            let request = read_request(&mut reader);
            assert_eq!(request["method"], "maki/live");
            seen.push("maki/live".to_owned());
            answer(&mut stream, &request, live.clone());
            if live["sessions"]
                .as_array()
                .is_some_and(|sessions| !sessions.is_empty())
            {
                let request = read_request(&mut reader);
                assert_eq!(request["method"], "maki/attach");
                seen.push("maki/attach".to_owned());
                write_message(
                    &mut stream,
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": "sess-1",
                            "update": {
                                "sessionUpdate": "agent_message_chunk",
                                "content": { "type": "text", "text": "replayed" },
                            },
                        },
                    }),
                );
                write_message(
                    &mut stream,
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "maki/ui/committed",
                        "params": {
                            "sessionId": "sess-1",
                            "messages": [],
                            "start": 0,
                            "replace": true,
                            "compacted": false,
                        },
                    }),
                );
                answer(
                    &mut stream,
                    &request,
                    serde_json::json!({
                        "sessionId": "sess-1",
                        "readOnly": false,
                        "replayed": 1,
                        "truncated": false,
                        "busy": false,
                        "configOptions": [],
                    }),
                );
            }
            seen
        });
        (address, server)
    }

    fn next_matching(
        events: &flume::Receiver<Event>,
        what: &str,
        matches: fn(&Event) -> bool,
    ) -> Event {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let event = events
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("no {what}"));
            if matches(&event) {
                return event;
            }
        }
    }

    #[test]
    fn a_dropped_lan_link_reconnects_and_reattaches_the_session() {
        let live = serde_json::json!({
            "sessions": [{ "sessionId": "sess-1", "watchers": 0 }],
        });
        let (address, server) = reconnect_agent(live);
        let target = Target::lan(&format!("lan://{address}:/srv/app"), Some("token")).unwrap();
        let connection = super::connect(&target).unwrap();
        assert_eq!(&*connection.session.0, "sess-1");
        let events = connection.events.clone();

        next_matching(&events, "reconnecting", |event| {
            matches!(event, Event::Reconnecting)
        });
        next_matching(&events, "rewound", |event| matches!(event, Event::Rewound));
        next_matching(&events, "replayed update", |event| {
            matches!(event, Event::Update(_))
        });
        next_matching(&events, "replayed history", |event| {
            matches!(event, Event::Committed { .. })
        });
        next_matching(&events, "reconnected", |event| {
            matches!(event, Event::Reconnected)
        });
        assert_eq!(
            server.join().unwrap(),
            [
                "initialize",
                "maki/live",
                "session/new",
                "initialize",
                "maki/live",
                "maki/attach"
            ]
        );
    }

    #[test]
    fn a_session_that_ended_is_not_retried() {
        let (address, server) = reconnect_agent(serde_json::json!({ "sessions": [] }));
        let target = Target::lan(&format!("lan://{address}:/srv/app"), Some("token")).unwrap();
        let connection = super::connect(&target).unwrap();
        let events = connection.events.clone();

        next_matching(&events, "reconnecting", |event| {
            matches!(event, Event::Reconnecting)
        });
        let Event::Closed(reason) =
            next_matching(&events, "closed", |event| matches!(event, Event::Closed(_)))
        else {
            unreachable!()
        };
        assert_eq!(reason, SESSION_ENDED);
        drop(connection);
        server.join().unwrap();
    }

    #[test]
    fn a_copied_link_without_a_session_does_not_start_one_there() {
        let (address, server) = fake_agent(2, |method, _| match method {
            "initialize" => initialized(method),
            "maki/live" => Some(serde_json::json!({ "sessions": [] })),
            _ => None,
        });
        let target = Target::lan(&format!("lan://{address}"), Some("token")).unwrap();
        let error = super::connect(&target)
            .err()
            .expect("a link without a session has nowhere to attach");
        assert!(error.contains("shares no session"), "{error}");
        assert_eq!(server.join().unwrap(), ["initialize", "maki/live"]);
    }

    #[test]
    fn sharing_answers_the_places_other_windows_reach() {
        const TOKEN: &str = "shared-session-token";

        let (address, server) = fake_agent(5, |method, _| match method {
            "initialize" => initialized(method),
            "maki/live" => Some(serde_json::json!({ "sessions": [] })),
            "session/new" => Some(serde_json::json!({
                "sessionId": "sess-new",
                "configOptions": [],
            })),
            "maki/share" => Some(serde_json::json!({
                "bind": "0.0.0.0:4000",
                "address": "192.168.1.4:4000",
                "link": "lan://192.168.1.4:4000",
                "token": TOKEN,
            })),
            "maki/unshare" => Some(serde_json::json!({ "shared": false })),
            _ => None,
        });
        let target = Target::lan(&format!("lan://{address}:/srv/app"), Some("token")).unwrap();
        let connection = super::connect(&target).unwrap();

        let share = connection.client.share().unwrap();
        assert_eq!(share.link, "lan://192.168.1.4:4000");
        assert_eq!(share.address, "192.168.1.4:4000");
        assert_eq!(share.token, TOKEN);

        connection.client.unshare().unwrap();
        drop(connection);
        assert_eq!(
            server.join().unwrap(),
            [
                "initialize",
                "maki/live",
                "session/new",
                "maki/share",
                "maki/unshare"
            ]
        );
    }

    #[test]
    fn an_acp_error_reads_as_its_message() {
        let error = serde_json::json!({
            "code": -32010,
            "message": "this connection is read-only",
        });
        assert_eq!(error_message(&error), "this connection is read-only");
        assert_eq!(error_message(&serde_json::json!("plain")), "\"plain\"");
    }
}
