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
//! **Only recent orders.** An order is eligible while it is past its
//! branch's window by no more than [`GRACE_HOURS`]. Anything older is stale:
//! the customer stopped waiting long ago, and a "sorry" for an order placed
//! weeks back is noise, not service. Without this bound, the first tick after
//! the sweep was switched on (or after a branch first set a window) would
//! reject every forgotten `received` row at once and message every one of
//! those customers. Stale rows are left exactly as they are and only counted
//! in the log ([`count_stale`]); a person clears them from the POS.
//!
//! **Once, across instances.** The claim is one statement whose rows are
//! taken `FOR UPDATE SKIP LOCKED` and re-checked `status = 'received'` by the
//! UPDATE itself, so two ticks (two processes, or a tick racing a teller's
//! Accept/Reject) can never both win the same row. Only the winner's RETURNING
//! carries the row, and only the winner talks to the customer: one rejection,
//! one message. A row already rejected matches nothing, so a re-run is a no-op.
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

/// How far past its branch's `auto_reject_minutes` an unaccepted order is
/// still the sweep's to reject. A day covers any realistic gap in the sweep
/// itself (a deploy, a restart, the switch briefly off) and the slowest
/// branch window anyone sets, while keeping weeks-old forgotten rows out.
pub const GRACE_HOURS: i32 = 24;

/// Reject every `received` order that has waited longer than its branch
/// allows, but by no more than [`GRACE_HOURS`]. One statement, so a branch
/// that shortens its window applies the change to what is already waiting,
/// and so two overlapping ticks cannot both claim a row (see the module
/// note). Returns what it rejected; the caller does the talking.
pub async fn reject_unaccepted(pool: &PgPool) -> Result<Vec<Rejected>, AppError> {
    let rows = sqlx::query_as!(
        Rejected,
        r#"WITH due AS (
             SELECT d.id
               FROM delivery_orders d
               JOIN branch_delivery_settings s ON s.branch_id = d.branch_id
              WHERE s.auto_reject_minutes IS NOT NULL
                AND NOT EXISTS (SELECT 1 FROM organizations fo
                                 WHERE fo.id = d.org_id AND fo.plan = 'free')
                AND d.status = 'received'
                AND d.created_at <  now() - make_interval(mins => s.auto_reject_minutes)
                AND d.created_at >= now() - make_interval(mins => s.auto_reject_minutes, hours => $2)
              FOR UPDATE OF d SKIP LOCKED)
           UPDATE delivery_orders d
              SET status = 'rejected', rejected_at = now(),
                  cancel_reason = $1, cancel_restocked = true, updated_at = now()
             FROM due
            WHERE d.id = due.id AND d.status = 'received'
        RETURNING d.id, d.branch_id, d.delivery_ref, d.customer_phone"#,
        AUTO_REJECT_REASON,
        GRACE_HOURS,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// `received` orders at branches with auto-reject on that are past the
/// window by more than [`GRACE_HOURS`]: the ones the sweep deliberately
/// leaves alone. Counted for the log only.
pub async fn count_stale(pool: &PgPool) -> Result<i64, AppError> {
    let n = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!"
             FROM delivery_orders d
             JOIN branch_delivery_settings s ON s.branch_id = d.branch_id
            WHERE s.auto_reject_minutes IS NOT NULL
              AND NOT EXISTS (SELECT 1 FROM organizations fo
                               WHERE fo.id = d.org_id AND fo.plan = 'free')
              AND d.status = 'received'
              AND d.created_at < now() - make_interval(mins => s.auto_reject_minutes, hours => $1)"#,
        GRACE_HOURS,
    )
    .fetch_one(pool)
    .await?;
    Ok(n)
}

/// What one tick did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tick {
    pub rejected: Vec<Rejected>,
    /// Stale `received` rows left alone ([`count_stale`]).
    pub stale: i64,
}

/// One tick: reject, then tell the customer and drop the order from the
/// till's live queue. Both after the write, never before it — a message about
/// a rejection that did not happen is worse than a late one. Only rows this
/// tick won are announced, so a row another instance claimed is never
/// messaged twice.
pub async fn sweep(pool: &PgPool, hub: &BranchEventHub) -> Result<Tick, AppError> {
    let rejected = reject_unaccepted(pool).await?;
    for r in &rejected {
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
    let stale = count_stale(pool).await?;
    Ok(Tick { rejected, stale })
}

/// [`sweep`], for callers that only care whether it failed.
pub async fn run_tick(pool: &PgPool, hub: &BranchEventHub) -> Result<(), AppError> {
    sweep(pool, hub).await.map(|_| ())
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
    crate::db::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(secs));
        // The stale count is logged when it changes (and on the first tick),
        // not every minute.
        let mut last_stale: Option<i64> = None;
        loop {
            ticker.tick().await;
            let last = &mut last_stale;
            crate::observability::report::guarded_tick("delivery_sweep", || async {
                let tick = sweep(&pool, &hub).await?;
                if *last != Some(tick.stale) {
                    if tick.stale > 0 {
                        tracing::warn!(
                            stale = tick.stale,
                            grace_hours = GRACE_HOURS,
                            "delivery sweep: leaving unaccepted orders older than their branch's \
                             auto-reject window plus the grace alone (clear them from the POS)"
                        );
                    }
                    *last = Some(tick.stale);
                }
                Ok::<(), AppError>(())
            })
            .await;
        }
    });
}
