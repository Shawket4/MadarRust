//! In-process pub/sub hub for delivery-order changes, fanned out to staff POS
//! clients over SSE (`GET /delivery-orders/stream`).
//!
//! One `broadcast::Sender` per branch, created lazily. Per-branch channels give
//! tenant isolation **by construction**: a subscriber for branch A holds a
//! receiver that is physically incapable of carrying branch B's events, so a
//! customer's name/phone/address can never leak across branches no matter what a
//! handler forgets to filter.
//!
//! Known limitation: this is single-instance, in-process memory. If the backend
//! is ever scaled horizontally, a publish on instance 1 won't reach a subscriber
//! pinned to instance 2. The migration path is to back `publish`/`subscribe` with
//! Redis pub/sub or Postgres LISTEN/NOTIFY — the handler-side API stays the same.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;
use utoipa::ToSchema;
use uuid::Uuid;

use super::staff::DeliveryOrder;

/// What rides the SSE wire. The POS upserts `order` by id, and on
/// `event_type == "created"` fires its new-order alert. `order` is the exact
/// same shape returned by `GET /delivery-orders`, so the client needs no second
/// model.
#[derive(Clone, serde::Serialize, ToSchema)]
pub struct DeliveryEvent {
    /// `"created"` (intake) | `"updated"` (status / cancel / finalize / prep-time).
    pub event_type: String,
    pub order: DeliveryOrder,
}

/// Per-branch broadcast capacity. A slow client that falls this far behind is
/// dropped (the stream surfaces `Lagged`, the SSE handler ends the connection,
/// and the POS re-GETs + reconnects), so this only bounds memory.
const CHANNEL_CAPACITY: usize = 128;

/// Branch-keyed broadcast registry. Cheap to clone (`Arc` inside) so it lives in
/// `web::Data` and is shared across all actix workers.
#[derive(Clone, Default)]
pub struct DeliveryHub {
    inner: Arc<Mutex<HashMap<Uuid, broadcast::Sender<DeliveryEvent>>>>,
}

impl DeliveryHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe to a branch's stream, creating the channel on first use.
    pub fn subscribe(&self, branch_id: Uuid) -> broadcast::Receiver<DeliveryEvent> {
        let mut map = self.inner.lock().expect("delivery hub mutex poisoned");
        map.entry(branch_id)
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .subscribe()
    }

    /// Publish an event to a branch. No-op if no one is subscribed (no channel
    /// exists yet, or `send` reports zero receivers) — both are fine.
    pub fn publish(&self, branch_id: Uuid, event: DeliveryEvent) {
        // Clone the sender out under the lock, then send outside it — the lock is
        // never held across the (non-async, but still avoidable) send work.
        let sender = {
            let map = self.inner.lock().expect("delivery hub mutex poisoned");
            map.get(&branch_id).cloned()
        };
        if let Some(tx) = sender {
            let _ = tx.send(event);
        }
    }
}
