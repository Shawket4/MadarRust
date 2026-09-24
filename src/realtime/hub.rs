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

/// The answer to a `Last-Event-ID` resume: what we still hold, and whether that
/// is everything the client missed.
pub struct Replay {
    pub events: Vec<BranchEvent>,
    pub complete: bool,
    /// The highest id this process has issued for the branch (0 if none). A client
    /// whose replay is incomplete resets its cursor to this: its own cursor may
    /// belong to a previous process lifetime, and trusting it would filter out
    /// every live event until the branch published past it.
    pub server_last_id: u64,
}

/// Live `/realtime/stream` connections that carry the `delivery` topic, per
/// (branch, install). A count, not a flag: one install can hold two streams
/// for a moment (a reconnect racing the old socket's teardown), and it is
/// connected until the LAST of them goes.
type Presence = HashMap<(Uuid, Uuid), usize>;

/// Branch-keyed broadcast registry. Cheap to clone (`Arc` inside) so it lives in
/// `web::Data` and is shared across all actix workers.
#[derive(Clone, Default)]
pub struct BranchEventHub {
    inner: Arc<Mutex<HashMap<Uuid, BranchBus>>>,
    presence: Arc<Mutex<Presence>>,
}

/// One install's live stream at one branch, held by the SSE body for as long
/// as the connection lives. Dropping it — actix drops the body when the
/// connection closes, errors or lags out — deregisters the connection.
pub struct Connection {
    presence: Arc<Mutex<Presence>>,
    key: (Uuid, Uuid),
}

impl Drop for Connection {
    fn drop(&mut self) {
        let mut map = self.presence.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = map.get_mut(&self.key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                map.remove(&self.key);
            }
        }
    }
}

impl BranchEventHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `device` has a live stream at `branch` which carries new
    /// online orders (the `delivery` topic). The push fallback
    /// (`push::pos`) skips such a device: the open app already alerts from the
    /// `delivery.created` event itself.
    pub fn connect(&self, branch_id: Uuid, device_id: Uuid) -> Connection {
        let key = (branch_id, device_id);
        *self
            .presence
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key)
            .or_insert(0) += 1;
        Connection {
            presence: self.presence.clone(),
            key,
        }
    }

    /// How many live delivery-carrying streams `device` holds at `branch`.
    pub fn connections(&self, branch_id: Uuid, device_id: Uuid) -> usize {
        self.presence
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(branch_id, device_id))
            .copied()
            .unwrap_or(0)
    }

    /// Does `device` hold a live delivery-carrying stream at `branch`?
    pub fn is_connected(&self, branch_id: Uuid, device_id: Uuid) -> bool {
        self.connections(branch_id, device_id) > 0
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
    /// reconnecting client's `Last-Event-ID` resume, plus whether that replay is
    /// COMPLETE. It is not when the cursor predates the retained window (events
    /// were evicted), when the cursor is ahead of anything this process issued
    /// (the server restarted and ids began again), or when this branch has no
    /// bus yet. The stream turns an incomplete replay into a `resync` frame so
    /// the client re-seeds instead of silently missing events.
    pub fn replay_since(&self, branch_id: Uuid, after_id: u64) -> Replay {
        let map = self.inner.lock().expect("realtime hub mutex poisoned");
        match map.get(&branch_id) {
            Some(bus) => {
                let events: Vec<BranchEvent> = bus
                    .recent
                    .iter()
                    .filter(|e| e.id > after_id)
                    .cloned()
                    .collect();
                let oldest = bus.recent.front().map(|e| e.id);
                let issued_any = bus.next_id > 1;
                let complete = if after_id >= bus.next_id {
                    false // cursor from a previous process lifetime
                } else if !issued_any {
                    after_id == 0
                } else {
                    // Nothing evicted since the cursor: the cursor is at or after
                    // the event just before the oldest retained one.
                    oldest.is_some_and(|o| after_id + 1 >= o)
                };
                Replay {
                    events,
                    complete,
                    server_last_id: bus.next_id.saturating_sub(1),
                }
            }
            None => Replay {
                events: Vec::new(),
                complete: after_id == 0,
                server_last_id: 0,
            },
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
            hub.replay_since(branch, 0).events.is_empty(),
            "no subscriber → nothing buffered"
        );

        let _rx = hub.subscribe(branch); // creates the bus
        hub.publish(branch, ev("one"));
        hub.publish(branch, ev("two"));
        hub.publish(branch, ev("three"));

        let all = hub.replay_since(branch, 0);
        assert!(all.complete, "cursor 0 with a full buffer is complete");
        let all = all.events;
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
                .events
                .iter()
                .map(|e| e.id)
                .collect::<Vec<_>>(),
            vec![3]
        );
        let caught_up = hub.replay_since(branch, 3);
        assert!(
            caught_up.events.is_empty() && caught_up.complete,
            "caught up → nothing to replay"
        );
        // A cursor AHEAD of anything issued (previous process lifetime) is a gap.
        assert!(
            !hub.replay_since(branch, 99).complete,
            "stale cursor → resync"
        );
    }

    #[test]
    fn a_device_is_connected_until_its_last_stream_closes() {
        let hub = BranchEventHub::new();
        let (branch, other_branch, device) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        assert!(!hub.is_connected(branch, device));
        let first = hub.connect(branch, device);
        let second = hub.connect(branch, device); // a reconnect racing the old socket
        assert_eq!(hub.connections(branch, device), 2);
        assert!(
            !hub.is_connected(other_branch, device),
            "a stream at one branch says nothing about another"
        );
        drop(first);
        assert!(hub.is_connected(branch, device), "one stream is still up");
        drop(second);
        assert!(!hub.is_connected(branch, device));
        // Clones share the registry (the hub lives in web::Data).
        let _c = hub.clone().connect(branch, device);
        assert!(hub.is_connected(branch, device));
    }

    #[test]
    fn replay_buffer_evicts_the_oldest_past_capacity() {
        let hub = BranchEventHub::new();
        let branch = Uuid::new_v4();
        let _rx = hub.subscribe(branch);
        for _ in 0..(REPLAY_BUFFER + 50) {
            hub.publish(branch, ev("x"));
        }
        let replay = hub.replay_since(branch, 0);
        assert!(
            !replay.complete,
            "the oldest 50 were evicted → the client must resync"
        );
        assert!(
            hub.replay_since(branch, 50).complete,
            "cursor at the eviction edge is complete"
        );
        assert!(
            !hub.replay_since(branch, 49).complete,
            "one before the edge is not"
        );
        let buffered = replay.events;
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
