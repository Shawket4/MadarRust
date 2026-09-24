//! The unified realtime bus: one per-branch SSE connection multiplexing every
//! topic (delivery, kitchen, waiter tickets, orders), filtered by topic +
//! permission. Domain modules publish [`event::BranchEvent`]s to the
//! [`hub::BranchEventHub`]; this module owns the transport, not any domain.

pub mod event;
pub mod hub;
pub mod routes;
pub mod stream;

/// The server's `h1_allow_half_closed` (see `main.rs`). Off, so a client that
/// closes its socket ends its `/realtime/stream` response at once, and the
/// push fallback's presence (`hub::Connection`) is released at once, instead
/// of lingering until a keep-alive ping fails to write.
pub const H1_ALLOW_HALF_CLOSED: bool = false;
