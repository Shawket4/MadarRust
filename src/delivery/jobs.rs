//! The delivery sweep: giving up on an order nobody accepted.
//!
//! A `received` order is a quote waiting for a teller to press Accept, and
//! until now it could wait for ever. A branch that missed the ping — a till
//! asleep, a shift that closed without looking at the queue — left the
//! customer staring at "received" on the tracking page with no message and
//! no way to know the food was never coming. `branch_delivery_settings.
//! auto_reject_minutes` is the branch's patience, NULL meaning infinite; this
//! tick rejects what has outwaited it and tells the customer, the way a
//! teller's Reject would.
//!
//! The flip is the same guarded UPDATE the cancel endpoint uses: only a row
//! still `received` is touched, so a teller who accepts in the same second
//! wins and the sweep finds nothing. Nothing was made, so nothing is wasted
//! (`cancel_restocked = true`), and nothing was paid — money only changes
//! hands at the door — so the message can say so. `cancelled_by` stays NULL:
//! nobody did this, the clock did.
//!
//! Wired from `main` beside the bookings sweep; `DELIVERY_SWEEP_ENABLED=0`
//! turns it off and `DELIVERY_SWEEP_INTERVAL_SECS` sets the tick.

use std::time::Duration;

use sqlx::PgPool;
use uuid::Uuid;

use super::staff::fetch_delivery_order;
use super::whatsapp;
use crate::errors::AppError;
use crate::realtime::event::{BranchEvent, Topic};
use crate::realtime::hub::BranchEventHub;

/// What the customer and the till both read on a swept order. Customer-facing
/// (the public tracking view shows `cancel_reason`), so it says what happened
/// to them, not what a setting is called.
pub const AUTO_REJECT_REASON: &str = "The shop could not take this order in time.";

/// One order the sweep gave up on — enough to tell the customer and the till.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub id: Uuid,
    pub branch_id: Uuid,
    pub delivery_ref: Option<String>,
    pub customer_phone: String,
}

/// Reject every `received` order that has waited longer than its branch
/// allows. One statement, so a branch that shortens its window applies the
/// change to what is already waiting, and so two overlapping ticks cannot
/// both claim a row. Returns what it rejected; the caller does the talking.
pub async fn reject_unaccepted(pool: &PgPool) -> Result<Vec<Rejected>, AppError> {
    let rows: Vec<(Uuid, Uuid, Option<String>, String)> = sqlx::query_as(
        "UPDATE delivery_orders d
            SET status = 'rejected', rejected_at = now(),
                cancel_reason = $1, cancel_restocked = true, updated_at = now()
           FROM branch_delivery_settings s
          WHERE s.branch_id = d.branch_id
            AND s.auto_reject_minutes IS NOT NULL
            AND d.status = 'received'
            AND d.created_at < now() - make_interval(mins => s.auto_reject_minutes)
          RETURNING d.id, d.branch_id, d.delivery_ref, d.customer_phone",
    )
    .bind(AUTO_REJECT_REASON)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, branch_id, delivery_ref, customer_phone)| Rejected {
            id,
            branch_id,
            delivery_ref,
            customer_phone,
        })
        .collect())
}

/// One tick: reject, then tell the customer and drop the order from the
/// till's live queue. Both after the write, never before it — a message about
/// a rejection that did not happen is worse than a late one.
pub async fn run_tick(pool: &PgPool, hub: &BranchEventHub) -> Result<(), AppError> {
    for r in reject_unaccepted(pool).await? {
        tracing::info!(
            delivery_order = %r.id,
            branch_id = %r.branch_id,
            "delivery sweep: rejected an order nobody accepted"
        );
        if let Some(ref dref) = r.delivery_ref {
            whatsapp::send_message(
                pool.clone(),
                r.customer_phone.clone(),
                whatsapp::build_order_rejected_message(dref, r.id),
            );
        }
        if let Some(updated) = fetch_delivery_order(pool, r.id).await? {
            hub.publish(
                r.branch_id,
                BranchEvent::new(Topic::Delivery, "delivery.updated", &updated),
            );
        }
    }
    Ok(())
}

/// Start the sweep (once, from `main`). Same knobs and floor as the bookings
/// sweep, so an operator who has tuned one knows the other.
pub fn spawn(pool: PgPool, hub: BranchEventHub) {
    let disabled = std::env::var("DELIVERY_SWEEP_ENABLED")
        .map(|v| matches!(v.as_str(), "0" | "false" | "no" | "off"))
        .unwrap_or(false);
    if disabled {
        tracing::info!("Delivery sweep disabled (DELIVERY_SWEEP_ENABLED)");
        return;
    }
    let secs = std::env::var("DELIVERY_SWEEP_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(60)
        .max(15);
    tracing::info!("Delivery sweep started ({secs}s tick)");
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(secs));
        loop {
            ticker.tick().await;
            crate::observability::report::guarded_tick("delivery_sweep", || run_tick(&pool, &hub))
                .await;
        }
    });
}
