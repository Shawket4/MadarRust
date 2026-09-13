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
    /// The live floor: table geometry, table status, and the transfer waitlist.
    /// Payloads are LEAN invalidation signals — devices re-pull via the sync
    /// endpoints, which stay the single source of truth either way.
    ///
    /// Everything floor-shaped rides THIS topic. `table.status_changed` used to
    /// be split across `floor` and `reservations` depending on which module
    /// wrote it, so a client subscribed to one silently missed half the events.
    Floor,
    /// Table bookings: `booking.created|changed|arriving`. Cloud-originated
    /// (guests book online, hosts on the dashboard) — the POS sees held tables
    /// and gets its "party arriving" ping through this.
    Bookings,
    /// Till lifecycle + payment-method availability: `till.*`, `payment_methods.*`.
    Tills,
    /// `sync.changed { branch_id }`: the branch's changefeed moved; devices
    /// `POST /sync/pull`. Gated only by branch access.
    Sync,
    /// `payment_methods.availability_changed`: which methods a branch / person /
    /// device may take moved. Its own topic (it used to ride `orders`, so a
    /// device that may charge but not read orders never heard it).
    PaymentMethods,
}

impl Topic {
    /// Every topic, for the "subscribe to all I'm allowed to read" default.
    pub const ALL: [Topic; 9] = [
        Topic::Delivery,
        Topic::Tickets,
        Topic::Kitchen,
        Topic::Orders,
        Topic::Floor,
        Topic::Bookings,
        Topic::Tills,
        Topic::Sync,
        Topic::PaymentMethods,
    ];

    pub fn parse(s: &str) -> Option<Topic> {
        match s.trim() {
            "delivery" => Some(Topic::Delivery),
            "tickets" => Some(Topic::Tickets),
            "kitchen" => Some(Topic::Kitchen),
            "orders" => Some(Topic::Orders),
            "floor" => Some(Topic::Floor),
            "bookings" => Some(Topic::Bookings),
            "tills" => Some(Topic::Tills),
            "sync" => Some(Topic::Sync),
            "payment_methods" => Some(Topic::PaymentMethods),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Topic::Delivery => "delivery",
            Topic::Tickets => "tickets",
            Topic::Kitchen => "kitchen",
            Topic::Orders => "orders",
            Topic::Floor => "floor",
            Topic::Bookings => "bookings",
            Topic::Tills => "tills",
            Topic::Sync => "sync",
            Topic::PaymentMethods => "payment_methods",
        }
    }

    /// The `(resource, action)` the caller must hold to receive this topic;
    /// `None` = branch access alone (the stream already enforces it).
    pub fn permission(self) -> Option<(&'static str, &'static str)> {
        Some(match self {
            Topic::Delivery => ("delivery_orders", "read"),
            Topic::Tickets => ("open_tickets", "read"),
            Topic::Kitchen => ("kitchen_orders", "read"),
            Topic::Orders => ("orders", "read"),
            Topic::Floor => ("floor_plan", "read"),
            Topic::Bookings => ("bookings", "read"),
            Topic::Tills => ("tills", "read"),
            Topic::PaymentMethods => ("payment_methods", "read"),
            Topic::Sync => return None,
        })
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
