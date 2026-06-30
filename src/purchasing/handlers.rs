use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    auth::jwt::Claims,
    costing::service::{apply_weighted_average_cost, round_piastres},
    errors::{AppError, AppErrorResponse},
    inventory::movements::{MovementParams, record_movement},
    models::UserRole,
    permissions::checker::check_permission,
};
use utoipa::{IntoParams, ToSchema};

// ── Response models ───────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct Supplier {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub contact_name: Option<String>,
    pub phone: Option<String>,
    pub email: Option<String>,
    pub is_active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct PurchaseOrderLine {
    pub id: Uuid,
    pub purchase_order_id: Uuid,
    pub org_ingredient_id: Uuid,
    pub ingredient_name: String,
    /// Ingredient's base stock unit.
    pub unit: String,
    pub purchase_unit: String,
    pub units_per_purchase_unit: f64,
    pub quantity_ordered: f64,
    pub quantity_received: f64,
    /// Piastres per PURCHASE unit.
    pub unit_cost: i64,
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct PurchaseOrder {
    pub id: Uuid,
    pub org_id: Uuid,
    pub branch_id: Uuid,
    /// Branch label — populated by the order lists so the "All branches" view
    /// can show which branch each PO belongs to; other endpoints leave it null.
    #[serde(default)]
    #[sqlx(default)]
    pub branch_name: Option<String>,
    pub supplier_id: Option<Uuid>,
    pub supplier_name: Option<String>,
    pub status: String,
    pub reference: Option<String>,
    pub note: Option<String>,
    pub expected_at: Option<chrono::DateTime<chrono::Utc>>,
    pub received_at: Option<chrono::DateTime<chrono::Utc>>,
    pub received_by: Option<Uuid>,
    pub created_by: Uuid,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct PurchaseOrderFull {
    #[serde(flatten)]
    pub order: PurchaseOrder,
    pub lines: Vec<PurchaseOrderLine>,
}

// ── Request types ─────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct CreateSupplierRequest {
    pub name: String,
    pub contact_name: Option<String>,
    pub phone: Option<String>,
    pub email: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateSupplierRequest {
    pub name: Option<String>,
    pub contact_name: Option<String>,
    pub phone: Option<String>,
    pub email: Option<String>,
    pub is_active: Option<bool>,
}

#[derive(Deserialize, ToSchema)]
pub struct POLineInput {
    pub org_ingredient_id: Uuid,
    pub purchase_unit: String,
    /// Stock units per purchase unit. Ignored when `purchase_unit` is a known
    /// inventory unit (the factor is derived from the ingredient's base unit).
    pub units_per_purchase_unit: Option<f64>,
    pub quantity_ordered: f64,
    /// Piastres per purchase unit.
    pub unit_cost: i64,
}

#[derive(Deserialize, ToSchema)]
pub struct CreatePurchaseOrderRequest {
    pub supplier_id: Option<Uuid>,
    pub reference: Option<String>,
    pub note: Option<String>,
    pub expected_at: Option<chrono::DateTime<chrono::Utc>>,
    pub lines: Vec<POLineInput>,
}

#[derive(Deserialize, ToSchema)]
pub struct ReceiveLineInput {
    pub line_id: Uuid,
    pub quantity_received: f64,
    /// Optional ACTUAL invoice cost (piastres per purchase unit) for this
    /// delivery, when it differs from the ordered price. Drives weighted-average
    /// cost + the ledger; omitted ⟹ the PO line's ordered cost is used.
    pub unit_cost: Option<i64>,
}

#[derive(Deserialize, ToSchema)]
pub struct ReceivePurchaseOrderRequest {
    pub lines: Vec<ReceiveLineInput>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListOrdersQuery {
    /// Filter by status: draft | ordered | partially_received | received | cancelled.
    pub status: Option<String>,
    /// Only orders expected on or before this instant (for "arriving by" views).
    pub expected_before: Option<chrono::DateTime<chrono::Utc>>,
}

// ── Suppliers ─────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/purchasing/orgs/{org_id}/suppliers",
    tag = "purchasing",
    params(("org_id" = Uuid, Path, description = "Organization ID")),
    request_body = CreateSupplierRequest,
    responses((status = 201, description = "Supplier created", body = Supplier), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_supplier(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    org_id: web::Path<Uuid>,
    body: web::Json<CreateSupplierRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "suppliers", "create").await?;
    require_org(&claims, *org_id)?;
    if body.name.trim().is_empty() {
        return Err(AppError::BadRequest("name cannot be empty".into()));
    }

    let row = sqlx::query_as::<_, Supplier>(
        r#"
        INSERT INTO suppliers (org_id, name, contact_name, phone, email)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id, org_id, name, contact_name, phone, email, is_active, created_at, updated_at
        "#,
    )
    .bind(*org_id)
    .bind(body.name.trim())
    .bind(&body.contact_name)
    .bind(&body.phone)
    .bind(&body.email)
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Created().json(row))
}

#[utoipa::path(
    get,
    path = "/purchasing/orgs/{org_id}/suppliers",
    tag = "purchasing",
    params(("org_id" = Uuid, Path, description = "Organization ID")),
    responses((status = 200, description = "List suppliers", body = Vec<Supplier>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_suppliers(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    org_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "suppliers", "read").await?;
    require_org(&claims, *org_id)?;

    let rows = sqlx::query_as::<_, Supplier>(
        "SELECT id, org_id, name, contact_name, phone, email, is_active, created_at, updated_at \
         FROM suppliers WHERE org_id = $1 AND deleted_at IS NULL ORDER BY name ASC",
    )
    .bind(*org_id)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    patch,
    path = "/purchasing/suppliers/{id}",
    tag = "purchasing",
    params(("id" = Uuid, Path, description = "Supplier ID")),
    request_body = UpdateSupplierRequest,
    responses((status = 200, description = "Supplier updated", body = Supplier), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_supplier(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
    body: web::Json<UpdateSupplierRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "suppliers", "update").await?;

    let org_id: Option<Uuid> =
        sqlx::query_scalar("SELECT org_id FROM suppliers WHERE id = $1 AND deleted_at IS NULL")
            .bind(*id)
            .fetch_optional(pool.get_ref())
            .await?;
    let org_id = org_id.ok_or_else(|| AppError::NotFound("Supplier not found".into()))?;
    require_org(&claims, org_id)?;

    let row = sqlx::query_as::<_, Supplier>(
        r#"
        UPDATE suppliers SET
            name         = COALESCE($2, name),
            contact_name = COALESCE($3, contact_name),
            phone        = COALESCE($4, phone),
            email        = COALESCE($5, email),
            is_active    = COALESCE($6, is_active),
            updated_at   = now()
        WHERE id = $1 AND deleted_at IS NULL
        RETURNING id, org_id, name, contact_name, phone, email, is_active, created_at, updated_at
        "#,
    )
    .bind(*id)
    .bind(&body.name)
    .bind(&body.contact_name)
    .bind(&body.phone)
    .bind(&body.email)
    .bind(body.is_active)
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(row))
}

#[utoipa::path(
    delete,
    path = "/purchasing/suppliers/{id}",
    tag = "purchasing",
    params(("id" = Uuid, Path, description = "Supplier ID")),
    responses((status = 204, description = "Supplier deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_supplier(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "suppliers", "delete").await?;

    let org_id: Option<Uuid> =
        sqlx::query_scalar("SELECT org_id FROM suppliers WHERE id = $1 AND deleted_at IS NULL")
            .bind(*id)
            .fetch_optional(pool.get_ref())
            .await?;
    let org_id = org_id.ok_or_else(|| AppError::NotFound("Supplier not found".into()))?;
    require_org(&claims, org_id)?;

    sqlx::query("UPDATE suppliers SET deleted_at = now() WHERE id = $1")
        .bind(*id)
        .execute(pool.get_ref())
        .await?;

    Ok(HttpResponse::NoContent().finish())
}

// ── Purchase orders ───────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/purchasing/branches/{branch_id}/orders",
    tag = "purchasing",
    operation_id = "create_purchase_order",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    request_body = CreatePurchaseOrderRequest,
    responses((status = 201, description = "Purchase order created", body = PurchaseOrderFull), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_order(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    body: web::Json<CreatePurchaseOrderRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "purchase_orders", "create").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    if body.lines.is_empty() {
        return Err(AppError::BadRequest(
            "a purchase order needs at least one line".into(),
        ));
    }

    let org_id: Uuid =
        sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL")
            .bind(*branch_id)
            .fetch_optional(pool.get_ref())
            .await?
            .flatten()
            .ok_or_else(|| AppError::NotFound("Branch not found".into()))?;

    if let Some(sup) = body.supplier_id {
        let sup_org: Option<Uuid> =
            sqlx::query_scalar("SELECT org_id FROM suppliers WHERE id = $1 AND deleted_at IS NULL")
                .bind(sup)
                .fetch_optional(pool.get_ref())
                .await?;
        if sup_org != Some(org_id) {
            return Err(AppError::BadRequest(
                "Supplier does not belong to this organization".into(),
            ));
        }
    }

    let mut tx = pool.get_ref().begin().await?;

    let order = sqlx::query_as::<_, PurchaseOrder>(
        r#"
        INSERT INTO purchase_orders (org_id, branch_id, supplier_id, reference, note, expected_at, created_by)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING id, org_id, branch_id, supplier_id,
                  (SELECT name FROM suppliers WHERE id = supplier_id) AS supplier_name,
                  status::text, reference, note, expected_at, received_at, received_by,
                  created_by, created_at, updated_at
        "#,
    )
    .bind(org_id)
    .bind(*branch_id)
    .bind(body.supplier_id)
    .bind(&body.reference)
    .bind(&body.note)
    .bind(body.expected_at)
    .bind(claims.user_id())
    .fetch_one(&mut *tx)
    .await?;

    for line in &body.lines {
        if line.quantity_ordered <= 0.0 {
            return Err(AppError::BadRequest(
                "quantity_ordered must be greater than 0".into(),
            ));
        }
        if line.unit_cost < 0 {
            return Err(AppError::BadRequest("unit_cost cannot be negative".into()));
        }

        // Validate ingredient belongs to org + resolve its base unit and pack.
        let ing: Option<(String, Option<String>, Option<rust_decimal::Decimal>)> = sqlx::query_as(
            "SELECT unit::text, pack_unit, pack_size \
             FROM org_ingredients WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL",
        )
        .bind(line.org_ingredient_id)
        .bind(org_id)
        .fetch_optional(&mut *tx)
        .await?;
        let (base_unit, pack_unit, pack_size) = ing.ok_or_else(|| {
            AppError::BadRequest("Ingredient not found in this organization".into())
        })?;

        // Factor = how many BASE STOCK units one purchase unit yields. Two paths,
        // both server-derived (the client's units_per_purchase_unit is ignored):
        //   * a named pack matching the ingredient's pack_unit → its pack_size;
        //   * otherwise a built-in measure unit (g/kg/ml/l/pcs) → unit conversion.
        let is_named_pack = pack_unit
            .as_deref()
            .map(|pu| !pu.is_empty() && line.purchase_unit.eq_ignore_ascii_case(pu))
            .unwrap_or(false);
        let factor: f64 = if is_named_pack {
            use rust_decimal::prelude::ToPrimitive;
            pack_size
                .and_then(|ps| ps.to_f64())
                .filter(|f| *f > 0.0)
                .ok_or_else(|| AppError::BadRequest(
                    "This ingredient's pack size is not configured (set pack_size on the catalog item).".into(),
                ))?
        } else {
            if !crate::units::is_valid_unit(&line.purchase_unit) {
                return Err(AppError::BadRequest(format!(
                    "Purchase unit must be one of g, kg, ml, l, pcs{}.",
                    pack_unit
                        .as_deref()
                        .filter(|p| !p.is_empty())
                        .map(|p| format!(" or the configured pack \"{p}\""))
                        .unwrap_or_default()
                )));
            }
            crate::units::convert(1.0, &line.purchase_unit, &base_unit).map_err(|_| {
                AppError::BadRequest(
                    "Purchase unit must match the ingredient's measure (g/kg for weight, ml/l for volume, pcs for count).".into(),
                )
            })?
        };

        sqlx::query(
            "INSERT INTO purchase_order_lines \
                 (purchase_order_id, org_ingredient_id, purchase_unit, units_per_purchase_unit, \
                  quantity_ordered, unit_cost) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(order.id)
        .bind(line.org_ingredient_id)
        .bind(&line.purchase_unit)
        .bind(factor)
        .bind(line.quantity_ordered)
        .bind(line.unit_cost)
        .execute(&mut *tx)
        .await?;
    }

    let lines = fetch_lines(&mut *tx, order.id).await?;
    tx.commit().await?;
    Ok(HttpResponse::Created().json(PurchaseOrderFull { order, lines }))
}

#[utoipa::path(
    get,
    path = "/purchasing/branches/{branch_id}/orders",
    tag = "purchasing",
    operation_id = "list_purchase_orders",
    params(("branch_id" = Uuid, Path, description = "Branch ID"), ListOrdersQuery),
    responses((status = 200, description = "List purchase orders", body = Vec<PurchaseOrder>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_orders(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query: web::Query<ListOrdersQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "purchase_orders", "read").await?;

    // nil UUID = every branch in the caller's org ("All branches"); otherwise the
    // one branch after the usual access check.
    let (scope_condition, scope_id): (&str, Uuid) = if branch_id.is_nil() {
        let org = claims
            .scope_org(crate::auth::middleware::header_org_id(&req))
            .ok_or_else(|| AppError::Forbidden("No organization in scope".into()))?;
        (
            "po.branch_id IN (SELECT id FROM branches WHERE org_id = $1 AND deleted_at IS NULL)",
            org,
        )
    } else {
        require_branch_access(pool.get_ref(), &claims, *branch_id).await?;
        ("po.branch_id = $1", *branch_id)
    };

    let sql = format!(
        r#"
        SELECT po.id, po.org_id, po.branch_id, b.name AS branch_name, po.supplier_id,
               s.name AS supplier_name,
               po.status::text, po.reference, po.note, po.expected_at,
               po.received_at, po.received_by, po.created_by, po.created_at, po.updated_at
        FROM purchase_orders po
        JOIN branches b       ON b.id = po.branch_id
        LEFT JOIN suppliers s ON s.id = po.supplier_id
        WHERE {scope_condition}
          AND ($2::text        IS NULL OR po.status::text = $2)
          AND ($3::timestamptz IS NULL OR po.expected_at <= $3)
        ORDER BY po.created_at DESC
        "#
    );
    let rows = sqlx::query_as::<_, PurchaseOrder>(&sql)
        .bind(scope_id)
        .bind(&query.status)
        .bind(query.expected_before)
        .fetch_all(pool.get_ref())
        .await?;

    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    get,
    path = "/purchasing/orgs/{org_id}/orders",
    tag = "purchasing",
    operation_id = "list_org_purchase_orders",
    params(("org_id" = Uuid, Path, description = "Organization ID"), ListOrdersQuery),
    responses((status = 200, description = "List purchase orders across the org", body = Vec<PurchaseOrder>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_org_orders(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    org_id: web::Path<Uuid>,
    query: web::Query<ListOrdersQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "purchase_orders", "read").await?;
    require_org(&claims, *org_id)?;

    let rows = sqlx::query_as::<_, PurchaseOrder>(
        r#"
        SELECT po.id, po.org_id, po.branch_id, b.name AS branch_name, po.supplier_id,
               s.name AS supplier_name,
               po.status::text, po.reference, po.note, po.expected_at,
               po.received_at, po.received_by, po.created_by, po.created_at, po.updated_at
        FROM purchase_orders po
        JOIN branches b       ON b.id = po.branch_id
        LEFT JOIN suppliers s ON s.id = po.supplier_id
        WHERE po.org_id = $1
          AND ($2::text        IS NULL OR po.status::text = $2)
          AND ($3::timestamptz IS NULL OR po.expected_at <= $3)
        ORDER BY po.expected_at ASC NULLS LAST, po.created_at DESC
        "#,
    )
    .bind(*org_id)
    .bind(&query.status)
    .bind(query.expected_before)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    get,
    path = "/purchasing/orders/{id}",
    tag = "purchasing",
    operation_id = "get_purchase_order",
    params(("id" = Uuid, Path, description = "Purchase order ID")),
    responses((status = 200, description = "Purchase order detail", body = PurchaseOrderFull), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_order(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "purchase_orders", "read").await?;
    let order = fetch_order_or_404(pool.get_ref(), *id).await?;
    require_branch_access(pool.get_ref(), &claims, order.branch_id).await?;

    let lines = fetch_lines(pool.get_ref(), *id).await?;
    Ok(HttpResponse::Ok().json(PurchaseOrderFull { order, lines }))
}

#[utoipa::path(
    post,
    path = "/purchasing/orders/{id}/receive",
    tag = "purchasing",
    operation_id = "receive_purchase_order",
    params(("id" = Uuid, Path, description = "Purchase order ID")),
    request_body = ReceivePurchaseOrderRequest,
    responses((status = 200, description = "Order received: stock + cost updated", body = PurchaseOrderFull), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn receive_order(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
    body: web::Json<ReceivePurchaseOrderRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "purchase_orders", "update").await?;
    let order = fetch_order_or_404(pool.get_ref(), *id).await?;
    require_branch_access(pool.get_ref(), &claims, order.branch_id).await?;

    if order.status == "received" || order.status == "cancelled" {
        return Err(AppError::Conflict(
            "Purchase order is already received or cancelled".into(),
        ));
    }
    if body.lines.is_empty() {
        return Err(AppError::BadRequest("no lines to receive".into()));
    }

    // Reject duplicate line_ids in one request — they would double-apply
    // stock/cost for the same line (V8).
    {
        let mut seen = std::collections::HashSet::new();
        for recv in &body.lines {
            if !seen.insert(recv.line_id) {
                return Err(AppError::BadRequest(
                    "Duplicate line_id in receive request".into(),
                ));
            }
        }
    }

    let mut tx = pool.get_ref().begin().await?;

    // (po_line_id, org_ingredient_id, base-unit qty, piastres/stock-unit) for the
    // goods-receipt record created at the end of this delivery.
    let mut receipt_lines: Vec<(Uuid, Uuid, f64, i64)> = Vec::new();

    // Lock the PO row and re-check status INSIDE the tx: two concurrent receives
    // must not both pass the status gate and double-apply stock/WAC (V7).
    let locked_status: String =
        sqlx::query_scalar("SELECT status::text FROM purchase_orders WHERE id = $1 FOR UPDATE")
            .bind(*id)
            .fetch_one(&mut *tx)
            .await?;
    if locked_status == "received" || locked_status == "cancelled" {
        return Err(AppError::Conflict(
            "Purchase order is already received or cancelled".into(),
        ));
    }

    for recv in &body.lines {
        if recv.quantity_received <= 0.0 {
            continue;
        }

        // Load + lock the line (and verify it belongs to this PO).
        let line: Option<(Uuid, f64, i64, f64, f64)> = sqlx::query_as(
            "SELECT org_ingredient_id, units_per_purchase_unit::float8, unit_cost, \
                    quantity_ordered::float8, quantity_received::float8 \
             FROM purchase_order_lines WHERE id = $1 AND purchase_order_id = $2 FOR UPDATE",
        )
        .bind(recv.line_id)
        .bind(*id)
        .fetch_optional(&mut *tx)
        .await?;
        let (ing_id, factor, unit_cost, qty_ordered, qty_received_so_far) =
            line.ok_or_else(|| {
                AppError::BadRequest("Line does not belong to this purchase order".into())
            })?;

        // Over-receive guard: cumulative received can't exceed ordered. Over-
        // receipt is almost always a data-entry slip; if a supplier genuinely
        // sent more, the operator amends the order quantity. (float tolerance)
        if qty_received_so_far + recv.quantity_received > qty_ordered + 1e-6 {
            return Err(AppError::BadRequest(format!(
                "Cannot receive {} — only {} of {} ordered remain on this line.",
                recv.quantity_received,
                (qty_ordered - qty_received_so_far).max(0.0),
                qty_ordered
            )));
        }

        // Price variance: the ACTUAL invoice cost (if supplied on the receive)
        // overrides the ordered cost for WAC + the ledger. Negative is rejected.
        if let Some(actual) = recv.unit_cost
            && actual < 0
        {
            return Err(AppError::BadRequest("unit_cost cannot be negative".into()));
        }
        let unit_cost = recv.unit_cost.unwrap_or(unit_cost);

        let stock_qty = recv.quantity_received * factor;
        // Piastres per base stock unit — kept at 2 dp so a cheap-per-base-unit
        // ingredient (e.g. 400 piastres/kg = 0.40/g) is NOT rounded down to 0
        // ("free") before it reaches the numeric(15,2) cost_per_unit (V10).
        let cost_per_stock_unit_dec: Decimal = if factor > 0.0 {
            (Decimal::from(unit_cost) / Decimal::from_f64_retain(factor).unwrap_or(Decimal::ONE))
                .round_dp(2)
        } else {
            Decimal::from(unit_cost)
        };
        let stock_qty_dec = Decimal::from_f64_retain(stock_qty)
            .unwrap_or(Decimal::ZERO)
            .round_dp(3);
        // The movement ledger unit_cost is a bigint column → whole-piastre snapshot.
        let cost_per_stock_unit = round_piastres(cost_per_stock_unit_dec);

        // Weighted-average cost must read PRIOR on-hand → before adding stock.
        apply_weighted_average_cost(
            &mut *tx,
            order.branch_id,
            ing_id,
            stock_qty_dec,
            cost_per_stock_unit_dec,
            claims.user_id(),
        )
        .await?;

        // Upsert branch stock (+received).
        let (bi_id, balance): (Uuid, f64) = sqlx::query_as(
            r#"
            INSERT INTO branch_inventory (branch_id, org_ingredient_id, current_stock, reorder_threshold)
            VALUES ($1, $2, $3, 0)
            ON CONFLICT (branch_id, org_ingredient_id)
            DO UPDATE SET current_stock = branch_inventory.current_stock + EXCLUDED.current_stock
            RETURNING id, current_stock::float8
            "#,
        )
        .bind(order.branch_id)
        .bind(ing_id)
        .bind(stock_qty)
        .fetch_one(&mut *tx)
        .await?;

        record_movement(
            &mut *tx,
            MovementParams {
                branch_id: order.branch_id,
                org_ingredient_id: ing_id,
                branch_inventory_id: Some(bi_id),
                movement_type: "purchase_in",
                quantity: stock_qty,
                balance_after: Some(balance),
                unit_cost: Some(cost_per_stock_unit),
                reason: None,
                below_zero: false,
                source_type: Some("purchase"),
                source_id: Some(*id),
                note: Some("Purchase received"),
                created_by: Some(claims.user_id()),
            },
        )
        .await?;

        sqlx::query(
            "UPDATE purchase_order_lines \
             SET quantity_received = quantity_received + $2 WHERE id = $1",
        )
        .bind(recv.line_id)
        .bind(recv.quantity_received)
        .execute(&mut *tx)
        .await?;

        receipt_lines.push((recv.line_id, ing_id, stock_qty, cost_per_stock_unit));
    }

    // Resulting status from the lines' received-vs-ordered totals: fully
    // received closes the PO; a non-zero partial leaves it open for the rest.
    let (all_full, any_received): (Option<bool>, Option<bool>) = sqlx::query_as(
        "SELECT bool_and(quantity_received >= quantity_ordered), \
                bool_or(quantity_received > 0) \
         FROM purchase_order_lines WHERE purchase_order_id = $1",
    )
    .bind(*id)
    .fetch_one(&mut *tx)
    .await?;
    let is_full = all_full.unwrap_or(false);
    let new_status: String = if is_full {
        "received".into()
    } else if any_received.unwrap_or(false) {
        "partially_received".into()
    } else {
        order.status.clone()
    };

    // Stamp the receiver/time on EVERY receipt (partial included), so a
    // multi-shipment PO records who received the latest delivery and when.
    let order = sqlx::query_as::<_, PurchaseOrder>(
        r#"
        UPDATE purchase_orders
        SET status      = $3::purchase_order_status,
            received_at = now(),
            received_by = $2,
            updated_at  = now()
        WHERE id = $1
        RETURNING id, org_id, branch_id, supplier_id,
                  (SELECT name FROM suppliers WHERE id = supplier_id) AS supplier_name,
                  status::text, reference, note, expected_at, received_at, received_by,
                  created_by, created_at, updated_at
        "#,
    )
    .bind(*id)
    .bind(claims.user_id())
    .bind(&new_status)
    .fetch_one(&mut *tx)
    .await?;
    let _ = is_full;

    // Record this delivery as a first-class goods receipt (audit trail for
    // multi-shipment partials + per-line actual cost).
    if !receipt_lines.is_empty() {
        let receipt_id: Uuid = sqlx::query_scalar(
            "INSERT INTO goods_receipts (org_id, branch_id, purchase_order_id, supplier_id, is_return, received_by) \
             VALUES ($1, $2, $3, $4, false, $5) RETURNING id"
        )
        .bind(order.org_id)
        .bind(order.branch_id)
        .bind(*id)
        .bind(order.supplier_id)
        .bind(claims.user_id())
        .fetch_one(&mut *tx)
        .await?;
        for (po_line_id, ing_id, stock_qty, unit_cost) in &receipt_lines {
            sqlx::query(
                "INSERT INTO goods_receipt_lines \
                     (goods_receipt_id, purchase_order_line_id, org_ingredient_id, quantity, unit_cost) \
                 VALUES ($1, $2, $3, $4, $5)"
            )
            .bind(receipt_id)
            .bind(po_line_id)
            .bind(ing_id)
            .bind(stock_qty)
            .bind(unit_cost)
            .execute(&mut *tx)
            .await?;
        }
    }

    let lines = fetch_lines(&mut *tx, *id).await?;
    tx.commit().await?;
    Ok(HttpResponse::Ok().json(PurchaseOrderFull { order, lines }))
}

/// Place a draft PO with the supplier: `draft → ordered`. Makes "ordered,
/// awaiting goods" a distinct, queryable state (outstanding-orders views) vs a
/// draft still being built. Receiving is still allowed directly from draft for
/// workflows that don't formally place orders first.
#[utoipa::path(
    post,
    path = "/purchasing/orders/{id}/submit",
    tag = "purchasing",
    operation_id = "submit_purchase_order",
    params(("id" = Uuid, Path, description = "Purchase order ID")),
    responses((status = 200, description = "Purchase order placed (ordered)", body = PurchaseOrder), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn submit_order(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "purchase_orders", "update").await?;
    let order = fetch_order_or_404(pool.get_ref(), *id).await?;
    require_branch_access(pool.get_ref(), &claims, order.branch_id).await?;

    if order.status != "draft" {
        return Err(AppError::Conflict(
            "Only a draft purchase order can be placed (submitted).".into(),
        ));
    }

    let updated = sqlx::query_as::<_, PurchaseOrder>(
        r#"
        UPDATE purchase_orders SET status = 'ordered', updated_at = now()
        WHERE id = $1
        RETURNING id, org_id, branch_id, supplier_id,
                  (SELECT name FROM suppliers WHERE id = supplier_id) AS supplier_name,
                  status::text, reference, note, expected_at, received_at, received_by,
                  created_by, created_at, updated_at
        "#,
    )
    .bind(*id)
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(updated))
}

#[utoipa::path(
    post,
    path = "/purchasing/orders/{id}/cancel",
    tag = "purchasing",
    operation_id = "cancel_purchase_order",
    params(("id" = Uuid, Path, description = "Purchase order ID")),
    responses((status = 200, description = "Purchase order cancelled", body = PurchaseOrder), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn cancel_order(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "purchase_orders", "update").await?;
    let order = fetch_order_or_404(pool.get_ref(), *id).await?;
    require_branch_access(pool.get_ref(), &claims, order.branch_id).await?;

    if order.status == "received" || order.status == "partially_received" {
        return Err(AppError::Conflict(
            "Cannot cancel a purchase order that has already received stock. \
             Reverse the received goods (return to supplier / stock adjustment) first."
                .into(),
        ));
    }

    let updated = sqlx::query_as::<_, PurchaseOrder>(
        r#"
        UPDATE purchase_orders SET status = 'cancelled', updated_at = now()
        WHERE id = $1
        RETURNING id, org_id, branch_id, supplier_id,
                  (SELECT name FROM suppliers WHERE id = supplier_id) AS supplier_name,
                  status::text, reference, note, expected_at, received_at, received_by,
                  created_by, created_at, updated_at
        "#,
    )
    .bind(*id)
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(updated))
}

/// One ingredient to reorder, with the quantity needed to reach its order-up-to
/// level (par_max, else the reorder point).
#[derive(serde::Serialize, ToSchema)]
pub struct ReorderLine {
    pub org_ingredient_id: Uuid,
    pub ingredient_name: String,
    pub unit: String,
    pub current_stock: f64,
    /// Quantity (in base units) to bring stock up to the order-up-to level.
    pub suggested_qty: f64,
}

/// Reorder suggestions grouped by the ingredient's default supplier so the
/// dashboard can raise one draft PO per supplier.
#[derive(serde::Serialize, ToSchema)]
pub struct ReorderSuggestion {
    pub supplier_id: Option<Uuid>,
    pub supplier_name: Option<String>,
    pub lines: Vec<ReorderLine>,
}

// ── GET /purchasing/branches/:branch_id/reorder-suggestions ───

/// Ingredients at/below their reorder point (par_min, else reorder_threshold),
/// with the quantity to reach the order-up-to level (par_max), grouped by the
/// ingredient's default supplier — the basis for one-click "create PO".
#[utoipa::path(
    get,
    path = "/purchasing/branches/{branch_id}/reorder-suggestions",
    tag = "purchasing",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    responses((status = 200, description = "Reorder suggestions grouped by supplier", body = [ReorderSuggestion]), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn reorder_suggestions(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "purchase_orders", "read").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    // (supplier_id, supplier_name, org_ingredient_id, name, unit, current, suggested)
    type Row = (Option<Uuid>, Option<String>, Uuid, String, String, f64, f64);
    let rows: Vec<Row> = sqlx::query_as(
        r#"
        SELECT oi.supplier_id,
               (SELECT name FROM suppliers WHERE id = oi.supplier_id) AS supplier_name,
               bi.org_ingredient_id, oi.name AS ingredient_name, oi.unit::text AS unit,
               bi.current_stock::float8,
               GREATEST(COALESCE(bi.par_max, COALESCE(bi.par_min, bi.reorder_threshold)) - bi.current_stock, 0)::float8 AS suggested_qty
        FROM branch_inventory bi
        JOIN org_ingredients oi ON oi.id = bi.org_ingredient_id AND oi.deleted_at IS NULL
        WHERE bi.branch_id = $1
          AND COALESCE(bi.par_min, bi.reorder_threshold) > 0
          AND bi.current_stock <= COALESCE(bi.par_min, bi.reorder_threshold)
        ORDER BY oi.supplier_id NULLS LAST, oi.name
        "#,
    )
    .bind(*branch_id)
    .fetch_all(pool.get_ref())
    .await?;

    // Group consecutive rows by supplier (query is ordered by supplier_id).
    let mut groups: Vec<ReorderSuggestion> = Vec::new();
    for (
        supplier_id,
        supplier_name,
        org_ingredient_id,
        ingredient_name,
        unit,
        current_stock,
        suggested_qty,
    ) in rows
    {
        let line = ReorderLine {
            org_ingredient_id,
            ingredient_name,
            unit,
            current_stock,
            suggested_qty,
        };
        match groups.last_mut() {
            Some(g) if g.supplier_id == supplier_id => g.lines.push(line),
            _ => groups.push(ReorderSuggestion {
                supplier_id,
                supplier_name,
                lines: vec![line],
            }),
        }
    }

    Ok(HttpResponse::Ok().json(groups))
}

// ── Goods receipts + supplier returns ─────────────────────────

#[derive(serde::Serialize, sqlx::FromRow, ToSchema)]
pub struct GoodsReceiptLine {
    pub id: Uuid,
    pub purchase_order_line_id: Option<Uuid>,
    pub org_ingredient_id: Uuid,
    pub ingredient_name: String,
    /// Base stock units received (+) or returned (−).
    #[schema(value_type = f64)]
    pub quantity: f64,
    /// Piastres per base stock unit (actual).
    pub unit_cost: Option<i64>,
}

#[derive(serde::Serialize, ToSchema)]
pub struct GoodsReceipt {
    pub id: Uuid,
    pub branch_id: Uuid,
    pub purchase_order_id: Option<Uuid>,
    pub supplier_id: Option<Uuid>,
    pub supplier_name: Option<String>,
    /// true ⟹ a return to supplier (negative stock effect).
    pub is_return: bool,
    pub reference: Option<String>,
    pub note: Option<String>,
    pub received_by: Uuid,
    pub received_by_name: Option<String>,
    pub received_at: chrono::DateTime<chrono::Utc>,
    pub lines: Vec<GoodsReceiptLine>,
}

#[derive(Deserialize, ToSchema)]
pub struct ReturnLineInput {
    pub org_ingredient_id: Uuid,
    /// Base stock units to return (must be ≤ on hand).
    pub quantity: f64,
    /// Piastres per base stock unit; defaults to the branch's actual cost.
    pub unit_cost: Option<i64>,
}

#[derive(Deserialize, ToSchema)]
pub struct CreateReturnRequest {
    pub supplier_id: Option<Uuid>,
    pub purchase_order_id: Option<Uuid>,
    pub reference: Option<String>,
    pub note: Option<String>,
    pub lines: Vec<ReturnLineInput>,
}

// ── GET /purchasing/orders/:id/receipts ───────────────────────

/// Per-delivery goods-receipt records for a purchase order (multi-shipment audit
/// trail, each with the actual received quantity + cost per line).
#[utoipa::path(
    get,
    path = "/purchasing/orders/{id}/receipts",
    tag = "purchasing",
    params(("id" = Uuid, Path, description = "Purchase order ID")),
    responses((status = 200, description = "Goods receipts for this PO", body = [GoodsReceipt]), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_po_receipts(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "purchase_orders", "read").await?;
    let order = fetch_order_or_404(pool.get_ref(), *id).await?;
    require_branch_access(pool.get_ref(), &claims, order.branch_id).await?;
    Ok(HttpResponse::Ok().json(fetch_receipts(pool.get_ref(), *id).await?))
}

// ── POST /purchasing/branches/:branch_id/returns ──────────────

/// Return stock to a supplier: decrements branch stock and posts a
/// 'purchase_return' movement per line, recorded as a goods receipt with
/// is_return = true. Returns remove stock at its current cost (WAC unchanged).
#[utoipa::path(
    post,
    path = "/purchasing/branches/{branch_id}/returns",
    tag = "purchasing",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    request_body = CreateReturnRequest,
    responses((status = 201, description = "Return recorded", body = GoodsReceipt), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_return(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    body: web::Json<CreateReturnRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "purchase_orders", "update").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    if body.lines.is_empty() {
        return Err(AppError::BadRequest(
            "a return needs at least one line".into(),
        ));
    }
    let org_id: Uuid =
        sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL")
            .bind(*branch_id)
            .fetch_optional(pool.get_ref())
            .await?
            .flatten()
            .ok_or_else(|| AppError::NotFound("Branch not found".into()))?;
    if let Some(sup) = body.supplier_id {
        let s_org: Option<Uuid> =
            sqlx::query_scalar("SELECT org_id FROM suppliers WHERE id = $1 AND deleted_at IS NULL")
                .bind(sup)
                .fetch_optional(pool.get_ref())
                .await?
                .flatten();
        if s_org != Some(org_id) {
            return Err(AppError::BadRequest(
                "Supplier does not belong to this organization".into(),
            ));
        }
    }

    let mut tx = pool.get_ref().begin().await?;

    let receipt_id: Uuid = sqlx::query_scalar(
        "INSERT INTO goods_receipts \
             (org_id, branch_id, purchase_order_id, supplier_id, is_return, reference, note, received_by) \
         VALUES ($1, $2, $3, $4, true, $5, $6, $7) RETURNING id"
    )
    .bind(org_id)
    .bind(*branch_id)
    .bind(body.purchase_order_id)
    .bind(body.supplier_id)
    .bind(&body.reference)
    .bind(&body.note)
    .bind(claims.user_id())
    .fetch_one(&mut *tx)
    .await?;

    for line in &body.lines {
        if line.quantity <= 0.0 {
            return Err(AppError::BadRequest(
                "return quantity must be greater than 0".into(),
            ));
        }
        // Lock + validate stock; a return can't take more than is on hand, and
        // resolve the cost (caller-supplied actual, else the branch's cost).
        let row: Option<(Uuid, f64, Option<i64>)> = sqlx::query_as(
            "SELECT bi.id, bi.current_stock::float8, \
                    round(COALESCE(bi.cost_per_unit, oi.cost_per_unit))::bigint \
             FROM branch_inventory bi JOIN org_ingredients oi ON oi.id = bi.org_ingredient_id \
             WHERE bi.branch_id = $1 AND bi.org_ingredient_id = $2 FOR UPDATE OF bi",
        )
        .bind(*branch_id)
        .bind(line.org_ingredient_id)
        .fetch_optional(&mut *tx)
        .await?;
        let (bi_id, current, branch_cost) = row.ok_or_else(|| {
            AppError::BadRequest("Ingredient is not tracked at this branch".into())
        })?;
        if current < line.quantity {
            return Err(AppError::BadRequest(format!(
                "Cannot return {} — only {} on hand.",
                line.quantity, current
            )));
        }
        let unit_cost = line.unit_cost.or(branch_cost);

        let balance: f64 = sqlx::query_scalar(
            "UPDATE branch_inventory SET current_stock = current_stock - $1, updated_at = now() \
             WHERE id = $2 RETURNING current_stock::float8",
        )
        .bind(line.quantity)
        .bind(bi_id)
        .fetch_one(&mut *tx)
        .await?;

        record_movement(
            &mut *tx,
            crate::inventory::movements::MovementParams {
                branch_id: *branch_id,
                org_ingredient_id: line.org_ingredient_id,
                branch_inventory_id: Some(bi_id),
                movement_type: "purchase_return",
                quantity: -line.quantity,
                balance_after: Some(balance),
                unit_cost,
                reason: None,
                below_zero: balance < 0.0,
                source_type: Some("goods_receipt"),
                source_id: Some(receipt_id),
                note: Some("Return to supplier"),
                created_by: Some(claims.user_id()),
            },
        )
        .await?;

        sqlx::query(
            "INSERT INTO goods_receipt_lines \
                 (goods_receipt_id, org_ingredient_id, quantity, unit_cost) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(receipt_id)
        .bind(line.org_ingredient_id)
        .bind(-line.quantity)
        .bind(unit_cost)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    let receipt = fetch_receipts(pool.get_ref(), body.purchase_order_id.unwrap_or(receipt_id))
        .await
        .ok()
        .and_then(|v| v.into_iter().find(|r| r.id == receipt_id));
    // If not linked to a PO, fetch the single receipt directly.
    let receipt = match receipt {
        Some(r) => r,
        None => fetch_receipt(pool.get_ref(), receipt_id).await?,
    };
    Ok(HttpResponse::Created().json(receipt))
}

// ── Helpers ───────────────────────────────────────────────────

async fn fetch_lines<'e, E>(executor: E, po_id: Uuid) -> Result<Vec<PurchaseOrderLine>, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let lines = sqlx::query_as::<_, PurchaseOrderLine>(
        r#"
        SELECT l.id, l.purchase_order_id, l.org_ingredient_id,
               oi.name AS ingredient_name, oi.unit::text AS unit,
               l.purchase_unit, l.units_per_purchase_unit::float8,
               l.quantity_ordered::float8, l.quantity_received::float8, l.unit_cost
        FROM purchase_order_lines l
        JOIN org_ingredients oi ON oi.id = l.org_ingredient_id
        WHERE l.purchase_order_id = $1
        ORDER BY oi.name ASC
        "#,
    )
    .bind(po_id)
    .fetch_all(executor)
    .await?;
    Ok(lines)
}

async fn fetch_order_or_404(pool: &PgPool, id: Uuid) -> Result<PurchaseOrder, AppError> {
    sqlx::query_as::<_, PurchaseOrder>(
        r#"
        SELECT po.id, po.org_id, po.branch_id, po.supplier_id,
               s.name AS supplier_name,
               po.status::text, po.reference, po.note, po.expected_at,
               po.received_at, po.received_by, po.created_by, po.created_at, po.updated_at
        FROM purchase_orders po
        LEFT JOIN suppliers s ON s.id = po.supplier_id
        WHERE po.id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Purchase order not found".into()))
}

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

/// All goods receipts for a purchase order (header + lines), newest first.
async fn fetch_receipts(pool: &PgPool, po_id: Uuid) -> Result<Vec<GoodsReceipt>, AppError> {
    fetch_receipts_where(pool, "gr.purchase_order_id = $1", po_id).await
}

/// A single goods receipt by id.
async fn fetch_receipt(pool: &PgPool, receipt_id: Uuid) -> Result<GoodsReceipt, AppError> {
    fetch_receipts_where(pool, "gr.id = $1", receipt_id)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::NotFound("Goods receipt not found".into()))
}

async fn fetch_receipts_where(
    pool: &PgPool,
    cond: &str,
    id: Uuid,
) -> Result<Vec<GoodsReceipt>, AppError> {
    #[derive(sqlx::FromRow)]
    struct H {
        id: Uuid,
        branch_id: Uuid,
        purchase_order_id: Option<Uuid>,
        supplier_id: Option<Uuid>,
        supplier_name: Option<String>,
        is_return: bool,
        reference: Option<String>,
        note: Option<String>,
        received_by: Uuid,
        received_by_name: Option<String>,
        received_at: chrono::DateTime<chrono::Utc>,
    }
    let sql = format!(
        "SELECT gr.id, gr.branch_id, gr.purchase_order_id, gr.supplier_id, \
                (SELECT name FROM suppliers WHERE id = gr.supplier_id) AS supplier_name, \
                gr.is_return, gr.reference, gr.note, gr.received_by, \
                (SELECT name FROM users WHERE id = gr.received_by) AS received_by_name, \
                gr.received_at \
         FROM goods_receipts gr WHERE {cond} ORDER BY gr.received_at DESC"
    );
    let headers: Vec<H> = sqlx::query_as(&sql).bind(id).fetch_all(pool).await?;
    if headers.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<Uuid> = headers.iter().map(|h| h.id).collect();
    let line_rows: Vec<(Uuid, GoodsReceiptLine)> = sqlx::query_as::<
        _,
        (Uuid, Uuid, Option<Uuid>, Uuid, String, f64, Option<i64>),
    >(
        "SELECT grl.goods_receipt_id, grl.id, grl.purchase_order_line_id, grl.org_ingredient_id, \
                oi.name AS ingredient_name, grl.quantity::float8, grl.unit_cost \
         FROM goods_receipt_lines grl \
         JOIN org_ingredients oi ON oi.id = grl.org_ingredient_id \
         WHERE grl.goods_receipt_id = ANY($1) ORDER BY oi.name",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|(rid, id, pol, ing, name, qty, cost)| {
        (
            rid,
            GoodsReceiptLine {
                id,
                purchase_order_line_id: pol,
                org_ingredient_id: ing,
                ingredient_name: name,
                quantity: qty,
                unit_cost: cost,
            },
        )
    })
    .collect();

    Ok(headers
        .into_iter()
        .map(|h| {
            let lines = line_rows
                .iter()
                .filter(|(rid, _)| *rid == h.id)
                .map(|(_, l)| GoodsReceiptLine {
                    id: l.id,
                    purchase_order_line_id: l.purchase_order_line_id,
                    org_ingredient_id: l.org_ingredient_id,
                    ingredient_name: l.ingredient_name.clone(),
                    quantity: l.quantity,
                    unit_cost: l.unit_cost,
                })
                .collect();
            GoodsReceipt {
                id: h.id,
                branch_id: h.branch_id,
                purchase_order_id: h.purchase_order_id,
                supplier_id: h.supplier_id,
                supplier_name: h.supplier_name,
                is_return: h.is_return,
                reference: h.reference,
                note: h.note,
                received_by: h.received_by,
                received_by_name: h.received_by_name,
                received_at: h.received_at,
                lines,
            }
        })
        .collect())
}

fn require_org(claims: &Claims, org_id: Uuid) -> Result<(), AppError> {
    if claims.role == UserRole::SuperAdmin {
        return Ok(());
    }
    if claims.org_id() != Some(org_id) {
        return Err(AppError::Forbidden("Access denied to this org".into()));
    }
    Ok(())
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
