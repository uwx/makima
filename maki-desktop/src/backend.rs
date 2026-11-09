use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use flume::Receiver;
use serde_json::Value;

use crate::acp::{self, Client, Event, PickOutcome, Target};

pub enum Command {
    Prompt(String),
    Cancel,
}

pub struct Backend {
    client: Arc<Client>,
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
        let backend = Backend {
            model: connection.model,
            session: acp::short_session(&connection.session),
            cwd: target.session_cwd().to_path_buf(),
            target: target.label().to_owned(),
            client: connection.client,
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
}
