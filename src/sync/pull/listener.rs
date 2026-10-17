//! R-realtime (§10.2): `LISTEN sync_changes` (payload = branch id, sent by
//! `sync_emit`) → `sync.changed { branch_id }` on `Topic::Sync`, debounced to at
//! most one event per branch per second. Devices pull on it; their poll
//! fallback stays.
use std::collections::HashMap;
use std::time::{Duration, Instant};

use sqlx::PgPool;
use uuid::Uuid;

use crate::realtime::event::{BranchEvent, Topic};
use crate::realtime::hub::BranchEventHub;

pub const CHANNEL: &str = "sync_changes";
pub const DEBOUNCE: Duration = Duration::from_secs(1);

/// Per-branch debounce: the first notification publishes immediately; later
/// ones inside the window are folded into ONE trailing publish at its end.
#[derive(Default)]
pub struct Debouncer {
    last: HashMap<Uuid, Instant>,
    pending: HashMap<Uuid, Instant>,
}

impl Debouncer {
    /// A notification arrived for `branch` at `now`: publish now?
    pub fn on_notify(&mut self, branch: Uuid, now: Instant) -> bool {
        match self.last.get(&branch) {
            Some(t) if now.duration_since(*t) < DEBOUNCE => {
                self.pending.entry(branch).or_insert(*t + DEBOUNCE);
                false
            }
            _ => {
                self.last.insert(branch, now);
                true
            }
        }
    }

    /// Branches whose trailing publish is due at `now`.
    pub fn due(&mut self, now: Instant) -> Vec<Uuid> {
        let due: Vec<Uuid> = self
            .pending
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(b, _)| *b)
            .collect();
        for b in &due {
            self.pending.remove(b);
            self.last.insert(*b, now);
        }
        due
    }
}

pub fn publish(hub: &BranchEventHub, branch: Uuid) {
    hub.publish(
        branch,
        BranchEvent::new(
            Topic::Sync,
            "sync.changed",
            &serde_json::json!({ "branch_id": branch }),
        ),
    );
}

/// The LISTEN task (now shared with the read fast paths: `fastpath::listener`,
/// which keeps this module's debounce and `sync.changed` publish).
pub fn spawn(pool: PgPool, hub: BranchEventHub) {
    crate::fastpath::listener::spawn(pool, hub)
}
