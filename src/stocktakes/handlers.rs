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
    pub started_by: Uuid,
    pub started_by_name: Option<String>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finalized_by: Option<Uuid>,
    pub finalized_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Branch label — only populated by the stocktakes list (so the "All
    /// branches" view can show which branch each stocktake belongs to). Other
    /// stocktake endpoints leave it `null`.
    #[serde(default)]
    #[sqlx(default)]
    pub branch_name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct StocktakeItem {
    pub id: Uuid,
    pub stocktake_id: Uuid,
    pub org_ingredient_id: Uuid,
    pub ingredient_name: String,
    pub unit: String,
    pub branch_inventory_id: Option<Uuid>,
    #[schema(value_type = f64)]
    pub expected_qty: sqlx::types::BigDecimal,
    #[schema(value_type = Option<f64>)]
    pub counted_qty: Option<sqlx::types::BigDecimal>,
    #[schema(value_type = Option<f64>)]
    pub variance: Option<sqlx::types::BigDecimal>,
    /// Piastres per unit snapshot; `null` ⟺ unknown.
    pub unit_cost: Option<i64>,
    pub note: Option<String>,
    /// theft | spoilage | breakage | miscount | supplier_short | transfer_error | other.
    pub variance_reason: Option<String>,
    pub counted_by: Option<Uuid>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct StocktakeFull {
    #[serde(flatten)]
    pub stocktake: Stocktake,
    pub items: Vec<StocktakeItem>,
    /// Org tolerance: a counted row whose |difference| is >= this percent of the
    /// expected quantity (or that appears-from / vanishes-to zero) is flagged and
    /// requires a `variance_reason` before the count can be finalized.
    pub variance_threshold_pct: f64,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct VarianceRow {
    pub org_ingredient_id: Uuid,
    pub ingredient_name: String,
    pub unit: String,
    pub expected_qty: f64,
    pub counted_qty: Option<f64>,
    pub variance: Option<f64>,
    pub unit_cost: Option<i64>,
    /// variance × unit_cost in piastres; `null` when cost unknown.
    pub variance_value: Option<i64>,
    /// theft | spoilage | breakage | miscount | supplier_short | transfer_error | other.
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
    /// Org tolerance used to compute `is_flagged`.
    pub variance_threshold_pct: f64,
}

// ── Request types ─────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct CreateStocktakeRequest {
    pub note: Option<String>,
    /// Cycle-count scope: snapshot only ingredients in this catalog category.
    /// Omit (with org_ingredient_ids) for a full-branch count.
    pub category: Option<String>,
    /// Cycle-count scope: snapshot only these specific ingredients.
    pub org_ingredient_ids: Option<Vec<Uuid>>,
}

#[derive(Deserialize, ToSchema)]
pub struct ItemCountInput {
    pub org_ingredient_id: Uuid,
    pub counted_qty: f64,
    pub note: Option<String>,
    /// Why the count differs from expected. One of: theft | spoilage | breakage |
    /// miscount | supplier_short | transfer_error | other. Required at finalize for
    /// rows whose difference exceeds the org's variance threshold.
    pub variance_reason: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct UpsertItemsRequest {
    pub items: Vec<ItemCountInput>,
}

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
    pool: web::Data<PgPool>,
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

    // One active stocktake per branch at a time.
    let open_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM stocktakes \
         WHERE branch_id = $1 AND status IN ('draft','in_progress'))",
    )
    .bind(*branch_id)
    .fetch_one(pool.get_ref())
    .await?;
    if open_exists {
        return Err(AppError::Conflict(
            "An open stocktake already exists for this branch. Finalize or cancel it first.".into(),
        ));
    }

    let mut tx = pool.get_ref().begin().await?;

    let header = sqlx::query_as::<_, Stocktake>(
        r#"
        INSERT INTO stocktakes (org_id, branch_id, status, note, started_by)
        VALUES ($1, $2, 'in_progress', $3, $4)
        RETURNING id, org_id, branch_id, status::text, note, started_by,
                  (SELECT name FROM users WHERE id = $4) AS started_by_name,
                  started_at, finalized_by, finalized_at, created_at
        "#,
    )
    .bind(org_id)
    .bind(*branch_id)
    .bind(&body.note)
    .bind(claims.user_id())
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match &e {
        // A concurrent open won the race; the partial unique index rejected us (V12).
        sqlx::Error::Database(db) if db.code().as_deref() == Some("23505") => AppError::Conflict(
            "An open stocktake already exists for this branch. Finalize or cancel it first.".into(),
        ),
        _ => AppError::Db(e),
    })?;

    // Snapshot current branch stock as the expected counts. Optional cycle-count
    // scope filters by explicit ingredient list and/or category; omitting both
    // snapshots the whole branch (soft-deleted ingredients are always excluded).
    sqlx::query(
        r#"
        INSERT INTO stocktake_items
            (stocktake_id, org_ingredient_id, branch_inventory_id, expected_qty, unit_cost)
        SELECT $1, bi.org_ingredient_id, bi.id, bi.current_stock,
               round(COALESCE(bi.cost_per_unit, oi.cost_per_unit))::bigint
        FROM branch_inventory bi
        JOIN org_ingredients oi ON oi.id = bi.org_ingredient_id
        WHERE bi.branch_id = $2 AND oi.deleted_at IS NULL
          AND ($3::uuid[] IS NULL OR bi.org_ingredient_id = ANY($3))
          AND ($4::text  IS NULL OR oi.category = $4)
        "#,
    )
    .bind(header.id)
    .bind(*branch_id)
    .bind(body.org_ingredient_ids.as_deref())
    .bind(&body.category)
    .execute(&mut *tx)
    .await?;

    let items = fetch_items(&mut *tx, header.id).await?;
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
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    responses((status = 200, description = "List stocktakes", body = Vec<Stocktake>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_stocktakes(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "stocktakes", "read").await?;

    // nil UUID = every branch in the caller's org ("All branches"); any other
    // UUID is that one branch after the usual access check. The org for the
    // roll-up is the token's org (or X-Org-Id for super admins).
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
        SELECT s.id, s.org_id, s.branch_id, s.status::text, s.note, s.started_by,
               u.name AS started_by_name,
               b.name AS branch_name,
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
    pool: web::Data<PgPool>,
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
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
    body: web::Json<UpsertItemsRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "stocktakes", "update").await?;
    let header = fetch_stocktake_or_404(pool.get_ref(), *id).await?;
    require_branch_access(pool.get_ref(), &claims, header.branch_id).await?;

    if header.status != "in_progress" && header.status != "draft" {
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
        // Upsert: update a snapshot row, OR add a FOUND item not in the snapshot
        // (e.g. physical stock the system wasn't tracking, or outside a cycle-count
        // scope). A found item snapshots its expected_qty from current branch stock
        // (0 if untracked) so its variance is meaningful. Cross-org / unknown
        // ingredients produce no row → rejected below.
        let affected = sqlx::query(
            "INSERT INTO stocktake_items \
                 (stocktake_id, org_ingredient_id, branch_inventory_id, expected_qty, unit_cost, \
                  counted_qty, note, counted_by, variance_reason) \
             SELECT $1, oi.id, bi.id, COALESCE(bi.current_stock, 0), \
                    round(COALESCE(bi.cost_per_unit, oi.cost_per_unit))::bigint, \
                    $3, $4, $5, $6::stocktake_variance_reason \
             FROM org_ingredients oi \
             LEFT JOIN branch_inventory bi \
                    ON bi.org_ingredient_id = oi.id AND bi.branch_id = $7 \
             WHERE oi.id = $2 AND oi.org_id = $8 AND oi.deleted_at IS NULL \
             ON CONFLICT (stocktake_id, org_ingredient_id) \
             DO UPDATE SET counted_qty = EXCLUDED.counted_qty, note = EXCLUDED.note, \
                           counted_by = EXCLUDED.counted_by, \
                           variance_reason = EXCLUDED.variance_reason",
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
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "stocktakes", "update").await?;
    let header = fetch_stocktake_or_404(pool.get_ref(), *id).await?;
    require_branch_access(pool.get_ref(), &claims, header.branch_id).await?;

    if header.status != "in_progress" && header.status != "draft" {
        return Err(AppError::Conflict("Stocktake is not open".into()));
    }

    let threshold = fetch_threshold(pool.get_ref(), header.org_id).await?;

    let mut tx = pool.get_ref().begin().await?;

    // Lock the stocktake row and re-check it is still open INSIDE the tx, so a
    // concurrent/retried finalize can't both pass the status gate above and
    // double-post stock_count movements (doubling shrinkage reports) (V11).
    let locked_status: String =
        sqlx::query_scalar("SELECT status::text FROM stocktakes WHERE id = $1 FOR UPDATE")
            .bind(*id)
            .fetch_one(&mut *tx)
            .await?;
    if locked_status != "in_progress" && locked_status != "draft" {
        return Err(AppError::Conflict("Stocktake is not open".into()));
    }

    // Counted items (snapshot values are immutable; live stock is read+locked
    // per row below). The reconciliation baseline is the LIVE book stock
    // (`system_qty`), NOT the open-time snapshot: legitimate sales/purchases
    // during the count already moved live stock, so true unexplained variance =
    // counted - live and those movements are preserved rather than erased /
    // mislabeled as shrinkage.
    type ItemRow = (Uuid, Option<Uuid>, String, f64, Option<i64>, Option<String>);
    let items: Vec<ItemRow> = sqlx::query_as(
        "SELECT si.org_ingredient_id, si.branch_inventory_id, oi.name, \
                si.counted_qty::float8, si.unit_cost, si.variance_reason::text \
         FROM stocktake_items si \
         JOIN org_ingredients oi ON oi.id = si.org_ingredient_id \
         WHERE si.stocktake_id = $1 AND si.counted_qty IS NOT NULL",
    )
    .bind(*id)
    .fetch_all(&mut *tx)
    .await?;

    // Lock + read each branch row's LIVE stock (can't FOR UPDATE the nullable
    // side of an outer join, so lock per row). The lock is held to commit, so
    // the live value can't move between read and reconcile.
    let mut counted: Vec<(Uuid, String, f64, Option<f64>, Option<i64>, Option<String>)> =
        Vec::with_capacity(items.len());
    for (ing_id, bi_id_opt, name, counted_qty, unit_cost, reason) in items {
        let live: Option<f64> = if let Some(bi_id) = bi_id_opt {
            sqlx::query_scalar(
                "SELECT current_stock::float8 FROM branch_inventory \
                 WHERE id = $1 AND branch_id = $2 FOR UPDATE",
            )
            .bind(bi_id)
            .bind(header.branch_id)
            .fetch_optional(&mut *tx)
            .await?
        } else {
            None
        };
        counted.push((ing_id, name, counted_qty, live, unit_cost, reason));
    }

    // Guardrail: every suspicious difference (vs LIVE book stock) must carry a
    // reason before the count is committed. A row whose tracking row was deleted
    // mid-count reconciles from a zero baseline (re-created below).
    let unexplained: Vec<String> = counted
        .iter()
        .filter(|(_, _, counted_qty, live, _, reason)| {
            is_variance_flagged(live.unwrap_or(0.0), *counted_qty, threshold) && reason.is_none()
        })
        .map(|(_, name, _, _, _, _)| name.clone())
        .collect();
    if !unexplained.is_empty() {
        return Err(AppError::Conflict(format!(
            "These items have a large difference and need a reason before finalizing: {}",
            unexplained.join(", ")
        )));
    }

    for (ing_id, _name, counted_qty, live_opt, unit_cost, variance_reason) in counted {
        // Reconcile to the counted ground-truth via a DELTA off live book stock,
        // so any sale/purchase posted during the count survives. A missing
        // tracking row (deleted mid-count) reconciles from 0 and is re-created,
        // so a counted item is never silently dropped.
        let system_qty = live_opt.unwrap_or(0.0);
        let delta = counted_qty - system_qty;

        let (bi_id, balance): (Uuid, f64) = sqlx::query_as(
            "INSERT INTO branch_inventory (branch_id, org_ingredient_id, current_stock, reorder_threshold) \
             VALUES ($1, $2, $3, 0) \
             ON CONFLICT (branch_id, org_ingredient_id) \
             DO UPDATE SET current_stock = EXCLUDED.current_stock, updated_at = now() \
             RETURNING id, current_stock::float8"
        )
        .bind(header.branch_id)
        .bind(ing_id)
        .bind(counted_qty)
        .fetch_one(&mut *tx)
        .await?;

        // Freeze the reconciliation baseline on the item for the variance report.
        sqlx::query(
            "UPDATE stocktake_items SET system_qty = $2 \
             WHERE stocktake_id = $1 AND org_ingredient_id = $3",
        )
        .bind(*id)
        .bind(system_qty)
        .bind(ing_id)
        .execute(&mut *tx)
        .await?;

        if delta != 0.0 {
            record_movement(
                &mut *tx,
                MovementParams {
                    branch_id: header.branch_id,
                    org_ingredient_id: ing_id,
                    branch_inventory_id: Some(bi_id),
                    movement_type: "stock_count",
                    quantity: delta,
                    balance_after: Some(balance),
                    unit_cost,
                    reason: variance_reason.as_deref(),
                    below_zero: balance < 0.0,
                    source_type: Some("stocktake"),
                    source_id: Some(*id),
                    note: Some("Stocktake reconciliation"),
                    created_by: Some(claims.user_id()),
                },
            )
            .await?;
        }
    }

    let header = sqlx::query_as::<_, Stocktake>(
        r#"
        UPDATE stocktakes
        SET status = 'finalized', finalized_by = $2, finalized_at = now()
        WHERE id = $1
        RETURNING id, org_id, branch_id, status::text, note, started_by,
                  (SELECT name FROM users WHERE id = started_by) AS started_by_name,
                  started_at, finalized_by, finalized_at, created_at
        "#,
    )
    .bind(*id)
    .bind(claims.user_id())
    .fetch_one(&mut *tx)
    .await?;

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
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "stocktakes", "update").await?;
    let header = fetch_stocktake_or_404(pool.get_ref(), *id).await?;
    require_branch_access(pool.get_ref(), &claims, header.branch_id).await?;

    if header.status != "in_progress" && header.status != "draft" {
        return Err(AppError::Conflict(
            "Only an open stocktake can be cancelled".into(),
        ));
    }

    let updated = sqlx::query_as::<_, Stocktake>(
        r#"
        UPDATE stocktakes SET status = 'cancelled'
        WHERE id = $1
        RETURNING id, org_id, branch_id, status::text, note, started_by,
                  (SELECT name FROM users WHERE id = started_by) AS started_by_name,
                  started_at, finalized_by, finalized_at, created_at
        "#,
    )
    .bind(*id)
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(updated))
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
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "stocktakes", "read").await?;
    let header = fetch_stocktake_or_404(pool.get_ref(), *id).await?;
    require_branch_access(pool.get_ref(), &claims, header.branch_id).await?;

    let threshold = fetch_threshold(pool.get_ref(), header.org_id).await?;

    let rows = sqlx::query_as::<_, VarianceRow>(
        r#"
        SELECT
            si.org_ingredient_id,
            oi.name       AS ingredient_name,
            oi.unit::text AS unit,
            si.expected_qty::float8,
            si.counted_qty::float8,
            -- True variance is measured against the live book stock at finalize
            -- (`system_qty`); legitimate activity during the count nets out. For
            -- a not-yet-finalized count, fall back to the open-time snapshot.
            (si.counted_qty - COALESCE(si.system_qty, si.expected_qty))::float8 AS variance,
            si.unit_cost,
            CASE WHEN si.unit_cost IS NULL OR si.counted_qty IS NULL THEN NULL
                 ELSE round((si.counted_qty - COALESCE(si.system_qty, si.expected_qty)) * si.unit_cost)::bigint END AS variance_value,
            si.variance_reason::text AS variance_reason,
            CASE
                WHEN si.counted_qty IS NULL THEN false
                WHEN COALESCE(si.system_qty, si.expected_qty) = 0 THEN si.counted_qty <> 0
                ELSE (abs(si.counted_qty - COALESCE(si.system_qty, si.expected_qty))
                      / abs(COALESCE(si.system_qty, si.expected_qty)) * 100)::float8 >= $2
            END AS is_flagged
        FROM stocktake_items si
        JOIN org_ingredients oi ON oi.id = si.org_ingredient_id
        WHERE si.stocktake_id = $1
        ORDER BY oi.name ASC
        "#,
    )
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

async fn fetch_items<'e, E>(executor: E, stocktake_id: Uuid) -> Result<Vec<StocktakeItem>, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let items = sqlx::query_as::<_, StocktakeItem>(
        r#"
        SELECT si.id, si.stocktake_id, si.org_ingredient_id,
               oi.name       AS ingredient_name,
               oi.unit::text AS unit,
               si.branch_inventory_id,
               si.expected_qty, si.counted_qty, si.variance, si.unit_cost,
               si.note, si.variance_reason::text AS variance_reason,
               si.counted_by, si.created_at
        FROM stocktake_items si
        JOIN org_ingredients oi ON oi.id = si.org_ingredient_id
        WHERE si.stocktake_id = $1
        ORDER BY oi.name ASC
        "#,
    )
    .bind(stocktake_id)
    .fetch_all(executor)
    .await?;
    Ok(items)
}

async fn fetch_stocktake_or_404(pool: &PgPool, id: Uuid) -> Result<Stocktake, AppError> {
    sqlx::query_as::<_, Stocktake>(
        r#"
        SELECT s.id, s.org_id, s.branch_id, s.status::text, s.note, s.started_by,
               u.name AS started_by_name,
               s.started_at, s.finalized_by, s.finalized_at, s.created_at
        FROM stocktakes s
        JOIN users u ON u.id = s.started_by
        WHERE s.id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Stocktake not found".into()))
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
/// expected quantity, or when stock appears from / vanishes to zero.
fn is_variance_flagged(expected: f64, counted: f64, pct: f64) -> bool {
    if expected.abs() < 1e-9 {
        counted.abs() > 1e-9
    } else {
        (counted - expected).abs() / expected.abs() * 100.0 >= pct
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
    if claims.role == UserRole::Teller {
        if let Some(token_branch) = claims.branch_id()
            && token_branch != branch_id
        {
            return Err(AppError::Forbidden(
                "This device is signed in to a different branch.".into(),
            ));
        }
    }

    Ok(())
}
