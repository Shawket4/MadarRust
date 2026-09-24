//! A stand-in for FCM that the integration tests read: once [`install`]ed,
//! every push the server would have sent to Google is recorded here instead,
//! and answered with the status the test chose (200 unless told otherwise).
//!
//! Inert in production: nothing installs it, and release builds never route a
//! push here (the transport check is compiled only under `debug_assertions`).
//! Process-global, which is what nextest's one-process-per-test wants; tests
//! still filter by their own tokens so a shared process would not confuse them.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

#[derive(Default)]
struct Sink {
    sent: Vec<Value>,
    status: HashMap<String, (u16, Value)>,
    default_status: Option<(u16, Value)>,
    done: Vec<String>,
}

static SINK: Mutex<Option<Sink>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut Sink) -> R) -> Option<R> {
    let mut g = SINK.lock().unwrap_or_else(|e| e.into_inner());
    g.as_mut().map(f)
}

/// Route every push in this process to the sink.
pub fn install() {
    let mut g = SINK.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        *g = Some(Sink::default());
    }
}

pub fn installed() -> bool {
    SINK.lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

/// Answer every send to `token` with `status` and FCM's error `body`.
pub fn respond(token: &str, status: u16, body: Value) {
    with(|s| s.status.insert(token.to_string(), (status, body)));
}

/// Answer every send with `status` (e.g. 503 = FCM down).
pub fn respond_all(status: u16) {
    with(|s| s.default_status = Some((status, Value::Null)));
}

/// Every message sent (and retried) so far, oldest first.
pub fn sent() -> Vec<Value> {
    with(|s| s.sent.clone()).unwrap_or_default()
}

/// The messages sent to one token.
pub fn sent_to(token: &str) -> Vec<Value> {
    sent()
        .into_iter()
        .filter(|m| m["message"]["token"] == token)
        .collect()
}

pub(crate) fn post(msg: &Value) -> Option<(u16, Value)> {
    with(|s| {
        s.sent.push(msg.clone());
        let token = msg["message"]["token"].as_str().unwrap_or_default();
        s.status
            .get(token)
            .or(s.default_status.as_ref())
            .cloned()
            .unwrap_or((200, Value::Null))
    })
}

/// A fan-out finished (every send tried, every dead token dropped).
pub(crate) fn mark_done(what: &str) {
    with(|s| s.done.push(what.to_string()));
}

/// Wait until the fan-out named `what` has finished; false on timeout.
pub async fn wait_done(what: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if with(|s| s.done.iter().any(|d| d == what)).unwrap_or(false) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

/// How many times the fan-out named `what` has finished.
pub fn done_count(what: &str) -> usize {
    with(|s| s.done.iter().filter(|d| *d == what).count()).unwrap_or(0)
}
