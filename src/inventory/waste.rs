//! Waste recorded at a till (capability `inventory.waste.record`, id 49).
//!
//! A till picks an INGREDIENT (quantity in any unit of its family) or a MENU
//! ITEM (whole units, exploded through its recipe with the same resolver a
//! sale uses). Either way the stock moves as ordinary ledger rows —
//! `inventory_movements` of type `waste`, `source_type = 'waste'`,
//! `source_id` = the waste's id — and a `waste_events` header keeps what the
//! person picked, where it came from, and the manager approval it carried.
//!
//! The write is split live / inner like every POS mutation:
//! * `POST /inventory/waste` checks the capability FIRST, then the branch, then
//!   the `max_value` limit, then calls [`record_waste_inner`];
//! * `/sync/replay` op `record_waste` re-checks and ACCEPTS AND FLAGS (the
//!   food is already in the bin), then calls the same inner.
//!
//! Idempotent by the client-minted id: a second write of the same id posts
//! nothing and answers the first one.
//!
//! Stock may go below zero here: a till that was offline cannot have known the
//! book stock, and refusing the record would lose a fact, not prevent one.

use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::auth::jwt::Claims;
use crate::authz::Cap;
use crate::errors::{AppError, AppErrorResponse};
use crate::inventory::movements::{MovementParams, record_movement};

/// Reasons a person may pick. `order_cancelled` is the system's own (a voided
/// made order) and is not offered here.
pub const WASTE_REASONS: &[&str] = &[
    "expired",
    "spoiled",
    "damaged",
    "overproduction",
    "theft",
    "other",
];

/// One waste as a till (or the API) records it.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct RecordWasteRequest {
    /// Client-minted; the idempotency key.
    pub id: Uuid,
    pub branch_id: Uuid,
    /// `ingredient` | `menu_item`
    pub subject_kind: String,
    /// An org ingredient id, or a menu item id.
    pub subject_id: Uuid,
    /// Menu items only: the size whose recipe is wasted (default: the first size).
    #[serde(default)]
    pub size_label: Option<String>,
    /// In `unit`. Whole units for a menu item.
    pub quantity: f64,
    /// `g` | `kg` | `ml` | `l` | `pcs`. Default: the ingredient's own unit; a
    /// menu item is always `pcs`.
    #[serde(default)]
    pub unit: Option<String>,
    /// expired | spoiled | damaged | overproduction | theft | other
    pub reason: String,
    #[serde(default)]
    pub note: Option<String>,
    /// When it happened on the device. Default: now.
    #[serde(default)]
    pub occurred_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub device_id: Option<Uuid>,
    /// A manager's one-time PIN approval for the LIVE route (owner,
    /// 2026-09-17), over the person's `max_value` limit. Additive.
    #[serde(default)]
    pub live_approval: Option<crate::sync::handlers::ReplayApproval>,
    #[serde(default)]
    pub till_id: Option<Uuid>,
}

/// One ingredient line of a recorded waste.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, sqlx::FromRow)]
pub struct WasteLine {
    pub movement_id: Uuid,
    pub org_ingredient_id: Uuid,
    pub ingredient_name: String,
    pub unit: String,
    /// Signed ledger delta (negative), in the ingredient's unit.
    pub quantity: f64,
    /// Piastres per unit; `null` = unknown.
    pub unit_cost: Option<i64>,
    pub balance_after: f64,
    pub below_zero: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct WasteRecorded {
    pub id: Uuid,
    pub branch_id: Uuid,
    pub source: String,
    pub subject_kind: String,
    pub subject_name: String,
    pub size_label: Option<String>,
    pub quantity: f64,
    pub unit: String,
    pub reason: String,
    pub note: Option<String>,
    /// Piastres at the branch's unit costs; `null` when no line had a cost.
    pub value_minor: Option<i64>,
    pub value_partial: bool,
    pub recorded_by: Option<Uuid>,
    pub occurred_at: DateTime<Utc>,
    /// `false` when this id had already been recorded (nothing new was posted).
    pub created: bool,
    pub lines: Vec<WasteLine>,
}

/// What a waste will post, before anything is written.
#[derive(Debug, Clone)]
pub struct WastePlan {
    pub subject_name: String,
    pub unit: String,
    /// `(ingredient, qty in its base unit (positive), exact unit cost in piastres)`.
    /// The ledger row stores the cost rounded to whole piastres; the value uses
    /// the exact figure, or sub-piastre costs (a ml of milk) would count as 0.
    pub lines: Vec<(Uuid, f64, Option<f64>)>,
    pub value_minor: Option<i64>,
    pub value_partial: bool,
}

/// `Σ qty × unit cost`, rounded once; partial when a line has no cost.
/// The till computes the same figure (`madar-core` `waste::value_of`).
pub fn value_of(lines: &[(Uuid, f64, Option<f64>)]) -> (Option<i64>, bool) {
    let known: Vec<f64> = lines
        .iter()
        .filter_map(|(_, q, c)| c.map(|c| q * c))
        .collect();
    let partial = known.len() < lines.len();
    if known.is_empty() {
        (None, partial)
    } else {
        (Some(known.iter().sum::<f64>().round() as i64), partial)
    }
}

/// The branch's actual cost per unit, else the org standard cost (unrounded).
pub async fn exact_unit_cost(
    pool: &PgPool,
    branch_id: Uuid,
    ing: Uuid,
) -> Result<Option<f64>, AppError> {
    Ok(sqlx::query_scalar::<_, Option<f64>>(
        "SELECT COALESCE(bs.cost_per_unit, oi.cost_per_unit)::float8 \
         FROM org_ingredients oi \
         LEFT JOIN branch_stock bs ON bs.org_ingredient_id = oi.id AND bs.branch_id = $2 \
         WHERE oi.id = $1",
    )
    .bind(ing)
    .bind(branch_id)
    .fetch_optional(pool)
    .await?
    .flatten())
}

fn bad(msg: impl Into<String>) -> AppError {
    AppError::BadRequest(msg.into())
}

/// Validate a request and work out its ledger lines and value. Reads only.
pub async fn plan_waste(
    pool: &PgPool,
    org_id: Uuid,
    req: &RecordWasteRequest,
) -> Result<WastePlan, AppError> {
    if !WASTE_REASONS.contains(&req.reason.as_str()) {
        return Err(bad(format!(
            "reason must be one of: {}",
            WASTE_REASONS.join(", ")
        )));
    }
    if !req.quantity.is_finite() || req.quantity <= 0.0 || req.quantity > 1_000_000.0 {
        return Err(bad("quantity must be greater than 0"));
    }
    let branch_org = crate::inventory::handlers::branch_org(pool, req.branch_id).await?;
    if branch_org != org_id {
        return Err(AppError::Forbidden(
            "Branch belongs to another organization".into(),
        ));
    }
    let mut lines: Vec<(Uuid, f64, Option<f64>)> = Vec::new();
    let (subject_name, unit) = match req.subject_kind.as_str() {
        "ingredient" => {
            let row: Option<(String, String)> = sqlx::query_as(
                "SELECT name, unit::text FROM org_ingredients \
                 WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL",
            )
            .bind(req.subject_id)
            .bind(org_id)
            .fetch_optional(pool)
            .await?;
            let (name, base) =
                row.ok_or_else(|| bad("Ingredient not found in this organization's catalog"))?;
            let unit = req.unit.clone().unwrap_or_else(|| base.clone());
            let qty = crate::units::convert(req.quantity, &unit, &base)?;
            if qty <= 0.0 {
                return Err(bad("quantity must be greater than 0"));
            }
            let cost = exact_unit_cost(pool, req.branch_id, req.subject_id).await?;
            lines.push((req.subject_id, qty, cost));
            (name, unit)
        }
        "menu_item" => {
            if req.unit.as_deref().is_some_and(|u| u != "pcs") {
                return Err(bad("a menu item is wasted in whole units (pcs)"));
            }
            if req.quantity.fract() != 0.0 {
                return Err(bad("a menu item is wasted in whole units"));
            }
            let name: Option<String> = sqlx::query_scalar(
                "SELECT name FROM menu_items WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL",
            )
            .bind(req.subject_id)
            .bind(org_id)
            .fetch_optional(pool)
            .await?;
            let name = name.ok_or_else(|| bad("Menu item not found in this organization"))?;
            // The SAME resolver a sale deducts through, with no options chosen.
            let config = crate::orders::component_resolve::resolve_menu_item_configuration(
                pool,
                req.subject_id,
                req.size_label.clone(),
                req.quantity as i32,
                &[],
                &[],
                req.branch_id,
            )
            .await?;
            for d in config.deductions.into_iter().filter(|d| !d.undeducted) {
                let Some(ing) = d.org_ingredient_id else {
                    continue;
                };
                match lines.iter_mut().find(|(id, _, _)| *id == ing) {
                    Some(l) => l.1 += d.quantity,
                    None => {
                        let cost = exact_unit_cost(pool, req.branch_id, ing).await?;
                        lines.push((ing, d.quantity, cost));
                    }
                }
            }
            if lines.is_empty() {
                return Err(bad(
                    "This item has no recipe, so there is no stock to waste. Waste its ingredients instead.",
                ));
            }
            (name, "pcs".to_string())
        }
        _ => return Err(bad("subject_kind must be `ingredient` or `menu_item`")),
    };
    let (value_minor, value_partial) = value_of(&lines);
    Ok(WastePlan {
        subject_name,
        unit,
        lines,
        value_minor,
        value_partial,
    })
}

/// Read a recorded waste back.
pub async fn load_waste(pool: &PgPool, id: Uuid) -> Result<Option<WasteRecorded>, AppError> {
    #[allow(clippy::type_complexity)]
    let head: Option<(
        Uuid,
        String,
        String,
        String,
        Option<String>,
        f64,
        String,
        String,
        Option<String>,
        Option<i64>,
        bool,
        Option<Uuid>,
        DateTime<Utc>,
    )> = sqlx::query_as(
        "SELECT branch_id, source, subject_kind, subject_name, size_label, quantity::float8, unit, \
                reason, note, value_minor, value_partial, recorded_by, occurred_at \
           FROM waste_events WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    let Some((
        branch_id,
        source,
        subject_kind,
        subject_name,
        size_label,
        quantity,
        unit,
        reason,
        note,
        value_minor,
        value_partial,
        recorded_by,
        occurred_at,
    )) = head
    else {
        return Ok(None);
    };
    let lines: Vec<WasteLine> = sqlx::query_as(
        "SELECT m.id AS movement_id, m.org_ingredient_id, oi.name AS ingredient_name, \
                oi.unit::text AS unit, m.quantity::float8 AS quantity, m.unit_cost, \
                m.balance_after::float8 AS balance_after, m.below_zero \
           FROM inventory_movements m JOIN org_ingredients oi ON oi.id = m.org_ingredient_id \
          WHERE m.source_type = 'waste' AND m.source_id = $1 AND m.branch_id = $2 \
          ORDER BY oi.name, m.id",
    )
    .bind(id)
    .bind(branch_id)
    .fetch_all(pool)
    .await?;
    Ok(Some(WasteRecorded {
        id,
        branch_id,
        source,
        subject_kind,
        subject_name,
        size_label,
        quantity,
        unit,
        reason,
        note,
        value_minor,
        value_partial,
        recorded_by,
        occurred_at,
        created: false,
        lines,
    }))
}

/// Post a waste: the header and its ledger rows in one transaction.
/// Idempotent on `req.id`. The caller has already decided WHO may.
pub async fn record_waste_inner(
    pool: &PgPool,
    org_id: Uuid,
    actor: Uuid,
    source: &str,
    req: &RecordWasteRequest,
    approval_id: Option<Uuid>,
) -> Result<WasteRecorded, AppError> {
    if let Some(existing) = existing_in_org(pool, org_id, req).await? {
        return Ok(existing);
    }
    let plan = plan_waste(pool, org_id, req).await?;
    let note = req
        .note
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(500).collect::<String>());
    let occurred_at = req.occurred_at.unwrap_or_else(Utc::now);

    let mut tx = pool.begin().await?;
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO waste_events (id, org_id, branch_id, source, subject_kind, org_ingredient_id, \
             menu_item_id, subject_name, size_label, quantity, unit, reason, note, value_minor, \
             value_partial, recorded_by, device_id, till_id, approval_id, occurred_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::numeric(12,3), $11, $12, $13, $14, $15, $16, $17, $18, $19, $20) \
         ON CONFLICT (id) DO NOTHING RETURNING id",
    )
    .bind(req.id)
    .bind(org_id)
    .bind(req.branch_id)
    .bind(source)
    .bind(&req.subject_kind)
    .bind((req.subject_kind == "ingredient").then_some(req.subject_id))
    .bind((req.subject_kind == "menu_item").then_some(req.subject_id))
    .bind(&plan.subject_name)
    .bind(if req.subject_kind == "menu_item" { req.size_label.clone() } else { None })
    .bind(req.quantity)
    .bind(&plan.unit)
    .bind(&req.reason)
    .bind(&note)
    .bind(plan.value_minor)
    .bind(plan.value_partial)
    .bind(actor)
    .bind(req.device_id)
    .bind(req.till_id)
    .bind(approval_id)
    .bind(occurred_at)
    .fetch_optional(&mut *tx)
    .await?;
    if inserted.is_none() {
        // A concurrent write of the same id won the race.
        tx.rollback().await?;
        return existing_in_org(pool, org_id, req)
            .await?
            .ok_or_else(|| AppError::Conflict("waste id already used".into()));
    }
    for (ing, qty, cost) in &plan.lines {
        record_movement(
            &mut *tx,
            MovementParams {
                branch_id: req.branch_id,
                org_ingredient_id: *ing,
                movement_type: "waste",
                quantity: -*qty,
                unit_cost: cost.map(|c| c.round() as i64),
                reason: Some(req.reason.as_str()),
                source_type: Some("waste"),
                source_id: Some(req.id),
                note: note.as_deref(),
                created_by: Some(actor),
            },
        )
        .await?;
    }
    tx.commit().await?;
    let mut out = load_waste(pool, req.id)
        .await?
        .ok_or_else(|| AppError::NotFound("waste not found after commit".into()))?;
    out.created = true;
    Ok(out)
}

/// An already-recorded waste with this id, when it is this org's and branch's.
/// The same id somewhere else is a client bug, not a replay: 409.
async fn existing_in_org(
    pool: &PgPool,
    org_id: Uuid,
    req: &RecordWasteRequest,
) -> Result<Option<WasteRecorded>, AppError> {
    let owner: Option<(Uuid, Uuid)> =
        sqlx::query_as("SELECT org_id, branch_id FROM waste_events WHERE id = $1")
            .bind(req.id)
            .fetch_optional(pool)
            .await?;
    match owner {
        None => Ok(None),
        Some((o, b)) if o == org_id && b == req.branch_id => load_waste(pool, req.id).await,
        Some(_) => Err(AppError::Conflict("waste id already used".into())),
    }
}

/// The limit request for a waste of this value.
pub fn limit_request(value_minor: Option<i64>) -> madar_authz::Request {
    let mut r = madar_authz::Request::of(Cap::InventoryWasteRecord);
    r.value = Some(value_minor.unwrap_or(0));
    r
}

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

/// POST /inventory/waste — record waste at a branch (ingredient or menu item).
#[utoipa::path(
    post,
    path = "/inventory/waste",
    tag = "inventory",
    request_body = RecordWasteRequest,
    responses(
        (status = 201, description = "Waste recorded", body = WasteRecorded),
        (status = 200, description = "This id was already recorded; nothing new was posted", body = WasteRecorded),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn record_waste(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<serde_json::Value>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    // Permission FIRST, before the body is even read.
    crate::authz::require::require(pool.get_ref(), &claims, Cap::InventoryWasteRecord, None)
        .await?;
    let body: RecordWasteRequest = serde_json::from_value(body.into_inner())
        .map_err(|e| bad(format!("Json deserialize error: {e}")))?;
    crate::authz::require::require(
        pool.get_ref(),
        &claims,
        Cap::InventoryWasteRecord,
        Some(body.branch_id),
    )
    .await?;
    crate::authz::scope::require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;
    let org_id = claims
        .org_id()
        .ok_or_else(|| bad("Token has no organization"))?;

    let plan = plan_waste(pool.get_ref(), org_id, &body).await?;
    let eff =
        crate::authz::require::effective_for_claims(pool.get_ref(), &claims, Some(body.branch_id))
            .await?;
    let decision = madar_authz::decide(&eff, &limit_request(plan.value_minor));
    let allowed_outright = crate::sync::handlers::allow_or_approved_live(
        pool.get_ref(),
        decision,
        Cap::InventoryWasteRecord,
        body.live_approval.as_ref(),
        claims.user_id(),
        org_id,
        plan.value_minor,
        None,
    )
    .await?;

    let mut body = body;
    body.device_id = body
        .device_id
        .or_else(|| crate::devices::DeviceHeader::from_request_headers(&req));
    let source = if body.device_id.is_some() {
        "pos"
    } else {
        "dashboard"
    };
    let approval_id = if allowed_outright {
        None
    } else {
        let a = body.live_approval.clone().expect("checked above");
        crate::sync::handlers::record_approval(
            pool.get_ref(),
            &a,
            org_id,
            Some(body.branch_id),
            None,
            claims.user_id(),
            "record_waste_live",
            chrono::Utc::now(),
            &Ok(Cap::InventoryWasteRecord),
        )
        .await;
        Some(a.id)
    };
    let out = record_waste_inner(
        pool.get_ref(),
        org_id,
        claims.user_id(),
        source,
        &body,
        approval_id,
    )
    .await?;
    Ok(if out.created {
        HttpResponse::Created().json(out)
    } else {
        HttpResponse::Ok().json(out)
    })
}
