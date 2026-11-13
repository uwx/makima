//! Renders Lua plugin UI actions on an ACP client and routes its answers back.
//!
//! A client that advertises [`UI_CAPABILITY_KEY`] receives `makima/ui/*`
//! traffic and may call [`COMPLETE_METHOD`]; any other client keeps those
//! commands hidden, so it never sees a command it cannot render.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use flume::{Receiver, Sender};
use makima_lua::ui_sink::UiSink;
use makima_lua::{PickerConfig, PickerItemSpec, PickerResult, UiAction};
use serde_json::{Value, json};

use crate::server::outgoing_request_id;

/// The `clientCapabilities._meta` key that opts a client into `makima/ui/*`.
pub const UI_CAPABILITY_KEY: &str = "makima-ui";
pub(crate) const UI_CAPABILITY_PATH: &str = "/params/clientCapabilities/_meta/makima-ui";
pub const FLASH_METHOD: &str = "makima/ui/flash";
pub const PICK_METHOD: &str = "makima/ui/pick";
/// Server to client: the history was rewound, so drop the transcript before
/// the replacement history arrives as `session/update` notifications.
pub const REWOUND_METHOD: &str = "makima/ui/rewind";
/// Client to server: complete the command argument being typed.
pub const COMPLETE_METHOD: &str = "makima/complete";

const CHOICE_FIELD: &str = "choice";
const DELETE_FIELD: &str = "delete";

/// The bridge's two ends: [`sink`](Self::sink) goes to the plugin host, and
/// [`events`](Self::events) are folded into the server's output stream.
pub struct UiChannel {
    pub sink: UiBridge,
    pub events: Receiver<Value>,
}

impl UiChannel {
    pub fn new() -> Self {
        let (out, events) = flume::unbounded();
        Self {
            sink: UiBridge {
                out,
                pending: Arc::default(),
                enabled: Arc::default(),
            },
            events,
        }
    }
}

impl Default for UiChannel {
    fn default() -> Self {
        Self::new()
    }
}

/// The [`UiSink`] a plugin host renders into. Clones share the opt-in flag and
/// the pending picks with the server.
#[derive(Clone)]
pub struct UiBridge {
    out: Sender<Value>,
    pending: Arc<Mutex<HashMap<i64, Sender<PickerResult>>>>,
    enabled: Arc<AtomicBool>,
}

/// A bridge no client reads: messages to it are dropped. Used where a server
/// has no opt-in client, such as tests.
impl Default for UiBridge {
    fn default() -> Self {
        UiChannel::new().sink
    }
}

impl UiBridge {
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn enable(&self) {
        self.enabled.store(true, Ordering::Relaxed);
    }

    /// Delivers the client's answer to request {id}; returns whether {id} was
    /// a picker request.
    pub fn answer(&self, id: i64, raw: &Value) -> bool {
        let Some(reply_tx) = self.pending.lock().unwrap().remove(&id) else {
            return false;
        };
        let _ = reply_tx.send(picker_result(raw));
        true
    }

    fn pick(
        &mut self,
        items: Vec<PickerItemSpec>,
        config: PickerConfig,
        reply_tx: Sender<PickerResult>,
    ) {
        if !self.enabled() {
            return;
        }
        let id = outgoing_request_id();
        self.pending.lock().unwrap().insert(id, reply_tx);
        let sent = self.out.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": PICK_METHOD,
            "params": pick_params(&items, &config),
        }));
        if sent.is_err() {
            self.pending.lock().unwrap().remove(&id);
        }
    }
}

impl UiSink for UiBridge {
    fn notify(&mut self, text: String) {
        if self.enabled() {
            let _ = self.out.send(json!({
                "jsonrpc": "2.0",
                "method": FLASH_METHOD,
                "params": { "text": text },
            }));
        }
    }

    fn render(&mut self, action: UiAction) {
        match action {
            UiAction::Flash(text) => self.notify(text),
            UiAction::OpenListPicker {
                items,
                config,
                reply_tx,
                ..
            } => self.pick(items, config, reply_tx),
            _ => {}
        }
    }
}

fn picker_result(raw: &Value) -> PickerResult {
    let Some(result) = raw.get("result") else {
        return PickerResult::Close;
    };
    if let Some(index) = result.get(DELETE_FIELD).and_then(Value::as_u64) {
        return PickerResult::Delete(index as usize);
    }
    match result.get(CHOICE_FIELD).and_then(Value::as_u64) {
        Some(index) => PickerResult::Choice(index as usize),
        None => PickerResult::Close,
    }
}

fn pick_params(items: &[PickerItemSpec], config: &PickerConfig) -> Value {
    let rows = items
        .iter()
        .map(|item| {
            json!({
                "label": item.label,
                "detail": item.detail,
                "section": item.section,
                "sectionDetail": item.section_detail,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "title": config.title,
        "footer": config.footer,
        "cursor": config.cursor,
        "notifyInitial": config.notify_initial,
        "items": rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TITLE: &str = "Pick a session";
    const LABEL: &str = "first";

    fn item(label: &str) -> PickerItemSpec {
        PickerItemSpec {
            label: label.to_owned(),
            detail: Some("detail".to_owned()),
            ..PickerItemSpec::default()
        }
    }

    #[test]
    fn capability_path_names_the_capability_key() {
        assert!(UI_CAPABILITY_PATH.ends_with(UI_CAPABILITY_KEY));
    }

    #[test]
    fn disabled_bridge_sends_nothing() {
        let channel = UiChannel::new();
        let mut bridge = channel.sink.clone();
        bridge.notify("ignored".to_owned());
        assert!(channel.events.try_recv().is_err());
    }

    #[test]
    fn flash_is_sent_once_enabled() {
        let channel = UiChannel::new();
        let mut bridge = channel.sink.clone();
        bridge.enable();
        bridge.notify("saved".to_owned());
        let message = channel.events.try_recv().unwrap();
        assert_eq!(message["method"], FLASH_METHOD);
        assert_eq!(message["params"]["text"], "saved");
    }

    #[test]
    fn pick_carries_items_and_title() {
        let channel = UiChannel::new();
        let mut bridge = channel.sink.clone();
        bridge.enable();
        let (reply_tx, _reply_rx) = flume::bounded(1);
        let config = PickerConfig {
            title: Some(TITLE.to_owned()),
            cursor: Some(1),
            ..PickerConfig::default()
        };
        bridge.pick(vec![item(LABEL)], config, reply_tx);
        let message = channel.events.try_recv().unwrap();
        assert_eq!(message["method"], PICK_METHOD);
        assert_eq!(message["params"]["title"], TITLE);
        assert_eq!(message["params"]["cursor"], 1);
        assert_eq!(message["params"]["items"][0]["label"], LABEL);
    }

    #[test]
    fn answer_marks_a_pick_done() {
        let channel = UiChannel::new();
        let mut bridge = channel.sink.clone();
        bridge.enable();
        let (reply_tx, reply_rx) = flume::bounded(1);
        bridge.pick(vec![item(LABEL)], PickerConfig::default(), reply_tx);
        let request = channel.events.try_recv().unwrap();
        let id = request["id"].as_i64().unwrap();
        assert!(bridge.answer(id, &json!({ "result": { (CHOICE_FIELD): 0 } })));
        assert_eq!(reply_rx.try_recv().unwrap(), PickerResult::Choice(0));
        assert!(!bridge.answer(id, &json!({ "result": {} })));
    }
}
