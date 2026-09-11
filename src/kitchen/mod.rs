//! Kitchen Display System.
//!
//! Stations (Grill, Bar…) per branch, category→station + per-item routing, and the
//! source-agnostic kitchen substrate (`kitchen_tickets` / `kitchen_ticket_items`)
//! fed by BOTH waiter open-ticket rounds and teller (counter) orders. The KDS
//! reads the substrate and bumps per station; readiness is derived (a ticket is
//! ready when every non-voided line is bumped). Tickets print on the client.
//!
//! A kitchen ticket has two independent clocks. `status` is the state of the
//! COOKING — firing, ready, voided. `closed_at` / `close_reason` is whether the
//! ticket is still the kitchen's business at all: the KDS feed is the set of
//! tickets with no `closed_at`, and a ticket leaves it when the kitchen bumps
//! the last line, when its bill settles, when its bill or round is voided, or
//! when a branch that never bumps (routing mode `till`) closes its shift. The
//! first close wins; a bump is the only close a recall may reopen.

pub mod kds;
pub mod routes;
pub mod stations;

#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgExecutor, Postgres, Transaction};
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
    /// The state of the cooking: `firing`, `ready`, `voided`.
    pub status: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// When the ticket left the kitchen's attention for good; `null` while it
    /// is live. A till queue that shows history renders closed tickets greyed;
    /// the KDS feed never returns them.
    #[serde(default)]
    pub closed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// `bumped`, `settled`, `voided` or `retired` — see [`CloseReason`].
    #[serde(default)]
    pub close_reason: Option<String>,
    pub items: Vec<KitchenTicketItemView>,
}

/// What a kitchen ticket is FOR. Exactly one reference is set; the
/// `source_type` / `source_id` pair the KDS wire contract reads is GENERATED
/// from these by the database, so a caller names the thing itself and the pair
/// cannot disagree with it.
#[derive(Debug, Clone, Copy)]
pub enum KitchenSource {
    /// A counter order, fired the moment it is paid.
    Order(Uuid),
    /// One round of a waiter's open ticket. The round is named as well as the
    /// ticket because one fire is one round is one kitchen ticket
    /// (`uq_kitchen_tickets_round`).
    Round {
        open_ticket_id: Uuid,
        round_id: Uuid,
    },
}

impl KitchenSource {
    fn order_id(self) -> Option<Uuid> {
        match self {
            KitchenSource::Order(id) => Some(id),
            KitchenSource::Round { .. } => None,
        }
    }
    fn open_ticket_id(self) -> Option<Uuid> {
        match self {
            KitchenSource::Order(_) => None,
            KitchenSource::Round { open_ticket_id, .. } => Some(open_ticket_id),
        }
    }
    fn round_id(self) -> Option<Uuid> {
        match self {
            KitchenSource::Order(_) => None,
            KitchenSource::Round { round_id, .. } => Some(round_id),
        }
    }
}

/// Why a kitchen ticket left the screen — `kitchen_ticket_close_reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// Every line done. The only close a recall (unbump) reopens.
    Bumped,
    /// Its bill was paid.
    Settled,
    /// Its bill or round was voided.
    Voided,
    /// Closed by hand, by migration, or at shift close where nobody bumps.
    /// Never by the kitchen, so it carries no `closed_by`.
    Retired,
}

impl CloseReason {
    fn as_str(self) -> &'static str {
        match self {
            CloseReason::Bumped => "bumped",
            CloseReason::Settled => "settled",
            CloseReason::Voided => "voided",
            CloseReason::Retired => "retired",
        }
    }
}

/// Which tickets a close addresses: everything fired for one counter order, or
/// every round of one open ticket.
#[derive(Debug, Clone, Copy)]
pub(crate) enum KitchenSourceRef {
    Order(Uuid),
    OpenTicket(Uuid),
}

/// Context for emitting a kitchen ticket from a source (order / open ticket).
pub struct EmitKitchen<'a> {
    pub org_id: Uuid,
    pub branch_id: Uuid,
    pub source: KitchenSource,
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
    let mode = routing_mode_on(&mut **tx, ctx.branch_id).await?;
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

    // The ticket is written by what it is FOR — the order, or the (ticket, round)
    // pair. `source_type` / `source_id` are generated from these columns and are
    // never named in an INSERT.
    //
    // Honor a client-derived ticket id (idempotent) so an offline projection and the
    // eventual server row share one id; else let Postgres generate it.
    let ticket_id: Uuid = match ctx.kitchen_ticket_id {
        Some(cid) => {
            sqlx::query(
                "INSERT INTO kitchen_tickets \
                    (id, org_id, branch_id, order_id, open_ticket_id, round_id, \
                     table_label, kitchen_ref, round_number) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) ON CONFLICT (id) DO NOTHING",
            )
            .bind(cid)
            .bind(ctx.org_id)
            .bind(ctx.branch_id)
            .bind(ctx.source.order_id())
            .bind(ctx.source.open_ticket_id())
            .bind(ctx.source.round_id())
            .bind(ctx.table_label)
            .bind(ctx.kitchen_ref)
            .bind(ctx.round_number)
            .execute(&mut **tx)
            .await?;
            cid
        }
        None => {
            sqlx::query_scalar(
                "INSERT INTO kitchen_tickets \
                (org_id, branch_id, order_id, open_ticket_id, round_id, \
                 table_label, kitchen_ref, round_number) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
            )
            .bind(ctx.org_id)
            .bind(ctx.branch_id)
            .bind(ctx.source.order_id())
            .bind(ctx.source.open_ticket_id())
            .bind(ctx.source.round_id())
            .bind(ctx.table_label)
            .bind(ctx.kitchen_ref)
            .bind(ctx.round_number)
            .fetch_one(&mut **tx)
            .await?
        }
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

// ── Finishing ─────────────────────────────────────────────────

/// Close every live kitchen ticket fired for `of`, inside the caller's tx.
///
/// `Settled` and `Voided` are the two closes a bill can cause; a bump is the
/// KDS's own (`kds::set_bump_inner`) and a retirement is the shift's
/// ([`retire_unbumped_at_shift_close`]). The FIRST close wins — a ticket the
/// kitchen already bumped keeps `bumped` when its bill settles an hour later,
/// because that is what happened. A void is the exception in one respect: it
/// is also a change to the COOKING, so `status` becomes `voided` and every
/// live line is voided (which takes it off any station's queue) whether or not
/// the ticket had already closed.
///
/// Returns the ids of every ticket touched, for the post-commit publish.
pub(crate) async fn close_kitchen_tickets(
    tx: &mut Transaction<'_, Postgres>,
    of: KitchenSourceRef,
    reason: CloseReason,
    closed_by: Option<Uuid>,
) -> Result<Vec<Uuid>, AppError> {
    let (column, id) = match of {
        KitchenSourceRef::Order(id) => ("order_id", id),
        KitchenSourceRef::OpenTicket(id) => ("open_ticket_id", id),
    };
    let voiding = reason == CloseReason::Voided;
    if voiding {
        sqlx::query(&format!(
            "UPDATE kitchen_ticket_items i SET voided_at = now() \
             FROM kitchen_tickets kt \
             WHERE i.kitchen_ticket_id = kt.id AND kt.{column} = $1 AND i.voided_at IS NULL"
        ))
        .bind(id)
        .execute(&mut **tx)
        .await?;
    }
    // In a SET list every column reference reads the row BEFORE the update, so
    // `closed_by` can be conditioned on the old `closed_at`.
    let ids: Vec<Uuid> = sqlx::query_scalar(&format!(
        "UPDATE kitchen_tickets SET \
             status       = CASE WHEN $3 THEN 'voided'::kitchen_ticket_status ELSE status END, \
             voided_at    = CASE WHEN $3 THEN COALESCE(voided_at, now()) ELSE voided_at END, \
             closed_at    = COALESCE(closed_at, now()), \
             close_reason = COALESCE(close_reason, $2::kitchen_ticket_close_reason), \
             closed_by    = CASE WHEN closed_at IS NULL THEN $4::uuid ELSE closed_by END \
         WHERE {column} = $1 \
           AND (closed_at IS NULL OR ($3 AND status <> 'voided')) \
         RETURNING id"
    ))
    .bind(id)
    .bind(reason.as_str())
    .bind(voiding)
    .bind(closed_by)
    .fetch_all(&mut **tx)
    .await?;
    Ok(ids)
}

/// Ruling (c): at a branch in routing mode `till` nobody bumps, so a kitchen
/// ticket there has no close of its own and would sit on the till queue for
/// ever (4,334 of 4,731 in production did). It closes when the branch's LAST
/// open shift closes — `settled` where its bill was paid, `retired` otherwise.
///
/// Last shift, not any shift: two tills at one branch share one queue, and a
/// till closing at ten must not wipe what the till open until eleven is still
/// serving from. Call this AFTER the closing shift's status has been written,
/// inside the same tx, so the "no shift still open" test sees it.
///
/// `closed_by` is stamped only on the `settled` closes; a retirement names no
/// actor, per the column's contract. Returns how many tickets closed.
pub(crate) async fn retire_unbumped_at_shift_close(
    tx: &mut Transaction<'_, Postgres>,
    branch_id: Uuid,
    closed_by: Option<Uuid>,
) -> Result<u64, AppError> {
    if routing_mode_on(&mut **tx, branch_id).await? != "till" {
        return Ok(0);
    }
    let another_open: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM shifts WHERE branch_id = $1 AND status = 'open')",
    )
    .bind(branch_id)
    .fetch_one(&mut **tx)
    .await?;
    if another_open {
        return Ok(0);
    }
    // A counter order is paid the moment it fires, so its bill has settled
    // unless the order was later voided; an open ticket's bill has settled when
    // it says so. Anything else — a voided order whose kitchen copy predates
    // voids closing it, a bill still open on the floor — is retired.
    let closed = sqlx::query(
        "UPDATE kitchen_tickets kt SET \
             closed_at    = now(), \
             close_reason = CASE WHEN settled THEN 'settled' ELSE 'retired' END::kitchen_ticket_close_reason, \
             closed_by    = CASE WHEN settled THEN $2::uuid ELSE NULL END \
         FROM (SELECT k.id, \
                      (k.order_id IS NOT NULL AND EXISTS ( \
                           SELECT 1 FROM orders o WHERE o.id = k.order_id AND o.status <> 'voided')) \
                      OR (k.open_ticket_id IS NOT NULL AND EXISTS ( \
                           SELECT 1 FROM open_tickets t WHERE t.id = k.open_ticket_id AND t.status = 'settled')) \
                      AS settled \
                 FROM kitchen_tickets k \
                WHERE k.branch_id = $1 AND k.closed_at IS NULL) live \
         WHERE kt.id = live.id",
    )
    .bind(branch_id)
    .bind(closed_by)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(closed)
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
    #[allow(clippy::type_complexity)]
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
        Option<chrono::DateTime<chrono::Utc>>,
        Option<String>,
    )> = sqlx::query_as(
        "SELECT id, branch_id, source_type, source_id, table_label, kitchen_ref, \
                    round_number, status::text, created_at, closed_at, close_reason::text \
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
        closed_at,
        close_reason,
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
        closed_at,
        close_reason,
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

/// The routing mode as seen from inside a transaction. Same rule as
/// [`effective_routing_mode`]; a separate body because a `&mut PgConnection`
/// is not `Copy` and the two reads have to borrow it in turn.
async fn routing_mode_on(conn: &mut PgConnection, branch_id: Uuid) -> Result<String, AppError> {
    let stored: Option<String> =
        sqlx::query_scalar("SELECT kitchen_routing_mode::text FROM branches WHERE id = $1")
            .bind(branch_id)
            .fetch_optional(&mut *conn)
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
    .fetch_one(&mut *conn)
    .await?;
    Ok(if has_station {
        "kds".into()
    } else {
        "till".into()
    })
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
