//! Stock counts — the front door of inventory.
//!
//! A count snapshots EVERY live catalog ingredient for the branch (or a
//! category / explicit subset), whether or not the branch has ever moved it:
//! an ingredient with no `branch_stock` row is simply "0 on hand, new here".
//! Counting is therefore also how a new branch is onboarded — there is no
//! per-branch setup step.
//!
//! Variance is measured against BOOK stock (the live balance at finalize),
//! not the open-time snapshot, so sales during a long count are not reported
//! as shrinkage. Finalize posts one `stock_count` movement per difference; the
//! ledger trigger moves the balance.

use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    auth::jwt::Claims,
    errors::{AppError, AppErrorResponse},
    inventory::movements::{MovementParams, record_movement},
    models::UserRole,
    permissions::checker::check_permission,
};
use utoipa::ToSchema;

// ── Response models ───────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct Stocktake {
    pub id: Uuid,
    pub org_id: Uuid,
    pub branch_id: Uuid,
    pub status: String,
    pub note: Option<String>,
    /// `{"kind":"full"}`, `{"kind":"category","category_id":…}` or
    /// `{"kind":"items","org_ingredient_ids":[…]}`.
    #[schema(value_type = Object)]
    pub scope: serde_json::Value,
    pub started_by: Uuid,
    pub started_by_name: Option<String>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finalized_by: Option<Uuid>,
    pub finalized_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Branch label — only populated by the stocktakes list (so the "All
    /// branches" view can show which branch each stocktake belongs to).
    #[serde(default)]
    #[sqlx(default)]
    pub branch_name: Option<String>,
    /// Items counted / items in scope; populated by the list endpoint only.
    #[serde(default)]
    #[sqlx(default)]
    pub counted_items: Option<i64>,
    #[serde(default)]
    #[sqlx(default)]
    pub total_items: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct StocktakeItem {
    pub id: Uuid,
    pub stocktake_id: Uuid,
    pub org_ingredient_id: Uuid,
    pub ingredient_name: String,
    pub unit: String,
    pub category_id: Uuid,
    pub category_name: String,
    /// Book stock when the count was opened (reference only).
    pub opening_qty: f64,
    /// The baseline the difference is measured against: live book stock while
    /// the count is open, frozen at finalize.
    pub book_qty: f64,
    pub counted_qty: Option<f64>,
    /// counted − book; `null` until counted.
    pub variance: Option<f64>,
    /// Piastres per unit snapshot; `null` ⟺ unknown.
    pub unit_cost: Option<i64>,
    pub note: Option<String>,
    /// theft | spoilage | breakage | miscount | supplier_short | transfer_error | other.
    pub variance_reason: Option<String>,
    pub counted_by: Option<Uuid>,
    /// True when the branch had no stock activity for this ingredient when the
    /// count opened — counting it is what starts tracking it here.
    pub is_new: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct StocktakeFull {
    #[serde(flatten)]
    pub stocktake: Stocktake,
    pub items: Vec<StocktakeItem>,
    /// Org tolerance: a counted row whose |difference| is >= this percent of
    /// book stock (or that appears-from / vanishes-to zero) is flagged and
    /// requires a `variance_reason` before the count can be finalized.
    pub variance_threshold_pct: f64,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct VarianceRow {
    pub org_ingredient_id: Uuid,
    pub ingredient_name: String,
    pub unit: String,
    pub category_name: String,
    /// Book stock when the count opened.
    pub opening_qty: f64,
    /// Book stock the difference is measured against (at finalize).
    pub book_qty: f64,
    pub counted_qty: Option<f64>,
    /// counted − book.
    pub variance: Option<f64>,
    pub unit_cost: Option<i64>,
    /// variance × unit_cost in piastres; `null` when cost unknown.
    pub variance_value: Option<i64>,
    pub variance_reason: Option<String>,
    /// True when |difference| exceeds the org threshold (or appears/vanishes from zero).
    pub is_flagged: bool,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct VarianceReport {
    pub stocktake_id: Uuid,
    pub rows: Vec<VarianceRow>,
    /// Piastres lost to shrinkage (negative variances), as a positive number.
    pub total_shrinkage_value: i64,
    /// Piastres of overage (positive variances).
    pub total_overage_value: i64,
    /// overage − shrinkage (net effect on inventory value).
    pub net_variance_value: i64,
    /// Count of counted rows whose cost was unknown (excluded from totals).
    pub unknown_cost_count: i64,
    pub variance_threshold_pct: f64,
}

// ── Request types ─────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct CreateStocktakeRequest {
    pub note: Option<String>,
    /// Cycle-count scope: only ingredients in this category.
    pub category_id: Option<Uuid>,
    /// Cycle-count scope: only these ingredients.
    pub org_ingredient_ids: Option<Vec<Uuid>>,
}

#[derive(Deserialize, ToSchema)]
pub struct ItemCountInput {
    pub org_ingredient_id: Uuid,
    pub counted_qty: f64,
    pub note: Option<String>,
    /// Why the count differs from book stock. One of: theft | spoilage |
    /// breakage | miscount | supplier_short | transfer_error | other. Required
    /// at finalize for rows whose difference exceeds the org's threshold.
    pub variance_reason: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct UpsertItemsRequest {
    pub items: Vec<ItemCountInput>,
}

// ── Shared SQL ────────────────────────────────────────────────

const STOCKTAKE_SELECT: &str = r#"
    SELECT s.id, s.org_id, s.branch_id, s.status::text AS status, s.note, s.scope, s.started_by,
           u.name AS started_by_name,
           s.started_at, s.finalized_by, s.finalized_at, s.created_at
    FROM stocktakes s
    JOIN users u ON u.id = s.started_by
"#;

/// Book stock for an item: live balance while the count is open, the frozen
/// `book_qty` once finalized (opening as a last resort for cancelled counts).
const BOOK_EXPR: &str = "CASE WHEN s.status IN ('draft','in_progress') THEN COALESCE(bs.on_hand, 0) \
                              ELSE COALESCE(si.book_qty, si.opening_qty) END";

// ── POST /stocktakes/branches/:branch_id ─────────────────────

#[utoipa::path(
    post,
    path = "/stocktakes/branches/{branch_id}",
    tag = "stocktakes",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    request_body = CreateStocktakeRequest,
    responses((status = 201, description = "Stocktake started", body = StocktakeFull), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_stocktake(
    req: HttpRequest,
    pool: crate::db::Db,
    branch_id: web::Path<Uuid>,
    body: web::Json<CreateStocktakeRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "stocktakes", "create").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    let org_id: Uuid =
        sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL")
            .bind(*branch_id)
            .fetch_optional(pool.get_ref())
            .await?
            .flatten()
            .ok_or_else(|| AppError::NotFound("Branch not found".into()))?;

    if body.category_id.is_some() && body.org_ingredient_ids.is_some() {
        return Err(AppError::BadRequest(
            "Pass either category_id or org_ingredient_ids, not both".into(),
        ));
    }
    if let Some(cat) = body.category_id {
        let ok: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM ingredient_categories WHERE id = $1 AND org_id = $2)",
        )
        .bind(cat)
        .bind(org_id)
        .fetch_one(pool.get_ref())
        .await?;
        if !ok {
            return Err(AppError::BadRequest(
                "Category not found in this organization".into(),
            ));
        }
    }
    if let Some(ids) = &body.org_ingredient_ids {
        if ids.is_empty() {
            return Err(AppError::BadRequest(
                "org_ingredient_ids cannot be empty".into(),
            ));
        }
        let found: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM org_ingredients WHERE id = ANY($1) AND org_id = $2 AND deleted_at IS NULL",
        )
        .bind(ids)
        .bind(org_id)
        .fetch_one(pool.get_ref())
        .await?;
        if found as usize != ids.len() {
            return Err(AppError::BadRequest(
                "One or more ingredients are not in this organization's catalog".into(),
            ));
        }
    }
    let scope = match (body.category_id, &body.org_ingredient_ids) {
        (Some(c), _) => serde_json::json!({ "kind": "category", "category_id": c }),
        (_, Some(ids)) => serde_json::json!({ "kind": "items", "org_ingredient_ids": ids }),
        _ => serde_json::json!({ "kind": "full" }),
    };

    let mut tx = pool.get_ref().begin().await?;

    // One open count per branch; the partial unique index makes this race-proof.
    let header_id: Uuid = sqlx::query_scalar(
        "INSERT INTO stocktakes (org_id, branch_id, status, note, scope, started_by) \
         VALUES ($1, $2, 'in_progress', $3, $4, $5) RETURNING id",
    )
    .bind(org_id)
    .bind(*branch_id)
    .bind(&body.note)
    .bind(&scope)
    .bind(claims.user_id())
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.code().as_deref() == Some("23505") => AppError::Conflict(
            "An open stocktake already exists for this branch. Finalize or cancel it first.".into(),
        ),
        _ => AppError::Db(e),
    })?;

    // Snapshot every live catalog ingredient in scope. No branch_stock row means
    // 0 on hand and `is_new`; the row is created only if the item gets counted.
    sqlx::query(
        r#"
        INSERT INTO stocktake_items
            (stocktake_id, org_ingredient_id, branch_stock_id, opening_qty, unit_cost)
        SELECT $1, oi.id, bs.id, COALESCE(bs.on_hand, 0),
               round(COALESCE(bs.cost_per_unit, oi.cost_per_unit))::bigint
        FROM org_ingredients oi
        LEFT JOIN branch_stock bs ON bs.org_ingredient_id = oi.id AND bs.branch_id = $2
        WHERE oi.org_id = $3 AND oi.deleted_at IS NULL
          AND ($4::uuid[] IS NULL OR oi.id = ANY($4))
          AND ($5::uuid   IS NULL OR oi.category_id = $5)
        "#,
    )
    .bind(header_id)
    .bind(*branch_id)
    .bind(org_id)
    .bind(body.org_ingredient_ids.as_deref())
    .bind(body.category_id)
    .execute(&mut *tx)
    .await?;

    let header = fetch_stocktake(&mut *tx, header_id).await?;
    let items = fetch_items(&mut *tx, header_id).await?;
    let variance_threshold_pct = fetch_threshold(&mut *tx, org_id).await?;

    tx.commit().await?;
    Ok(HttpResponse::Created().json(StocktakeFull {
        stocktake: header,
        items,
        variance_threshold_pct,
    }))
}

// ── GET /stocktakes/branches/:branch_id ──────────────────────

#[utoipa::path(
    get,
    path = "/stocktakes/branches/{branch_id}",
    tag = "stocktakes",
    params(("branch_id" = Uuid, Path, description = "Branch ID, or the all-zeros UUID for every branch in the org")),
    responses((status = 200, description = "List stocktakes", body = Vec<Stocktake>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_stocktakes(
    req: HttpRequest,
    pool: crate::db::Db,
    branch_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "stocktakes", "read").await?;

    // nil UUID = every branch in the caller's org ("All branches"); any other
    // UUID is that one branch after the usual access check.
    let (scope_condition, scope_id): (&str, Uuid) = if branch_id.is_nil() {
        let org = claims
            .scope_org(crate::auth::middleware::header_org_id(&req))
            .ok_or_else(|| AppError::Forbidden("No organization in scope".into()))?;
        (
            "s.branch_id IN (SELECT id FROM branches WHERE org_id = $1 AND deleted_at IS NULL)",
            org,
        )
    } else {
        require_branch_access(pool.get_ref(), &claims, *branch_id).await?;
        ("s.branch_id = $1", *branch_id)
    };

    let sql = format!(
        r#"
        SELECT s.id, s.org_id, s.branch_id, s.status::text AS status, s.note, s.scope, s.started_by,
               u.name AS started_by_name,
               b.name AS branch_name,
               (SELECT count(*) FROM stocktake_items si WHERE si.stocktake_id = s.id AND si.counted_qty IS NOT NULL) AS counted_items,
               (SELECT count(*) FROM stocktake_items si WHERE si.stocktake_id = s.id) AS total_items,
               s.started_at, s.finalized_by, s.finalized_at, s.created_at
        FROM stocktakes s
        JOIN users u    ON u.id = s.started_by
        JOIN branches b ON b.id = s.branch_id
        WHERE {scope_condition}
        ORDER BY s.started_at DESC
        "#
    );
    let rows = sqlx::query_as::<_, Stocktake>(&sql)
        .bind(scope_id)
        .fetch_all(pool.get_ref())
        .await?;

    Ok(HttpResponse::Ok().json(rows))
}

// ── GET /stocktakes/:id ──────────────────────────────────────

#[utoipa::path(
    get,
    path = "/stocktakes/{id}",
    tag = "stocktakes",
    params(("id" = Uuid, Path, description = "Stocktake ID")),
    responses((status = 200, description = "Stocktake detail", body = StocktakeFull), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_stocktake(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "stocktakes", "read").await?;
    let header = fetch_stocktake_or_404(pool.get_ref(), *id).await?;
    require_branch_access(pool.get_ref(), &claims, header.branch_id).await?;

    let items = fetch_items(pool.get_ref(), *id).await?;
    let variance_threshold_pct = fetch_threshold(pool.get_ref(), header.org_id).await?;
    Ok(HttpResponse::Ok().json(StocktakeFull {
        stocktake: header,
        items,
        variance_threshold_pct,
    }))
}

// ── PUT /stocktakes/:id/items ────────────────────────────────

#[utoipa::path(
    put,
    path = "/stocktakes/{id}/items",
    tag = "stocktakes",
    params(("id" = Uuid, Path, description = "Stocktake ID")),
    request_body = UpsertItemsRequest,
    responses((status = 200, description = "Counts saved", body = StocktakeFull), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn upsert_items(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<UpsertItemsRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "stocktakes", "update").await?;
    let header = fetch_stocktake_or_404(pool.get_ref(), *id).await?;
    require_branch_access(pool.get_ref(), &claims, header.branch_id).await?;

    if !is_open(&header.status) {
        return Err(AppError::Conflict(
            "Cannot edit counts on a finalized or cancelled stocktake".into(),
        ));
    }

    let mut tx = pool.get_ref().begin().await?;
    for item in &body.items {
        if item.counted_qty < 0.0 {
            return Err(AppError::BadRequest(
                "counted_qty cannot be negative".into(),
            ));
        }
        if let Some(reason) = &item.variance_reason {
            validate_variance_reason(reason)?;
        }
        // Upsert: update a snapshot row, OR add a FOUND item outside a cycle
        // scope. A found item snapshots its opening qty from current book stock
        // (0 when the branch has never moved it). Cross-org / unknown
        // ingredients produce no row → rejected below.
        let affected = sqlx::query(
            r#"
            INSERT INTO stocktake_items
                (stocktake_id, org_ingredient_id, branch_stock_id, opening_qty, unit_cost,
                 counted_qty, note, counted_by, variance_reason)
            SELECT $1, oi.id, bs.id, COALESCE(bs.on_hand, 0),
                   round(COALESCE(bs.cost_per_unit, oi.cost_per_unit))::bigint,
                   $3, $4, $5, $6::stocktake_variance_reason
            FROM org_ingredients oi
            LEFT JOIN branch_stock bs ON bs.org_ingredient_id = oi.id AND bs.branch_id = $7
            WHERE oi.id = $2 AND oi.org_id = $8 AND oi.deleted_at IS NULL
            ON CONFLICT (stocktake_id, org_ingredient_id)
            DO UPDATE SET counted_qty = EXCLUDED.counted_qty, note = EXCLUDED.note,
                          counted_by = EXCLUDED.counted_by,
                          variance_reason = EXCLUDED.variance_reason
            "#,
        )
        .bind(*id)
        .bind(item.org_ingredient_id)
        .bind(item.counted_qty)
        .bind(&item.note)
        .bind(claims.user_id())
        .bind(&item.variance_reason)
        .bind(header.branch_id)
        .bind(header.org_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if affected == 0 {
            return Err(AppError::BadRequest(
                "Ingredient not found in this organization's catalog".into(),
            ));
        }
    }
    tx.commit().await?;

    let items = fetch_items(pool.get_ref(), *id).await?;
    let variance_threshold_pct = fetch_threshold(pool.get_ref(), header.org_id).await?;
    Ok(HttpResponse::Ok().json(StocktakeFull {
        stocktake: header,
        items,
        variance_threshold_pct,
    }))
}

// ── POST /stocktakes/:id/finalize ────────────────────────────

#[utoipa::path(
    post,
    path = "/stocktakes/{id}/finalize",
    tag = "stocktakes",
    params(("id" = Uuid, Path, description = "Stocktake ID")),
    responses((status = 200, description = "Stocktake finalized + stock reconciled", body = StocktakeFull), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn finalize_stocktake(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "stocktakes", "update").await?;
    let header = fetch_stocktake_or_404(pool.get_ref(), *id).await?;
    require_branch_access(pool.get_ref(), &claims, header.branch_id).await?;

    if !is_open(&header.status) {
        return Err(AppError::Conflict("Stocktake is not open".into()));
    }

    let threshold = fetch_threshold(pool.get_ref(), header.org_id).await?;

    let mut tx = pool.get_ref().begin().await?;

    // Lock the stocktake row and re-check it is still open INSIDE the tx, so a
    // concurrent/retried finalize can't double-post stock_count movements.
    let locked_status: String =
        sqlx::query_scalar("SELECT status::text FROM stocktakes WHERE id = $1 FOR UPDATE")
            .bind(*id)
            .fetch_one(&mut *tx)
            .await?;
    if !is_open(&locked_status) {
        return Err(AppError::Conflict("Stocktake is not open".into()));
    }

    type ItemRow = (Uuid, String, f64, Option<i64>, Option<String>);
    let items: Vec<ItemRow> = sqlx::query_as(
        "SELECT si.org_ingredient_id, oi.name, si.counted_qty::float8, si.unit_cost, \
                si.variance_reason::text \
         FROM stocktake_items si \
         JOIN org_ingredients oi ON oi.id = si.org_ingredient_id \
         WHERE si.stocktake_id = $1 AND si.counted_qty IS NOT NULL \
         ORDER BY oi.name",
    )
    .bind(*id)
    .fetch_all(&mut *tx)
    .await?;

    // Pass 1 — make sure every counted ingredient has a balance row (counting
    // it is what starts tracking it here), then lock each row and read LIVE
    // book stock. The locks are held to commit, so the baseline can't move.
    // (ingredient, name, counted, book, unit_cost, reason)
    type Counted = (Uuid, String, f64, f64, Option<i64>, Option<String>);
    let mut counted: Vec<Counted> = Vec::with_capacity(items.len());
    for (ing_id, name, counted_qty, unit_cost, reason) in items {
        sqlx::query(
            "INSERT INTO branch_stock (branch_id, org_ingredient_id, on_hand) VALUES ($1, $2, 0) \
             ON CONFLICT (branch_id, org_ingredient_id) DO NOTHING",
        )
        .bind(header.branch_id)
        .bind(ing_id)
        .execute(&mut *tx)
        .await?;
        let book: f64 = sqlx::query_scalar(
            "SELECT on_hand::float8 FROM branch_stock \
             WHERE branch_id = $1 AND org_ingredient_id = $2 FOR UPDATE",
        )
        .bind(header.branch_id)
        .bind(ing_id)
        .fetch_one(&mut *tx)
        .await?;
        counted.push((ing_id, name, counted_qty, book, unit_cost, reason));
    }

    // Guardrail: every suspicious difference (vs book stock) needs a reason.
    let unexplained: Vec<String> = counted
        .iter()
        .filter(|(_, _, counted_qty, book, _, reason)| {
            is_variance_flagged(*book, *counted_qty, threshold) && reason.is_none()
        })
        .map(|(_, name, _, _, _, _)| name.clone())
        .collect();
    if !unexplained.is_empty() {
        return Err(AppError::Conflict(format!(
            "These items have a large difference and need a reason before finalizing: {}",
            unexplained.join(", ")
        )));
    }

    // Pass 2 — freeze the baseline, post the difference to the ledger, stamp
    // the count date.
    for (ing_id, _name, counted_qty, book, unit_cost, variance_reason) in counted {
        sqlx::query(
            "UPDATE stocktake_items SET book_qty = $3 \
             WHERE stocktake_id = $1 AND org_ingredient_id = $2",
        )
        .bind(*id)
        .bind(ing_id)
        .bind(book)
        .execute(&mut *tx)
        .await?;

        let delta = counted_qty - book;
        if delta.abs() > 1e-9 {
            record_movement(
                &mut *tx,
                MovementParams {
                    branch_id: header.branch_id,
                    org_ingredient_id: ing_id,
                    movement_type: "stock_count",
                    quantity: delta,
                    unit_cost,
                    reason: variance_reason.as_deref(),
                    source_type: Some("stocktake"),
                    source_id: Some(*id),
                    note: Some("Stock count"),
                    created_by: Some(claims.user_id()),
                },
            )
            .await?;
        }

        sqlx::query(
            "UPDATE branch_stock SET last_counted_at = now() \
             WHERE branch_id = $1 AND org_ingredient_id = $2",
        )
        .bind(header.branch_id)
        .bind(ing_id)
        .execute(&mut *tx)
        .await?;
    }

    sqlx::query(
        "UPDATE stocktakes SET status = 'finalized', finalized_by = $2, finalized_at = now() WHERE id = $1",
    )
    .bind(*id)
    .bind(claims.user_id())
    .execute(&mut *tx)
    .await?;

    let header = fetch_stocktake(&mut *tx, *id).await?;
    let items = fetch_items(&mut *tx, *id).await?;
    tx.commit().await?;
    Ok(HttpResponse::Ok().json(StocktakeFull {
        stocktake: header,
        items,
        variance_threshold_pct: threshold,
    }))
}

// ── POST /stocktakes/:id/cancel ──────────────────────────────

#[utoipa::path(
    post,
    path = "/stocktakes/{id}/cancel",
    tag = "stocktakes",
    params(("id" = Uuid, Path, description = "Stocktake ID")),
    responses((status = 200, description = "Stocktake cancelled", body = Stocktake), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn cancel_stocktake(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "stocktakes", "update").await?;
    let header = fetch_stocktake_or_404(pool.get_ref(), *id).await?;
    require_branch_access(pool.get_ref(), &claims, header.branch_id).await?;

    if !is_open(&header.status) {
        return Err(AppError::Conflict(
            "Only an open stocktake can be cancelled".into(),
        ));
    }

    sqlx::query("UPDATE stocktakes SET status = 'cancelled' WHERE id = $1")
        .bind(*id)
        .execute(pool.get_ref())
        .await?;

    Ok(HttpResponse::Ok().json(fetch_stocktake(pool.get_ref(), *id).await?))
}

// ── GET /stocktakes/:id/variance-report ──────────────────────

#[utoipa::path(
    get,
    path = "/stocktakes/{id}/variance-report",
    tag = "stocktakes",
    params(("id" = Uuid, Path, description = "Stocktake ID")),
    responses((status = 200, description = "Variance report (valued)", body = VarianceReport), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn variance_report(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "stocktakes", "read").await?;
    let header = fetch_stocktake_or_404(pool.get_ref(), *id).await?;
    require_branch_access(pool.get_ref(), &claims, header.branch_id).await?;

    let threshold = fetch_threshold(pool.get_ref(), header.org_id).await?;

    let sql = format!(
        r#"
        SELECT
            si.org_ingredient_id,
            oi.name       AS ingredient_name,
            oi.unit::text AS unit,
            ic.name       AS category_name,
            si.opening_qty::float8 AS opening_qty,
            ({BOOK_EXPR})::float8  AS book_qty,
            si.counted_qty::float8 AS counted_qty,
            (si.counted_qty - ({BOOK_EXPR}))::float8 AS variance,
            si.unit_cost,
            CASE WHEN si.unit_cost IS NULL OR si.counted_qty IS NULL THEN NULL
                 ELSE round((si.counted_qty - ({BOOK_EXPR})) * si.unit_cost)::bigint END AS variance_value,
            si.variance_reason::text AS variance_reason,
            CASE
                WHEN si.counted_qty IS NULL THEN false
                WHEN ({BOOK_EXPR}) = 0 THEN si.counted_qty <> 0
                ELSE (abs(si.counted_qty - ({BOOK_EXPR})) / abs({BOOK_EXPR}) * 100)::float8 >= $2
            END AS is_flagged
        FROM stocktake_items si
        JOIN stocktakes s ON s.id = si.stocktake_id
        JOIN org_ingredients oi ON oi.id = si.org_ingredient_id
        JOIN ingredient_categories ic ON ic.id = oi.category_id
        LEFT JOIN branch_stock bs ON bs.branch_id = s.branch_id AND bs.org_ingredient_id = si.org_ingredient_id
        WHERE si.stocktake_id = $1
        ORDER BY oi.name ASC
        "#
    );
    let rows = sqlx::query_as::<_, VarianceRow>(&sql)
        .bind(*id)
        .bind(threshold)
        .fetch_all(pool.get_ref())
        .await?;

    let mut total_shrinkage_value = 0i64;
    let mut total_overage_value = 0i64;
    let mut unknown_cost_count = 0i64;
    for r in &rows {
        match (r.counted_qty, r.variance_value) {
            (Some(_), Some(v)) if v < 0 => total_shrinkage_value += -v,
            (Some(_), Some(v)) => total_overage_value += v,
            (Some(_), None) => unknown_cost_count += 1,
            _ => {}
        }
    }

    Ok(HttpResponse::Ok().json(VarianceReport {
        stocktake_id: *id,
        rows,
        total_shrinkage_value,
        total_overage_value,
        net_variance_value: total_overage_value - total_shrinkage_value,
        unknown_cost_count,
        variance_threshold_pct: threshold,
    }))
}

// ── Helpers ───────────────────────────────────────────────────

fn is_open(status: &str) -> bool {
    status == "in_progress" || status == "draft"
}

async fn fetch_items<'e, E>(executor: E, stocktake_id: Uuid) -> Result<Vec<StocktakeItem>, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let sql = format!(
        r#"
        SELECT si.id, si.stocktake_id, si.org_ingredient_id,
               oi.name       AS ingredient_name,
               oi.unit::text AS unit,
               oi.category_id, ic.name AS category_name,
               si.opening_qty::float8 AS opening_qty,
               ({BOOK_EXPR})::float8  AS book_qty,
               si.counted_qty::float8 AS counted_qty,
               (si.counted_qty - ({BOOK_EXPR}))::float8 AS variance,
               si.unit_cost, si.note, si.variance_reason::text AS variance_reason,
               si.counted_by,
               (si.branch_stock_id IS NULL) AS is_new,
               si.created_at
        FROM stocktake_items si
        JOIN stocktakes s ON s.id = si.stocktake_id
        JOIN org_ingredients oi ON oi.id = si.org_ingredient_id
        JOIN ingredient_categories ic ON ic.id = oi.category_id
        LEFT JOIN branch_stock bs ON bs.branch_id = s.branch_id AND bs.org_ingredient_id = si.org_ingredient_id
        WHERE si.stocktake_id = $1
        ORDER BY ic.sort_order, ic.name, oi.name ASC
        "#
    );
    let items = sqlx::query_as::<_, StocktakeItem>(&sql)
        .bind(stocktake_id)
        .fetch_all(executor)
        .await?;
    Ok(items)
}

async fn fetch_stocktake<'e, E>(executor: E, id: Uuid) -> Result<Stocktake, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let sql = format!("{STOCKTAKE_SELECT} WHERE s.id = $1");
    sqlx::query_as::<_, Stocktake>(&sql)
        .bind(id)
        .fetch_optional(executor)
        .await?
        .ok_or_else(|| AppError::NotFound("Stocktake not found".into()))
}

async fn fetch_stocktake_or_404(pool: &PgPool, id: Uuid) -> Result<Stocktake, AppError> {
    fetch_stocktake(pool, id).await
}

/// The org's stocktake variance tolerance (percent). Defaults to 10 on the column.
async fn fetch_threshold<'e, E>(executor: E, org_id: Uuid) -> Result<f64, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let pct: f64 = sqlx::query_scalar(
        "SELECT stocktake_variance_threshold_pct::float8 FROM organizations WHERE id = $1",
    )
    .bind(org_id)
    .fetch_one(executor)
    .await?;
    Ok(pct)
}

/// A counted row is "suspicious" when its |difference| is at least `pct` of the
/// book quantity, or when stock appears from / vanishes to zero.
fn is_variance_flagged(book: f64, counted: f64, pct: f64) -> bool {
    if book.abs() < 1e-9 {
        counted.abs() > 1e-9
    } else {
        (counted - book).abs() / book.abs() * 100.0 >= pct
    }
}

const VARIANCE_REASONS: &[&str] = &[
    "theft",
    "spoilage",
    "breakage",
    "miscount",
    "supplier_short",
    "transfer_error",
    "other",
];

fn validate_variance_reason(reason: &str) -> Result<(), AppError> {
    if VARIANCE_REASONS.contains(&reason) {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!(
            "Invalid variance reason '{}'. Allowed: {}",
            reason,
            VARIANCE_REASONS.join(", ")
        )))
    }
}

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

async fn require_branch_access(
    pool: &PgPool,
    claims: &Claims,
    branch_id: Uuid,
) -> Result<(), AppError> {
    if claims.role == UserRole::SuperAdmin {
        return Ok(());
    }

    let branch_org: Option<Uuid> =
        sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL")
            .bind(branch_id)
            .fetch_optional(pool)
            .await?
            .flatten();

    let branch_org = branch_org.ok_or_else(|| AppError::NotFound("Branch not found".into()))?;

    if claims.org_id() != Some(branch_org) {
        return Err(AppError::Forbidden(
            "Branch belongs to a different org".into(),
        ));
    }

    if claims.role == UserRole::OrgAdmin {
        return Ok(());
    }

    let assigned: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM user_branch_assignments WHERE user_id = $1 AND branch_id = $2)"
    )
    .bind(claims.user_id())
    .bind(branch_id)
    .fetch_one(pool)
    .await?;

    if !assigned {
        return Err(AppError::Forbidden("Not assigned to this branch".into()));
    }

    // A teller token is bound to the branch it authenticated for: a token minted
    // for one branch must not act on another, even when the teller is assigned to
    // both. The None guard keeps legacy/non-teller tokens working (V26).
    if claims.role == UserRole::Teller
        && let Some(token_branch) = claims.branch_id()
        && token_branch != branch_id
    {
        return Err(AppError::Forbidden(
            "This device is signed in to a different branch.".into(),
        ));
    }

    Ok(())
}
