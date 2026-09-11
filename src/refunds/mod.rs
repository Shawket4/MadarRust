//! Refunds — money returned to a customer against a settled order.
//!
//! A refund is not a void. A void corrects a mistake on a bill (the sale never
//! happened); a refund returns money already taken (the sale stands, and some
//! or all of the money went back). Migration `20260912090000` is the
//! specification: `order_refunds` is append-only, the cumulative amount per
//! order is bounded by `orders.total_amount` under the order's row lock, the
//! status flip to `refunded` at the ceiling and the proportional loyalty
//! clawback are BOTH done by triggers on that table. This module therefore
//! writes the row and nothing else — it must not flip the status or reverse
//! loyalty itself, or the second writer fails the refund.
//!
//! What lives here: the request/response shapes, the handler that issues a
//! refund into the actor's open shift (idempotent on `client_ref`, like a
//! cash movement), the reads a receipt reprint and the dashboard need, and
//! the two figures other modules fold in — [`handlers::fetch_order_refunds`]
//! for an order's detail view and [`handlers::shift_cash_refunds`] for the
//! drawer maths (`compute_system_cash` subtracts it).

pub mod handlers;
pub mod routes;

/// Why money went back. The first three share their spelling with
/// [`crate::orders::VoidReason`] so a report reads "wrong order" across voids
/// and refunds alike; the rest exist only for refunds (you do not void a sale
/// for being late). Bound as text; the table's CHECK is the authority on the
/// vocabulary and this enum mirrors it.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RefundReason {
    CustomerRequest,
    WrongOrder,
    QualityIssue,
    /// The bill was wrong; the difference goes back.
    Overcharged,
    /// Delivery or pickup that did not arrive, or not in time.
    LateOrUndelivered,
    /// Nothing was wrong on paper; the shop chose to.
    Goodwill,
    /// The catch-all. Requires a note — an `other` with nothing behind it
    /// tells the report nothing.
    Other,
}

impl RefundReason {
    pub fn as_str(self) -> &'static str {
        match self {
            RefundReason::CustomerRequest => "customer_request",
            RefundReason::WrongOrder => "wrong_order",
            RefundReason::QualityIssue => "quality_issue",
            RefundReason::Overcharged => "overcharged",
            RefundReason::LateOrUndelivered => "late_or_undelivered",
            RefundReason::Goodwill => "goodwill",
            RefundReason::Other => "other",
        }
    }
}

#[cfg(test)]
mod tests;
