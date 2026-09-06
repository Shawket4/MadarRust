//! The bookings sweep — one background task spawned once from `main`, like the
//! attendance sweep. Every tick, per branch settings:
//!
//!   1. **Reminders.** Confirmed bookings inside their reminder lead that were
//!      made before the lead began get one WhatsApp reminder (`reminder_sent_at`
//!      makes it idempotent).
//!   2. **Arriving.** When a confirmed booking enters its hold window, publish
//!      `booking.arriving` once — the POS pings the floor ("party due in 15").
//!   3. **No-shows.** Confirmed past `starts_at + auto_no_show_minutes` (or past
//!      the window's end when the grace is off) roll to `no_show`.
//!   4. **Completion.** Seated bookings whose window ended an hour ago, with no
//!      still-open ticket, roll to `completed`.
//!
//! Runs on the OWNER pool (bypasses RLS) — every query is keyed by branch.

use std::time::Duration;

use sqlx::PgPool;
use uuid::Uuid;

use super::whatsapp::{Kind, notify};
use super::{booking_view, publish_booking};
use crate::realtime::hub::BranchEventHub;

pub fn spawn(pool: PgPool, hub: BranchEventHub) {
    let disabled = std::env::var("BOOKINGS_SWEEP_ENABLED")
        .map(|v| matches!(v.as_str(), "0" | "false" | "no" | "off"))
        .unwrap_or(false);
    if disabled {
        tracing::info!("Bookings sweep disabled (BOOKINGS_SWEEP_ENABLED)");
        return;
    }
    let secs = std::env::var("BOOKINGS_SWEEP_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(60)
        .max(15);
    tracing::info!("Bookings sweep started ({secs}s tick)");
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(secs));
        loop {
            ticker.tick().await;
            crate::observability::report::guarded_tick("bookings_sweep", || run_tick(&pool, &hub))
                .await;
        }
    });
}

pub async fn run_tick(pool: &PgPool, hub: &BranchEventHub) -> Result<(), crate::errors::AppError> {
    send_reminders(pool).await?;
    announce_arrivals(pool, hub).await?;
    roll_no_shows(pool, hub).await?;
    complete_finished(pool, hub).await?;
    Ok(())
}

async fn send_reminders(pool: &PgPool) -> Result<(), crate::errors::AppError> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE bookings b SET reminder_sent_at = now(), updated_at = now() \
         FROM branch_booking_settings s \
         WHERE s.branch_id = b.branch_id AND s.reminder_lead_minutes IS NOT NULL \
           AND b.status = 'confirmed' AND b.reminder_sent_at IS NULL \
           AND b.starts_at > now() \
           AND b.starts_at <= now() + make_interval(mins => s.reminder_lead_minutes) \
           AND b.created_at < b.starts_at - make_interval(mins => s.reminder_lead_minutes) \
         RETURNING b.id",
    )
    .fetch_all(pool)
    .await?;
    for id in ids {
        if let Ok(Some(view)) = booking_view(pool, id).await {
            notify(pool, &view, Kind::Reminder).await;
        }
    }
    Ok(())
}

async fn announce_arrivals(
    pool: &PgPool,
    hub: &BranchEventHub,
) -> Result<(), crate::errors::AppError> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE bookings b SET arriving_notified_at = now(), updated_at = now() \
         FROM (SELECT b2.id, COALESCE(s.hold_minutes, 15) AS hold \
               FROM bookings b2 LEFT JOIN branch_booking_settings s ON s.branch_id = b2.branch_id) h \
         WHERE h.id = b.id AND b.status = 'confirmed' AND b.arriving_notified_at IS NULL \
           AND b.starts_at - make_interval(mins => h.hold) <= now() \
           AND b.starts_at + interval '30 minutes' > now() \
         RETURNING b.id",
    )
    .fetch_all(pool)
    .await?;
    for id in ids {
        publish_booking(pool, hub, "booking.arriving", id).await;
    }
    Ok(())
}

async fn roll_no_shows(pool: &PgPool, hub: &BranchEventHub) -> Result<(), crate::errors::AppError> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE bookings b SET status = 'no_show', no_show_at = now(), updated_at = now() \
         FROM (SELECT b2.id, s.auto_no_show_minutes AS grace \
               FROM bookings b2 LEFT JOIN branch_booking_settings s ON s.branch_id = b2.branch_id) h \
         WHERE h.id = b.id AND b.status = 'confirmed' \
           AND ((h.grace IS NOT NULL AND b.starts_at + make_interval(mins => h.grace) < now()) \
                OR (h.grace IS NULL AND b.ends_at < now())) \
         RETURNING b.id",
    )
    .fetch_all(pool)
    .await?;
    for id in ids {
        publish_booking(pool, hub, "booking.changed", id).await;
    }
    Ok(())
}

async fn complete_finished(
    pool: &PgPool,
    hub: &BranchEventHub,
) -> Result<(), crate::errors::AppError> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE bookings b SET status = 'completed', completed_at = now(), updated_at = now() \
         WHERE b.status = 'seated' AND b.ends_at + interval '60 minutes' < now() \
           AND NOT EXISTS (SELECT 1 FROM open_tickets ot \
                           WHERE ot.id = b.open_ticket_id AND ot.status = 'open') \
         RETURNING b.id",
    )
    .fetch_all(pool)
    .await?;
    for id in ids {
        publish_booking(pool, hub, "booking.changed", id).await;
    }
    Ok(())
}
