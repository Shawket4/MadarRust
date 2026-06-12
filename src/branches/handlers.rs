use actix_web::{web, HttpRequest, HttpResponse};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;
use actix_web::HttpMessage;
use utoipa::{IntoParams, ToSchema};

use crate::{
    auth::{guards::require_same_org, jwt::Claims},
    errors::{AppError, AppErrorResponse},
    permissions::checker::check_permission,
};

#[derive(Debug, Serialize, Deserialize, sqlx::Type, Clone, PartialEq, ToSchema)]
#[sqlx(type_name = "printer_brand", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum PrinterBrand {
    Star,
    Epson,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct Branch {
    pub id:                Uuid,
    pub org_id:            Uuid,
    #[schema(example = "Zamalek")]
    pub name:              String,
    #[schema(example = "26 July Corridor, Zamalek, Cairo")]
    pub address:           Option<String>,
    #[schema(example = "+201234567890")]
    pub phone:             Option<String>,
    /// IANA timezone name. Defaults to `Africa/Cairo`.
    #[schema(example = "Africa/Cairo")]
    pub timezone:          String,
    pub printer_brand:     Option<PrinterBrand>,
    #[schema(example = "192.168.1.50")]
    pub printer_ip:        Option<String>,
    #[schema(example = 9100)]
    pub printer_port:      Option<i32>,
    pub is_active:         bool,
    /// Convenience field — populated from the parent org's `logo_url`.
    pub org_logo_url:      Option<String>,
    /// WGS-84 latitude for geofenced branch resolution.
    pub latitude:          Option<f64>,
    /// WGS-84 longitude for geofenced branch resolution.
    pub longitude:         Option<f64>,
    /// Radius in meters within which this branch is considered a match. Defaults to 200.
    pub geo_radius_meters: Option<i32>,
    pub created_at:        DateTime<Utc>,
    pub updated_at:        DateTime<Utc>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListBranchesQuery {
    /// Organization whose branches to list. Must match the caller's JWT org.
    pub org_id: Uuid,
}

#[derive(Deserialize, ToSchema)]
pub struct CreateBranchRequest {
    pub org_id:            Uuid,
    #[schema(example = "Zamalek")]
    pub name:              String,
    pub address:           Option<String>,
    pub phone:             Option<String>,
    /// IANA timezone name. Defaults to `Africa/Cairo` if absent.
    #[schema(example = "Africa/Cairo")]
    pub timezone:          Option<String>,
    pub printer_brand:     Option<PrinterBrand>,
    pub printer_ip:        Option<String>,
    /// TCP port for the receipt printer. Defaults to `9100` if absent.
    #[schema(example = 9100)]
    pub printer_port:      Option<i32>,
    pub latitude:          Option<f64>,
    pub longitude:         Option<f64>,
    /// Geofence radius in meters. Defaults to 200.
    pub geo_radius_meters: Option<i32>,
}

/// PATCH-style update. Fields fall into three categories:
///
/// - **Absent** from JSON → keep existing value.
/// - **Present as `null`** (only the `printer_*` fields) → clear the column.
/// - **Present as a value** → set to that value.
///
/// OpenAPI cannot express the absent-vs-null distinction cleanly, so all
/// fields are documented as optional and nullable. Clients targeting this
/// endpoint should send only the fields they want to change.
#[derive(Deserialize, ToSchema)]
pub struct UpdateBranchRequest {
    pub name:      Option<String>,
    pub address:   Option<String>,
    pub phone:     Option<String>,
    pub timezone:  Option<String>,
    pub is_active: Option<bool>,

    // Nullable fields — use double-option pattern (see fn below).
    // The `value_type` override collapses the inner Option<Option<T>>
    // into a single nullable T for the generated schema.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(nullable, value_type = Option<PrinterBrand>)]
    pub printer_brand: Option<Option<PrinterBrand>>,

    #[serde(default, deserialize_with = "double_option")]
    #[schema(nullable, value_type = Option<String>)]
    pub printer_ip:    Option<Option<String>>,

    #[serde(default, deserialize_with = "double_option")]
    #[schema(nullable, value_type = Option<i32>)]
    pub printer_port:  Option<Option<i32>>,

    // Clearable geo fields
    #[serde(default, deserialize_with = "double_option")]
    #[schema(nullable, value_type = Option<f64>)]
    pub latitude: Option<Option<f64>>,

    #[serde(default, deserialize_with = "double_option")]
    #[schema(nullable, value_type = Option<f64>)]
    pub longitude: Option<Option<f64>>,

    pub geo_radius_meters: Option<i32>,
}

/// Deserializes a field that can be:
///  - absent          → None        (don't update)
///  - present as null → Some(None)  (set to null)
///  - present as value→ Some(Some(v))(set to value)
fn double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: serde::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(de).map(Some)
}

#[utoipa::path(
    get,
    path = "/branches",
    tag = "branches",
    params(ListBranchesQuery),
    responses(
        (status = 200, description = "List of branches in the organization", body = Vec<Branch>),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn list_branches(
    req:   HttpRequest,
    pool:  web::Data<PgPool>,
    query: web::Query<ListBranchesQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "branches", "read").await?;
    require_same_org(&claims, Some(query.org_id))?;

    let branches = if claims.role == crate::models::UserRole::BranchManager || claims.role == crate::models::UserRole::Teller {
        sqlx::query_as::<_, Branch>(
            r#"
            SELECT b.id, b.org_id, b.name, b.address, b.phone, b.timezone,
                   b.printer_brand, b.printer_ip::text, b.printer_port,
                   b.is_active, o.logo_url as org_logo_url,
                   b.latitude, b.longitude, b.geo_radius_meters,
                   b.created_at, b.updated_at
            FROM branches b
            JOIN organizations o ON o.id = b.org_id
            JOIN user_branch_assignments uba ON uba.branch_id = b.id
            WHERE b.org_id = $1 AND uba.user_id = $2 AND b.deleted_at IS NULL
            ORDER BY b.name
            "#,
        )
        .bind(query.org_id)
        .bind(claims.user_id())
        .fetch_all(pool.get_ref())
        .await?
    } else {
        sqlx::query_as::<_, Branch>(
            r#"
            SELECT b.id, b.org_id, b.name, b.address, b.phone, b.timezone,
                   b.printer_brand, b.printer_ip::text, b.printer_port,
                   b.is_active, o.logo_url as org_logo_url,
                   b.latitude, b.longitude, b.geo_radius_meters,
                   b.created_at, b.updated_at
            FROM branches b
            JOIN organizations o ON o.id = b.org_id
            WHERE b.org_id = $1 AND b.deleted_at IS NULL
            ORDER BY b.name
            "#,
        )
        .bind(query.org_id)
        .fetch_all(pool.get_ref())
        .await?
    };

    Ok(HttpResponse::Ok().json(branches))
}

#[utoipa::path(
    get,
    path = "/branches/{id}",
    tag = "branches",
    params(
        ("id" = Uuid, Path, description = "Branch ID")
    ),
    responses(
        (status = 200, description = "The requested branch", body = Branch),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn get_branch(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    id:   web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "branches", "read").await?;

    let branch = fetch_branch(pool.get_ref(), *id).await?;
    require_same_org(&claims, Some(branch.org_id))?;

    Ok(HttpResponse::Ok().json(branch))
}

#[utoipa::path(
    post,
    path = "/branches",
    tag = "branches",
    request_body = CreateBranchRequest,
    responses(
        (status = 201, description = "Branch created", body = Branch),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn create_branch(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<CreateBranchRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "branches", "create").await?;
    require_same_org(&claims, Some(body.org_id))?;

    let branch = sqlx::query_as::<_, Branch>(
        r#"
        WITH inserted AS (
            INSERT INTO branches (org_id, name, address, phone, timezone, printer_brand, printer_ip, printer_port, latitude, longitude, geo_radius_meters)
            VALUES ($1, $2, $3, $4, $5, $6, $7::inet, $8, $9, $10, $11)
            RETURNING id, org_id, name, address, phone, timezone,
                      printer_brand, printer_ip, printer_port,
                      is_active, latitude, longitude, geo_radius_meters,
                      created_at, updated_at
        )
        SELECT i.id, i.org_id, i.name, i.address, i.phone, i.timezone,
               i.printer_brand, i.printer_ip::text, i.printer_port,
               i.is_active, o.logo_url as org_logo_url,
               i.latitude, i.longitude, i.geo_radius_meters,
               i.created_at, i.updated_at
        FROM inserted i
        JOIN organizations o ON o.id = i.org_id
        "#,
    )
    .bind(body.org_id)
    .bind(&body.name)
    .bind(&body.address)
    .bind(&body.phone)
    .bind(body.timezone.as_deref().unwrap_or("Africa/Cairo"))
    .bind(&body.printer_brand)
    .bind(&body.printer_ip)
    .bind(body.printer_port.unwrap_or(9100))
    .bind(body.latitude)
    .bind(body.longitude)
    .bind(body.geo_radius_meters)
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Created().json(branch))
}

#[utoipa::path(
    put,
    path = "/branches/{id}",
    tag = "branches",
    params(
        ("id" = Uuid, Path, description = "Branch ID")
    ),
    request_body = UpdateBranchRequest,
    responses(
        (status = 200, description = "Branch updated", body = Branch),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn update_branch(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    id:   web::Path<Uuid>,
    body: web::Json<UpdateBranchRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "branches", "update").await?;

    let existing = fetch_branch(pool.get_ref(), *id).await?;
    require_same_org(&claims, Some(existing.org_id))?;

    // Resolve each nullable field:
    //   Some(Some(v)) → use v
    //   Some(None)    → explicit null (clear the field)
    //   None          → keep existing value
    let new_printer_brand: Option<Option<PrinterBrand>> = body.printer_brand.clone();
    let new_printer_ip:    Option<Option<String>>       = body.printer_ip.clone();
    let new_printer_port:  Option<Option<i32>>          = body.printer_port;
    let new_latitude:      Option<Option<f64>>          = body.latitude;
    let new_longitude:     Option<Option<f64>>          = body.longitude;

    let branch = sqlx::query_as::<_, Branch>(
        r#"
        WITH updated AS (
            UPDATE branches SET
                name              = COALESCE($2, name),
                address           = COALESCE($3, address),
                phone             = COALESCE($4, phone),
                timezone          = COALESCE($5, timezone),
                is_active         = COALESCE($6, is_active),
                printer_brand     = CASE
                                      WHEN $7 THEN $8
                                      ELSE printer_brand
                                    END,
                printer_ip        = CASE
                                      WHEN $9  THEN $10::inet
                                      ELSE printer_ip
                                    END,
                printer_port      = CASE
                                      WHEN $11 THEN $12
                                      ELSE printer_port
                                    END,
                latitude          = CASE
                                      WHEN $13 THEN $14
                                      ELSE latitude
                                    END,
                longitude         = CASE
                                      WHEN $15 THEN $16
                                      ELSE longitude
                                    END,
                geo_radius_meters = COALESCE($17, geo_radius_meters)
            WHERE id = $1 AND deleted_at IS NULL
            RETURNING id, org_id, name, address, phone, timezone,
                      printer_brand, printer_ip, printer_port,
                      is_active, latitude, longitude, geo_radius_meters,
                      created_at, updated_at
        )
        SELECT u.id, u.org_id, u.name, u.address, u.phone, u.timezone,
               u.printer_brand, u.printer_ip::text, u.printer_port,
               u.is_active, o.logo_url as org_logo_url,
               u.latitude, u.longitude, u.geo_radius_meters,
               u.created_at, u.updated_at
        FROM updated u
        JOIN organizations o ON o.id = u.org_id
        "#,
    )
    .bind(*id)
    .bind(&body.name)
    .bind(&body.address)
    .bind(&body.phone)
    .bind(&body.timezone)
    .bind(body.is_active)
    .bind(new_printer_brand.is_some())
    .bind(new_printer_brand.as_ref().and_then(|o| o.clone()))
    .bind(new_printer_ip.is_some())
    .bind(new_printer_ip.as_ref().and_then(|o| o.clone()))
    .bind(new_printer_port.is_some())
    .bind(new_printer_port.and_then(|o| o))
    .bind(new_latitude.is_some())
    .bind(new_latitude.and_then(|o| o))
    .bind(new_longitude.is_some())
    .bind(new_longitude.and_then(|o| o))
    .bind(body.geo_radius_meters)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Branch not found".into()))?;

    Ok(HttpResponse::Ok().json(branch))
}

#[utoipa::path(
    delete,
    path = "/branches/{id}",
    tag = "branches",
    params(
        ("id" = Uuid, Path, description = "Branch ID")
    ),
    responses(
        (status = 204, description = "Branch deleted (soft delete)"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn delete_branch(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    id:   web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "branches", "delete").await?;

    let existing = fetch_branch(pool.get_ref(), *id).await?;
    require_same_org(&claims, Some(existing.org_id))?;

    sqlx::query(
        "UPDATE branches SET deleted_at = NOW() WHERE id = $1 AND deleted_at IS NULL"
    )
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

async fn fetch_branch(pool: &PgPool, id: Uuid) -> Result<Branch, AppError> {
    sqlx::query_as::<_, Branch>(
        r#"
        SELECT b.id, b.org_id, b.name, b.address, b.phone, b.timezone,
               b.printer_brand, b.printer_ip::text, b.printer_port,
               b.is_active, o.logo_url as org_logo_url,
               b.latitude, b.longitude, b.geo_radius_meters,
               b.created_at, b.updated_at
        FROM branches b
        JOIN organizations o ON o.id = b.org_id
        WHERE b.id = $1 AND b.deleted_at IS NULL
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Branch not found".into()))
}