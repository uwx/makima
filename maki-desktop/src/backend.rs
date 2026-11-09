use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use flume::{Receiver, Sender};
use serde_json::Value;

use crate::acp::{self, Client, Event, PickOutcome, SessionView, Target, ThemeCatalog};

pub enum Command {
    Prompt(String),
    Cancel,
}

pub struct Backend {
    client: Arc<Client>,
    arguments: Sender<String>,
    pub themes: ThemeCatalog,
    pub options: SessionView,
    pub model: String,
    pub session: String,
    pub cwd: PathBuf,
    pub target: String,
}

pub struct Started {
    pub backend: Backend,
    pub events: Receiver<Event>,
}

impl Backend {
    pub fn start(target: Target) -> Result<Started, String> {
        let connection = acp::connect(&target)?;
        let arguments = spawn_argument_worker(Arc::clone(&connection.client))?;
        let backend = Backend {
            client: connection.client,
            arguments,
            themes: connection.themes,
            options: connection.options,
            model: connection.model,
            session: acp::short_session(&connection.session),
            cwd: target.session_cwd().to_path_buf(),
            target: target.label().to_owned(),
        };
        Ok(Started {
            backend,
            events: connection.events,
        })
    }

    pub fn send(&self, command: Command) -> Result<(), String> {
        match command {
            Command::Prompt(text) => {
                let client = Arc::clone(&self.client);
                thread::Builder::new()
                    .name("maki-prompt".into())
                    .spawn(move || {
                        let result = client.prompt(&text);
                        let _ = client.emit(Event::PromptDone(result));
                    })
                    .map_err(|error| format!("start prompt: {error}"))?;
                Ok(())
            }
            Command::Cancel => {
                self.client.cancel();
                Ok(())
            }
        }
    }

    pub fn answer(&self, request: &Value, option: Option<&str>) {
        self.client.answer(request.clone(), option);
    }

    pub fn answer_pick(&self, request: &Value, outcome: PickOutcome) {
        self.client.answer_pick(request.clone(), outcome);
    }

    pub fn complete_arguments(&self, text: String) {
        _ = self.arguments.send(text);
    }

    pub fn set_theme(&self, name: String) -> Result<(), String> {
        let client = Arc::clone(&self.client);
        thread::Builder::new()
            .name("maki-theme".into())
            .spawn(move || {
                let result = client.set_theme(&name);
                let _ = client.emit(Event::Theme(result));
            })
            .map_err(|error| format!("start theme switch: {error}"))?;
        Ok(())
    }

    pub fn list_sessions(&self, query: String) -> Result<(), String> {
        let client = Arc::clone(&self.client);
        thread::Builder::new()
            .name("maki-sessions".into())
            .spawn(move || {
                let result = client.list_sessions(&query);
                let _ = client.emit(Event::Sessions(result));
            })
            .map_err(|error| format!("start session list: {error}"))?;
        Ok(())
    }

    pub fn load_session(&self, id: String) -> Result<(), String> {
        let client = Arc::clone(&self.client);
        let cwd = self.cwd.clone();
        thread::Builder::new()
            .name("maki-session-load".into())
            .spawn(move || {
                let result = client.load_session(&id, &cwd);
                let _ = client.emit(Event::Loaded(result));
            })
            .map_err(|error| format!("start session load: {error}"))?;
        Ok(())
    }

    pub fn list_rewind_turns(&self) -> Result<(), String> {
        let client = Arc::clone(&self.client);
        thread::Builder::new()
            .name("maki-rewind-turns".into())
            .spawn(move || {
                let result = client.rewind_turns();
                let _ = client.emit(Event::RewindTurns(result));
            })
            .map_err(|error| format!("start rewind list: {error}"))?;
        Ok(())
    }

    pub fn rewind(&self, offset: usize) -> Result<(), String> {
        let client = Arc::clone(&self.client);
        thread::Builder::new()
            .name("maki-rewind".into())
            .spawn(move || {
                let result = client.rewind(offset);
                let _ = client.emit(Event::RewindDone(result));
            })
            .map_err(|error| format!("start rewind: {error}"))?;
        Ok(())
    }
}

/// Completion is requested on every composer change, so the worker drops
/// superseded requests and answers only the newest text.
fn spawn_argument_worker(client: Arc<Client>) -> Result<Sender<String>, String> {
    let (tx, rx) = flume::unbounded::<String>();
    thread::Builder::new()
        .name("maki-complete".into())
        .spawn(move || {
            while let Ok(first) = rx.recv() {
                let mut text = first;
                while let Ok(newer) = rx.try_recv() {
                    text = newer;
                }
                let items = client.complete_arguments(&text).unwrap_or_default();
                if client.emit(Event::Completion { text, items }).is_err() {
                    return;
                }
            }
        })
        .map_err(|error| format!("start completion worker: {error}"))?;
    Ok(tx)
}
