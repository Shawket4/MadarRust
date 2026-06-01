use actix_web::{web, HttpRequest, HttpResponse, HttpMessage};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::HashMap;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    auth::jwt::Claims,
    errors::{AppError, AppErrorResponse},
    permissions::checker::check_permission,
};

use crate::translation::ensure_translations;

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct OrgPaymentMethod {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub label_translations: serde_json::Value,
    pub color: String,
    pub icon: String,
    pub is_cash: bool,
    pub is_active: bool,
    pub display_order: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Deserialize, ToSchema)]
pub struct CreatePaymentMethodRequest {
    pub name: String,
    pub label_translations: HashMap<String, String>,
    pub color: String,
    pub icon: String,
    pub is_cash: bool,
    pub is_active: Option<bool>,
    pub display_order: Option<i32>,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdatePaymentMethodRequest {
    pub name: Option<String>,
    pub label_translations: Option<HashMap<String, String>>,
    pub color: Option<String>,
    pub icon: Option<String>,
    pub is_cash: Option<bool>,
    pub is_active: Option<bool>,
    pub display_order: Option<i32>,
}

// ── GET /payment-methods ──────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/payment-methods",
    tag = "payment_methods",
    responses((status = 200, description = "List of payment methods", body = Vec<OrgPaymentMethod>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_payment_methods(
    req: HttpRequest,
    pool: web::Data<PgPool>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    // Requires general read access, or specific if needed. Using orders read as proxy for now.
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;

    let org_id = claims.org_id().ok_or_else(|| AppError::Forbidden("No org id".into()))?;

    let rows = sqlx::query_as::<_, OrgPaymentMethod>(
        "SELECT id, org_id, name, label_translations, color, icon, is_cash, is_active, display_order, created_at, updated_at 
         FROM org_payment_methods 
         WHERE org_id = $1 
         ORDER BY display_order ASC, created_at ASC"
    )
    .bind(org_id)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

// ── POST /payment-methods ─────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/payment-methods",
    tag = "payment_methods",
    request_body = CreatePaymentMethodRequest,
    responses((status = 201, description = "Created payment method", body = OrgPaymentMethod), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_payment_method(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    mut body: web::Json<CreatePaymentMethodRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orgs", "create").await?;
    let org_id = claims.org_id().ok_or_else(|| AppError::Forbidden("No org id".into()))?;

    ensure_translations(&mut body.label_translations)
        .await
        .map_err(|_| AppError::Internal)?;

    let translations_json = serde_json::to_value(&body.label_translations)
        .map_err(|_| AppError::Internal)?;

    let method = sqlx::query_as::<_, OrgPaymentMethod>(
        r#"
        INSERT INTO org_payment_methods 
        (org_id, name, label_translations, color, icon, is_cash, is_active, display_order)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        RETURNING id, org_id, name, label_translations, color, icon, is_cash, is_active, display_order, created_at, updated_at
        "#
    )
    .bind(org_id)
    .bind(&body.name)
    .bind(translations_json)
    .bind(&body.color)
    .bind(&body.icon)
    .bind(body.is_cash)
    .bind(body.is_active.unwrap_or(true))
    .bind(body.display_order.unwrap_or(0))
    .fetch_one(pool.get_ref())
    .await
    .map_err(|e| {
        if let sqlx::Error::Database(db_err) = &e {
            if db_err.constraint() == Some("org_payment_methods_org_id_name_key") {
                return AppError::Conflict("A payment method with this name already exists for the organization".into());
            }
        }
        AppError::from(e)
    })?;

    Ok(HttpResponse::Created().json(method))
}

// ── PUT /payment-methods/:id ──────────────────────────────────────

#[utoipa::path(
    put,
    path = "/payment-methods/{id}",
    tag = "payment_methods",
    params(("id" = Uuid, Path, description = "Payment Method ID")),
    request_body = UpdatePaymentMethodRequest,
    responses((status = 200, description = "Updated payment method", body = OrgPaymentMethod), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_payment_method(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
    mut body: web::Json<UpdatePaymentMethodRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orgs", "update").await?;
    let org_id = claims.org_id().ok_or_else(|| AppError::Forbidden("No org id".into()))?;

    // Verify ownership
    let existing: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT label_translations FROM org_payment_methods WHERE id = $1 AND org_id = $2"
    )
    .bind(*id)
    .bind(org_id)
    .fetch_optional(pool.get_ref())
    .await?;

    if existing.is_none() {
        return Err(AppError::NotFound("Payment method not found".into()));
    }

    let mut update_translations = None;
    if let Some(ref mut tr) = body.label_translations {
        ensure_translations(tr).await
            .map_err(|_| AppError::Internal)?;
        update_translations = Some(serde_json::to_value(tr).unwrap());
    }

    let method = sqlx::query_as::<_, OrgPaymentMethod>(
        r#"
        UPDATE org_payment_methods 
        SET 
            name = COALESCE($1, name),
            label_translations = COALESCE($2, label_translations),
            color = COALESCE($3, color),
            icon = COALESCE($4, icon),
            is_cash = COALESCE($5, is_cash),
            is_active = COALESCE($6, is_active),
            display_order = COALESCE($7, display_order)
        WHERE id = $8 AND org_id = $9
        RETURNING id, org_id, name, label_translations, color, icon, is_cash, is_active, display_order, created_at, updated_at
        "#
    )
    .bind(&body.name)
    .bind(update_translations)
    .bind(&body.color)
    .bind(&body.icon)
    .bind(body.is_cash)
    .bind(body.is_active)
    .bind(body.display_order)
    .bind(*id)
    .bind(org_id)
    .fetch_one(pool.get_ref())
    .await
    .map_err(|e| {
        if let sqlx::Error::Database(db_err) = &e {
            if db_err.constraint() == Some("org_payment_methods_org_id_name_key") {
                return AppError::Conflict("A payment method with this name already exists".into());
            }
        }
        AppError::from(e)
    })?;

    Ok(HttpResponse::Ok().json(method))
}

// ── POST /payment-methods/:id/activate ────────────────────────────

#[utoipa::path(
    post,
    path = "/payment-methods/{id}/activate",
    tag = "payment_methods",
    params(("id" = Uuid, Path, description = "Payment Method ID")),
    responses((status = 200, description = "Activated payment method", body = OrgPaymentMethod), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn activate_payment_method(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orgs", "update").await?;
    let org_id = claims.org_id().ok_or_else(|| AppError::Forbidden("No org id".into()))?;

    let method = sqlx::query_as::<_, OrgPaymentMethod>(
        "UPDATE org_payment_methods SET is_active = true WHERE id = $1 AND org_id = $2 RETURNING *"
    )
    .bind(*id)
    .bind(org_id)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Payment method not found".into()))?;

    Ok(HttpResponse::Ok().json(method))
}

// ── POST /payment-methods/:id/deactivate ──────────────────────────

#[utoipa::path(
    post,
    path = "/payment-methods/{id}/deactivate",
    tag = "payment_methods",
    params(("id" = Uuid, Path, description = "Payment Method ID")),
    responses((status = 200, description = "Deactivated payment method", body = OrgPaymentMethod), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn deactivate_payment_method(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orgs", "update").await?;
    let org_id = claims.org_id().ok_or_else(|| AppError::Forbidden("No org id".into()))?;

    let method = sqlx::query_as::<_, OrgPaymentMethod>(
        "UPDATE org_payment_methods SET is_active = false WHERE id = $1 AND org_id = $2 RETURNING *"
    )
    .bind(*id)
    .bind(org_id)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Payment method not found".into()))?;

    Ok(HttpResponse::Ok().json(method))
}

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}
