//! Watching a live session from another connection. Each session records what
//! it emitted, so an attach can replay that first and then follow along live.
//! Watchers never write: the connection that owns the session stays the only
//! one that can prompt, change options or rewind.

use std::collections::{HashMap, VecDeque};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use flume::Sender;
use serde_json::{Value, json};

use crate::ui::REWOUND_METHOD;

/// Client to server: watch the session with this id, read only.
pub const ATTACH_METHOD: &str = "maki/attach";
/// Client to server: the sessions this process can be watched, with how many
/// watchers each has.
pub const LIVE_METHOD: &str = "maki/live";
/// Client to server: an older client asking to drive the session. Every
/// connection may act on it, so the answer is always yes.
pub const TAKE_CONTROL_METHOD: &str = "maki/take_control";
/// Server to client: the session started or finished a turn, so a window knows
/// whether its Stop button would do anything.
pub const TURN_METHOD: &str = "maki/ui/turn";

/// Answers `session/new` or `session/load` while another connection already
/// owns the process's session.
pub const BUSY_CODE: i32 = -32011;
pub const BUSY_MESSAGE: &str = "this listener already serves a session; attach to it instead";

const SESSION_UPDATE: &str = "session/update";
const CONFIG_OPTION_UPDATE: &str = "config_option_update";
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
    pub fn register(&self, id: &str, tap: Arc<SessionTap>) {
        self.sessions.lock().unwrap().insert(id.to_owned(), tap);
    }

    /// A session tap no connection owns yet, for tests and for the eager tap of
    /// a connection that has not created a session.
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
                    if message
                        .pointer("/params/update/sessionUpdate")
                        .and_then(Value::as_str)
                        == Some(CONFIG_OPTION_UPDATE)
                        && let Some(options) = message.pointer("/params/update/configOptions")
                    {
                        tap.set_info(options.clone());
                    }
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
        // A turn can end in a task of its own, whose last message is this one,
        // so every message is a chance for the flag to have moved.
        for tap in self.sessions.lock().unwrap().values() {
            tap.refresh_busy();
        }
    }
}

/// Whether the session is in a turn, as its own bookkeeping sees it.
pub type BusyProbe = Arc<dyn Fn() -> bool + Send + Sync>;

/// A request from a watching connection that holds control, on its way to the
/// loop that owns the session.
pub struct RemoteRequest {
    pub message: Value,
    pub reply: Sender<Value>,
}

/// What one session emitted, who is watching it, and which connection may act
/// on it.
pub struct SessionTap {
    state: Mutex<TapState>,
    /// Whether the session is in a turn, for attaching watchers and for the
    /// announcements below.
    busy: AtomicBool,
    /// Reads the session's own idea of that, so a turn that ends inside another
    /// task still moves the flag.
    busy_probe: Mutex<Option<BusyProbe>>,
    /// The connection the session belongs to. It shows the session whether or
    /// not it is the one acting, so it hears about turns and resets.
    owner: Mutex<Option<Sender<Value>>>,
    /// The connection that asked for the turn in flight, whose window answers
    /// what the session asks: permission questions, pickers, completions.
    asker: Mutex<Option<Sender<Value>>>,
    /// The session's config options, so an attach can paint its status bar
    /// without asking the owning loop.
    info: Mutex<Option<Value>>,
    inbound: (Sender<RemoteRequest>, flume::Receiver<RemoteRequest>),
}

impl Default for SessionTap {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            busy: AtomicBool::new(false),
            busy_probe: Mutex::default(),
            owner: Mutex::default(),
            asker: Mutex::default(),
            info: Mutex::default(),
            inbound: flume::unbounded(),
        }
    }
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

    pub fn busy(&self) -> bool {
        self.busy.load(Ordering::Relaxed)
    }

    /// Watches the session's turn state, so [`Self::refresh_busy`] can follow it.
    pub fn watch_busy(&self, probe: BusyProbe) {
        *self.busy_probe.lock().unwrap() = Some(probe);
    }

    /// Asks the session whether a turn is running, announcing any change.
    pub fn refresh_busy(&self) {
        let probe = self.busy_probe.lock().unwrap().clone();
        if let Some(probe) = probe {
            self.set_busy(probe());
        }
    }

    /// Records whether a turn is running, and tells the watchers when that
    /// changed.
    pub fn set_busy(&self, busy: bool) {
        if self.busy.swap(busy, Ordering::Relaxed) == busy {
            return;
        }
        self.announce(json!({
            "jsonrpc": "2.0",
            "method": TURN_METHOD,
            "params": { "running": busy },
        }));
    }

    pub fn watchers(&self) -> usize {
        self.state.lock().unwrap().subscribers.len()
    }

    /// Where requests from controlling watchers arrive.
    pub fn inbox(&self) -> flume::Receiver<RemoteRequest> {
        self.inbound.1.clone()
    }

    /// Hands a request from a watcher to the owning loop. False when the loop
    /// is gone.
    pub fn forward(&self, message: Value, reply: Sender<Value>) -> bool {
        self.inbound
            .0
            .send(RemoteRequest { message, reply })
            .is_ok()
    }

    pub fn set_info(&self, options: Value) {
        *self.info.lock().unwrap() = Some(options);
    }

    pub fn info(&self) -> Option<Value> {
        self.info.lock().unwrap().clone()
    }

    pub fn set_owner(&self, owner: Sender<Value>) {
        *self.owner.lock().unwrap() = Some(owner);
    }

    pub fn set_asker(&self, asker: Sender<Value>) {
        *self.asker.lock().unwrap() = Some(asker);
    }

    /// Where this session's questions go: the connection that asked for the
    /// turn in flight, or the one that owns the session when nobody did.
    pub fn asker(&self) -> Option<Sender<Value>> {
        self.asker
            .lock()
            .unwrap()
            .clone()
            .or_else(|| self.owner.lock().unwrap().clone())
    }

    /// Sends {message} to the owner and every watcher, without recording it:
    /// these are about the session, not part of its transcript.
    pub fn announce(&self, message: Value) {
        let mut targets: Vec<Sender<Value>> = self.state.lock().unwrap().subscribers.to_vec();
        if let Some(owner) = self.owner.lock().unwrap().clone()
            && !targets
                .iter()
                .any(|target| target.same_channel(&owner))
        {
            targets.push(owner);
        }
        for target in targets {
            let _ = target.send(message.clone());
        }
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
        record(&mut state, message);
        for subscriber in &state.subscribers {
            let _ = subscriber.send(message.clone());
        }
    }

    /// Records a message and shows it to the owner and every watcher except
    /// {asker}, which already drew it.
    pub fn publish_elsewhere(&self, message: &Value, asker: &Sender<Value>) {
        {
            let mut state = self.state.lock().unwrap();
            record(&mut state, message);
            for subscriber in &state.subscribers {
                if !subscriber.same_channel(asker) {
                    let _ = subscriber.send(message.clone());
                }
            }
        }
        if let Some(owner) = self.owner.lock().unwrap().clone()
            && !owner.same_channel(asker)
        {
            let _ = owner.send(message.clone());
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

fn record(state: &mut TapState, message: &Value) {
    if state.log.len() == MAX_LOG_UPDATES {
        state.log.pop_front();
        state.truncated = true;
    }
    state.log.push_back(message.clone());
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
        assert_eq!(
            rx.recv().unwrap()["params"]["update"]["content"]["text"],
            "one"
        );
        assert_eq!(
            rx.recv().unwrap()["params"]["update"]["content"]["text"],
            "two"
        );

        taps.observe(&update("s1", "three"));
        assert_eq!(
            rx.recv().unwrap()["params"]["update"]["content"]["text"],
            "three"
        );
    }

    #[test]
    fn a_message_carries_the_turn_state_its_probe_reports() {
        let taps = Taps::default();
        let tap = taps.ensure("s1");
        let running = Arc::new(AtomicBool::new(false));
        let probe = Arc::clone(&running);
        tap.watch_busy(Arc::new(move || probe.load(Ordering::Relaxed)));
        let (tx, rx) = flume::unbounded();
        tap.subscribe(tx);

        let next_turn = || loop {
            let message = rx.recv().unwrap();
            if message["method"] == TURN_METHOD {
                return message["params"]["running"].clone();
            }
        };

        running.store(true, Ordering::Relaxed);
        taps.observe(&update("s1", "one"));
        assert_eq!(next_turn(), json!(true));

        running.store(false, Ordering::Relaxed);
        taps.observe(&update("s1", "two"));
        assert_eq!(next_turn(), json!(false));
    }

    #[test]
    fn a_turn_is_announced_once_per_change() {
        let taps = Taps::default();
        let tap = taps.ensure("s1");
        let (tx, rx) = flume::unbounded();
        tap.subscribe(tx);

        tap.set_busy(true);
        tap.set_busy(true);
        tap.set_busy(false);
        tap.set_busy(false);

        let running: Vec<Value> = rx
            .drain()
            .map(|message| message["params"]["running"].clone())
            .collect();
        assert_eq!(running, vec![json!(true), json!(false)]);
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
        assert_eq!(
            rx.recv().unwrap()["params"]["update"]["content"]["text"],
            "two"
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn questions_go_to_the_asker_and_to_the_owner_without_one() {
        let tap = SessionTap::default();
        let (owner, _owner_rx) = flume::unbounded();
        let (phone, _phone_rx) = flume::unbounded();
        tap.set_owner(owner.clone());
        assert!(tap.asker().unwrap().same_channel(&owner));
        tap.set_asker(phone.clone());
        assert!(tap.asker().unwrap().same_channel(&phone));
    }

    #[test]
    fn a_forwarded_request_carries_its_reply() {
        let tap = SessionTap::default();
        let (out, out_rx) = flume::unbounded();
        assert!(tap.forward(json!({ "id": 1 }), out));
        let request = tap.inbox().recv().unwrap();
        assert_eq!(request.message["id"], 1);
        let _ = request.reply.send(json!({ "ok": true }));
        assert_eq!(out_rx.recv().unwrap()["ok"], true);
    }

    #[test]
    fn announcements_reach_the_owner_and_watchers() {
        let tap = SessionTap::default();
        let (owner, owner_rx) = flume::unbounded();
        let (watcher, watcher_rx) = flume::unbounded();
        tap.set_owner(owner);
        tap.subscribe(watcher);
        tap.announce(json!({ "method": REWOUND_METHOD }));
        assert_eq!(owner_rx.recv().unwrap()["method"], REWOUND_METHOD);
        assert_eq!(watcher_rx.recv().unwrap()["method"], REWOUND_METHOD);
    }

    #[test]
    fn a_turn_reaches_the_owner_and_every_watcher() {
        let tap = SessionTap::default();
        let (owner, owner_rx) = flume::unbounded();
        let (phone, phone_rx) = flume::unbounded();
        tap.set_owner(owner);
        tap.subscribe(phone);

        tap.set_busy(true);

        assert_eq!(owner_rx.recv().unwrap()["params"]["running"], json!(true));
        assert_eq!(phone_rx.recv().unwrap()["params"]["running"], json!(true));
    }

    #[test]
    fn an_owner_that_also_watches_is_told_once() {
        let tap = SessionTap::default();
        let (owner, owner_rx) = flume::unbounded();
        tap.set_owner(owner.clone());
        tap.subscribe(owner);

        tap.set_busy(true);

        assert_eq!(owner_rx.recv().unwrap()["params"]["running"], json!(true));
        assert!(owner_rx.try_recv().is_err());
    }

    #[test]
    fn a_prompt_is_shown_to_everyone_but_the_one_who_asked() {
        let tap = SessionTap::default();
        let (asker, asker_rx) = flume::unbounded();
        let (watcher, watcher_rx) = flume::unbounded();
        let (owner, owner_rx) = flume::unbounded();
        tap.set_owner(owner);
        tap.subscribe(watcher);

        tap.publish_elsewhere(&update("s1", "one"), &asker);

        assert!(asker_rx.try_recv().is_err(), "the asker drew it already");
        assert!(watcher_rx.try_recv().is_ok());
        assert!(owner_rx.try_recv().is_ok());

        let (late, _late_rx) = flume::unbounded();
        assert_eq!(tap.subscribe(late).0, 1, "a later watcher replays it");
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
