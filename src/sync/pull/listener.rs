//! R-realtime (§10.2): `LISTEN sync_changes` (payload = branch id, sent by
//! `sync_emit`) → `sync.changed { branch_id }` on `Topic::Sync`, debounced to at
//! most one event per branch per second. Devices pull on it; their poll
//! fallback stays.
use std::collections::HashMap;
use std::time::{Duration, Instant};

use sqlx::PgPool;
use sqlx::postgres::PgListener;
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

pub fn spawn(pool: PgPool, hub: BranchEventHub) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = run(&pool, &hub).await {
                tracing::warn!(error = %e, "sync LISTEN task restarting");
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}

async fn run(pool: &PgPool, hub: &BranchEventHub) -> Result<(), sqlx::Error> {
    let mut listener = PgListener::connect_with(pool).await?;
    listener.listen(CHANNEL).await?;
    let mut debounce = Debouncer::default();
    loop {
        let next = tokio::time::timeout(Duration::from_millis(250), listener.recv()).await;
        let now = Instant::now();
        if let Ok(n) = next {
            let n = n?;
            if let Ok(branch) = Uuid::parse_str(n.payload()) {
                if debounce.on_notify(branch, now) {
                    publish(hub, branch);
                }
            }
        }
        for branch in debounce.due(now) {
            publish(hub, branch);
        }
    }
}
