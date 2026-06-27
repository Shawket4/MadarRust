//! The unified realtime bus: one per-branch SSE connection multiplexing every
//! topic (delivery, kitchen, waiter tickets, orders), filtered by topic +
//! permission. Domain modules publish [`event::BranchEvent`]s to the
//! [`hub::BranchEventHub`]; this module owns the transport, not any domain.

pub mod event;
pub mod hub;
pub mod routes;
pub mod stream;
