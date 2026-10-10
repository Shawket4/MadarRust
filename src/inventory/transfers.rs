//! Stock transfers between locations (WAREHOUSE_DESIGN.md §5–6).
//!
//! A transfer is a document with lines and a lifecycle:
//! `requested → draft → dispatched → received`, or `cancelled`. Which side may
//! take which action in which status, and the capability it needs, is ONE
//! table in madar-shared (`madar_inventory::transfer::step`); every action
//! here goes through [`authorize`], which reads it.
//!
//! Stock moves twice through the ledger: `transfer_out` at the source on
//! dispatch, `transfer_in` at the destination on receive (the quantity that
//! actually arrived). In transit is not a balance anywhere — it is the sent
//! quantity of `dispatched` transfers. The source's cost is frozen on each
//! line at dispatch and blended into the destination's WAC on receive.

use std::collections::{HashMap, HashSet};

use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use madar_inventory::api::{
    AcceptTransferRequest, BranchKind, CloseTransferRequest, CreateTransferRequest,
    ReceiveTransferRequest, ReplenishmentRow, StockTransfer, StockTransferLine,
    TransferDifferenceRow, TransferLineInput, TransferStamp, UpdateTransferRequest,
};
use madar_inventory::transfer::{self, Action, ReceiveRefusal, Side, Step, TransferStatus};
use madar_inventory::{milli, replenish};
use rust_decimal::Decimal;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use serde::Deserialize;
use sqlx::{PgConnection, PgPool};
use utoipa::IntoParams;
use uuid::Uuid;

use crate::authz::{Cap, require::require};
use crate::{
    auth::jwt::Claims,
    errors::{AppError, AppErrorResponse},
    inventory::movements::{MovementParams, lock_on_hand, record_movement},
};

/// Lines on one transfer. A warehouse run is tens of lines, never thousands.
const MAX_LINES: usize = 500;

// ── Reading ───────────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct HeaderRow {
    id: Uuid,
    org_id: Uuid,
    number: i32,
    status: String,
    source_branch_id: Uuid,
    source_branch_name: String,
    source_kind: String,
    destination_branch_id: Uuid,
    destination_branch_name: String,
    destination_kind: String,
    note: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
    created_by: Uuid,
    created_by_name: String,
    requested_at: Option<chrono::DateTime<chrono::Utc>>,
    requested_by: Option<Uuid>,
    requested_by_name: Option<String>,
    dispatched_at: Option<chrono::DateTime<chrono::Utc>>,
    dispatched_by: Option<Uuid>,
    dispatched_by_name: Option<String>,
    received_at: Option<chrono::DateTime<chrono::Utc>>,
    received_by: Option<Uuid>,
    received_by_name: Option<String>,
    cancelled_at: Option<chrono::DateTime<chrono::Utc>>,
    cancelled_by: Option<Uuid>,
    cancelled_by_name: Option<String>,
}

const HEADER_SELECT: &str = r#"
    SELECT t.id, t.org_id, t.number, t.status::text AS status,
           t.source_branch_id,      sb.name AS source_branch_name,      sb.kind::text AS source_kind,
           t.destination_branch_id, db.name AS destination_branch_name, db.kind::text AS destination_kind,
           t.note,
           t.initiated_at AS created_at, t.initiated_by AS created_by, cu.name AS created_by_name,
           t.requested_at,  t.requested_by,  ru.name AS requested_by_name,
           t.dispatched_at, t.dispatched_by, du.name AS dispatched_by_name,
           t.received_at,   t.received_by,   vu.name AS received_by_name,
           t.cancelled_at,  t.cancelled_by,  xu.name AS cancelled_by_name
    FROM stock_transfers t
    JOIN branches sb ON sb.id = t.source_branch_id
    JOIN branches db ON db.id = t.destination_branch_id
    JOIN users cu    ON cu.id = t.initiated_by
    LEFT JOIN users ru ON ru.id = t.requested_by
    LEFT JOIN users du ON du.id = t.dispatched_by
    LEFT JOIN users vu ON vu.id = t.received_by
    LEFT JOIN users xu ON xu.id = t.cancelled_by
"#;

#[derive(sqlx::FromRow)]
struct LineRow {
    id: Uuid,
    transfer_id: Uuid,
    org_ingredient_id: Uuid,
    ingredient_name: String,
    unit: String,
    qty_sent: f64,
    qty_received: Option<f64>,
    unit_cost: Option<f64>,
    note: Option<String>,
}

fn parse_status(s: &str) -> TransferStatus {
    match s {
        "requested" => TransferStatus::Requested,
        "draft" => TransferStatus::Draft,
        "dispatched" => TransferStatus::Dispatched,
        "received" => TransferStatus::Received,
        _ => TransferStatus::Cancelled,
    }
}

fn kind(s: &str) -> BranchKind {
    s.parse().unwrap_or_default()
}

fn stamp(
    at: Option<chrono::DateTime<chrono::Utc>>,
    by: Option<Uuid>,
    name: Option<String>,
) -> Option<TransferStamp> {
    Some(TransferStamp {
        at: at?,
        by: by?,
        by_name: name.unwrap_or_default(),
    })
}

/// Transfers with their lines, in the order the headers came.
async fn assemble(
    conn: &mut PgConnection,
    headers: Vec<HeaderRow>,
) -> Result<Vec<StockTransfer>, AppError> {
    let ids: Vec<Uuid> = headers.iter().map(|h| h.id).collect();
    let rows: Vec<LineRow> = sqlx::query_as(
        "SELECT l.id, l.transfer_id, l.org_ingredient_id, oi.name AS ingredient_name, \
                oi.unit::text AS unit, l.qty_sent::float8 AS qty_sent, \
                l.qty_received::float8 AS qty_received, l.unit_cost::float8 AS unit_cost, l.note \
         FROM stock_transfer_lines l \
         JOIN org_ingredients oi ON oi.id = l.org_ingredient_id \
         WHERE l.transfer_id = ANY($1) ORDER BY oi.name, l.id",
    )
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await?;
    let mut lines: HashMap<Uuid, Vec<StockTransferLine>> = HashMap::new();
    for l in rows {
        lines
            .entry(l.transfer_id)
            .or_default()
            .push(StockTransferLine {
                id: l.id,
                org_ingredient_id: l.org_ingredient_id,
                ingredient_name: l.ingredient_name,
                unit: l.unit,
                qty_sent: l.qty_sent,
                qty_received: l.qty_received,
                unit_cost: l.unit_cost,
                note: l.note,
            });
    }
    Ok(headers
        .into_iter()
        .map(|h| StockTransfer {
            lines: lines.remove(&h.id).unwrap_or_default(),
            id: h.id,
            org_id: h.org_id,
            reference: format!("TR-{}", h.number),
            status: parse_status(&h.status),
            source_branch_id: h.source_branch_id,
            source_branch_name: h.source_branch_name,
            source_kind: kind(&h.source_kind),
            destination_branch_id: h.destination_branch_id,
            destination_branch_name: h.destination_branch_name,
            destination_kind: kind(&h.destination_kind),
            note: h.note,
            created: TransferStamp {
                at: h.created_at,
                by: h.created_by,
                by_name: h.created_by_name,
            },
            requested: stamp(h.requested_at, h.requested_by, h.requested_by_name),
            dispatched: stamp(h.dispatched_at, h.dispatched_by, h.dispatched_by_name),
            received: stamp(h.received_at, h.received_by, h.received_by_name),
            cancelled: stamp(h.cancelled_at, h.cancelled_by, h.cancelled_by_name),
        })
        .collect())
}

async fn fetch(conn: &mut PgConnection, id: Uuid) -> Result<StockTransfer, AppError> {
    let h: Option<HeaderRow> = sqlx::query_as(&format!("{HEADER_SELECT} WHERE t.id = $1"))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    let h = h.ok_or_else(|| AppError::NotFound("Transfer not found".into()))?;
    Ok(assemble(conn, vec![h]).await?.remove(0))
}

// ── Authorizing ───────────────────────────────────────────────────────

/// The few header fields an action is decided on.
#[derive(sqlx::FromRow, Clone, Copy)]
struct Head {
    org_id: Uuid,
    #[sqlx(try_from = "String")]
    status: StatusText,
    source_branch_id: Uuid,
    destination_branch_id: Uuid,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct StatusText(TransferStatus);
impl From<String> for StatusText {
    fn from(s: String) -> Self {
        StatusText(parse_status(&s))
    }
}

const HEAD_SELECT: &str = "SELECT org_id, status::text AS status, source_branch_id, \
                           destination_branch_id FROM stock_transfers WHERE id = $1";

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

fn same_org(claims: &Claims, org_id: Uuid) -> Result<(), AppError> {
    if claims.role == crate::models::UserRole::SuperAdmin || claims.org_id() == Some(org_id) {
        Ok(())
    } else {
        Err(AppError::NotFound("Transfer not found".into()))
    }
}

/// "receive" + "ed" read "receiveed": each verb's own past form.
fn past(action: Action) -> String {
    let verb = format!("{action:?}").to_lowercase();
    if verb.ends_with('e') {
        format!("{verb}d")
    } else {
        format!("{verb}ed")
    }
}

/// "Every location" is every location the caller works at (None = the whole
/// org, for an org-wide reader): transfers carry costs.
async fn my_locations(pool: &PgPool, claims: &Claims) -> Result<Option<Vec<Uuid>>, AppError> {
    Ok(
        match crate::authz::scope::branch_scope(pool, claims).await? {
            crate::authz::scope::BranchScope::All => None,
            crate::authz::scope::BranchScope::Only(ids) => Some(ids),
        },
    )
}

fn not_open(status: TransferStatus, action: Action) -> AppError {
    AppError::Refused {
        code: "TRANSFER_STEP_NOT_OPEN",
        reason: format!("A {status:?} transfer can't be {}.", past(action)).to_lowercase(),
    }
}

/// Read the transfer, look the action up in the shared table, and require its
/// capability at the side's location (which also requires working there).
async fn authorize(
    pool: &PgPool,
    claims: &Claims,
    id: Uuid,
    action: Action,
) -> Result<(Head, Step), AppError> {
    // Refuse anyone who can't take this action anywhere before looking the
    // transfer up, so ids can't be probed. The exact check, at the side's
    // location, follows below.
    require(pool, claims, gate(action), None).await?;
    let head: Head = sqlx::query_as(HEAD_SELECT)
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| AppError::NotFound("Transfer not found".into()))?;
    same_org(claims, head.org_id)?;
    let step = transfer::step(head.status.0, action).ok_or(not_open(head.status.0, action))?;
    let at = match step.side {
        Side::Source => head.source_branch_id,
        Side::Destination => head.destination_branch_id,
    };
    require(pool, claims, step.cap, Some(at)).await?;
    Ok((head, step))
}

/// The least capability an action can need in any status.
fn gate(action: Action) -> Cap {
    match action {
        Action::Receive => Cap::InventoryTransfersEdit,
        _ => Cap::InventoryTransfersCreate,
    }
}

/// Inside the transaction: lock the header and refuse when someone moved it
/// on between [`authorize`] and here.
async fn lock(conn: &mut PgConnection, id: Uuid, was: TransferStatus) -> Result<(), AppError> {
    let head: Head = sqlx::query_as(&format!("{HEAD_SELECT} FOR UPDATE"))
        .bind(id)
        .fetch_one(conn)
        .await?;
    if head.status.0 != was {
        return Err(AppError::Refused {
            code: "TRANSFER_CHANGED",
            reason: "Someone else just changed this transfer. Reload it and try again.".into(),
        });
    }
    Ok(())
}

async fn set_status(
    conn: &mut PgConnection,
    id: Uuid,
    to: TransferStatus,
    user: Uuid,
) -> Result<(), AppError> {
    let col = match to {
        TransferStatus::Dispatched => "dispatched",
        TransferStatus::Received => "received",
        TransferStatus::Cancelled => "cancelled",
        TransferStatus::Draft | TransferStatus::Requested => {
            sqlx::query(
                "UPDATE stock_transfers SET status = $2::stock_transfer_status WHERE id = $1",
            )
            .bind(id)
            .bind(status_label(to))
            .execute(conn)
            .await?;
            return Ok(());
        }
    };
    sqlx::query(&format!(
        "UPDATE stock_transfers SET status = $2::stock_transfer_status, \
         {col}_at = now(), {col}_by = $3 WHERE id = $1"
    ))
    .bind(id)
    .bind(status_label(to))
    .bind(user)
    .execute(conn)
    .await?;
    Ok(())
}

fn status_label(s: TransferStatus) -> &'static str {
    match s {
        TransferStatus::Requested => "requested",
        TransferStatus::Draft => "draft",
        TransferStatus::Dispatched => "dispatched",
        TransferStatus::Received => "received",
        TransferStatus::Cancelled => "cancelled",
    }
}

/// Add a line to the transfer's note (a decline or cancel reason) without
/// losing what was there.
async fn append_note(
    conn: &mut PgConnection,
    id: Uuid,
    note: Option<&str>,
) -> Result<(), AppError> {
    let Some(note) = note.map(str::trim).filter(|n| !n.is_empty()) else {
        return Ok(());
    };
    sqlx::query(
        "UPDATE stock_transfers SET note = CASE WHEN note IS NULL OR note = '' THEN $2 \
         ELSE note || E'\\n' || $2 END WHERE id = $1",
    )
    .bind(id)
    .bind(note)
    .execute(conn)
    .await?;
    Ok(())
}

// ── Lines ─────────────────────────────────────────────────────────────

async fn validate_lines(
    conn: &mut PgConnection,
    org_id: Uuid,
    lines: &[TransferLineInput],
) -> Result<(), AppError> {
    if lines.is_empty() {
        return Err(AppError::BadRequest(
            "A transfer needs at least one item.".into(),
        ));
    }
    if lines.len() > MAX_LINES {
        return Err(AppError::BadRequest(format!(
            "A transfer can carry at most {MAX_LINES} items."
        )));
    }
    let mut seen = HashSet::new();
    for l in lines {
        if !l.quantity.is_finite() || milli(l.quantity) <= 0 {
            return Err(AppError::BadRequest(
                "Every quantity must be greater than 0.".into(),
            ));
        }
        if !seen.insert(l.org_ingredient_id) {
            return Err(AppError::BadRequest(
                "Each item can appear only once on a transfer.".into(),
            ));
        }
    }
    let ids: Vec<Uuid> = seen.into_iter().collect();
    let found: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM org_ingredients \
         WHERE id = ANY($1) AND org_id = $2 AND deleted_at IS NULL",
    )
    .bind(&ids)
    .bind(org_id)
    .fetch_one(conn)
    .await?;
    if found as usize != ids.len() {
        return Err(AppError::BadRequest(
            "An item is not in this organization's catalog.".into(),
        ));
    }
    Ok(())
}

async fn replace_lines(
    conn: &mut PgConnection,
    id: Uuid,
    lines: &[TransferLineInput],
) -> Result<(), AppError> {
    sqlx::query("DELETE FROM stock_transfer_lines WHERE transfer_id = $1")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    let ing: Vec<Uuid> = lines.iter().map(|l| l.org_ingredient_id).collect();
    let qty: Vec<f64> = lines.iter().map(|l| l.quantity).collect();
    let note: Vec<Option<String>> = lines
        .iter()
        .map(|l| {
            l.note
                .as_deref()
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(String::from)
        })
        .collect();
    sqlx::query(
        "INSERT INTO stock_transfer_lines (transfer_id, org_ingredient_id, qty_sent, note) \
         SELECT $1, i, round(q::numeric, 3), n FROM unnest($2::uuid[], $3::float8[], $4::text[]) AS u(i, q, n)",
    )
    .bind(id)
    .bind(&ing)
    .bind(&qty)
    .bind(&note)
    .execute(conn)
    .await?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct MoveLine {
    id: Uuid,
    org_ingredient_id: Uuid,
    ingredient_name: String,
    qty_sent: f64,
    unit_cost: Option<Decimal>,
}

/// Lines in ingredient order, so two dispatches lock balances in the same
/// order and never deadlock each other.
async fn move_lines(conn: &mut PgConnection, id: Uuid) -> Result<Vec<MoveLine>, AppError> {
    Ok(sqlx::query_as(
        "SELECT l.id, l.org_ingredient_id, oi.name AS ingredient_name, \
                l.qty_sent::float8 AS qty_sent, l.unit_cost \
         FROM stock_transfer_lines l JOIN org_ingredients oi ON oi.id = l.org_ingredient_id \
         WHERE l.transfer_id = $1 ORDER BY l.org_ingredient_id",
    )
    .bind(id)
    .fetch_all(conn)
    .await?)
}

fn whole_piastres(c: Decimal) -> i64 {
    c.round().to_i64().unwrap_or(0)
}

fn dec(q: f64) -> Decimal {
    Decimal::from_f64(q).unwrap_or(Decimal::ZERO).round_dp(3)
}

/// Stock lands at `branch` at a known cost: blend it into the branch's WAC
/// first (WAC reads the prior on-hand), then post the movement.
#[allow(clippy::too_many_arguments)]
async fn land(
    conn: &mut PgConnection,
    branch: Uuid,
    ingredient: Uuid,
    qty: f64,
    cost: Option<Decimal>,
    transfer_id: Uuid,
    note: &str,
    user: Uuid,
) -> Result<(), AppError> {
    if let Some(c) = cost {
        crate::costing::service::apply_weighted_average_cost(
            &mut *conn,
            branch,
            ingredient,
            dec(qty),
            c,
            user,
        )
        .await?;
    }
    record_movement(
        &mut *conn,
        MovementParams {
            branch_id: branch,
            org_ingredient_id: ingredient,
            movement_type: "transfer_in",
            quantity: qty,
            unit_cost: cost.map(|c| c.round_dp(crate::inventory::movements::COST_DP)),
            reason: None,
            source_type: Some("transfer"),
            source_id: Some(transfer_id),
            note: Some(note),
            created_by: Some(user),
        },
    )
    .await?;
    Ok(())
}

// ── Handlers ──────────────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/inventory/transfers",
    tag = "inventory",
    request_body = CreateTransferRequest,
    responses((status = 201, description = "Transfer created (requested or draft)", body = StockTransfer), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_transfer(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CreateTransferRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let b = body.into_inner();
    if b.source_branch_id == b.destination_branch_id {
        return Err(AppError::BadRequest("Pick two different locations.".into()));
    }
    // A request is made by the side that will receive; a draft by the sender.
    let at = if b.request {
        b.destination_branch_id
    } else {
        b.source_branch_id
    };
    require(
        pool.get_ref(),
        &claims,
        Cap::InventoryTransfersCreate,
        Some(at),
    )
    .await?;

    let orgs: Vec<Uuid> =
        sqlx::query_scalar("SELECT org_id FROM branches WHERE id = ANY($1) AND deleted_at IS NULL")
            .bind([b.source_branch_id, b.destination_branch_id])
            .fetch_all(pool.get_ref())
            .await?;
    if orgs.len() != 2 || orgs[0] != orgs[1] {
        return Err(AppError::BadRequest(
            "Both locations must exist and belong to the same organization.".into(),
        ));
    }
    let org_id = orgs[0];
    same_org(&claims, org_id)?;

    let mut tx = pool.get_ref().begin().await?;
    validate_lines(&mut tx, org_id, &b.lines).await?;
    // Per-org running number: serialize creators of one org.
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('stock_transfers:' || $1::text, 0))",
    )
    .bind(org_id)
    .execute(&mut *tx)
    .await?;
    let note = b.note.as_deref().map(str::trim).filter(|n| !n.is_empty());
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO stock_transfers \
             (org_id, source_branch_id, destination_branch_id, note, initiated_by, status, number, \
              requested_at, requested_by) \
         VALUES ($1, $2, $3, $4, $5, $6::stock_transfer_status, \
                 (SELECT COALESCE(MAX(number), 0) + 1 FROM stock_transfers WHERE org_id = $1), \
                 CASE WHEN $7 THEN now() END, CASE WHEN $7 THEN $5 END) \
         RETURNING id",
    )
    .bind(org_id)
    .bind(b.source_branch_id)
    .bind(b.destination_branch_id)
    .bind(note)
    .bind(claims.user_id())
    .bind(if b.request { "requested" } else { "draft" })
    .bind(b.request)
    .fetch_one(&mut *tx)
    .await?;
    replace_lines(&mut tx, id, &b.lines).await?;
    let out = fetch(&mut tx, id).await?;
    tx.commit().await?;
    Ok(HttpResponse::Created().json(out))
}

#[utoipa::path(
    get,
    path = "/inventory/transfers/{id}",
    tag = "inventory",
    params(("id" = Uuid, Path, description = "Transfer ID")),
    responses((status = 200, description = "The transfer with its lines", body = StockTransfer), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_transfer(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(pool.get_ref(), &claims, Cap::InventoryTransfersRead, None).await?;
    let head: Head = sqlx::query_as(HEAD_SELECT)
        .bind(*id)
        .fetch_optional(pool.get_ref())
        .await?
        .ok_or_else(|| AppError::NotFound("Transfer not found".into()))?;
    same_org(&claims, head.org_id)?;
    let p = pool.get_ref();
    let cap = Cap::InventoryTransfersRead;
    if !(crate::authz::require::can(p, &claims, cap, Some(head.source_branch_id)).await?
        || crate::authz::require::can(p, &claims, cap, Some(head.destination_branch_id)).await?)
    {
        return Err(crate::authz::require::denied(cap));
    }
    let mut conn = p.acquire().await?;
    Ok(HttpResponse::Ok().json(fetch(&mut conn, *id).await?))
}

#[utoipa::path(
    patch,
    path = "/inventory/transfers/{id}",
    tag = "inventory",
    params(("id" = Uuid, Path, description = "Transfer ID")),
    request_body = UpdateTransferRequest,
    responses((status = 200, description = "Request or draft updated", body = StockTransfer), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_transfer(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<UpdateTransferRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let (head, _) = authorize(pool.get_ref(), &claims, *id, Action::Edit).await?;
    let mut tx = pool.get_ref().begin().await?;
    lock(&mut tx, *id, head.status.0).await?;
    if let Some(lines) = &body.lines {
        validate_lines(&mut tx, head.org_id, lines).await?;
        replace_lines(&mut tx, *id, lines).await?;
    }
    if let Some(note) = &body.note {
        sqlx::query("UPDATE stock_transfers SET note = NULLIF(btrim($2), '') WHERE id = $1")
            .bind(*id)
            .bind(note)
            .execute(&mut *tx)
            .await?;
    }
    let out = fetch(&mut tx, *id).await?;
    tx.commit().await?;
    Ok(HttpResponse::Ok().json(out))
}

#[utoipa::path(
    post,
    path = "/inventory/transfers/{id}/accept",
    tag = "inventory",
    params(("id" = Uuid, Path, description = "Transfer ID")),
    request_body = AcceptTransferRequest,
    responses((status = 200, description = "Request accepted; now the source's draft", body = StockTransfer), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn accept_transfer(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<AcceptTransferRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let (head, step) = authorize(pool.get_ref(), &claims, *id, Action::Accept).await?;
    let mut tx = pool.get_ref().begin().await?;
    lock(&mut tx, *id, head.status.0).await?;
    if let Some(lines) = &body.lines {
        validate_lines(&mut tx, head.org_id, lines).await?;
        replace_lines(&mut tx, *id, lines).await?;
    }
    set_status(&mut tx, *id, step.next, claims.user_id()).await?;
    let out = fetch(&mut tx, *id).await?;
    tx.commit().await?;
    Ok(HttpResponse::Ok().json(out))
}

#[utoipa::path(
    post,
    path = "/inventory/transfers/{id}/decline",
    tag = "inventory",
    params(("id" = Uuid, Path, description = "Transfer ID")),
    request_body = CloseTransferRequest,
    responses((status = 200, description = "Request declined (note required)", body = StockTransfer), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn decline_transfer(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<CloseTransferRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(pool.get_ref(), &claims, gate(Action::Decline), None).await?;
    let note = body
        .note
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty());
    let Some(note) = note else {
        return Err(AppError::BadRequest(
            "Say why you're declining the request.".into(),
        ));
    };
    let (head, step) = authorize(pool.get_ref(), &claims, *id, Action::Decline).await?;
    let mut tx = pool.get_ref().begin().await?;
    lock(&mut tx, *id, head.status.0).await?;
    append_note(&mut tx, *id, Some(&format!("Declined: {note}"))).await?;
    set_status(&mut tx, *id, step.next, claims.user_id()).await?;
    let out = fetch(&mut tx, *id).await?;
    tx.commit().await?;
    Ok(HttpResponse::Ok().json(out))
}

#[utoipa::path(
    post,
    path = "/inventory/transfers/{id}/dispatch",
    tag = "inventory",
    params(("id" = Uuid, Path, description = "Transfer ID")),
    responses((status = 200, description = "Dispatched: stock left the source and is in transit", body = StockTransfer), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn dispatch_transfer(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let (head, step) = authorize(pool.get_ref(), &claims, *id, Action::Dispatch).await?;
    let user = claims.user_id();
    let mut tx = pool.get_ref().begin().await?;
    lock(&mut tx, *id, head.status.0).await?;

    let lines = move_lines(&mut tx, *id).await?;
    let mut short = Vec::new();
    for l in &lines {
        // Lock and validate atomically: no sale or other transfer slips in.
        let on_hand = lock_on_hand(&mut *tx, head.source_branch_id, l.org_ingredient_id)
            .await?
            .unwrap_or(0.0);
        if milli(on_hand) < milli(l.qty_sent) {
            short.push(format!("{} ({on_hand:.3} on hand)", l.ingredient_name));
        }
    }
    if !short.is_empty() {
        return Err(AppError::Refused {
            code: "TRANSFER_INSUFFICIENT_STOCK",
            reason: format!("Not enough stock to send: {}.", short.join(", ")),
        });
    }
    for l in &lines {
        // Cost travels with the goods: the source's actual cost, org default
        // as fallback, frozen now so a cost change in transit doesn't revalue it.
        let cost: Option<Decimal> = sqlx::query_scalar(
            "SELECT COALESCE(bs.cost_per_unit, oi.cost_per_unit) FROM org_ingredients oi \
             LEFT JOIN branch_stock bs ON bs.org_ingredient_id = oi.id AND bs.branch_id = $2 \
             WHERE oi.id = $1",
        )
        .bind(l.org_ingredient_id)
        .bind(head.source_branch_id)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("UPDATE stock_transfer_lines SET unit_cost = $2 WHERE id = $1")
            .bind(l.id)
            .bind(cost)
            .execute(&mut *tx)
            .await?;
        record_movement(
            &mut *tx,
            MovementParams {
                branch_id: head.source_branch_id,
                org_ingredient_id: l.org_ingredient_id,
                movement_type: "transfer_out",
                quantity: -l.qty_sent,
                unit_cost: cost.map(|c| c.round_dp(crate::inventory::movements::COST_DP)),
                reason: None,
                source_type: Some("transfer"),
                source_id: Some(*id),
                note: Some("Transfer out"),
                created_by: Some(user),
            },
        )
        .await?;
    }
    set_status(&mut tx, *id, step.next, user).await?;
    let out = fetch(&mut tx, *id).await?;
    tx.commit().await?;
    Ok(HttpResponse::Ok().json(out))
}

#[utoipa::path(
    post,
    path = "/inventory/transfers/{id}/receive",
    tag = "inventory",
    params(("id" = Uuid, Path, description = "Transfer ID")),
    request_body = ReceiveTransferRequest,
    responses((status = 200, description = "Received: what arrived is on hand at the destination", body = StockTransfer), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn receive_transfer(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<ReceiveTransferRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let (head, step) = authorize(pool.get_ref(), &claims, *id, Action::Receive).await?;
    let user = claims.user_id();
    let mut tx = pool.get_ref().begin().await?;
    lock(&mut tx, *id, head.status.0).await?;

    let lines = move_lines(&mut tx, *id).await?;
    let given: HashMap<Uuid, &madar_inventory::api::ReceiveTransferLine> =
        body.lines.iter().map(|l| (l.line_id, l)).collect();
    if given.len() != body.lines.len()
        || given.len() != lines.len()
        || lines.iter().any(|l| !given.contains_key(&l.id))
    {
        return Err(AppError::BadRequest(
            "Say what arrived for every line on the transfer, once each.".into(),
        ));
    }
    for l in &lines {
        let g = given[&l.id];
        let note = g.note.as_deref().map(str::trim).filter(|n| !n.is_empty());
        if !g.qty_received.is_finite() {
            return Err(AppError::BadRequest(
                "qty_received must be a number.".into(),
            ));
        }
        match transfer::check_receive_line(l.qty_sent, g.qty_received, note) {
            Ok(_) => {}
            Err(ReceiveRefusal::Negative) => {
                return Err(AppError::BadRequest(
                    "A received quantity can't be negative.".into(),
                ));
            }
            Err(ReceiveRefusal::OverNeedsNote) => {
                return Err(AppError::Coded {
                    // A conflict with what was sent, as the inventory doc states.
                    status: 409,
                    code: "OVER_RECEIVE_NEEDS_NOTE",
                    reason: format!(
                        "More {} arrived than was sent. Add a note saying why.",
                        l.ingredient_name
                    ),
                });
            }
        }
        sqlx::query(
            "UPDATE stock_transfer_lines SET qty_received = round($2::numeric, 3), \
             note = CASE WHEN $3::text IS NULL THEN note WHEN note IS NULL THEN $3 \
             ELSE note || E'\\n' || $3 END WHERE id = $1",
        )
        .bind(l.id)
        .bind(g.qty_received)
        .bind(note)
        .execute(&mut *tx)
        .await?;
        if milli(g.qty_received) > 0 {
            land(
                &mut tx,
                head.destination_branch_id,
                l.org_ingredient_id,
                g.qty_received,
                l.unit_cost,
                *id,
                "Transfer in",
                user,
            )
            .await?;
        }
    }
    append_note(&mut tx, *id, body.note.as_deref()).await?;
    set_status(&mut tx, *id, step.next, user).await?;
    let out = fetch(&mut tx, *id).await?;
    tx.commit().await?;
    Ok(HttpResponse::Ok().json(out))
}

#[utoipa::path(
    post,
    path = "/inventory/transfers/{id}/cancel",
    tag = "inventory",
    operation_id = "cancel_stock_transfer",
    params(("id" = Uuid, Path, description = "Transfer ID")),
    request_body = CloseTransferRequest,
    responses((status = 200, description = "Cancelled; a dispatched transfer's stock is back at the source", body = StockTransfer), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn cancel_transfer(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<CloseTransferRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let (head, step) = authorize(pool.get_ref(), &claims, *id, Action::Cancel).await?;
    let user = claims.user_id();
    let mut tx = pool.get_ref().begin().await?;
    lock(&mut tx, *id, head.status.0).await?;
    if head.status.0 == TransferStatus::Dispatched {
        for l in move_lines(&mut tx, *id).await? {
            land(
                &mut tx,
                head.source_branch_id,
                l.org_ingredient_id,
                l.qty_sent,
                l.unit_cost,
                *id,
                "Transfer cancelled",
                user,
            )
            .await?;
        }
    }
    append_note(&mut tx, *id, body.note.as_deref()).await?;
    set_status(&mut tx, *id, step.next, user).await?;
    let out = fetch(&mut tx, *id).await?;
    tx.commit().await?;
    Ok(HttpResponse::Ok().json(out))
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListTransfersQuery {
    /// `incoming` | `outgoing`; omitted = both.
    pub direction: Option<String>,
    /// `requested` | `draft` | `dispatched` | `received` | `cancelled`; omitted = all.
    pub status: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[utoipa::path(
    get,
    path = "/inventory/branches/{branch_id}/transfers",
    tag = "inventory",
    params(("branch_id" = Uuid, Path, description = "Branch or warehouse ID; the nil UUID = every location in the org"), ListTransfersQuery),
    responses((status = 200, description = "Transfers with their lines, newest first", body = Vec<StockTransfer>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_transfers(
    req: HttpRequest,
    pool: crate::db::Db,
    branch_id: web::Path<Uuid>,
    query: web::Query<ListTransfersQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let all = branch_id.is_nil();
    let scope: Uuid = if all {
        require(pool.get_ref(), &claims, Cap::InventoryTransfersRead, None).await?;
        claims
            .scope_org(crate::auth::middleware::header_org_id(&req))
            .ok_or_else(|| AppError::Forbidden("No organization in scope".into()))?
    } else {
        require(
            pool.get_ref(),
            &claims,
            Cap::InventoryTransfersRead,
            Some(*branch_id),
        )
        .await?;
        *branch_id
    };
    let status = match query.status.as_deref() {
        None | Some("") => None,
        Some(s @ ("requested" | "draft" | "dispatched" | "received" | "cancelled")) => Some(s),
        Some(_) => return Err(AppError::BadRequest("Unknown transfer status.".into())),
    };
    let mine = match all {
        true => my_locations(pool.get_ref(), &claims).await?,
        false => None,
    };
    let side = |col: &str| {
        if all {
            format!(
                "t.org_id = $1 AND {col} IS NOT NULL AND ($5::uuid[] IS NULL OR {col} = ANY($5))"
            )
        } else {
            format!("{col} = $1")
        }
    };
    let cond = match query.direction.as_deref() {
        Some("incoming") => side("t.destination_branch_id"),
        Some("outgoing") => side("t.source_branch_id"),
        _ if all => "t.org_id = $1 AND ($5::uuid[] IS NULL \
                     OR t.source_branch_id = ANY($5) OR t.destination_branch_id = ANY($5))"
            .to_string(),
        _ => "(t.source_branch_id = $1 OR t.destination_branch_id = $1)".to_string(),
    };
    let (limit, offset) = (
        query.limit.unwrap_or(100).clamp(1, 500),
        query.offset.unwrap_or(0).max(0),
    );
    let sql = format!(
        "{HEADER_SELECT} WHERE {cond} AND ($2::text IS NULL OR t.status::text = $2) \
         ORDER BY t.initiated_at DESC, t.number DESC LIMIT $3 OFFSET $4"
    );
    let mut conn = pool.get_ref().acquire().await?;
    let mut list = sqlx::query_as::<_, HeaderRow>(&sql)
        .bind(scope)
        .bind(status)
        .bind(limit)
        .bind(offset);
    if all {
        list = list.bind(mine);
    }
    let headers = list.fetch_all(&mut *conn).await?;
    Ok(HttpResponse::Ok().json(assemble(&mut conn, headers).await?))
}

// ── Replenishment ─────────────────────────────────────────────────────

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ReplenishmentQuery {
    /// The branch to fill from this warehouse.
    pub branch_id: Uuid,
}

#[derive(sqlx::FromRow)]
struct ReplenishSql {
    org_ingredient_id: Uuid,
    ingredient_name: String,
    unit: String,
    category_name: String,
    on_hand: f64,
    par_min: f64,
    par_max: Option<f64>,
    in_transit: f64,
    open_inbound: f64,
    warehouse_on_hand: f64,
    warehouse_drafted_out: f64,
}

#[utoipa::path(
    get,
    path = "/inventory/warehouses/{warehouse_id}/replenishment",
    tag = "inventory",
    params(("warehouse_id" = Uuid, Path, description = "Warehouse ID"), ReplenishmentQuery),
    responses((status = 200, description = "What the branch is low on and what the warehouse can send", body = Vec<ReplenishmentRow>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn replenishment(
    req: HttpRequest,
    pool: crate::db::Db,
    warehouse_id: web::Path<Uuid>,
    query: web::Query<ReplenishmentQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let (wh, branch) = (*warehouse_id, query.branch_id);
    require(
        pool.get_ref(),
        &claims,
        Cap::InventoryTransfersRead,
        Some(wh),
    )
    .await?;
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT org_id, kind::text FROM branches WHERE id = ANY($1) AND deleted_at IS NULL \
         ORDER BY (id = $2) DESC",
    )
    .bind([wh, branch])
    .bind(wh)
    .fetch_all(pool.get_ref())
    .await?;
    if rows.len() != 2 || rows[0].0 != rows[1].0 || wh == branch {
        return Err(AppError::NotFound("Warehouse or branch not found".into()));
    }
    if rows[0].1 != "warehouse" {
        return Err(AppError::BadRequest(
            "Replenishment comes from a warehouse.".into(),
        ));
    }
    same_org(&claims, rows[0].0)?;

    let found: Vec<ReplenishSql> = sqlx::query_as(
        r#"
        WITH lines AS (
            SELECT t.source_branch_id, t.destination_branch_id, t.status, l.org_ingredient_id, l.qty_sent
            FROM stock_transfers t JOIN stock_transfer_lines l ON l.transfer_id = t.id
            WHERE t.status IN ('requested', 'draft', 'dispatched')
              AND (t.source_branch_id = $1 OR t.destination_branch_id = $2)
        )
        SELECT oi.id AS org_ingredient_id, oi.name AS ingredient_name, oi.unit::text AS unit,
               c.name AS category_name,
               bs.on_hand::float8 AS on_hand, bs.par_min::float8 AS par_min, bs.par_max::float8 AS par_max,
               COALESCE((SELECT SUM(qty_sent) FROM lines WHERE destination_branch_id = $2
                          AND org_ingredient_id = oi.id AND status = 'dispatched'), 0)::float8 AS in_transit,
               COALESCE((SELECT SUM(qty_sent) FROM lines WHERE destination_branch_id = $2
                          AND org_ingredient_id = oi.id AND status IN ('requested', 'draft')), 0)::float8 AS open_inbound,
               COALESCE(ws.on_hand, 0)::float8 AS warehouse_on_hand,
               COALESCE((SELECT SUM(qty_sent) FROM lines WHERE source_branch_id = $1
                          AND org_ingredient_id = oi.id AND status = 'draft'), 0)::float8 AS warehouse_drafted_out
        FROM branch_stock bs
        JOIN org_ingredients oi       ON oi.id = bs.org_ingredient_id AND oi.deleted_at IS NULL
        JOIN ingredient_categories c  ON c.id = oi.category_id
        LEFT JOIN branch_stock ws     ON ws.branch_id = $1 AND ws.org_ingredient_id = oi.id
        WHERE bs.branch_id = $2 AND bs.par_min > 0 AND bs.on_hand <= bs.par_min
        ORDER BY c.name, oi.name
        "#,
    )
    .bind(wh)
    .bind(branch)
    .fetch_all(pool.get_ref())
    .await?;

    let out: Vec<ReplenishmentRow> = found
        .into_iter()
        .map(|r| {
            let s = replenish::suggest(&replenish::Input {
                on_hand: r.on_hand,
                par_min: r.par_min,
                par_max: r.par_max,
                in_transit: r.in_transit,
                open_inbound: r.open_inbound,
                warehouse_on_hand: r.warehouse_on_hand,
                warehouse_drafted_out: r.warehouse_drafted_out,
            });
            ReplenishmentRow {
                org_ingredient_id: r.org_ingredient_id,
                ingredient_name: r.ingredient_name,
                unit: r.unit,
                category_name: r.category_name,
                on_hand: r.on_hand,
                par_min: r.par_min,
                par_max: r.par_max,
                in_transit: r.in_transit,
                open_inbound: r.open_inbound,
                warehouse_on_hand: r.warehouse_on_hand,
                need: s.need,
                available: s.available,
                suggested: s.suggested,
            }
        })
        .collect();
    Ok(HttpResponse::Ok().json(out))
}

// ── Differences report ────────────────────────────────────────────────

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DifferencesQuery {
    /// Received on or after (inclusive).
    pub from: Option<chrono::DateTime<chrono::Utc>>,
    /// Received before (exclusive).
    pub to: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(sqlx::FromRow)]
struct DifferenceSql {
    transfer_id: Uuid,
    number: i32,
    received_at: chrono::DateTime<chrono::Utc>,
    source_branch_name: String,
    destination_branch_name: String,
    org_ingredient_id: Uuid,
    ingredient_name: String,
    unit: String,
    qty_sent: f64,
    qty_received: f64,
    unit_cost: Option<Decimal>,
    note: Option<String>,
}

#[utoipa::path(
    get,
    path = "/inventory/orgs/{org_id}/transfer-differences",
    tag = "inventory",
    params(("org_id" = Uuid, Path, description = "Organization ID"), DifferencesQuery),
    responses((status = 200, description = "Received lines that differ from what was sent", body = Vec<TransferDifferenceRow>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn transfer_differences(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
    query: web::Query<DifferencesQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    same_org(&claims, *org_id)?;
    require(pool.get_ref(), &claims, Cap::InventoryTransfersRead, None).await?;
    let rows: Vec<DifferenceSql> = sqlx::query_as(
        "SELECT t.id AS transfer_id, t.number, t.received_at, \
                sb.name AS source_branch_name, db.name AS destination_branch_name, \
                l.org_ingredient_id, oi.name AS ingredient_name, oi.unit::text AS unit, \
                l.qty_sent::float8 AS qty_sent, l.qty_received::float8 AS qty_received, \
                l.unit_cost, l.note \
         FROM stock_transfers t \
         JOIN stock_transfer_lines l ON l.transfer_id = t.id \
         JOIN branches sb ON sb.id = t.source_branch_id \
         JOIN branches db ON db.id = t.destination_branch_id \
         JOIN org_ingredients oi ON oi.id = l.org_ingredient_id \
         WHERE t.org_id = $1 AND t.status = 'received' AND l.qty_received <> l.qty_sent \
           AND ($2::timestamptz IS NULL OR t.received_at >= $2) \
           AND ($3::timestamptz IS NULL OR t.received_at < $3) \
           AND ($4::uuid[] IS NULL OR t.source_branch_id = ANY($4) OR t.destination_branch_id = ANY($4)) \
         ORDER BY t.received_at DESC, oi.name LIMIT 2000",
    )
    .bind(*org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(my_locations(pool.get_ref(), &claims).await?)
    .fetch_all(pool.get_ref())
    .await?;
    let out: Vec<TransferDifferenceRow> = rows
        .into_iter()
        .map(|r| {
            let difference = madar_inventory::from_milli(milli(r.qty_received) - milli(r.qty_sent));
            TransferDifferenceRow {
                transfer_id: r.transfer_id,
                reference: format!("TR-{}", r.number),
                received_at: r.received_at,
                source_branch_name: r.source_branch_name,
                destination_branch_name: r.destination_branch_name,
                org_ingredient_id: r.org_ingredient_id,
                ingredient_name: r.ingredient_name,
                unit: r.unit,
                qty_sent: r.qty_sent,
                qty_received: r.qty_received,
                difference,
                unit_cost: r.unit_cost.and_then(|c| c.to_f64()),
                value_difference: r.unit_cost.map(|c| whole_piastres(dec(difference) * c)),
                note: r.note,
            }
        })
        .collect();
    Ok(HttpResponse::Ok().json(out))
}

/// A location a transfer can go to or come from.
#[derive(Debug, serde::Serialize, utoipa::ToSchema, sqlx::FromRow)]
pub struct TransferLocation {
    pub id: Uuid,
    pub name: String,
    /// `branch` | `warehouse`
    pub kind: String,
}

/// Every live location of the org, for the other side of a transfer. Someone
/// who works at one shop still sends to, and requests from, the rest; `GET
/// /branches` lists only where the caller works.
#[utoipa::path(
    get,
    path = "/inventory/orgs/{org_id}/transfer-locations",
    tag = "inventory",
    params(("org_id" = Uuid, Path, description = "Organization ID")),
    responses((status = 200, body = Vec<TransferLocation>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn transfer_locations(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    same_org(&claims, *org_id)?;
    require(pool.get_ref(), &claims, Cap::InventoryTransfersCreate, None).await?;
    let rows: Vec<TransferLocation> = sqlx::query_as(
        "SELECT id, name, kind::text AS kind FROM branches \
          WHERE org_id = $1 AND is_active AND deleted_at IS NULL ORDER BY kind, name",
    )
    .bind(*org_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}
