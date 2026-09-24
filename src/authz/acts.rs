//! The VOID and REFUND limit decisions — one place, asked by both the live
//! route and offline replay (PERMISSIONS phase 5).
//!
//! The templates lock two rules (`authz/spec/capabilities.toml`, applied to new
//! orgs by `orgs::provision`):
//!   * teller `orders.void`    = `{ own, max_age_minutes 10 }` — your OWN sale,
//!     rung in the last ten minutes;
//!   * teller `refunds.create` = `{ max_amount 0 }` — with `approval = true`,
//!     so every refund goes to a manager. Waiters hold the capability at all,
//!     so they are refused before any limit is read.
//!
//! Until this module existed the limits were written down and enforced NOWHERE:
//! both the live routes (`orders::handlers::void_order`,
//! `refunds::handlers::create_refund`) and `sync::handlers::replay` asked only
//! the legacy `(resource, action)` cell, which knows nothing about whose sale
//! it is or how old. An online teller could void anybody's sale, at any age.
//!
//! Existing orgs carry no limits (deliberately — see PERMISSIONS_RESUME,
//! "The void and refund DEFAULTS are deliberately not applied yet"), so an
//! unrestricted grant decides `Allow` here exactly as before and no till in a
//! shop running today is affected.

use uuid::Uuid;

use crate::authz::{Cap, Request};
use crate::errors::AppError;

/// The facts a void limit is judged on: whose sale, and how old at the moment
/// it was voided (`at` — `now()` live, the queued op's own timestamp on
/// replay, so a queue drained hours later is not judged by the drain time).
/// madar-shared's (`madar_authz::acts`), the till's too (A2).
pub use madar_authz::acts::{VoidAsk, void_facts};

/// Read the sale being voided: is it the actor's own, and how old is it.
/// A sale the actor did not ring is `own = false`; a clock that puts the sale
/// in the future is read as age 0 rather than a negative age.
///
/// `None` when there is no such order: whether it is missing is not a
/// permission question, and the handler that owns the 404 answers it. Asking
/// here first would turn a would-be 403 into a 404 and leak that the order
/// does not exist.
pub async fn void_ask(
    pool: &sqlx::PgPool,
    order_id: Uuid,
    actor: Uuid,
    at: chrono::DateTime<chrono::Utc>,
) -> Result<Option<VoidAsk>, AppError> {
    let row: Option<(Uuid, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as("SELECT teller_id, created_at FROM orders WHERE id = $1")
            .bind(order_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(teller_id, created_at)| void_facts(&teller_id, &actor, created_at, at)))
}

/// A refund is judged on the money going back (`max_amount`, minor units).
pub fn refund_request(amount_minor: i64) -> Request {
    let mut r = Request::of(Cap::RefundsCreate);
    r.amount = Some(amount_minor);
    r
}
