//! Per-branch in-process pub/sub hub, generalizing the old `DeliveryHub` to carry
//! every topic ([`BranchEvent`]) on one channel. One `broadcast::Sender` per
//! branch, created lazily — per-branch channels give tenant isolation by
//! construction (a subscriber for branch A physically cannot receive branch B's
//! events). The stream handler additionally filters by topic + permission.
//!
//! Single-instance, in-process. If the backend is scaled horizontally, back
//! `publish`/`subscribe` with Redis pub/sub or Postgres LISTEN/NOTIFY — the
//! handler-side API stays the same.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;
use uuid::Uuid;

use super::event::BranchEvent;

/// Per-branch broadcast capacity. A slow client that falls this far behind is
/// dropped (the stream surfaces `Lagged`, the SSE handler ends the connection,
/// and the client re-seeds + reconnects), so this only bounds memory. Sized a
/// bit larger than the old delivery-only hub since it now multiplexes topics.
const CHANNEL_CAPACITY: usize = 256;

/// Branch-keyed broadcast registry. Cheap to clone (`Arc` inside) so it lives in
/// `web::Data` and is shared across all actix workers.
#[derive(Clone, Default)]
pub struct BranchEventHub {
    inner: Arc<Mutex<HashMap<Uuid, broadcast::Sender<BranchEvent>>>>,
}

impl BranchEventHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe to a branch's bus, creating the channel on first use.
    pub fn subscribe(&self, branch_id: Uuid) -> broadcast::Receiver<BranchEvent> {
        let mut map = self.inner.lock().expect("realtime hub mutex poisoned");
        map.entry(branch_id)
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .subscribe()
    }

    /// Publish an event to a branch. No-op if no one is subscribed.
    pub fn publish(&self, branch_id: Uuid, event: BranchEvent) {
        let sender = {
            let map = self.inner.lock().expect("realtime hub mutex poisoned");
            map.get(&branch_id).cloned()
        };
        if let Some(tx) = sender {
            let _ = tx.send(event);
        }
    }
}
