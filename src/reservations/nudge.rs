//! The reservations nudge scheduler — a single background task spawned once from
//! `main` (NOT per worker). Every tick it:
//!   1. Departure (flat): WhatsApp `lead_minutes` before each reservation.
//!   2. No-show warn: "running late?" at `reserved_for + grace_minutes` — the
//!      table stays held; the host releases it (auto-warn policy).
//!   3. Holds: pre-assigned tables flip free→held inside the hold window.
//!   4. Waitlist head-out (OSRM): for waitlist entries with a quoted ready time
//!      and a saved location, nudge when their drive time ≈ the wait left.
//!
//! All sends are idempotent via `booking_nudges (booking_id, kind)`. WhatsApp and
//! OSRM are degrade-safe (unset/unreachable ⇒ that step is a no-op this tick).

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::delivery::whatsapp;
use crate::realtime::event::{BranchEvent, Topic};
use crate::realtime::hub::BranchEventHub;

/// Spawn the scheduler. No-op when `RESERVATION_NUDGES_ENABLED` is falsy.
pub fn spawn(pool: PgPool, hub: BranchEventHub) {
    let disabled = std::env::var("RESERVATION_NUDGES_ENABLED")
        .map(|v| matches!(v.as_str(), "0" | "false" | "no" | "off"))
        .unwrap_or(false);
    if disabled {
        tracing::info!("Reservation nudge scheduler disabled (RESERVATION_NUDGES_ENABLED)");
        return;
    }
    let secs = std::env::var("RESERVATION_NUDGE_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(60)
        .max(10);

    tracing::info!("Reservation nudge scheduler started ({secs}s tick)");
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(secs));
        loop {
            ticker.tick().await;
            if let Err(e) = run_tick(&pool, &hub).await {
                tracing::warn!(error = %e, "reservation nudge tick failed");
            }
        }
    });
}

async fn run_tick(pool: &PgPool, hub: &BranchEventHub) -> Result<(), sqlx::Error> {
    departure(pool, hub).await?;
    no_show_warn(pool, hub).await?;
    apply_holds(pool, hub).await?;
    waitlist_headout(pool).await?;
    Ok(())
}

async fn publish_update(pool: &PgPool, hub: &BranchEventHub, booking_id: Uuid, branch_id: Uuid) {
    if let Ok(view) = super::bookings::fetch_view(pool, booking_id).await {
        hub.publish(
            branch_id,
            BranchEvent::new(Topic::Reservations, "booking.updated", &view),
        );
    }
}

/// 1. Flat departure nudge.
async fn departure(pool: &PgPool, hub: &BranchEventHub) -> Result<(), sqlx::Error> {
    let rows: Vec<(Uuid, Uuid, String, String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT b.id, b.branch_id, b.customer_name, b.customer_phone, b.reserved_for \
         FROM bookings b \
         LEFT JOIN branch_reservation_settings s ON s.branch_id = b.branch_id \
         WHERE b.status = 'confirmed' AND b.reserved_for IS NOT NULL \
           AND now() >= b.reserved_for - make_interval(mins => COALESCE(s.lead_minutes, 30)) \
           AND now() <  b.reserved_for \
           AND NOT EXISTS (SELECT 1 FROM booking_nudges n WHERE n.booking_id = b.id AND n.kind = 'departure') \
         LIMIT 200",
    )
    .fetch_all(pool)
    .await?;

    for (id, branch_id, name, phone, reserved_for) in rows {
        let when = reserved_for.format("%H:%M UTC").to_string();
        whatsapp::send_message(
            pool.clone(),
            phone,
            whatsapp::build_reservation_departure_message(&name, &when),
        );
        let logged = sqlx::query(
            "INSERT INTO booking_nudges (booking_id, kind) VALUES ($1, 'departure') \
             ON CONFLICT (booking_id, kind) DO NOTHING",
        )
        .bind(id)
        .execute(pool)
        .await?;
        if logged.rows_affected() == 0 {
            continue; // another worker/tick beat us to it
        }
        sqlx::query(
            "UPDATE bookings SET status = 'notified', notified_at = now(), updated_at = now() \
             WHERE id = $1 AND status = 'confirmed'",
        )
        .bind(id)
        .execute(pool)
        .await?;
        publish_update(pool, hub, id, branch_id).await;
    }
    Ok(())
}

/// 2. No-show warn — auto-warn, host releases (status unchanged).
async fn no_show_warn(pool: &PgPool, hub: &BranchEventHub) -> Result<(), sqlx::Error> {
    let rows: Vec<(Uuid, Uuid, String, String)> = sqlx::query_as(
        "SELECT b.id, b.branch_id, b.customer_name, b.customer_phone \
         FROM bookings b \
         LEFT JOIN branch_reservation_settings s ON s.branch_id = b.branch_id \
         WHERE b.status IN ('confirmed','notified') AND b.reserved_for IS NOT NULL \
           AND now() >= b.reserved_for + make_interval(mins => COALESCE(s.grace_minutes, 15)) \
           AND NOT EXISTS (SELECT 1 FROM booking_nudges n WHERE n.booking_id = b.id AND n.kind = 'no_show_warn') \
         LIMIT 200",
    )
    .fetch_all(pool)
    .await?;

    for (id, branch_id, name, phone) in rows {
        let logged = sqlx::query(
            "INSERT INTO booking_nudges (booking_id, kind) VALUES ($1, 'no_show_warn') \
             ON CONFLICT (booking_id, kind) DO NOTHING",
        )
        .bind(id)
        .execute(pool)
        .await?;
        if logged.rows_affected() == 0 {
            continue;
        }
        whatsapp::send_message(
            pool.clone(),
            phone,
            whatsapp::build_reservation_running_late_message(&name),
        );
        // Surface on the host board; the host decides whether to release/no_show.
        publish_update(pool, hub, id, branch_id).await;
    }
    Ok(())
}

/// 3. Flip pre-assigned tables free→held inside the hold window.
async fn apply_holds(pool: &PgPool, hub: &BranchEventHub) -> Result<(), sqlx::Error> {
    let touched: Vec<(Uuid,)> = sqlx::query_as(
        "UPDATE branch_tables t SET status = 'held', updated_at = now() \
         FROM bookings b \
         JOIN booking_tables bt ON bt.booking_id = b.id \
         LEFT JOIN branch_reservation_settings s ON s.branch_id = b.branch_id \
         WHERE bt.table_id = t.id \
           AND b.status = 'confirmed' AND b.reserved_for IS NOT NULL \
           AND now() >= b.reserved_for - make_interval(mins => COALESCE(s.hold_lead_minutes, 120)) \
           AND now() <  b.reserved_for \
           AND t.status = 'free' \
         RETURNING t.branch_id",
    )
    .fetch_all(pool)
    .await?;

    // One coalesced floor refresh per affected branch.
    let branches: HashSet<Uuid> = touched.into_iter().map(|(b,)| b).collect();
    for branch_id in branches {
        hub.publish(
            branch_id,
            BranchEvent::new(
                Topic::Reservations,
                "floor.updated",
                &serde_json::json!({ "reason": "hold" }),
            ),
        );
    }
    Ok(())
}

/// 4. OSRM-driven waitlist head-out: nudge when drive time ≈ wait remaining.
async fn waitlist_headout(pool: &PgPool) -> Result<(), sqlx::Error> {
    let rows: Vec<(Uuid, Uuid, String, String, f64, f64, DateTime<Utc>)> = sqlx::query_as(
        "SELECT b.id, b.branch_id, b.customer_name, b.customer_phone, \
                b.customer_lat, b.customer_lng, b.quoted_ready_at \
         FROM bookings b \
         WHERE b.status IN ('confirmed','notified') AND b.reserved_for IS NULL \
           AND b.quoted_ready_at IS NOT NULL \
           AND b.customer_lat IS NOT NULL AND b.customer_lng IS NOT NULL \
           AND now() <  b.quoted_ready_at \
           AND b.quoted_ready_at <= now() + interval '90 minutes' \
           AND NOT EXISTS (SELECT 1 FROM booking_nudges n WHERE n.booking_id = b.id AND n.kind = 'waitlist_headout') \
         LIMIT 50",
    )
    .fetch_all(pool)
    .await?;

    for (id, branch_id, name, phone, lat, lng, ready_at) in rows {
        let Some(eta_min) = super::bookings::branch_eta_minutes(pool, branch_id, lat, lng).await
        else {
            continue; // OSRM unavailable — try again next tick
        };
        let ready_in_min = (ready_at - Utc::now()).num_minutes();
        // Time to leave: the drive is now at least as long as the wait remaining.
        if eta_min < ready_in_min {
            continue;
        }
        let logged = sqlx::query(
            "INSERT INTO booking_nudges (booking_id, kind, eta_seconds) VALUES ($1, 'waitlist_headout', $2) \
             ON CONFLICT (booking_id, kind) DO NOTHING",
        )
        .bind(id)
        .bind((eta_min * 60) as i32)
        .execute(pool)
        .await?;
        if logged.rows_affected() == 0 {
            continue;
        }
        whatsapp::send_message(
            pool.clone(),
            phone,
            whatsapp::build_waitlist_headout_message(&name, eta_min),
        );
    }
    Ok(())
}
