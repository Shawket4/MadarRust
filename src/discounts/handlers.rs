use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    auth::jwt::Claims,
    errors::{AppError, AppErrorResponse},
    models::UserRole,
    permissions::checker::check_permission,
};
use utoipa::{IntoParams, ToSchema};

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct Discount {
    pub id:         Uuid,
    pub org_id:     Uuid,
    pub name:       String,
    pub dtype:      String,
    pub value:      i32,
    pub is_active:  bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    pub org_id: Uuid,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateDiscountRequest {
    pub org_id:    Uuid,
    pub name:      String,
    pub dtype:     String,
    pub value:     i32,
    pub is_active: Option<bool>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpdateDiscountRequest {
    pub name:      Option<String>,
    pub dtype:     Option<String>,
    pub value:     Option<i32>,
    pub is_active: Option<bool>,
}

#[utoipa::path(
    get,
    path = "/discounts",
    tag = "discounts",
    params(ListQuery),
    responses((status = 200, description = "List discounts", body = Vec<Discount>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_discounts(
    req:   HttpRequest,
    pool:  web::Data<PgPool>,
    query: web::Query<ListQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    require_org_access(&claims, query.org_id)?;

    let rows = sqlx::query_as::<_, Discount>(
        r#"
        SELECT id, org_id, name, type::text AS dtype, value, is_active, created_at, updated_at
        FROM discounts
        WHERE org_id = $1
        ORDER BY name
        "#,
    )
    .bind(query.org_id)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    post,
    path = "/discounts",
    tag = "discounts",
    request_body = CreateDiscountRequest,
    responses((status = 201, description = "Discount created", body = Discount), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_discount(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<CreateDiscountRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "create").await?;
    require_org_access(&claims, body.org_id)?;
    validate_dtype(&body.dtype)?;
    validate_value(body.value, &body.dtype)?;

    let row = sqlx::query_as::<_, Discount>(
        r#"
        INSERT INTO discounts (org_id, name, type, value, is_active)
        VALUES ($1, $2, $3::discount_type, $4, $5)
        RETURNING id, org_id, name, type::text AS dtype, value, is_active, created_at, updated_at
        "#,
    )
    .bind(body.org_id)
    .bind(&body.name)
    .bind(&body.dtype)
    .bind(body.value)
    .bind(body.is_active.unwrap_or(true))
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Created().json(row))
}

#[utoipa::path(
    patch,
    path = "/discounts/{id}",
    tag = "discounts",
    params(("id" = Uuid, Path, description = "Discount ID")),
    request_body = UpdateDiscountRequest,
    responses((status = 200, description = "Discount updated", body = Discount), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_discount(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    id:   web::Path<Uuid>,
    body: web::Json<UpdateDiscountRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;
    let existing = fetch_or_404(pool.get_ref(), *id).await?;
    require_org_access(&claims, existing.org_id)?;

    if let Some(ref dt) = body.dtype { validate_dtype(dt)?; }
    if let (Some(v), Some(dt)) = (body.value, &body.dtype) { validate_value(v, dt)?; }

    let row = sqlx::query_as::<_, Discount>(
        r#"
        UPDATE discounts SET
            name       = COALESCE($2, name),
            type       = COALESCE($3::discount_type, type),
            value      = COALESCE($4, value),
            is_active  = COALESCE($5, is_active),
            updated_at = NOW()
        WHERE id = $1
        RETURNING id, org_id, name, type::text AS dtype, value, is_active, created_at, updated_at
        "#,
    )
    .bind(*id)
    .bind(&body.name)
    .bind(&body.dtype)
    .bind(body.value)
    .bind(body.is_active)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Discount not found".into()))?;

    Ok(HttpResponse::Ok().json(row))
}

#[utoipa::path(
    delete,
    path = "/discounts/{id}",
    tag = "discounts",
    params(("id" = Uuid, Path, description = "Discount ID")),
    responses((status = 204, description = "Discount deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_discount(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    id:   web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "delete").await?;
    let existing = fetch_or_404(pool.get_ref(), *id).await?;
    require_org_access(&claims, existing.org_id)?;

    sqlx::query("DELETE FROM discounts WHERE id = $1")
        .bind(*id)
        .execute(pool.get_ref())
        .await?;

    Ok(HttpResponse::NoContent().finish())
}

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

async fn fetch_or_404(pool: &PgPool, id: Uuid) -> Result<Discount, AppError> {
    sqlx::query_as::<_, Discount>(
        "SELECT id, org_id, name, type::text AS dtype, value, is_active, created_at, updated_at
         FROM discounts WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Discount not found".into()))
}

fn require_org_access(claims: &Claims, org_id: Uuid) -> Result<(), AppError> {
    if claims.role == UserRole::SuperAdmin { return Ok(()); }
    if claims.org_id() != Some(org_id) {
        return Err(AppError::Forbidden("Not your org".into()));
    }
    Ok(())
}

fn validate_dtype(dt: &str) -> Result<(), AppError> {
    match dt {
        "percentage" | "fixed" => Ok(()),
        _ => Err(AppError::BadRequest("type must be 'percentage' or 'fixed'".into())),
    }
}

fn validate_value(value: i32, dtype: &str) -> Result<(), AppError> {
    if value < 0 {
        return Err(AppError::BadRequest("value must be >= 0".into()));
    }
    if dtype == "percentage" && value > 100 {
        return Err(AppError::BadRequest("percentage value must be 0-100".into()));
    }
    Ok(())
}
