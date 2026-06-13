use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    auth::jwt::Claims,
    costing::service::{apply_weighted_average_cost, round_piastres},
    errors::{AppError, AppErrorResponse},
    inventory::movements::{record_movement, MovementParams},
    models::UserRole,
    permissions::checker::check_permission,
};
use utoipa::{IntoParams, ToSchema};

// ── Response models ───────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct Supplier {
    pub id:           Uuid,
    pub org_id:       Uuid,
    pub name:         String,
    pub contact_name: Option<String>,
    pub phone:        Option<String>,
    pub email:        Option<String>,
    pub is_active:    bool,
    pub created_at:   chrono::DateTime<chrono::Utc>,
    pub updated_at:   chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct PurchaseOrderLine {
    pub id:                      Uuid,
    pub purchase_order_id:       Uuid,
    pub org_ingredient_id:       Uuid,
    pub ingredient_name:         String,
    /// Ingredient's base stock unit.
    pub unit:                    String,
    pub purchase_unit:           String,
    pub units_per_purchase_unit: f64,
    pub quantity_ordered:        f64,
    pub quantity_received:       f64,
    /// Piastres per PURCHASE unit.
    pub unit_cost:               i64,
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct PurchaseOrder {
    pub id:            Uuid,
    pub org_id:        Uuid,
    pub branch_id:     Uuid,
    pub supplier_id:   Option<Uuid>,
    pub supplier_name: Option<String>,
    pub status:        String,
    pub reference:     Option<String>,
    pub note:          Option<String>,
    pub expected_at:   Option<chrono::DateTime<chrono::Utc>>,
    pub received_at:   Option<chrono::DateTime<chrono::Utc>>,
    pub received_by:   Option<Uuid>,
    pub created_by:    Uuid,
    pub created_at:    chrono::DateTime<chrono::Utc>,
    pub updated_at:    chrono::DateTime<chrono::Utc>,
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
    pub name:         String,
    pub contact_name: Option<String>,
    pub phone:        Option<String>,
    pub email:        Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateSupplierRequest {
    pub name:         Option<String>,
    pub contact_name: Option<String>,
    pub phone:        Option<String>,
    pub email:        Option<String>,
    pub is_active:    Option<bool>,
}

#[derive(Deserialize, ToSchema)]
pub struct POLineInput {
    pub org_ingredient_id:       Uuid,
    pub purchase_unit:           String,
    /// Stock units per purchase unit. Ignored when `purchase_unit` is a known
    /// inventory unit (the factor is derived from the ingredient's base unit).
    pub units_per_purchase_unit: Option<f64>,
    pub quantity_ordered:        f64,
    /// Piastres per purchase unit.
    pub unit_cost:               i64,
}

#[derive(Deserialize, ToSchema)]
pub struct CreatePurchaseOrderRequest {
    pub supplier_id: Option<Uuid>,
    pub reference:   Option<String>,
    pub note:        Option<String>,
    pub expected_at: Option<chrono::DateTime<chrono::Utc>>,
    pub lines:       Vec<POLineInput>,
}

#[derive(Deserialize, ToSchema)]
pub struct ReceiveLineInput {
    pub line_id:           Uuid,
    pub quantity_received: f64,
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
    req:    HttpRequest,
    pool:   web::Data<PgPool>,
    org_id: web::Path<Uuid>,
    body:   web::Json<CreateSupplierRequest>,
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
    req:    HttpRequest,
    pool:   web::Data<PgPool>,
    org_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "suppliers", "read").await?;
    require_org(&claims, *org_id)?;

    let rows = sqlx::query_as::<_, Supplier>(
        "SELECT id, org_id, name, contact_name, phone, email, is_active, created_at, updated_at \
         FROM suppliers WHERE org_id = $1 AND deleted_at IS NULL ORDER BY name ASC"
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
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    id:   web::Path<Uuid>,
    body: web::Json<UpdateSupplierRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "suppliers", "update").await?;

    let org_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT org_id FROM suppliers WHERE id = $1 AND deleted_at IS NULL"
    )
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
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    id:   web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "suppliers", "delete").await?;

    let org_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT org_id FROM suppliers WHERE id = $1 AND deleted_at IS NULL"
    )
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
    req:       HttpRequest,
    pool:      web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    body:      web::Json<CreatePurchaseOrderRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "purchase_orders", "create").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    if body.lines.is_empty() {
        return Err(AppError::BadRequest("a purchase order needs at least one line".into()));
    }

    let org_id: Uuid = sqlx::query_scalar(
        "SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL"
    )
    .bind(*branch_id)
    .fetch_optional(pool.get_ref())
    .await?
    .flatten()
    .ok_or_else(|| AppError::NotFound("Branch not found".into()))?;

    if let Some(sup) = body.supplier_id {
        let sup_org: Option<Uuid> = sqlx::query_scalar(
            "SELECT org_id FROM suppliers WHERE id = $1 AND deleted_at IS NULL"
        )
        .bind(sup).fetch_optional(pool.get_ref()).await?;
        if sup_org != Some(org_id) {
            return Err(AppError::BadRequest("Supplier does not belong to this organization".into()));
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
            return Err(AppError::BadRequest("quantity_ordered must be greater than 0".into()));
        }
        if line.unit_cost < 0 {
            return Err(AppError::BadRequest("unit_cost cannot be negative".into()));
        }

        // Validate ingredient belongs to org + resolve its base stock unit.
        let base_unit: Option<String> = sqlx::query_scalar(
            "SELECT unit::text FROM org_ingredients WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL"
        )
        .bind(line.org_ingredient_id)
        .bind(org_id)
        .fetch_optional(&mut *tx)
        .await?;
        let base_unit = base_unit
            .ok_or_else(|| AppError::BadRequest("Ingredient not found in this organization".into()))?;

        // The purchase unit must be a real stock unit in the ingredient's
        // measure: g/kg for weight, ml/l for volume, pcs for count. No free-text
        // packs — so the pack factor is always derived, never trusted from the
        // client (`units_per_purchase_unit` is ignored).
        if !crate::units::is_valid_unit(&line.purchase_unit) {
            return Err(AppError::BadRequest(
                "Purchase unit must be one of: g, kg, ml, l, pcs.".into(),
            ));
        }
        let factor = crate::units::convert(1.0, &line.purchase_unit, &base_unit).map_err(|_| {
            AppError::BadRequest(
                "Purchase unit must match the ingredient's measure (g/kg for weight, ml/l for volume, pcs for count).".into(),
            )
        })?;

        sqlx::query(
            "INSERT INTO purchase_order_lines \
                 (purchase_order_id, org_ingredient_id, purchase_unit, units_per_purchase_unit, \
                  quantity_ordered, unit_cost) \
             VALUES ($1, $2, $3, $4, $5, $6)"
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
    req:       HttpRequest,
    pool:      web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query:     web::Query<ListOrdersQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "purchase_orders", "read").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    let rows = sqlx::query_as::<_, PurchaseOrder>(
        r#"
        SELECT po.id, po.org_id, po.branch_id, po.supplier_id,
               s.name AS supplier_name,
               po.status::text, po.reference, po.note, po.expected_at,
               po.received_at, po.received_by, po.created_by, po.created_at, po.updated_at
        FROM purchase_orders po
        LEFT JOIN suppliers s ON s.id = po.supplier_id
        WHERE po.branch_id = $1
          AND ($2::text        IS NULL OR po.status::text = $2)
          AND ($3::timestamptz IS NULL OR po.expected_at <= $3)
        ORDER BY po.created_at DESC
        "#,
    )
    .bind(*branch_id)
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
    req:    HttpRequest,
    pool:   web::Data<PgPool>,
    org_id: web::Path<Uuid>,
    query:  web::Query<ListOrdersQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "purchase_orders", "read").await?;
    require_org(&claims, *org_id)?;

    let rows = sqlx::query_as::<_, PurchaseOrder>(
        r#"
        SELECT po.id, po.org_id, po.branch_id, po.supplier_id,
               s.name AS supplier_name,
               po.status::text, po.reference, po.note, po.expected_at,
               po.received_at, po.received_by, po.created_by, po.created_at, po.updated_at
        FROM purchase_orders po
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
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    id:   web::Path<Uuid>,
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
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    id:   web::Path<Uuid>,
    body: web::Json<ReceivePurchaseOrderRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "purchase_orders", "update").await?;
    let order = fetch_order_or_404(pool.get_ref(), *id).await?;
    require_branch_access(pool.get_ref(), &claims, order.branch_id).await?;

    if order.status == "received" || order.status == "cancelled" {
        return Err(AppError::Conflict("Purchase order is already received or cancelled".into()));
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

    // Lock the PO row and re-check status INSIDE the tx: two concurrent receives
    // must not both pass the status gate and double-apply stock/WAC (V7).
    let locked_status: String = sqlx::query_scalar(
        "SELECT status::text FROM purchase_orders WHERE id = $1 FOR UPDATE"
    )
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

        // Load the line (and verify it belongs to this PO).
        let line: Option<(Uuid, f64, i64)> = sqlx::query_as(
            "SELECT org_ingredient_id, units_per_purchase_unit::float8, unit_cost \
             FROM purchase_order_lines WHERE id = $1 AND purchase_order_id = $2"
        )
        .bind(recv.line_id)
        .bind(*id)
        .fetch_optional(&mut *tx)
        .await?;
        let (ing_id, factor, unit_cost) = line
            .ok_or_else(|| AppError::BadRequest("Line does not belong to this purchase order".into()))?;

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
        apply_weighted_average_cost(&mut *tx, ing_id, stock_qty_dec, cost_per_stock_unit_dec, claims.user_id()).await?;

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

        record_movement(&mut *tx, MovementParams {
            branch_id:           order.branch_id,
            org_ingredient_id:   ing_id,
            branch_inventory_id: Some(bi_id),
            movement_type:       "purchase_in",
            quantity:            stock_qty,
            balance_after:       Some(balance),
            unit_cost:           Some(cost_per_stock_unit),
            reason:              None,
            below_zero:          false,
            source_type:         Some("purchase"),
            source_id:           Some(*id),
            note:                Some("Purchase received"),
            created_by:          Some(claims.user_id()),
        })
        .await?;

        sqlx::query(
            "UPDATE purchase_order_lines \
             SET quantity_received = quantity_received + $2 WHERE id = $1"
        )
        .bind(recv.line_id)
        .bind(recv.quantity_received)
        .execute(&mut *tx)
        .await?;
    }

    // Resulting status from the lines' received-vs-ordered totals: fully
    // received closes the PO; a non-zero partial leaves it open for the rest.
    let (all_full, any_received): (Option<bool>, Option<bool>) = sqlx::query_as(
        "SELECT bool_and(quantity_received >= quantity_ordered), \
                bool_or(quantity_received > 0) \
         FROM purchase_order_lines WHERE purchase_order_id = $1"
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

    let order = sqlx::query_as::<_, PurchaseOrder>(
        r#"
        UPDATE purchase_orders
        SET status      = $3::purchase_order_status,
            received_at = CASE WHEN $4 THEN now() ELSE received_at END,
            received_by = CASE WHEN $4 THEN $2 ELSE received_by END,
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
    .bind(is_full)
    .fetch_one(&mut *tx)
    .await?;

    let lines = fetch_lines(&mut *tx, *id).await?;
    tx.commit().await?;
    Ok(HttpResponse::Ok().json(PurchaseOrderFull { order, lines }))
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
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    id:   web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "purchase_orders", "update").await?;
    let order = fetch_order_or_404(pool.get_ref(), *id).await?;
    require_branch_access(pool.get_ref(), &claims, order.branch_id).await?;

    if order.status == "received" {
        return Err(AppError::Conflict("Cannot cancel a received purchase order".into()));
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

fn require_org(claims: &Claims, org_id: Uuid) -> Result<(), AppError> {
    if claims.role == UserRole::SuperAdmin { return Ok(()); }
    if claims.org_id() != Some(org_id) {
        return Err(AppError::Forbidden("Access denied to this org".into()));
    }
    Ok(())
}

async fn require_branch_access(
    pool:      &PgPool,
    claims:    &Claims,
    branch_id: Uuid,
) -> Result<(), AppError> {
    if claims.role == UserRole::SuperAdmin { return Ok(()); }

    let branch_org: Option<Uuid> = sqlx::query_scalar(
        "SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL"
    )
    .bind(branch_id)
    .fetch_optional(pool)
    .await?
    .flatten();

    let branch_org = branch_org.ok_or_else(|| AppError::NotFound("Branch not found".into()))?;

    if claims.org_id() != Some(branch_org) {
        return Err(AppError::Forbidden("Branch belongs to a different org".into()));
    }

    if claims.role == UserRole::OrgAdmin { return Ok(()); }

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

    Ok(())
}
