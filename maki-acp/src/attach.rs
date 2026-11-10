//! Watching a live session from another connection. Each session records what
//! it emitted, so an attach can replay that first and then follow along live.
//! Watchers never write: the connection that owns the session stays the only
//! one that can prompt, change options or rewind.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use flume::Sender;
use serde_json::{Value, json};

use crate::ui::REWOUND_METHOD;

/// Client to server: watch the session with this id, read only.
pub const ATTACH_METHOD: &str = "maki/attach";
/// Client to server: the sessions this process can be watched, with how many
/// watchers each has.
pub const LIVE_METHOD: &str = "maki/live";
/// Answers a session-scoped request from a connection that only watches.
pub const READ_ONLY_CODE: i32 = -32010;
pub const READ_ONLY_MESSAGE: &str = "this connection is read-only";

/// Answers `session/new` or `session/load` while another connection already
/// owns the process's session.
pub const BUSY_CODE: i32 = -32011;
pub const BUSY_MESSAGE: &str = "this listener already serves a session; attach to it instead";

const SESSION_UPDATE: &str = "session/update";
/// Older updates than this are dropped, and the attach says so.
const MAX_LOG_UPDATES: usize = 4000;

/// The live sessions of one server process, by id.
#[derive(Default)]
pub struct Taps {
    sessions: Mutex<HashMap<String, Arc<SessionTap>>>,
    /// The one session this process serves. A second `session/new` would starve
    /// the first of the plugin host, so it is refused.
    owner: Mutex<Option<String>>,
}

impl Taps {
    /// Claims the process's session for {id}, or reports that another
    /// connection got there first.
    pub fn claim(&self, id: &str) -> bool {
        let mut owner = self.owner.lock().unwrap();
        match owner.as_deref() {
            None => {
                *owner = Some(id.to_owned());
                true
            }
            Some(current) => current == id,
        }
    }

    pub fn owner(&self) -> Option<String> {
        self.owner.lock().unwrap().clone()
    }

    /// Frees the session slot, so the next connection can create one.
    pub fn release(&self, id: &str) {
        let mut owner = self.owner.lock().unwrap();
        if owner.as_deref() == Some(id) {
            *owner = None;
        }
    }

    /// Starts recording the session {id}, replacing any earlier recording of
    /// the same id.
    pub fn ensure(&self, id: &str) -> Arc<SessionTap> {
        self.sessions
            .lock()
            .unwrap()
            .entry(id.to_owned())
            .or_default()
            .clone()
    }

    pub fn get(&self, id: &str) -> Option<Arc<SessionTap>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    /// Forgets the session, so attaching to it fails.
    pub fn remove(&self, id: &str) {
        self.sessions.lock().unwrap().remove(id);
    }

    /// The live sessions, oldest registration first only if ids sort so.
    pub fn live(&self) -> Vec<Value> {
        self.sessions
            .lock()
            .unwrap()
            .iter()
            .map(|(id, tap)| json!({ "sessionId": id, "watchers": tap.watchers() }))
            .collect()
    }

    /// Records a message a session emitted and forwards it to its watchers.
    /// Every other message is left alone, so responses stay private.
    pub fn observe(&self, message: &Value) {
        match message.get("method").and_then(Value::as_str) {
            Some(SESSION_UPDATE) => {
                let Some(id) = message.pointer("/params/sessionId").and_then(Value::as_str) else {
                    return;
                };
                if let Some(tap) = self.get(id) {
                    tap.publish(message);
                }
            }
            // A rewind replaced the transcript: watchers clear what they show,
            // and the updates that follow rebuild it.
            Some(REWOUND_METHOD) => {
                for tap in self.sessions.lock().unwrap().values() {
                    tap.reset(message);
                }
            }
            _ => {}
        }
    }
}

/// What one session emitted, and who is watching it.
#[derive(Default)]
pub struct SessionTap {
    state: Mutex<TapState>,
}

#[derive(Default)]
struct TapState {
    log: VecDeque<Value>,
    subscribers: Vec<Sender<Value>>,
    truncated: bool,
}

impl SessionTap {
    /// Sends what the session emitted so far to {out}, then adds it as a
    /// watcher. The replay and the subscription happen under one lock, so no
    /// update is missed or delivered twice. Returns the replay size and
    /// whether older updates had been dropped.
    pub fn subscribe(&self, out: Sender<Value>) -> (usize, bool) {
        let mut state = self.state.lock().unwrap();
        for message in &state.log {
            let _ = out.send(message.clone());
        }
        let replayed = state.log.len();
        let truncated = state.truncated;
        state.subscribers.push(out);
        (replayed, truncated)
    }

    pub fn watchers(&self) -> usize {
        self.state.lock().unwrap().subscribers.len()
    }

    pub fn detach(&self, out: &Sender<Value>) {
        self.state
            .lock()
            .unwrap()
            .subscribers
            .retain(|subscriber| !subscriber.same_channel(out));
    }

    fn publish(&self, message: &Value) {
        let mut state = self.state.lock().unwrap();
        if state.log.len() == MAX_LOG_UPDATES {
            state.log.pop_front();
            state.truncated = true;
        }
        state.log.push_back(message.clone());
        for subscriber in &state.subscribers {
            let _ = subscriber.send(message.clone());
        }
    }

    fn reset(&self, message: &Value) {
        let mut state = self.state.lock().unwrap();
        state.log.clear();
        state.truncated = false;
        for subscriber in &state.subscribers {
            let _ = subscriber.send(message.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn update(session: &str, text: &str) -> Value {
        json!({
            "jsonrpc": "2.0",
            "method": SESSION_UPDATE,
            "params": { "sessionId": session, "update": { "sessionUpdate": "agentMessageChunk", "content": { "type": "text", "text": text } } },
        })
    }

    #[test]
    fn an_attach_replays_what_the_session_emitted() {
        let taps = Taps::default();
        taps.ensure("s1");
        taps.observe(&update("s1", "one"));
        taps.observe(&update("s1", "two"));

        let (tx, rx) = flume::unbounded();
        let (replayed, truncated) = taps.get("s1").unwrap().subscribe(tx);
        assert_eq!(replayed, 2);
        assert!(!truncated);
        assert_eq!(rx.recv().unwrap()["params"]["update"]["content"]["text"], "one");
        assert_eq!(rx.recv().unwrap()["params"]["update"]["content"]["text"], "two");

        taps.observe(&update("s1", "three"));
        assert_eq!(rx.recv().unwrap()["params"]["update"]["content"]["text"], "three");
    }

    #[test]
    fn watching_another_session_sees_nothing() {
        let taps = Taps::default();
        taps.ensure("s1");
        taps.ensure("s2");
        let (tx, rx) = flume::unbounded();
        taps.get("s2").unwrap().subscribe(tx);

        taps.observe(&update("s1", "one"));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_detached_watcher_stops_receiving() {
        let taps = Taps::default();
        taps.ensure("s1");
        let (tx, rx) = flume::unbounded();
        let tap = taps.get("s1").unwrap();
        tap.subscribe(tx.clone());
        tap.detach(&tx);

        taps.observe(&update("s1", "one"));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_rewind_clears_the_replay_and_tells_watchers() {
        let taps = Taps::default();
        taps.ensure("s1");
        taps.observe(&update("s1", "one"));
        let (tx, rx) = flume::unbounded();
        taps.get("s1").unwrap().subscribe(tx);
        let _ = rx.recv();

        taps.observe(&json!({ "jsonrpc": "2.0", "method": REWOUND_METHOD, "params": {} }));
        assert_eq!(rx.recv().unwrap()["method"], REWOUND_METHOD);
        taps.observe(&update("s1", "two"));

        let (tx, rx) = flume::unbounded();
        let (replayed, _) = taps.get("s1").unwrap().subscribe(tx);
        assert_eq!(replayed, 1);
        assert_eq!(rx.recv().unwrap()["params"]["update"]["content"]["text"], "two");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn one_connection_owns_the_session_slot() {
        let taps = Taps::default();
        assert!(taps.claim("s1"));
        assert!(taps.claim("s1"));
        assert!(!taps.claim("s2"));
        taps.release("s1");
        assert!(taps.claim("s2"));
    }

    #[test]
    fn live_lists_watchers() {
        let taps = Taps::default();
        taps.ensure("s1");
        let (tx, _rx) = flume::unbounded();
        taps.get("s1").unwrap().subscribe(tx);
        let live = taps.live();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0]["sessionId"], "s1");
        assert_eq!(live[0]["watchers"], 1);
    }

    #[test]
    fn a_closed_session_cannot_be_attached() {
        let taps = Taps::default();
        taps.ensure("s1");
        taps.remove("s1");
        assert!(taps.get("s1").is_none());
    }
}
