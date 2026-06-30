//! Waiter open tickets (fire-now-pay-later). A waiter opens a dine-in ticket and
//! fires items in rounds; a cashier settles it later into a paid `orders` row via
//! the shared delivery snapshot machinery. The bill (priced lines) lives in
//! `open_ticket_items`; the kitchen copy is emitted to the source-agnostic
//! `kitchen_tickets` substrate so the KDS shows waiter + counter orders alike.

pub mod handlers;
pub mod routes;

#[cfg(test)]
mod tests;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgExecutor, Postgres, Transaction};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::errors::AppError;
use crate::kitchen::{EmitKitchen, KitchenLine, emit_kitchen_ticket};
use crate::orders::handlers::OrderItemInput;
use crate::realtime::event::{BranchEvent, Topic};
use crate::realtime::hub::BranchEventHub;

pub(crate) use crate::delivery::require_branch_access;
pub(crate) use crate::orgs::handlers::extract_claims;

// ── Read models ───────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OpenTicketItemView {
    pub id: Uuid,
    pub round_number: i32,
    pub menu_item_id: Option<Uuid>,
    /// The frozen priced SnapshotLine (name, size, addons, totals).
    pub line: serde_json::Value,
    pub line_total: i32,
    pub voided: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OpenTicketView {
    pub id: Uuid,
    pub branch_id: Uuid,
    pub table_id: Option<Uuid>,
    pub ticket_ref: Option<String>,
    pub status: String,
    pub opened_by: Uuid,
    pub opened_by_name: Option<String>,
    pub customer_name: Option<String>,
    pub notes: Option<String>,
    pub guest_count: Option<i32>,
    pub subtotal: i32,
    pub order_id: Option<Uuid>,
    pub opened_at: DateTime<Utc>,
    pub ready_at: Option<DateTime<Utc>>,
    pub settled_at: Option<DateTime<Utc>>,
    pub items: Vec<OpenTicketItemView>,
}

#[allow(clippy::type_complexity)]
pub(crate) async fn open_ticket_view<'e, E>(
    executor: E,
    ticket_id: Uuid,
) -> Result<Option<OpenTicketView>, AppError>
where
    E: PgExecutor<'e> + Copy,
{
    let row: Option<(
        Uuid,
        Uuid,
        Option<Uuid>,
        Option<String>,
        String,
        Uuid,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i32>,
        i32,
        Option<Uuid>,
        DateTime<Utc>,
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
    )> = sqlx::query_as(
        "SELECT ot.id, ot.branch_id, ot.table_id, ot.ticket_ref, ot.status::text, \
                    ot.opened_by, u.name, ot.customer_name, ot.notes, ot.guest_count, \
                    ot.subtotal, ot.order_id, ot.opened_at, ot.ready_at, ot.settled_at \
             FROM open_tickets ot LEFT JOIN users u ON u.id = ot.opened_by WHERE ot.id = $1",
    )
    .bind(ticket_id)
    .fetch_optional(executor)
    .await?;
    let Some((
        id,
        branch_id,
        table_id,
        ticket_ref,
        status,
        opened_by,
        opened_by_name,
        customer_name,
        notes,
        guest_count,
        subtotal,
        order_id,
        opened_at,
        ready_at,
        settled_at,
    )) = row
    else {
        return Ok(None);
    };

    let items = sqlx::query_as::<_, (Uuid, i32, Option<Uuid>, serde_json::Value, i32, bool)>(
        "SELECT oti.id, r.round_number, oti.menu_item_id, oti.line, oti.line_total, \
                (oti.voided_at IS NOT NULL) AS voided \
         FROM open_ticket_items oti JOIN open_ticket_rounds r ON r.id = oti.round_id \
         WHERE oti.open_ticket_id = $1 ORDER BY r.round_number, oti.created_at",
    )
    .bind(id)
    .fetch_all(executor)
    .await?
    .into_iter()
    .map(
        |(id, round_number, menu_item_id, line, line_total, voided)| OpenTicketItemView {
            id,
            round_number,
            menu_item_id,
            line,
            line_total,
            voided,
        },
    )
    .collect();

    Ok(Some(OpenTicketView {
        id,
        branch_id,
        table_id,
        ticket_ref,
        status,
        opened_by,
        opened_by_name,
        customer_name,
        notes,
        guest_count,
        subtotal,
        order_id,
        opened_at,
        ready_at,
        settled_at,
        items,
    }))
}

// ── Shared fire logic (CLIENT-authoritative, like the teller) ─────
//
// The waiter client prices the cart itself (same as the POS create-order path) so
// it can fire OFFLINE; the server records the prices verbatim and only resolves
// display names + a fallback price for the kitchen/bill. Settlement replays the
// stored items through `create_order_inner` (the exact client-authoritative path),
// which computes deductions/inventory/tax/discount and mints the paid order.

/// A fired line: the client's priced `OrderItemInput` (stored as JSON for the
/// settle replay) plus a frozen display projection (bill + kitchen).
#[derive(Serialize, Deserialize)]
pub(crate) struct StoredTicketLine {
    /// Serialized `OrderItemInput` — replayed verbatim at settle.
    pub input: serde_json::Value,
    pub name: String,
    pub size_label: Option<String>,
    pub modifiers: Vec<String>,
    pub qty: i32,
    pub unit_price: i32,
    pub line_total: i32,
}

fn to_kitchen_line(l: &StoredTicketLine) -> KitchenLine {
    let menu_item_id = l
        .input
        .get("menu_item_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok());
    let notes = l
        .input
        .get("notes")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    KitchenLine {
        menu_item_id,
        name: l.name.clone(),
        qty: l.qty,
        size_label: l.size_label.clone(),
        modifiers: l.modifiers.clone(),
        notes,
        kitchen_item_id: None, // assigned by `fire_round` from the round idem key
    }
}

/// Resolve display names + a fallback price for the client's items. Pricing stays
/// client-authoritative: each item's `unit_price` is used when present, else the
/// catalog base price. Runs server-side (online or at replay), so an offline-fired
/// ticket gets its names when it syncs.
async fn resolve_ticket_lines(
    pool: &sqlx::PgPool,
    items: &[OrderItemInput],
) -> Result<Vec<StoredTicketLine>, AppError> {
    let item_ids: Vec<Uuid> = items.iter().filter_map(|i| i.menu_item_id).collect();
    let addon_ids: Vec<Uuid> = items
        .iter()
        .flat_map(|i| i.addons.iter().map(|a| a.addon_item_id))
        .collect();

    let item_map: std::collections::HashMap<Uuid, (String, i32)> = if item_ids.is_empty() {
        Default::default()
    } else {
        sqlx::query_as::<_, (Uuid, String, i32)>(
            "SELECT id, name, base_price FROM menu_items WHERE id = ANY($1)",
        )
        .bind(&item_ids)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|(id, n, p)| (id, (n, p)))
        .collect()
    };
    let addon_map: std::collections::HashMap<Uuid, String> = if addon_ids.is_empty() {
        Default::default()
    } else {
        sqlx::query_as::<_, (Uuid, String)>("SELECT id, name FROM addon_items WHERE id = ANY($1)")
            .bind(&addon_ids)
            .fetch_all(pool)
            .await?
            .into_iter()
            .collect()
    };

    let mut out = Vec::with_capacity(items.len());
    for it in items {
        let (name, base) = it
            .menu_item_id
            .and_then(|id| item_map.get(&id).cloned())
            .unwrap_or_else(|| ("Item".into(), 0));
        let unit_price = it.unit_price.unwrap_or(base);
        let addon_total: i32 = it
            .addons
            .iter()
            .map(|a| a.unit_price.unwrap_or(0) * a.quantity)
            .sum();
        let line_total = (unit_price + addon_total) * it.quantity;
        let modifiers: Vec<String> = it
            .addons
            .iter()
            .filter_map(|a| {
                addon_map.get(&a.addon_item_id).map(|n| {
                    if a.quantity > 1 {
                        format!("{}× {}", a.quantity, n)
                    } else {
                        n.clone()
                    }
                })
            })
            .collect();
        out.push(StoredTicketLine {
            input: serde_json::to_value(it).unwrap_or(serde_json::Value::Null),
            name,
            size_label: it.size_label.clone(),
            modifiers,
            qty: it.quantity,
            unit_price,
            line_total,
        });
    }
    Ok(out)
}

/// Fire a round of client-priced items onto an open ticket inside `tx`: store the
/// bill lines (with the client input for replay), bump the running subtotal, and
/// emit a kitchen ticket (returns its id for the post-commit publish).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn fire_round(
    tx: &mut Transaction<'_, Postgres>,
    pool: &sqlx::PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    open_ticket_id: Uuid,
    round_number: i32,
    fired_by: Uuid,
    round_idem: Option<Uuid>,
    items: &[OrderItemInput],
    table_label: Option<&str>,
    ticket_ref: Option<&str>,
) -> Result<Option<Uuid>, AppError> {
    let lines = resolve_ticket_lines(pool, items).await?;

    let round_id: Uuid = sqlx::query_scalar(
        "INSERT INTO open_ticket_rounds (open_ticket_id, round_number, fired_by, idempotency_key) \
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(open_ticket_id)
    .bind(round_number)
    .bind(fired_by)
    .bind(round_idem)
    .fetch_one(&mut **tx)
    .await?;

    let mut round_subtotal: i32 = 0;
    for line in &lines {
        let menu_item_id = line
            .input
            .get("menu_item_id")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok());
        sqlx::query(
            "INSERT INTO open_ticket_items \
                (open_ticket_id, round_id, menu_item_id, line, line_total) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(open_ticket_id)
        .bind(round_id)
        .bind(menu_item_id)
        .bind(serde_json::to_value(line).unwrap_or(serde_json::Value::Null))
        .bind(line.line_total)
        .execute(&mut **tx)
        .await?;
        round_subtotal += line.line_total;
    }

    sqlx::query(
        "UPDATE open_tickets SET subtotal = subtotal + $2, status = 'open', \
             ready_at = NULL, updated_at = now() WHERE id = $1",
    )
    .bind(open_ticket_id)
    .bind(round_subtotal)
    .execute(&mut **tx)
    .await?;

    // Derive the kitchen-ticket + per-line ids from the round's CLIENT idempotency
    // key, so an offline device that fired this round predicted the SAME ids (its KDS
    // projection + a later bump reconcile on sync). No key (a non-client fire) → the
    // server generates ids as before.
    let kitchen_ticket_id = round_idem.map(crate::kitchen::derive_kitchen_ticket_id);
    let mut klines: Vec<KitchenLine> = lines.iter().map(to_kitchen_line).collect();
    if let Some(kt) = kitchen_ticket_id {
        for (i, kl) in klines.iter_mut().enumerate() {
            kl.kitchen_item_id = Some(crate::kitchen::derive_kitchen_item_id(kt, i));
        }
    }
    let kt_id = emit_kitchen_ticket(
        tx,
        &EmitKitchen {
            org_id,
            branch_id,
            source_type: "open_ticket",
            source_id: open_ticket_id,
            round_number,
            table_label,
            kitchen_ref: ticket_ref,
            kitchen_ticket_id,
        },
        &klines,
    )
    .await?;

    Ok(kt_id)
}

/// Publish ticket + kitchen events after a fire commits. The ticket event always
/// fires; the kitchen event only when a kitchen ticket was actually emitted (it
/// isn't, e.g., in `off` mode).
pub(crate) async fn publish_fired(
    pool: &sqlx::PgPool,
    hub: &BranchEventHub,
    branch_id: Uuid,
    open_ticket_id: Uuid,
    kitchen_ticket_id: Option<Uuid>,
    event_type: &str,
) {
    if let Ok(Some(view)) = open_ticket_view(pool, open_ticket_id).await {
        hub.publish(
            branch_id,
            BranchEvent::new(Topic::Tickets, event_type, &view),
        );
    }
    if let Some(kt_id) = kitchen_ticket_id {
        crate::kitchen::publish_kitchen(pool, hub, branch_id, "kitchen.fired", kt_id).await;
    }
}

/// Mint a human-readable ticket ref `T-<branchcode>-<YYMMDD>-<NNNN>`.
pub(crate) async fn mint_ticket_ref(
    tx: &mut Transaction<'_, Postgres>,
    branch_id: Uuid,
    at: DateTime<Utc>,
) -> Result<String, AppError> {
    let (branch_code, biz_date): (String, chrono::NaiveDate) = sqlx::query_as(
        "SELECT COALESCE(b.code, 'T'), \
                ($1::timestamptz AT TIME ZONE COALESCE(b.timezone, o.timezone)::text)::date \
         FROM branches b JOIN organizations o ON o.id = b.org_id WHERE b.id = $2",
    )
    .bind(at)
    .bind(branch_id)
    .fetch_one(&mut **tx)
    .await?;
    let seq: i32 = sqlx::query_scalar(
        "INSERT INTO ticket_ref_counters (branch_id, business_date, last_seq) VALUES ($1, $2, 1) \
         ON CONFLICT (branch_id, business_date) \
         DO UPDATE SET last_seq = ticket_ref_counters.last_seq + 1 RETURNING last_seq",
    )
    .bind(branch_id)
    .bind(biz_date)
    .fetch_one(&mut **tx)
    .await?;
    Ok(format!(
        "T-{}-{}-{:04}",
        branch_code,
        biz_date.format("%y%m%d"),
        seq
    ))
}
