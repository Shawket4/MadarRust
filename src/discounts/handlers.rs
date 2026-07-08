use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
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
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub dtype: String,
    pub value: i32,
    pub is_active: bool,
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
    pub org_id: Uuid,
    pub name: String,
    #[schema(value_type = Option<Object>)]
    pub name_translations: Option<serde_json::Value>,
    pub dtype: String,
    pub value: i32,
    pub is_active: Option<bool>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpdateDiscountRequest {
    pub name: Option<String>,
    #[schema(value_type = Option<Object>)]
    pub name_translations: Option<serde_json::Value>,
    pub dtype: Option<String>,
    pub value: Option<i32>,
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
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<ListQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "discounts", "read").await?;
    require_org_access(&claims, query.org_id)?;

    let rows = sqlx::query_as::<_, Discount>(
        r#"
        SELECT id, org_id, name, name_translations, type::text AS dtype, value, is_active, created_at, updated_at
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
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CreateDiscountRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "discounts", "create").await?;
    require_org_access(&claims, body.org_id)?;
    validate_dtype(&body.dtype)?;
    validate_value(body.value, &body.dtype)?;

    let mut_body = body.into_inner();
    let mut name_translations = mut_body
        .name_translations
        .unwrap_or_else(|| serde_json::json!({}));
    crate::translation::ensure_translations_json(&mut name_translations, Some(&mut_body.name))
        .await
        .map_err(|_| AppError::Internal)?;

    let row = sqlx::query_as::<_, Discount>(
        r#"
        INSERT INTO discounts (org_id, name, name_translations, type, value, is_active)
        VALUES ($1, $2, $3, $4::discount_type, $5, $6)
        RETURNING id, org_id, name, name_translations, type::text AS dtype, value, is_active, created_at, updated_at
        "#,
    )
    .bind(mut_body.org_id)
    .bind(&mut_body.name)
    .bind(name_translations)
    .bind(&mut_body.dtype)
    .bind(mut_body.value)
    .bind(mut_body.is_active.unwrap_or(true))
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
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<UpdateDiscountRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "discounts", "update").await?;
    let existing = fetch_or_404(pool.get_ref(), *id).await?;
    require_org_access(&claims, existing.org_id)?;

    let mut_body = body.into_inner();

    if let Some(ref dt) = mut_body.dtype {
        validate_dtype(dt)?;
    }
    if let (Some(v), Some(dt)) = (mut_body.value, &mut_body.dtype) {
        validate_value(v, dt)?;
    }

    let mut name_translations = existing.name_translations;
    if let Some(new_name) = &mut_body.name {
        crate::translation::ensure_translations_json(&mut name_translations, Some(new_name))
            .await
            .map_err(|_| AppError::Internal)?;
    } else if let Some(new_tr) = mut_body.name_translations {
        name_translations = new_tr;
        crate::translation::ensure_translations_json(&mut name_translations, Some(&existing.name))
            .await
            .map_err(|_| AppError::Internal)?;
    }

    let row = sqlx::query_as::<_, Discount>(
        r#"
        UPDATE discounts SET
            name              = COALESCE($2, name),
            name_translations = $3,
            type              = COALESCE($4::discount_type, type),
            value             = COALESCE($5, value),
            is_active         = COALESCE($6, is_active),
            updated_at        = NOW()
        WHERE id = $1
        RETURNING id, org_id, name, name_translations, type::text AS dtype, value, is_active, created_at, updated_at
        "#,
    )
    .bind(*id)
    .bind(&mut_body.name)
    .bind(name_translations)
    .bind(&mut_body.dtype)
    .bind(mut_body.value)
    .bind(mut_body.is_active)
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
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "discounts", "delete").await?;
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
        "SELECT id, org_id, name, name_translations, type::text AS dtype, value, is_active, created_at, updated_at
         FROM discounts WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Discount not found".into()))
}

fn require_org_access(claims: &Claims, org_id: Uuid) -> Result<(), AppError> {
    if claims.role == UserRole::SuperAdmin {
        return Ok(());
    }
    if claims.org_id() != Some(org_id) {
        return Err(AppError::Forbidden("Not your org".into()));
    }
    Ok(())
}

fn validate_dtype(dt: &str) -> Result<(), AppError> {
    match dt {
        "percentage" | "fixed" => Ok(()),
        _ => Err(AppError::BadRequest(
            "type must be 'percentage' or 'fixed'".into(),
        )),
    }
}

fn validate_value(value: i32, dtype: &str) -> Result<(), AppError> {
    if value < 0 {
        return Err(AppError::BadRequest("value must be >= 0".into()));
    }
    if dtype == "percentage" && value > 100 {
        return Err(AppError::BadRequest(
            "percentage value must be 0-100".into(),
        ));
    }
    Ok(())
}

/// Discount amount (piastres) for `subtotal` given a discount type
/// (`"percentage"` | `"fixed"`) and its value. Percentage rounds half-away-from-
/// zero so it matches the POS preview to the piastre; fixed is capped at the
/// subtotal. Always clamped to `[0, subtotal]` so a malformed discount can never
/// drive a total negative or inflated. Single source of truth for both the POS
/// order path and delivery-order intake/finalize.
pub fn calc_discount(dtype: Option<&str>, value: i32, subtotal: i32) -> i32 {
    let d = match dtype {
        Some("percentage") => (subtotal as f64 * value as f64 / 100.0).round() as i32,
        Some("fixed") => value.min(subtotal),
        _ => 0,
    };
    d.clamp(0, subtotal)
}

#[cfg(test)]
mod calc_discount_tests {
    use super::calc_discount;

    #[test]
    fn percentage_of_subtotal() {
        assert_eq!(calc_discount(Some("percentage"), 10, 1000), 100);
    }

    #[test]
    fn percentage_rounds_half_away_from_zero() {
        // 105 × 10 / 100 = 10.5 → 11.
        assert_eq!(calc_discount(Some("percentage"), 10, 105), 11);
    }

    #[test]
    fn percentage_over_100_is_capped_at_subtotal() {
        assert_eq!(calc_discount(Some("percentage"), 150, 1000), 1000);
    }

    #[test]
    fn negative_percentage_clamps_to_zero() {
        assert_eq!(calc_discount(Some("percentage"), -10, 1000), 0);
    }

    #[test]
    fn fixed_is_taken_verbatim() {
        assert_eq!(calc_discount(Some("fixed"), 300, 1000), 300);
    }

    #[test]
    fn fixed_larger_than_subtotal_caps_at_subtotal() {
        assert_eq!(calc_discount(Some("fixed"), 5000, 1000), 1000);
    }

    #[test]
    fn negative_fixed_clamps_to_zero() {
        assert_eq!(calc_discount(Some("fixed"), -50, 1000), 0);
    }

    #[test]
    fn unknown_type_is_no_discount() {
        assert_eq!(calc_discount(Some("bogus"), 50, 1000), 0);
        assert_eq!(calc_discount(None, 50, 1000), 0);
    }
}
