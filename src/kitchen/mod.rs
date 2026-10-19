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
    /// The BILL line this was fired from (`open_ticket_items.id`), so voiding
    /// that line can take this one off the board. `None` for a counter order,
    /// which has no bill lines. `#[serde(skip)]` — it is a link, not something
    /// the kitchen reads.
    #[serde(skip)]
    pub open_ticket_item_id: Option<Uuid>,
    /// The combo this line is a part of (C12): each part routes to its own
    /// station, tagged with the combo's name. `null` for a plain line; old KDS
    /// builds ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub combo: Option<crate::combos::types::KitchenComboTag>,
}

/// Deterministic kitchen ids: the kitchen-ticket id a fire will create, derived
/// from the round's CLIENT idempotency key, and the id of the line at `index`
/// within it. A device computes the same values offline to project the fire to
/// its KDS and to dedup against the server feed by id on reconnect — one copy
/// with the POS core, in madar-shared (`madar_sync::kitchen`).
pub use madar_sync::kitchen::{derive_kitchen_item_id, derive_kitchen_ticket_id};

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
    /// A counter cart sent ahead of its sale (the till's "Send to kitchen"),
    /// by the key the sale will carry. The sale takes the ticket over.
    Cart(Uuid),
}

impl KitchenSource {
    fn order_id(self) -> Option<Uuid> {
        match self {
            KitchenSource::Order(id) => Some(id),
            _ => None,
        }
    }
    fn open_ticket_id(self) -> Option<Uuid> {
        match self {
            KitchenSource::Round { open_ticket_id, .. } => Some(open_ticket_id),
            _ => None,
        }
    }
    fn round_id(self) -> Option<Uuid> {
        match self {
            KitchenSource::Round { round_id, .. } => Some(round_id),
            _ => None,
        }
    }
    fn cart_key(self) -> Option<Uuid> {
        match self {
            KitchenSource::Cart(key) => Some(key),
            _ => None,
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
pub async fn resolve_station(
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

/// [`resolve_station`] for many lines in ONE query, in the order given: the
/// same three rules, each a unique lookup (`(branch_id, menu_item_id)`,
/// `(branch_id, category_id)`, one default station per branch), so the same
/// station. A line without a menu item gets the branch default, as there.
pub async fn resolve_stations(
    tx: &mut Transaction<'_, Postgres>,
    branch_id: Uuid,
    menu_item_ids: &[Option<Uuid>],
) -> Result<Vec<Option<Uuid>>, AppError> {
    if menu_item_ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query!(
        r#"SELECT COALESCE(
                  (SELECT misr.station_id FROM menu_item_station_routes misr
                     JOIN kitchen_stations ks ON ks.id = misr.station_id
                        AND ks.deleted_at IS NULL AND ks.is_active
                    WHERE misr.branch_id = $1 AND misr.menu_item_id = u.mi),
                  (SELECT csr.station_id FROM category_station_routes csr
                     JOIN menu_items mi ON mi.category_id = csr.category_id
                     JOIN kitchen_stations ks ON ks.id = csr.station_id
                        AND ks.deleted_at IS NULL AND ks.is_active
                    WHERE csr.branch_id = $1 AND mi.id = u.mi),
                  (SELECT id FROM kitchen_stations
                    WHERE branch_id = $1 AND is_default AND is_active AND deleted_at IS NULL)
              ) AS station_id
             FROM UNNEST($2::uuid[]) WITH ORDINALITY AS u(mi, ord)
            ORDER BY u.ord"#,
        branch_id,
        menu_item_ids as &[Option<Uuid>],
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows.into_iter().map(|r| r.station_id).collect())
}

/// Insert a kitchen ticket + its (station-frozen) items inside an existing tx,
/// honoring the branch routing mode. Returns the new kitchen_ticket id, or `None`
/// when nothing should hit the kitchen:
///   - `off`  → no kitchen ticket at all (retail / no-kitchen branch);
///   - `kds`  → lines that route to NO station are dropped (a bottled water
///     doesn't clutter the grill screen); if none remain, no ticket;
///   - `till`/`both` → every line is kept (unrouted lines show on the till queue).
///
/// The caller publishes `kitchen.fired` AFTER commit (so subscribers never read
/// uncommitted rows) and only when this returns `Some`.
pub(crate) async fn emit_kitchen_ticket(
    tx: &mut Transaction<'_, Postgres>,
    ctx: &EmitKitchen<'_>,
    lines: &[KitchenLine],
) -> Result<Option<Uuid>, AppError> {
    let mode = routing_mode_on(tx, ctx.branch_id).await?;
    if mode == "off" {
        return Ok(None);
    }

    // Resolve + freeze the station for each line; in kds mode, drop unrouted lines.
    // One query for every line (see `resolve_stations`).
    let item_ids: Vec<Option<Uuid>> = lines.iter().map(|l| l.menu_item_id).collect();
    let stations = resolve_stations(tx, ctx.branch_id, &item_ids).await?;
    let mut routed: Vec<(Option<Uuid>, &KitchenLine)> = Vec::with_capacity(lines.len());
    for (line, station_id) in lines.iter().zip(stations) {
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
            sqlx::query!(
                "INSERT INTO kitchen_tickets \
                    (id, org_id, branch_id, order_id, open_ticket_id, round_id, \
                     table_label, kitchen_ref, round_number, cart_key) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) ON CONFLICT (id) DO NOTHING",
                cid,
                ctx.org_id,
                ctx.branch_id,
                ctx.source.order_id(),
                ctx.source.open_ticket_id(),
                ctx.source.round_id(),
                ctx.table_label,
                ctx.kitchen_ref,
                ctx.round_number,
                ctx.source.cart_key(),
            )
            .execute(&mut **tx)
            .await?;
            cid
        }
        None => {
            sqlx::query_scalar!(
                "INSERT INTO kitchen_tickets \
                (org_id, branch_id, order_id, open_ticket_id, round_id, \
                 table_label, kitchen_ref, round_number, cart_key) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING id",
                ctx.org_id,
                ctx.branch_id,
                ctx.source.order_id(),
                ctx.source.open_ticket_id(),
                ctx.source.round_id(),
                ctx.table_label,
                ctx.kitchen_ref,
                ctx.round_number,
                ctx.source.cart_key(),
            )
            .fetch_one(&mut **tx)
            .await?
        }
    };

    // Every line in ONE statement, in routing order. A client-derived line id
    // (idempotent: a re-sent fire inserts nothing) when present, else a
    // server-generated one (which cannot conflict). `org_id` is the ticket's,
    // written here rather than looked up per row by `fill_org_id`.
    let ids: Vec<Option<Uuid>> = routed.iter().map(|(_, l)| l.kitchen_item_id).collect();
    let station: Vec<Option<Uuid>> = routed.iter().map(|(s, _)| *s).collect();
    let menu_item: Vec<Option<Uuid>> = routed.iter().map(|(_, l)| l.menu_item_id).collect();
    let line_json: Vec<serde_json::Value> = routed
        .iter()
        .map(|(_, l)| serde_json::to_value(l).unwrap_or(serde_json::Value::Null))
        .collect();
    let qty: Vec<i32> = routed.iter().map(|(_, l)| l.qty).collect();
    let bill_line: Vec<Option<Uuid>> = routed.iter().map(|(_, l)| l.open_ticket_item_id).collect();
    sqlx::query!(
        "INSERT INTO kitchen_ticket_items \
            (id, org_id, kitchen_ticket_id, station_id, menu_item_id, line, qty, open_ticket_item_id) \
         SELECT COALESCE(u.id, gen_random_uuid()), $1, $2, u.station, u.menu_item, u.line, u.qty, u.bill_line \
           FROM UNNEST($3::uuid[], $4::uuid[], $5::uuid[], $6::jsonb[], $7::int4[], $8::uuid[]) \
                WITH ORDINALITY AS u(id, station, menu_item, line, qty, bill_line, ord) \
          ORDER BY u.ord \
         ON CONFLICT (id) DO NOTHING",
        ctx.org_id,
        ticket_id,
        &ids as &[Option<Uuid>],
        &station as &[Option<Uuid>],
        &menu_item as &[Option<Uuid>],
        &line_json,
        &qty,
        &bill_line as &[Option<Uuid>],
    )
    .execute(&mut **tx)
    .await?;
    Ok(Some(ticket_id))
}

// ── A counter cart sent ahead ─────────────────────────────────

/// A sent cart's lines, numbered as the till numbers its projection: the line
/// at `index` is [`derive_kitchen_item_id`] of the cart's ticket.
pub(crate) fn cart_kitchen_lines(req: &madar_sync::kitchen::SendCartRequest) -> Vec<KitchenLine> {
    let kt = derive_kitchen_ticket_id(req.cart_key);
    req.lines
        .iter()
        .enumerate()
        .map(|(i, l)| KitchenLine {
            menu_item_id: l.menu_item_id,
            name: l.name.clone(),
            qty: l.qty.max(1),
            size_label: l.size_label.clone(),
            modifiers: l.modifiers.clone(),
            notes: l.notes.clone(),
            kitchen_item_id: Some(derive_kitchen_item_id(kt, i)),
            open_ticket_item_id: None,
            combo: None,
        })
        .collect()
}

/// Bring a ticket that already exists in line with `lines` (each carrying its
/// id): a line still there takes its new quantity and words, a line that is
/// gone is voided, and a ticket the kitchen had finished reopens when it has
/// work again. A line the cook finished stays finished. New lines are the
/// caller's (`emit_kitchen_ticket` inserts them, idempotently by id).
pub(crate) async fn refresh_kitchen_lines(
    tx: &mut Transaction<'_, Postgres>,
    ticket_id: Uuid,
    lines: &[KitchenLine],
) -> Result<(), AppError> {
    let ids: Vec<Uuid> = lines.iter().filter_map(|l| l.kitchen_item_id).collect();
    let line_json: Vec<serde_json::Value> = lines
        .iter()
        .filter(|l| l.kitchen_item_id.is_some())
        .map(|l| serde_json::to_value(l).unwrap_or(serde_json::Value::Null))
        .collect();
    let qty: Vec<i32> = lines
        .iter()
        .filter(|l| l.kitchen_item_id.is_some())
        .map(|l| l.qty)
        .collect();
    sqlx::query!(
        "UPDATE kitchen_ticket_items k SET line = u.line, qty = u.qty \
           FROM UNNEST($2::uuid[], $3::jsonb[], $4::int4[]) AS u(id, line, qty) \
          WHERE k.id = u.id AND k.kitchen_ticket_id = $1 AND k.voided_at IS NULL",
        ticket_id,
        &ids,
        &line_json,
        &qty,
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!(
        "UPDATE kitchen_ticket_items SET voided_at = now() \
          WHERE kitchen_ticket_id = $1 AND voided_at IS NULL AND id <> ALL($2)",
        ticket_id,
        &ids,
    )
    .execute(&mut **tx)
    .await?;
    // A finished ticket with work again is the kitchen's again; only the
    // kitchen's own close ("bumped") is undone, as a recall would.
    sqlx::query!(
        "UPDATE kitchen_tickets SET status = 'firing', ready_at = NULL, closed_at = NULL, \
                close_reason = NULL, closed_by = NULL \
          WHERE id = $1 AND close_reason = 'bumped' AND EXISTS ( \
                SELECT 1 FROM kitchen_ticket_items \
                 WHERE kitchen_ticket_id = $1 AND voided_at IS NULL AND bumped_at IS NULL)",
        ticket_id,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The sale of a cart sent ahead takes that cart's kitchen ticket over: it is
/// this order's from now on, numbered as the order. `None` when the cart was
/// never sent (the sale fires its own ticket as before).
pub(crate) async fn take_over_cart_ticket(
    tx: &mut Transaction<'_, Postgres>,
    org_id: Uuid,
    cart_key: Uuid,
    order_id: Uuid,
    kitchen_ref: Option<&str>,
) -> Result<Option<Uuid>, AppError> {
    Ok(sqlx::query_scalar!(
        "UPDATE kitchen_tickets SET order_id = $3, cart_key = NULL, \
                kitchen_ref = COALESCE($4, kitchen_ref) \
          WHERE org_id = $1 AND cart_key = $2 RETURNING id",
        org_id,
        cart_key,
        order_id,
        kitchen_ref,
    )
    .fetch_optional(&mut **tx)
    .await?)
}

/// Send a counter cart to the kitchen (live route and replay). The first send
/// fires (`kitchen.fired`, which also pushes a screen that is not watching);
/// a send again of the same cart updates the ticket (`kitchen.changed`). A
/// cart already sold is a no-op: its ticket is the order's now.
pub(crate) async fn send_cart_inner(
    pool: &sqlx::PgPool,
    hub: Option<&BranchEventHub>,
    org_id: Uuid,
    req: &madar_sync::kitchen::SendCartRequest,
) -> Result<Option<Uuid>, AppError> {
    let kt = derive_kitchen_ticket_id(req.cart_key);
    let lines = cart_kitchen_lines(req);
    let mut tx = pool.begin().await?;
    let existing: Option<Option<Uuid>> = sqlx::query_scalar!(
        "SELECT cart_key FROM kitchen_tickets WHERE id = $1 AND org_id = $2 FOR UPDATE",
        kt,
        org_id,
    )
    .fetch_optional(&mut *tx)
    .await?;
    if matches!(existing, Some(None)) {
        return Ok(Some(kt));
    }
    let emitted = emit_kitchen_ticket(
        &mut tx,
        &EmitKitchen {
            org_id,
            branch_id: req.branch_id,
            source: KitchenSource::Cart(req.cart_key),
            round_number: 1,
            table_label: None,
            kitchen_ref: req.kitchen_ref.as_deref(),
            kitchen_ticket_id: Some(kt),
        },
        &lines,
    )
    .await?;
    if existing.is_some() {
        refresh_kitchen_lines(&mut tx, kt, &lines).await?;
        if let Some(r) = req.kitchen_ref.as_deref() {
            sqlx::query!(
                "UPDATE kitchen_tickets SET kitchen_ref = $2 WHERE id = $1",
                kt,
                r
            )
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    let ticket = if existing.is_some() {
        Some(kt)
    } else {
        emitted
    };
    if let (Some(hub), Some(id)) = (hub, ticket) {
        let event = if existing.is_some() {
            "kitchen.changed"
        } else {
            "kitchen.fired"
        };
        publish_kitchen(pool, hub, req.branch_id, event, id).await;
    }
    Ok(ticket)
}

// ── Finishing ─────────────────────────────────────────────────

/// Close every live kitchen ticket fired for `of`, inside the caller's tx.
///
/// `Settled` and `Voided` are the two closes a bill can cause; a bump is the
/// KDS's own (`kds::set_bump_inner`) and a retirement is the shift's
/// ([`retire_unbumped_at_till_close`]). The FIRST close wins — a ticket the
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
pub(crate) async fn retire_unbumped_at_till_close(
    tx: &mut Transaction<'_, Postgres>,
    branch_id: Uuid,
    closed_by: Option<Uuid>,
) -> Result<u64, AppError> {
    if routing_mode_on(tx, branch_id).await? != "till" {
        return Ok(0);
    }
    let another_open: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM tills WHERE branch_id = $1 AND status = 'open')",
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
pub(crate) async fn kitchen_ticket_view(
    pool: &sqlx::PgPool,
    ticket_id: Uuid,
) -> Result<Option<KitchenTicketView>, AppError> {
    let mut conn = pool.acquire().await?;
    Ok(kitchen_ticket_views(&mut conn, &[ticket_id]).await?.pop())
}

/// Many kitchen ticket views in two queries, in the order of `ids` (missing
/// ids are skipped). Runs on the caller's connection.
pub(crate) async fn kitchen_ticket_views(
    conn: &mut PgConnection,
    ids: &[Uuid],
) -> Result<Vec<KitchenTicketView>, AppError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
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
             FROM kitchen_tickets WHERE id = ANY($1)",
    )
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    #[derive(sqlx::FromRow)]
    struct ItemRow {
        kitchen_ticket_id: Uuid,
        #[sqlx(flatten)]
        item: KitchenTicketItemView,
    }
    let found: Vec<Uuid> = rows.iter().map(|r| r.0).collect();
    let item_rows = sqlx::query_as::<_, ItemRow>(
        "SELECT kti.kitchen_ticket_id, kti.id, kti.station_id, ks.name AS station_name, kti.line, kti.qty, \
                (kti.bumped_at IS NOT NULL) AS bumped \
         FROM kitchen_ticket_items kti \
         LEFT JOIN kitchen_stations ks ON ks.id = kti.station_id \
         WHERE kti.kitchen_ticket_id = ANY($1) AND kti.voided_at IS NULL \
         ORDER BY kti.kitchen_ticket_id, kti.created_at",
    )
    .bind(&found)
    .fetch_all(&mut *conn)
    .await?;
    let mut items_of: std::collections::HashMap<Uuid, Vec<KitchenTicketItemView>> =
        std::collections::HashMap::new();
    for r in item_rows {
        items_of
            .entry(r.kitchen_ticket_id)
            .or_default()
            .push(r.item);
    }

    let mut by_id: std::collections::HashMap<Uuid, KitchenTicketView> = rows
        .into_iter()
        .map(
            |(
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
            )| {
                let items = items_of.remove(&id).unwrap_or_default();
                (
                    id,
                    KitchenTicketView {
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
                    },
                )
            },
        )
        .collect();
    Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
}

/// Publish a kitchen event for a ticket (best-effort: skips if the view is gone).
pub(crate) async fn publish_kitchen(
    executor: &sqlx::PgPool,
    hub: &BranchEventHub,
    branch_id: Uuid,
    event_type: &str,
    ticket_id: Uuid,
) {
    if let Ok(Some(view)) = kitchen_ticket_view(executor, ticket_id).await {
        hub.publish(
            branch_id,
            BranchEvent::new(Topic::Kitchen, event_type, &view),
        );
        if event_type == "kitchen.fired" {
            crate::push::kitchen::fired(executor, hub, &view);
        }
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
