//! The unified per-branch realtime event-bus vocabulary.
//!
//! One bus carries every realtime concern (delivery, kitchen, waiter tickets,
//! order status) for a branch, multiplexed by **topic**. A client opens a single
//! SSE connection and receives only the topics it both asked for AND holds
//! `:read` permission on — so a waiter device never sees till/cash order events.
//!
//! The payload is pre-serialized JSON: the hub and stream stay decoupled from
//! each feature's view types (the publisher owns the shape), which keeps this
//! module free of circular dependencies on `kitchen`/`tickets`/`delivery`.

use serde::Serialize;

/// A realtime topic. Each maps to a permission resource; the stream forwards an
/// event only when the caller subscribed to its topic and may read that resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Topic {
    Delivery,
    Tickets,
    Kitchen,
    Orders,
    Reservations,
}

impl Topic {
    /// Every topic, for the "subscribe to all I'm allowed to read" default.
    pub const ALL: [Topic; 5] = [
        Topic::Delivery,
        Topic::Tickets,
        Topic::Kitchen,
        Topic::Orders,
        Topic::Reservations,
    ];

    pub fn parse(s: &str) -> Option<Topic> {
        match s.trim() {
            "delivery" => Some(Topic::Delivery),
            "tickets" => Some(Topic::Tickets),
            "kitchen" => Some(Topic::Kitchen),
            "orders" => Some(Topic::Orders),
            "reservations" => Some(Topic::Reservations),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Topic::Delivery => "delivery",
            Topic::Tickets => "tickets",
            Topic::Kitchen => "kitchen",
            Topic::Orders => "orders",
            Topic::Reservations => "reservations",
        }
    }

    /// The `(resource, action)` the caller must hold to receive this topic.
    pub fn permission(self) -> (&'static str, &'static str) {
        match self {
            Topic::Delivery => ("delivery_orders", "read"),
            Topic::Tickets => ("open_tickets", "read"),
            Topic::Kitchen => ("kitchen_orders", "read"),
            Topic::Orders => ("orders", "read"),
            Topic::Reservations => ("reservations", "read"),
        }
    }
}

/// One event on the per-branch bus. On the wire it becomes
/// `event: <event_type>\ndata: <data>\n\n`.
#[derive(Clone)]
pub struct BranchEvent {
    pub topic: Topic,
    /// Dotted event name, e.g. `"ticket.fired"`, `"kitchen.item_bumped"`,
    /// `"delivery.updated"`. The client switches on this.
    pub event_type: String,
    pub data: serde_json::Value,
    /// Monotonic per-branch sequence, assigned by the hub at publish time (0 until
    /// then). Emitted as the SSE `id:` field so a reconnecting client can request
    /// replay via `Last-Event-ID`.
    pub id: u64,
}

impl BranchEvent {
    /// Build an event, serializing `payload` into `data`. Serialization failure
    /// degrades to `null` (the client re-seeds from the snapshot), never panics.
    pub fn new(topic: Topic, event_type: impl Into<String>, payload: &impl Serialize) -> Self {
        let data = serde_json::to_value(payload).unwrap_or(serde_json::Value::Null);
        Self {
            topic,
            event_type: event_type.into(),
            data,
            id: 0, // assigned by the hub at publish
        }
    }
}
