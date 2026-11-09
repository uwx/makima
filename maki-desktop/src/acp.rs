use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use agent_client_protocol_schema::{
    CancelNotification, ClientCapabilities, ContentBlock, FileSystemCapabilities,
    InitializeRequest, JsonRpcMessage, NewSessionRequest, NewSessionResponse, Notification,
    PermissionOption, PromptRequest, PromptResponse, ProtocolVersion, Request, RequestId,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
    SessionId, SessionNotification, SessionUpdate, StopReason, TextContent,
};
use flume::{Receiver, Sender};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const AGENT_ENV: &str = "MAKI_DESKTOP_AGENT";
const REMOTE_ENV: &str = "MAKI_DESKTOP_REMOTE";
const MODEL_ENV: &str = "MAKI_DESKTOP_MODEL";
const DEFAULT_AGENT: &str = "makima";
const ACP_SUBCOMMAND: &str = "acp";
const MODEL_FLAG: &str = "--model";
const SSH_PROGRAM: &str = "ssh";
const AGENT_GONE: &str = "agent is not running";
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
const FLASH_METHOD: &str = "maki/ui/flash";
const PICK_METHOD: &str = "maki/ui/pick";
const COMPLETE_METHOD: &str = "maki/complete";
const SHORT_SESSION_CHARS: usize = 8;
const COMPLETE_TIMEOUT: Duration = Duration::from_secs(5);

/// An inbound event from the agent, already narrowed to what the desktop renders.
pub enum Event {
    Update(Box<SessionUpdate>),
    Permission(PermissionPrompt),
    Flash(String),
    Pick(PickPrompt),
    /// Argument candidates for `text`, the composer content that asked.
    Completion {
        text: String,
        items: Vec<ArgItem>,
    },
    PromptDone(Result<StopReason, String>),
    Closed(String),
}

/// A permission request waiting for a click.
pub struct PermissionPrompt {
    pub id: Value,
    pub title: String,
    pub options: Vec<PermissionOption>,
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

/// Where the ACP agent lives: a local process or an SSH connection.
pub struct Target {
    label: String,
    program: String,
    args: Vec<String>,
    process_cwd: Option<PathBuf>,
    session_cwd: PathBuf,
}

impl Target {
    pub fn from_env() -> Result<Self, String> {
        let cwd = std::env::current_dir()
            .map_err(|error| format!("resolve current directory: {error}"))?;
        let agent = std::env::var(AGENT_ENV).unwrap_or_else(|_| DEFAULT_AGENT.to_owned());
        let model = std::env::var(MODEL_ENV).ok();
        match std::env::var(REMOTE_ENV) {
            Ok(remote) if !remote.trim().is_empty() => {
                Self::remote(remote.trim(), &agent, model.as_deref())
            }
            _ => Ok(Self::local(cwd, &agent, model.as_deref())),
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn session_cwd(&self) -> &Path {
        &self.session_cwd
    }

    fn local(cwd: PathBuf, agent: &str, model: Option<&str>) -> Self {
        let mut args = vec![ACP_SUBCOMMAND.to_owned()];
        push_model(&mut args, model);
        Self {
            label: "local".to_owned(),
            program: agent.to_owned(),
            args,
            process_cwd: Some(cwd.clone()),
            session_cwd: cwd,
        }
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
        })
    }

    fn spawn(&self) -> Result<Child, String> {
        let mut command = Process::new(&self.program);
        command
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(cwd) = &self.process_cwd {
            command.current_dir(cwd);
        }
        command
            .spawn()
            .map_err(|error| format!("start `{}`: {error}", self.program))
    }
}

pub struct Connection {
    pub client: Arc<Client>,
    pub events: Receiver<Event>,
    pub session: SessionId,
    pub model: String,
}

pub fn connect(target: &Target) -> Result<Connection, String> {
    let mut child = target.spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "agent has no stdin".to_owned())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "agent has no stdout".to_owned())?;

    let (out_tx, out_rx) = flume::unbounded::<String>();
    let (events_tx, events_rx) = flume::unbounded::<Event>();
    let client = Arc::new(Client {
        next_id: AtomicI64::new(0),
        out: out_tx,
        pending: Mutex::new(HashMap::new()),
        events: events_tx,
        session: Mutex::new(None),
    });

    spawn_writer(stdin, out_rx)?;
    spawn_reader(stdout, Arc::clone(&client))?;
    thread::Builder::new()
        .name("acp-wait".into())
        .spawn(move || {
            let _ = child.wait();
        })
        .map_err(|error| format!("start agent waiter: {error}"))?;

    client.initialize()?;
    let (session, model) = client.new_session(target.session_cwd())?;
    Ok(Connection {
        client,
        events: events_rx,
        session,
        model,
    })
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

fn spawn_reader(
    stdout: impl std::io::Read + Send + 'static,
    client: Arc<Client>,
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
                client.dispatch(message);
            }
            client.abort_pending();
            let _ = client.emit(Event::Closed("agent closed the connection".to_owned()));
        })
        .map_err(|error| format!("start agent reader: {error}"))?;
    Ok(())
}

pub struct Client {
    next_id: AtomicI64,
    out: Sender<String>,
    pending: Mutex<HashMap<i64, Sender<Result<Value, String>>>>,
    events: Sender<Event>,
    session: Mutex<Option<SessionId>>,
}

impl Client {
    fn abort_pending(&self) {
        for (_, sender) in self.pending.lock().unwrap().drain() {
            let _ = sender.send(Err(AGENT_GONE.to_owned()));
        }
    }

    pub fn emit(&self, event: Event) -> Result<(), String> {
        self.events.send(event).map_err(|_| "ui closed".to_owned())
    }

    pub fn initialize(&self) -> Result<(), String> {
        let mut meta = serde_json::Map::new();
        meta.insert(UI_CAPABILITY_KEY.to_owned(), Value::Bool(true));
        let request = InitializeRequest::new(ProtocolVersion::LATEST).client_capabilities(
            ClientCapabilities::new()
                .fs(FileSystemCapabilities::new())
                .meta(meta),
        );
        self.request::<_, Value>(INITIALIZE_METHOD, request, Some(INITIALIZE_TIMEOUT))?;
        Ok(())
    }

    pub fn complete_arguments(&self, text: &str) -> Result<Vec<ArgItem>, String> {
        let response: CompleteResponse = self.request(
            COMPLETE_METHOD,
            json!({ "text": text }),
            Some(COMPLETE_TIMEOUT),
        )?;
        Ok(response.items)
    }

    pub fn new_session(&self, cwd: &Path) -> Result<(SessionId, String), String> {
        let response: NewSessionResponse = self.request(
            SESSION_NEW_METHOD,
            NewSessionRequest::new(cwd),
            Some(SESSION_TIMEOUT),
        )?;
        let model = response
            .config_options
            .as_deref()
            .and_then(model_from)
            .unwrap_or_default();
        *self.session.lock().unwrap() = Some(response.session_id.clone());
        Ok((response.session_id, model))
    }

    pub fn prompt(&self, text: &str) -> Result<StopReason, String> {
        let session = self
            .session
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| "no active session".to_owned())?;
        let request = PromptRequest::new(
            session,
            vec![ContentBlock::Text(TextContent::new(text.to_owned()))],
        );
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
        if self.out.send(text).is_err() {
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
            Some(error) => Err(error.to_string()),
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
            let _ = self.out.send(text);
        }
    }
}

pub fn model_from(options: &[SessionConfigOption]) -> Option<String> {
    options
        .iter()
        .find(|option| matches!(option.category, Some(SessionConfigOptionCategory::Model)))
        .and_then(|option| match &option.kind {
            SessionConfigKind::Select(select) => Some(select.current_value.0.to_string()),
            _ => None,
        })
        .filter(|model| !model.is_empty())
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

    use super::{ACP_SUBCOMMAND, MODEL_FLAG, SSH_PROGRAM, Target};
    use test_case::test_case;

    const HOST: &str = "buildbox";
    const REMOTE_PATH: &str = "/srv/app";
    const QUOTED_PATH: &str = "/srv/it's";
    const MODEL: &str = "claude-sonnet-4";

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
}
