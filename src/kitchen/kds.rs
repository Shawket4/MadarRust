//! The KDS feed (outstanding work, optionally per station) and the per-line bump.
//! Permission resource `kitchen_orders` (the kitchen-display / till devices).

use actix_web::{HttpRequest, HttpResponse, web};
use serde::Deserialize;
use sqlx::PgPool;
use utoipa::IntoParams;
use uuid::Uuid;

use super::{
    KitchenTicketView, extract_claims, kitchen_ticket_view, publish_kitchen, require_branch_access,
};
use crate::errors::{AppError, AppErrorResponse};
use crate::permissions::checker::check_permission;
use crate::realtime::event::{BranchEvent, Topic};
use crate::realtime::hub::BranchEventHub;
use crate::sync::ActingContext;

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct FeedQuery {
    pub branch_id: Uuid,
    /// Optional station filter — only tickets with pending work for this station.
    /// (Items are returned in full; the client greys/filters by station.)
    #[serde(default)]
    pub station_id: Option<Uuid>,
}

/// The branch's LIVE kitchen tickets — not closed, oldest first — optionally
/// narrowed to those with un-bumped work for one station. Seed for the KDS;
/// live updates arrive on `/realtime/stream?topics=kitchen`.
///
/// "Live" is `closed_at IS NULL`, not "has an un-bumped line": a ticket at a
/// branch that never bumps stays on the till queue until its bill settles or
/// the shift closes, and a ticket the kitchen finished is closed `bumped` the
/// moment its last line is.
#[utoipa::path(get, path = "/kitchen/orders", tag = "kitchen", params(FeedQuery),
    responses((status = 200, body = Vec<KitchenTicketView>), AppErrorResponse), security(("bearer_jwt" = [])))]
pub async fn feed(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<FeedQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "kitchen_orders", "read").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;

    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT kt.id FROM kitchen_tickets kt \
         WHERE kt.branch_id = $1 AND kt.closed_at IS NULL AND kt.status <> 'voided' \
           AND ($2::uuid IS NULL OR EXISTS ( \
                   SELECT 1 FROM kitchen_ticket_items kti \
                   WHERE kti.kitchen_ticket_id = kt.id \
                     AND kti.voided_at IS NULL AND kti.bumped_at IS NULL \
                     AND kti.station_id = $2)) \
         ORDER BY kt.created_at",
    )
    .bind(query.branch_id)
    .bind(query.station_id)
    .fetch_all(pool.get_ref())
    .await?;

    let mut out: Vec<KitchenTicketView> = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(v) = kitchen_ticket_view(pool.get_ref(), id).await? {
            out.push(v);
        }
    }
    Ok(HttpResponse::Ok().json(out))
}

/// Bump/unbump one fired line — claims-free core shared by the live route and the
/// offline `/sync/replay` path. `actor` attributes `bumped_by`; `hub` is `Some` for
/// a live action (publish the change) and `None` on replay (the bump is historical;
/// cloud consumers re-seed via the realtime snapshot on reconnect).
///
/// Idempotency: the `item_id` is the natural key — re-bumping a bumped line is a
/// no-op (`bumped_at IS NULL` guard). On **replay** a line that's gone (voided, or
/// never present) returns a clean 204 rather than a 404, so a queued bump can never
/// wedge the FIFO drain. The live route 404s on a genuinely-missing line.
///
/// The bill is not touched. `open_tickets.status` says what the BILL is; whether
/// the kitchen has plated it is read from `kitchen_tickets` by whoever asks
/// (`tickets::open_ticket_view` derives `ready` per ticket). The one thing the
/// bump leaves on the bill is `ready_at`, the moment the whole ticket was last
/// plated — history for the timing reports.
pub(crate) async fn set_bump_inner(
    pool: &PgPool,
    hub: Option<&BranchEventHub>,
    actor: &ActingContext,
    item_id: Uuid,
    bumped: bool,
) -> Result<HttpResponse, AppError> {
    // Locate the line's ticket (branch, source) so we can recompute readiness.
    let row: Option<(Uuid, Uuid, Option<Uuid>)> = sqlx::query_as(
        "SELECT kt.id, kt.branch_id, kt.open_ticket_id \
         FROM kitchen_ticket_items kti JOIN kitchen_tickets kt ON kt.id = kti.kitchen_ticket_id \
         WHERE kti.id = $1 AND kti.voided_at IS NULL",
    )
    .bind(item_id)
    .fetch_optional(pool)
    .await?;
    let Some((ticket_id, branch_id, open_ticket_id)) = row else {
        if actor.replay {
            return Ok(HttpResponse::NoContent().finish());
        }
        return Err(AppError::NotFound("Kitchen line not found".into()));
    };

    let mut tx = pool.begin().await?;
    if bumped {
        sqlx::query(
            "UPDATE kitchen_ticket_items SET bumped_at = now(), bumped_by = $2 \
             WHERE id = $1 AND bumped_at IS NULL AND voided_at IS NULL",
        )
        .bind(item_id)
        .bind(actor.teller_id)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query(
            "UPDATE kitchen_ticket_items SET bumped_at = NULL, bumped_by = NULL \
             WHERE id = $1 AND voided_at IS NULL",
        )
        .bind(item_id)
        .execute(&mut *tx)
        .await?;
    }

    // Recompute the kitchen ticket: ready ⇔ no non-voided line is un-bumped.
    //
    // Readiness is also a CLOSE — the kitchen is done with the ticket — and the
    // only close a recall may reopen. So the last bump writes `closed_at` with
    // `bumped` unless something already closed it (a bill settled under a
    // still-firing ticket keeps `settled`: first close wins), and an unbump
    // clears the close only when it was the kitchen's own. Column references
    // in the SET list read the row before the update, which is what lets
    // `closed_by` be conditioned on the old `closed_at`.
    let ticket_ready: bool = sqlx::query_scalar(
        "WITH d AS ( \
             SELECT NOT EXISTS ( \
                 SELECT 1 FROM kitchen_ticket_items \
                 WHERE kitchen_ticket_id = $1 AND voided_at IS NULL AND bumped_at IS NULL \
             ) AS done) \
         UPDATE kitchen_tickets kt SET \
             status       = CASE WHEN d.done THEN 'ready'::kitchen_ticket_status \
                                 ELSE 'firing'::kitchen_ticket_status END, \
             ready_at     = CASE WHEN d.done THEN COALESCE(kt.ready_at, now()) ELSE NULL END, \
             closed_at    = CASE WHEN d.done THEN COALESCE(kt.closed_at, now()) \
                                 WHEN kt.close_reason = 'bumped' THEN NULL \
                                 ELSE kt.closed_at END, \
             close_reason = CASE WHEN d.done THEN COALESCE(kt.close_reason, 'bumped'::kitchen_ticket_close_reason) \
                                 WHEN kt.close_reason = 'bumped' THEN NULL \
                                 ELSE kt.close_reason END, \
             closed_by    = CASE WHEN d.done AND kt.closed_at IS NULL THEN $2::uuid \
                                 WHEN d.done THEN kt.closed_by \
                                 WHEN kt.close_reason = 'bumped' THEN NULL \
                                 ELSE kt.closed_by END \
         FROM d WHERE kt.id = $1 AND kt.status <> 'voided' \
         RETURNING (kt.status = 'ready')",
    )
    .bind(ticket_id)
    .bind(actor.teller_id)
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or(false);

    // For a waiter ticket: the bill is ready ⇔ every line across all of its
    // kitchen tickets is bumped. Derived, never stored as a status — the only
    // mark left on the bill is `ready_at` (when it last happened) and an
    // `updated_at` so a till polling `?updated_after=` re-pulls the view.
    let mut open_ticket_ready = false;
    if let Some(ot) = open_ticket_id {
        open_ticket_ready = sqlx::query_scalar::<_, bool>(
            "WITH d AS ( \
                 SELECT NOT EXISTS ( \
                     SELECT 1 FROM kitchen_tickets kt \
                     JOIN kitchen_ticket_items kti ON kti.kitchen_ticket_id = kt.id \
                     WHERE kt.open_ticket_id = $1 \
                       AND kti.voided_at IS NULL AND kti.bumped_at IS NULL \
                 ) AS done) \
             UPDATE open_tickets t SET \
                 ready_at   = CASE WHEN d.done THEN now() ELSE t.ready_at END, \
                 updated_at = now() \
             FROM d WHERE t.id = $1 AND t.status = 'open' RETURNING d.done",
        )
        .bind(ot)
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(false);
    }

    tx.commit().await?;

    // Publish after commit so subscribers read committed state. Replay skips this
    // (hub = None): the write is historical and consumers re-snapshot on reconnect.
    if let Some(hub) = hub {
        publish_kitchen(
            pool,
            hub,
            branch_id,
            if bumped {
                "kitchen.item_bumped"
            } else {
                "kitchen.item_unbumped"
            },
            ticket_id,
        )
        .await;
        if ticket_ready {
            publish_kitchen(pool, hub, branch_id, "kitchen.ticket_ready", ticket_id).await;
        }
        if let Some(ot) = open_ticket_id
            && open_ticket_ready
        {
            hub.publish(
                branch_id,
                BranchEvent::new(
                    Topic::Tickets,
                    "ticket.ready",
                    &serde_json::json!({ "open_ticket_id": ot }),
                ),
            );
        }
    }

    Ok(HttpResponse::NoContent().finish())
}

/// Live bump/unbump: permission + branch gate, then delegate to the shared core.
async fn set_bump(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    item_id: Uuid,
    bumped: bool,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "kitchen_orders", "update").await?;

    // Branch-gate against the line's ticket before mutating.
    let branch_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT kt.branch_id FROM kitchen_ticket_items kti \
         JOIN kitchen_tickets kt ON kt.id = kti.kitchen_ticket_id \
         WHERE kti.id = $1 AND kti.voided_at IS NULL",
    )
    .bind(item_id)
    .fetch_optional(pool.get_ref())
    .await?;
    let Some(branch_id) = branch_id else {
        return Err(AppError::NotFound("Kitchen line not found".into()));
    };
    require_branch_access(pool.get_ref(), &claims, branch_id).await?;

    let actor = ActingContext::live(&claims)?;
    set_bump_inner(pool.get_ref(), Some(hub.get_ref()), &actor, item_id, bumped).await
}

#[utoipa::path(post, path = "/kitchen/items/{item_id}/bump", tag = "kitchen",
    params(("item_id" = Uuid, Path, description = "Kitchen line ID")),
    responses((status = 204), AppErrorResponse), security(("bearer_jwt" = [])))]
pub async fn bump(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    item_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    set_bump(req, pool, hub, item_id.into_inner(), true).await
}

#[utoipa::path(post, path = "/kitchen/items/{item_id}/unbump", tag = "kitchen",
    params(("item_id" = Uuid, Path, description = "Kitchen line ID")),
    responses((status = 204), AppErrorResponse), security(("bearer_jwt" = [])))]
pub async fn unbump(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    item_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    set_bump(req, pool, hub, item_id.into_inner(), false).await
}
