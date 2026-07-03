//! Per-branch in-process pub/sub hub, generalizing the old `DeliveryHub` to carry
//! every topic ([`BranchEvent`]) on one channel. One `broadcast::Sender` per
//! branch, created lazily — per-branch channels give tenant isolation by
//! construction (a subscriber for branch A physically cannot receive branch B's
//! events). The stream handler additionally filters by topic + permission.
//!
//! Single-instance, in-process. If the backend is scaled horizontally, back
//! `publish`/`subscribe` with Redis pub/sub or Postgres LISTEN/NOTIFY — the
//! handler-side API stays the same.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;
use uuid::Uuid;

use super::event::BranchEvent;

/// Per-branch broadcast capacity. A slow client that falls this far behind is
/// dropped (the stream surfaces `Lagged`, the SSE handler ends the connection,
/// and the client re-seeds + reconnects), so this only bounds memory. Sized a
/// bit larger than the old delivery-only hub since it now multiplexes topics.
const CHANNEL_CAPACITY: usize = 256;

/// Recent events retained per branch for `Last-Event-ID` reconnect replay. A client
/// gone longer than this window falls outside it and re-seeds from the snapshot
/// instead. Bounds memory (one cloned event each).
const REPLAY_BUFFER: usize = 512;

/// Per-branch bus: the live broadcast channel, the next event id, and a bounded log
/// of recent events for reconnect replay — all behind the registry lock so the id is
/// assigned and the event buffered atomically with its broadcast.
struct BranchBus {
    tx: broadcast::Sender<BranchEvent>,
    next_id: u64,
    recent: VecDeque<BranchEvent>,
}

impl BranchBus {
    fn new() -> Self {
        Self {
            tx: broadcast::channel(CHANNEL_CAPACITY).0,
            next_id: 1,
            recent: VecDeque::new(),
        }
    }
}

/// Branch-keyed broadcast registry. Cheap to clone (`Arc` inside) so it lives in
/// `web::Data` and is shared across all actix workers.
#[derive(Clone, Default)]
pub struct BranchEventHub {
    inner: Arc<Mutex<HashMap<Uuid, BranchBus>>>,
}

impl BranchEventHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe to a branch's bus, creating it on first use.
    pub fn subscribe(&self, branch_id: Uuid) -> broadcast::Receiver<BranchEvent> {
        let mut map = self.inner.lock().expect("realtime hub mutex poisoned");
        map.entry(branch_id)
            .or_insert_with(BranchBus::new)
            .tx
            .subscribe()
    }

    /// Publish an event to a branch: stamp the next per-branch id, retain it in the
    /// replay buffer, and broadcast. No-op if the branch has no bus yet (nobody has
    /// ever subscribed) — there is no client, live or reconnecting, to deliver to.
    pub fn publish(&self, branch_id: Uuid, mut event: BranchEvent) {
        let mut map = self.inner.lock().expect("realtime hub mutex poisoned");
        if let Some(bus) = map.get_mut(&branch_id) {
            event.id = bus.next_id;
            bus.next_id += 1;
            bus.recent.push_back(event.clone());
            while bus.recent.len() > REPLAY_BUFFER {
                bus.recent.pop_front();
            }
            let _ = bus.tx.send(event);
        }
    }

    /// Buffered events with id strictly greater than `after_id` (oldest first), for a
    /// reconnecting client's `Last-Event-ID` resume. If `after_id` predates the
    /// retained window some events were evicted — the caller pairs this with the
    /// client's snapshot re-seed (belt and braces) to cover that gap.
    pub fn replay_since(&self, branch_id: Uuid, after_id: u64) -> Vec<BranchEvent> {
        let map = self.inner.lock().expect("realtime hub mutex poisoned");
        match map.get(&branch_id) {
            Some(bus) => bus
                .recent
                .iter()
                .filter(|e| e.id > after_id)
                .cloned()
                .collect(),
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::realtime::event::Topic;

    fn ev(name: &str) -> BranchEvent {
        BranchEvent::new(Topic::Kitchen, name, &serde_json::json!({}))
    }

    #[test]
    fn publish_assigns_monotonic_ids_and_replays_after_a_cursor() {
        let hub = BranchEventHub::new();
        let branch = Uuid::new_v4();
        // No bus yet (nobody subscribed) → publish is a no-op, nothing to replay.
        hub.publish(branch, ev("dropped"));
        assert!(
            hub.replay_since(branch, 0).is_empty(),
            "no subscriber → nothing buffered"
        );

        let _rx = hub.subscribe(branch); // creates the bus
        hub.publish(branch, ev("one"));
        hub.publish(branch, ev("two"));
        hub.publish(branch, ev("three"));

        let all = hub.replay_since(branch, 0);
        assert_eq!(
            all.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "monotonic ids from 1"
        );
        assert_eq!(
            all.iter()
                .map(|e| e.event_type.as_str())
                .collect::<Vec<_>>(),
            vec!["one", "two", "three"],
        );
        // A reconnecting client replays only what's AFTER its last-seen id.
        assert_eq!(
            hub.replay_since(branch, 2)
                .iter()
                .map(|e| e.id)
                .collect::<Vec<_>>(),
            vec![3]
        );
        assert!(
            hub.replay_since(branch, 3).is_empty(),
            "caught up → nothing to replay"
        );
    }

    #[test]
    fn replay_buffer_evicts_the_oldest_past_capacity() {
        let hub = BranchEventHub::new();
        let branch = Uuid::new_v4();
        let _rx = hub.subscribe(branch);
        for _ in 0..(REPLAY_BUFFER + 50) {
            hub.publish(branch, ev("x"));
        }
        let buffered = hub.replay_since(branch, 0);
        assert_eq!(buffered.len(), REPLAY_BUFFER, "buffer is bounded");
        // The oldest 50 were evicted; ids stay monotonic, so the window starts at 51.
        assert_eq!(buffered.first().unwrap().id, 51, "oldest evicted");
        assert_eq!(
            buffered.last().unwrap().id,
            (REPLAY_BUFFER + 50) as u64,
            "newest retained"
        );
    }
}
