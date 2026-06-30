//! Kitchen Display System.
//!
//! Stations (Grill, Bar…) per branch, category→station + per-item routing, and the
//! source-agnostic kitchen substrate (`kitchen_tickets` / `kitchen_ticket_items`)
//! fed by BOTH waiter open-ticket rounds and teller (counter) orders. The KDS
//! reads the substrate and bumps per station; readiness is derived (a ticket is
//! ready when every non-voided line is bumped). Tickets print on the client.

pub mod kds;
pub mod routes;
pub mod stations;

#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};
use sqlx::{PgExecutor, Postgres, Transaction};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::errors::AppError;
use crate::realtime::event::{BranchEvent, Topic};
use crate::realtime::hub::BranchEventHub;

pub(crate) use crate::delivery::require_branch_access;
pub(crate) use crate::orgs::handlers::extract_claims;

// ── Shared shapes ─────────────────────────────────────────────

/// A slim kitchen display line (NO prices) — what the cook reads. Built from an
/// order item or a ticket round line by the caller.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct KitchenLine {
    pub menu_item_id: Option<Uuid>,
    pub name: String,
    pub qty: i32,
    #[serde(default)]
    pub size_label: Option<String>,
    #[serde(default)]
    pub modifiers: Vec<String>,
    #[serde(default)]
    pub notes: Option<String>,
    /// Client-DERIVED stable row id (from the round's idempotency key — see
    /// [`derive_kitchen_item_id`]), so a device's offline projection of this fire and
    /// a later bump on the same id reconcile once the fire syncs. Set transiently
    /// before insert; `#[serde(skip)]` keeps it out of the display `line` JSON.
    #[serde(skip)]
    pub kitchen_item_id: Option<Uuid>,
}

/// Fixed namespace for deterministic kitchen ids — MUST match the client's
/// `madar-core` copy byte-for-byte (it's the contract that lets an offline device
/// predict the ids the server will mint). "madar_kitchen_ns" as bytes.
const KITCHEN_ID_NS: Uuid = Uuid::from_u128(0x6d61_6461_725f_6b69_7463_6865_6e5f_6e73);

/// The kitchen-ticket id a fire will create, derived from the round's CLIENT
/// idempotency key. A device computes the same value offline to project the fire to
/// its KDS and to dedup against the server feed by id on reconnect.
pub fn derive_kitchen_ticket_id(round_idem: Uuid) -> Uuid {
    Uuid::new_v5(&KITCHEN_ID_NS, round_idem.as_bytes())
}

/// The kitchen-line id for the line at `index` within its (derived) kitchen ticket.
pub fn derive_kitchen_item_id(kitchen_ticket_id: Uuid, index: usize) -> Uuid {
    Uuid::new_v5(&kitchen_ticket_id, &(index as u32).to_le_bytes())
}

#[cfg(test)]
mod id_tests {
    // CROSS-REPO CONTRACT: these derived ids MUST match `madar-core`'s `kds::derive_*`
    // byte-for-byte (same namespace + v5 logic) — that's what lets an offline device
    // predict the ids the server will mint. If this changes, the client's pinned test
    // (and the namespace) MUST change in lockstep, or offline projections won't dedup.
    #[test]
    fn kitchen_id_derivation_is_pinned() {
        let kt = super::derive_kitchen_ticket_id(uuid::Uuid::nil());
        assert_eq!(kt.to_string(), "e9b2a598-f8ea-5510-8382-927f5e218fff");
        assert_eq!(
            super::derive_kitchen_item_id(kt, 0).to_string(),
            "0b40ac60-7d15-5bef-858f-849b09850f69"
        );
        assert_eq!(
            super::derive_kitchen_item_id(kt, 1).to_string(),
            "50cef3f1-fced-57d3-bb6c-daa1c917a8b6"
        );
    }
}

/// A KDS line as displayed/bumped.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, sqlx::FromRow)]
pub struct KitchenTicketItemView {
    pub id: Uuid,
    pub station_id: Option<Uuid>,
    pub station_name: Option<String>,
    pub line: serde_json::Value,
    pub qty: i32,
    pub bumped: bool,
}

/// One fire event projected for the kitchen (a round or a counter order).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct KitchenTicketView {
    pub id: Uuid,
    pub branch_id: Uuid,
    pub source_type: String,
    pub source_id: Uuid,
    pub table_label: Option<String>,
    pub kitchen_ref: Option<String>,
    pub round_number: i32,
    pub status: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub items: Vec<KitchenTicketItemView>,
}

/// Context for emitting a kitchen ticket from a source (order / open ticket).
pub struct EmitKitchen<'a> {
    pub org_id: Uuid,
    pub branch_id: Uuid,
    pub source_type: &'a str, // "order" | "open_ticket"
    pub source_id: Uuid,
    pub round_number: i32,
    pub table_label: Option<&'a str>,
    pub kitchen_ref: Option<&'a str>,
    /// Client-DERIVED kitchen-ticket id (see [`derive_kitchen_ticket_id`]). When set,
    /// the ticket is inserted with this id (idempotently) so a device's offline
    /// projection + the eventual server row share one id. `None` → server-generated.
    pub kitchen_ticket_id: Option<Uuid>,
}

// ── Routing ───────────────────────────────────────────────────

/// Resolve the station a line routes to, frozen at fire time:
/// item override → category rule → branch default station → `None` (unrouted).
pub(crate) async fn resolve_station(
    tx: &mut Transaction<'_, Postgres>,
    branch_id: Uuid,
    menu_item_id: Option<Uuid>,
) -> Result<Option<Uuid>, AppError> {
    if let Some(mi) = menu_item_id {
        // 1. Per-item override.
        if let Some(s) = sqlx::query_scalar::<_, Uuid>(
            "SELECT misr.station_id FROM menu_item_station_routes misr \
             JOIN kitchen_stations ks ON ks.id = misr.station_id \
                AND ks.deleted_at IS NULL AND ks.is_active \
             WHERE misr.branch_id = $1 AND misr.menu_item_id = $2",
        )
        .bind(branch_id)
        .bind(mi)
        .fetch_optional(&mut **tx)
        .await?
        {
            return Ok(Some(s));
        }
        // 2. Category rule for the item's category.
        if let Some(s) = sqlx::query_scalar::<_, Uuid>(
            "SELECT csr.station_id FROM category_station_routes csr \
             JOIN menu_items mi ON mi.category_id = csr.category_id \
             JOIN kitchen_stations ks ON ks.id = csr.station_id \
                AND ks.deleted_at IS NULL AND ks.is_active \
             WHERE csr.branch_id = $1 AND mi.id = $2",
        )
        .bind(branch_id)
        .bind(mi)
        .fetch_optional(&mut **tx)
        .await?
        {
            return Ok(Some(s));
        }
    }
    // 3. Branch default station (catch-all), else None (unrouted bucket).
    Ok(sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM kitchen_stations \
         WHERE branch_id = $1 AND is_default AND is_active AND deleted_at IS NULL",
    )
    .bind(branch_id)
    .fetch_optional(&mut **tx)
    .await?)
}

/// Insert a kitchen ticket + its (station-frozen) items inside an existing tx,
/// honoring the branch routing mode. Returns the new kitchen_ticket id, or `None`
/// when nothing should hit the kitchen:
///   - `off`  → no kitchen ticket at all (retail / no-kitchen branch);
///   - `kds`  → lines that route to NO station are dropped (a bottled water
///              doesn't clutter the grill screen); if none remain, no ticket;
///   - `till`/`both` → every line is kept (unrouted lines show on the till queue).
/// The caller publishes `kitchen.fired` AFTER commit (so subscribers never read
/// uncommitted rows) and only when this returns `Some`.
pub(crate) async fn emit_kitchen_ticket(
    tx: &mut Transaction<'_, Postgres>,
    ctx: &EmitKitchen<'_>,
    lines: &[KitchenLine],
) -> Result<Option<Uuid>, AppError> {
    // Effective routing mode (inline; `effective_routing_mode` needs a Copy executor).
    let stored: Option<String> =
        sqlx::query_scalar("SELECT kitchen_routing_mode::text FROM branches WHERE id = $1")
            .bind(ctx.branch_id)
            .fetch_optional(&mut **tx)
            .await?
            .flatten();
    let mode = match stored {
        Some(m) => m,
        None => {
            let has_station: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM kitchen_stations \
                 WHERE branch_id = $1 AND is_active AND deleted_at IS NULL)",
            )
            .bind(ctx.branch_id)
            .fetch_one(&mut **tx)
            .await?;
            if has_station {
                "kds".into()
            } else {
                "till".into()
            }
        }
    };
    if mode == "off" {
        return Ok(None);
    }

    // Resolve + freeze the station for each line; in kds mode, drop unrouted lines.
    let mut routed: Vec<(Option<Uuid>, &KitchenLine)> = Vec::with_capacity(lines.len());
    for line in lines {
        let station_id = resolve_station(tx, ctx.branch_id, line.menu_item_id).await?;
        if mode == "kds" && station_id.is_none() {
            continue;
        }
        routed.push((station_id, line));
    }
    if routed.is_empty() {
        return Ok(None);
    }

    // Honor a client-derived ticket id (idempotent) so an offline projection and the
    // eventual server row share one id; else let Postgres generate it.
    let ticket_id: Uuid = match ctx.kitchen_ticket_id {
        Some(cid) => {
            sqlx::query(
                "INSERT INTO kitchen_tickets \
                    (id, org_id, branch_id, source_type, source_id, table_label, kitchen_ref, round_number) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (id) DO NOTHING",
            )
            .bind(cid)
            .bind(ctx.org_id)
            .bind(ctx.branch_id)
            .bind(ctx.source_type)
            .bind(ctx.source_id)
            .bind(ctx.table_label)
            .bind(ctx.kitchen_ref)
            .bind(ctx.round_number)
            .execute(&mut **tx)
            .await?;
            cid
        }
        None => sqlx::query_scalar(
            "INSERT INTO kitchen_tickets \
                (org_id, branch_id, source_type, source_id, table_label, kitchen_ref, round_number) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
        )
        .bind(ctx.org_id)
        .bind(ctx.branch_id)
        .bind(ctx.source_type)
        .bind(ctx.source_id)
        .bind(ctx.table_label)
        .bind(ctx.kitchen_ref)
        .bind(ctx.round_number)
        .fetch_one(&mut **tx)
        .await?,
    };

    for (station_id, line) in routed {
        // A client-derived line id (idempotent) when present, else server-generated.
        match line.kitchen_item_id {
            Some(iid) => {
                sqlx::query(
                    "INSERT INTO kitchen_ticket_items \
                        (id, kitchen_ticket_id, station_id, menu_item_id, line, qty) \
                     VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (id) DO NOTHING",
                )
                .bind(iid)
                .bind(ticket_id)
                .bind(station_id)
                .bind(line.menu_item_id)
                .bind(serde_json::to_value(line).unwrap_or(serde_json::Value::Null))
                .bind(line.qty)
                .execute(&mut **tx)
                .await?;
            }
            None => {
                sqlx::query(
                    "INSERT INTO kitchen_ticket_items \
                        (kitchen_ticket_id, station_id, menu_item_id, line, qty) \
                     VALUES ($1, $2, $3, $4, $5)",
                )
                .bind(ticket_id)
                .bind(station_id)
                .bind(line.menu_item_id)
                .bind(serde_json::to_value(line).unwrap_or(serde_json::Value::Null))
                .bind(line.qty)
                .execute(&mut **tx)
                .await?;
            }
        }
    }
    Ok(Some(ticket_id))
}

// ── Read models ───────────────────────────────────────────────

/// Build the view for one kitchen ticket (used for the KDS feed and the
/// `kitchen.fired` / `kitchen.*` event payloads).
pub(crate) async fn kitchen_ticket_view<'e, E>(
    executor: E,
    ticket_id: Uuid,
) -> Result<Option<KitchenTicketView>, AppError>
where
    E: PgExecutor<'e> + Copy,
{
    let row: Option<(
        Uuid,
        Uuid,
        String,
        Uuid,
        Option<String>,
        Option<String>,
        i32,
        String,
        chrono::DateTime<chrono::Utc>,
    )> = sqlx::query_as(
        "SELECT id, branch_id, source_type, source_id, table_label, kitchen_ref, \
                    round_number, status::text, created_at \
             FROM kitchen_tickets WHERE id = $1",
    )
    .bind(ticket_id)
    .fetch_optional(executor)
    .await?;
    let Some((
        id,
        branch_id,
        source_type,
        source_id,
        table_label,
        kitchen_ref,
        round_number,
        status,
        created_at,
    )) = row
    else {
        return Ok(None);
    };

    let items = sqlx::query_as::<_, KitchenTicketItemView>(
        "SELECT kti.id, kti.station_id, ks.name AS station_name, kti.line, kti.qty, \
                (kti.bumped_at IS NOT NULL) AS bumped \
         FROM kitchen_ticket_items kti \
         LEFT JOIN kitchen_stations ks ON ks.id = kti.station_id \
         WHERE kti.kitchen_ticket_id = $1 AND kti.voided_at IS NULL \
         ORDER BY kti.created_at",
    )
    .bind(id)
    .fetch_all(executor)
    .await?;

    Ok(Some(KitchenTicketView {
        id,
        branch_id,
        source_type,
        source_id,
        table_label,
        kitchen_ref,
        round_number,
        status,
        created_at,
        items,
    }))
}

/// Publish a kitchen event for a ticket (best-effort: skips if the view is gone).
pub(crate) async fn publish_kitchen<'e, E>(
    executor: E,
    hub: &BranchEventHub,
    branch_id: Uuid,
    event_type: &str,
    ticket_id: Uuid,
) where
    E: PgExecutor<'e> + Copy,
{
    if let Ok(Some(view)) = kitchen_ticket_view(executor, ticket_id).await {
        hub.publish(
            branch_id,
            BranchEvent::new(Topic::Kitchen, event_type, &view),
        );
    }
}

/// The effective routing mode for a branch: the explicit override, else auto
/// (`kds` when the branch has any active station, else `till`).
pub(crate) async fn effective_routing_mode<'e, E>(
    executor: E,
    branch_id: Uuid,
) -> Result<String, AppError>
where
    E: PgExecutor<'e> + Copy,
{
    let stored: Option<String> =
        sqlx::query_scalar("SELECT kitchen_routing_mode::text FROM branches WHERE id = $1")
            .bind(branch_id)
            .fetch_optional(executor)
            .await?
            .flatten();
    if let Some(mode) = stored {
        return Ok(mode);
    }
    let has_station: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM kitchen_stations \
         WHERE branch_id = $1 AND is_active AND deleted_at IS NULL)",
    )
    .bind(branch_id)
    .fetch_one(executor)
    .await?;
    Ok(if has_station {
        "kds".into()
    } else {
        "till".into()
    })
}
