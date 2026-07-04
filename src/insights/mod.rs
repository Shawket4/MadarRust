//! Menu profitability insights — the replacement for the retired menu advisor
//! and menu-engineering report.
//!
//! Three layers, each honest and each linked to the fix:
//!   - LEDGER  (what happened): a ranked margin ledger per SKU (item × size),
//!     computed live from order history + cost snapshots. Cost-unknown rows are
//!     RETURNED and flagged — never counted as 0, never silently dropped.
//!   - SIGNALS (what needs attention): transparent arithmetic flags computed in
//!     the same request (below cost / below target / cost spike / price
//!     candidate / removal candidate / recipe incomplete), each carrying its
//!     evidence so the client can render a plain-language reason and deep-link
//!     to the fix (Studio / pricing matrix).
//!   - DECISIONS (what you did): an append-only log of operator responses.
//!     A dismissal suppresses that signal for the SKU for a cooldown unless the
//!     evidence materially worsens; impact is measured automatically from order
//!     history (baseline window frozen at decision time vs the window after).

pub mod handlers;
pub mod routes;

#[cfg(test)]
mod tests;
