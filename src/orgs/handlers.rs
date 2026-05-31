use actix_multipart::Multipart;
use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
use futures::TryStreamExt;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    auth::{guards::{require_super_admin, require_same_org}, jwt::Claims},
    errors::{AppError, AppErrorResponse},
    permissions::checker::check_permission,
    uploads::handlers::delete_old_image,
};

// ── Models ────────────────────────────────────────────────────

#[derive(Debug, sqlx::FromRow, Serialize, Deserialize, ToSchema)]
pub struct Org {
    pub id:             Uuid,
    #[schema(example = "The Rue")]
    pub name:           String,
    #[schema(example = "the-rue")]
    pub slug:           String,
    #[serde(serialize_with = "crate::uploads::handlers::serialize_opt_url")]
    pub logo_url:       Option<String>,
    #[schema(example = "EGP")]
    pub currency_code:  String,
    /// Tax rate as a decimal (e.g. `0.14` for 14% VAT).
    /// Stored as `BigDecimal` internally; transmitted as a JSON number.
    #[schema(value_type = f64, example = 0.14)]
    pub tax_rate:       sqlx::types::BigDecimal,
    pub receipt_footer: Option<String>,
    pub is_active:      bool,
}

// ── Request types ─────────────────────────────────────────────

// CreateOrgRequest is consumed from multipart fields, not JSON.
// We keep this struct for the non-file fields parsed out of the form.
#[derive(Default)]
struct CreateOrgFields {
    name:           Option<String>,
    slug:           Option<String>,
    currency_code:  Option<String>,
    tax_rate:       Option<f64>,
    receipt_footer: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateOrgRequest {
    pub name:           Option<String>,
    pub slug:           Option<String>,
    pub currency_code:  Option<String>,
    #[schema(example = 0.14)]
    pub tax_rate:       Option<f64>,
    pub receipt_footer: Option<String>,
    pub is_active:      Option<bool>,
    /// `null` clears the logo; absent leaves it unchanged. To set a new
    /// logo, use `PUT /orgs/{id}/logo` (multipart) instead — JSON updates
    /// only accept the clear-to-null case here.
    #[serde(default, deserialize_with = "crate::menu::handlers::deserialize_double_option")]
    #[schema(nullable, value_type = Option<String>)]
    pub logo_url:       Option<Option<String>>,
}

// ── OpenAPI-only multipart schemas ────────────────────────────
//
// These structs exist solely to describe the shape of multipart/form-data
// request bodies in the generated spec. They are never constructed at
// runtime — the handlers parse multipart fields directly. Keeping them
// `pub` and `ToSchema`-derived lets `#[utoipa::path]` reference them.

#[derive(ToSchema)]
#[allow(dead_code)]
pub struct CreateOrgMultipart {
    #[schema(example = "The Rue")]
    pub name: String,

    #[schema(example = "the-rue")]
    pub slug: String,

    #[schema(example = "EGP")]
    pub currency_code: Option<String>,

    #[schema(example = 0.14)]
    pub tax_rate: Option<f64>,

    pub receipt_footer: Option<String>,

    /// Logo image file. PNG, JPEG, or WebP. Optional — omit the field
    /// entirely to create the org without a logo.
    #[schema(format = Binary, content_media_type = "image/*")]
    pub logo: Option<String>,
}

#[derive(ToSchema)]
#[allow(dead_code)]
pub struct UploadLogoMultipart {
    /// Logo image file. PNG, JPEG, or WebP. Required.
    #[schema(format = Binary, content_media_type = "image/*")]
    pub logo: String,
}

// ── POST /orgs  (super_admin only, multipart/form-data) ──────

#[utoipa::path(
    post,
    path = "/orgs",
    tag = "orgs",
    request_body(
        content = CreateOrgMultipart,
        content_type = "multipart/form-data",
        description = "Multipart form with text fields and an optional logo file."
    ),
    responses(
        (status = 201, description = "Organization created", body = Org),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn create_org(
    req:     HttpRequest,
    pool:    web::Data<PgPool>,
    mut mp:  Multipart,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orgs", "create").await?;
    require_super_admin(&claims)?;

    let uploads_dir = std::env::var("UPLOADS_DIR").unwrap_or_else(|_| "./uploads".to_string());
    let base_url    = std::env::var("UPLOADS_BASE_URL").unwrap_or_default();

    let mut fields    = CreateOrgFields::default();
    let mut logo_url: Option<String> = None;

    while let Some(mut field) = mp.try_next().await.map_err(|e| {
        AppError::BadRequest(format!("Multipart error: {e}"))
    })? {
        let name = field.name().unwrap_or("").to_string();

        match name.as_str() {
            "logo" => {
                let mut bytes = Vec::new();
                while let Some(chunk) = field.try_next().await.map_err(|e| {
                    AppError::BadRequest(format!("Upload read error: {e}"))
                })? {
                    bytes.extend_from_slice(chunk.as_ref());
                }
                if !bytes.is_empty() {
                    let ct = field
                        .content_type()
                        .map(|m| m.to_string())
                        .unwrap_or_default();
                    let ext = match ct.as_str() {
                        "image/png"  => "png",
                        "image/webp" => "webp",
                        _            => "jpg",
                    };
                    let filename  = format!("{}.{}", Uuid::new_v4(), ext);
                    let file_path = format!("{}/logos/{}", uploads_dir, filename);
                    std::fs::create_dir_all(format!("{}/logos", uploads_dir))
                        .map_err(|_| AppError::Internal)?;
                    std::fs::write(&file_path, &bytes)
                        .map_err(|_| AppError::Internal)?;
                    logo_url = Some(format!("{}/logos/{}", base_url.trim_end_matches('/'), filename));
                }
            }
            "name"           => fields.name           = text_field(&mut field).await?,
            "slug"           => fields.slug           = text_field(&mut field).await?,
            "currency_code"  => fields.currency_code  = text_field(&mut field).await?,
            "tax_rate"       => {
                if let Some(s) = text_field(&mut field).await? {
                    fields.tax_rate = s.parse::<f64>().ok();
                }
            }
            "receipt_footer" => fields.receipt_footer = text_field(&mut field).await?,
            _                => { drain_field(&mut field).await?; }
        }
    }

    let name = fields.name.ok_or_else(|| AppError::BadRequest("name is required".into()))?;
    let slug = fields.slug.ok_or_else(|| AppError::BadRequest("slug is required".into()))?;

    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM organizations WHERE slug = $1)"
    )
    .bind(&slug)
    .fetch_one(pool.get_ref())
    .await?;

    if exists {
        return Err(AppError::Conflict(format!("Slug '{}' is already taken", slug)));
    }

    let currency = fields.currency_code.as_deref().unwrap_or("EGP");
    let tax_rate = fields.tax_rate.unwrap_or(0.14);

    let mut tx = pool.begin().await?;

    let org = sqlx::query_as::<_, Org>(
        r#"
        INSERT INTO organizations (name, slug, logo_url, currency_code, tax_rate, receipt_footer)
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING id, name, slug, logo_url, currency_code, tax_rate, receipt_footer, is_active
        "#,
    )
    .bind(&name)
    .bind(&slug)
    .bind(&logo_url)
    .bind(currency)
    .bind(tax_rate)
    .bind(&fields.receipt_footer)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO org_payment_methods (org_id, name, label_translations, color, icon, is_cash, display_order)
        VALUES 
            ($1, 'cash', '{"en": "Cash", "ar": "نقدي"}', 'emerald', 'payments_outlined', true, 1),
            ($1, 'card', '{"en": "Card", "ar": "بطاقة"}', 'blue', 'credit_card_rounded', false, 2),
            ($1, 'digital_wallet', '{"en": "Digital Wallet", "ar": "محفظة رقمية"}', 'purple', 'account_balance_wallet_rounded', false, 3),
            ($1, 'mixed', '{"en": "Mixed", "ar": "مختلط"}', 'amber', 'pie_chart_rounded', false, 4),
            ($1, 'talabat_online', '{"en": "Talabat Online", "ar": "طلبات أونلاين"}', 'orange', 'delivery_dining_rounded', false, 5),
            ($1, 'talabat_cash', '{"en": "Talabat Cash", "ar": "طلبات كاش"}', 'orange', 'delivery_dining_rounded', true, 6)
        "#
    )
    .bind(org.id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(HttpResponse::Created().json(org))
}

// ── GET /orgs  (super_admin only) ────────────────────────────

#[utoipa::path(
    get,
    path = "/orgs",
    tag = "orgs",
    responses(
        (status = 200, description = "List of all organizations", body = Vec<Org>),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn list_orgs(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orgs", "read").await?;
    require_super_admin(&claims)?;

    let orgs = sqlx::query_as::<_, Org>(
        r#"
        SELECT id, name, slug, logo_url, currency_code, tax_rate, receipt_footer, is_active
        FROM organizations
        WHERE deleted_at IS NULL
        ORDER BY name
        "#,
    )
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(orgs))
}

// ── GET /orgs/:id  (org-scoped read) ─────────────────────────

#[utoipa::path(
    get,
    path = "/orgs/{id}",
    tag = "orgs",
    params(
        ("id" = Uuid, Path, description = "Organization ID")
    ),
    responses(
        (status = 200, description = "The requested organization", body = Org),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn get_org(
    req:    HttpRequest,
    pool:   web::Data<PgPool>,
    org_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orgs", "read").await?;
    require_same_org(&claims, Some(*org_id))?;

    let org = fetch_org(pool.get_ref(), *org_id).await?;
    Ok(HttpResponse::Ok().json(org))
}

// ── PATCH /orgs/:id  (super_admin only) ──────────────────────
// JSON only — logo swap uses PUT /orgs/{id}/logo.

#[utoipa::path(
    patch,
    path = "/orgs/{id}",
    tag = "orgs",
    params(
        ("id" = Uuid, Path, description = "Organization ID")
    ),
    request_body = UpdateOrgRequest,
    responses(
        (status = 200, description = "Organization updated", body = Org),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn update_org(
    req:    HttpRequest,
    pool:   web::Data<PgPool>,
    org_id: web::Path<Uuid>,
    body:   web::Json<UpdateOrgRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orgs", "update").await?;
    require_super_admin(&claims)?;

    let existing = fetch_org(pool.get_ref(), *org_id).await?;

    if let Some(slug) = &body.slug {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM organizations WHERE slug = $1 AND id != $2)"
        )
        .bind(slug)
        .bind(*org_id)
        .fetch_one(pool.get_ref())
        .await?;

        if exists {
            return Err(AppError::Conflict(format!("Slug '{}' is already taken", slug)));
        }
    }

    let logo_url_is_present = body.logo_url.is_some();
    let logo_url_val        = body.logo_url.as_ref().and_then(|o| o.clone());

    let org = sqlx::query_as::<_, Org>(
        r#"
        UPDATE organizations SET
            name           = COALESCE($2, name),
            slug           = COALESCE($3, slug),
            currency_code  = COALESCE($4, currency_code),
            tax_rate       = COALESCE($5, tax_rate),
            receipt_footer = COALESCE($6, receipt_footer),
            is_active      = COALESCE($7, is_active),
            logo_url       = CASE WHEN $9 THEN $8 ELSE logo_url END,
            updated_at     = NOW()
        WHERE id = $1 AND deleted_at IS NULL
        RETURNING id, name, slug, logo_url, currency_code, tax_rate, receipt_footer, is_active
        "#,
    )
    .bind(*org_id)
    .bind(&body.name)
    .bind(&body.slug)
    .bind(&body.currency_code)
    .bind(body.tax_rate)
    .bind(&body.receipt_footer)
    .bind(body.is_active)
    .bind(&logo_url_val)
    .bind(logo_url_is_present)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Org not found".into()))?;

    if body.logo_url == Some(None)
        && let Some(old_url) = existing.logo_url {
            let uploads_dir = std::env::var("UPLOADS_DIR").unwrap_or_else(|_| "./uploads".to_string());
            let base_url    = std::env::var("UPLOADS_BASE_URL").unwrap_or_default();
            delete_old_image(&old_url, &base_url, &uploads_dir).await;
        }

    Ok(HttpResponse::Ok().json(org))
}

// ── PUT /orgs/:id/logo  (super_admin only, multipart) ────────

#[utoipa::path(
    put,
    path = "/orgs/{id}/logo",
    tag = "orgs",
    params(
        ("id" = Uuid, Path, description = "Organization ID")
    ),
    request_body(
        content = UploadLogoMultipart,
        content_type = "multipart/form-data",
        description = "Multipart form with a single `logo` file field."
    ),
    responses(
        (status = 200, description = "Logo replaced; updated organization returned", body = Org),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn upload_org_logo(
    req:    HttpRequest,
    pool:   web::Data<PgPool>,
    org_id: web::Path<Uuid>,
    mut mp: Multipart,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orgs", "update").await?;
    require_super_admin(&claims)?;

    let existing    = fetch_org(pool.get_ref(), *org_id).await?;
    let uploads_dir = std::env::var("UPLOADS_DIR").unwrap_or_else(|_| "./uploads".to_string());
    let base_url    = std::env::var("UPLOADS_BASE_URL").unwrap_or_default();

    let mut new_logo_url: Option<String> = None;

    while let Some(mut field) = mp.try_next().await.map_err(|e| {
        AppError::BadRequest(format!("Multipart error: {e}"))
    })? {
        if field.name().unwrap_or("") != "logo" {
            drain_field(&mut field).await?;
            continue;
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = field.try_next().await.map_err(|e| {
            AppError::BadRequest(format!("Upload read error: {e}"))
        })? {
            bytes.extend_from_slice(chunk.as_ref());
        }
        if !bytes.is_empty() {
            let ct  = field.content_type().map(|m| m.to_string()).unwrap_or_default();
            let ext = match ct.as_str() {
                "image/png"  => "png",
                "image/webp" => "webp",
                _            => "jpg",
            };
            let filename  = format!("{}.{}", Uuid::new_v4(), ext);
            let dir       = format!("{}/logos", uploads_dir);
            std::fs::create_dir_all(&dir)
                .map_err(|_| AppError::Internal)?;
            std::fs::write(format!("{}/{}", dir, filename), &bytes)
                .map_err(|_| AppError::Internal)?;
            new_logo_url = Some(format!(
                "{}/logos/{}",
                base_url.trim_end_matches('/'),
                filename,
            ));
        }
    }

    let new_logo_url = new_logo_url
        .ok_or_else(|| AppError::BadRequest("No logo file received in field 'logo'".into()))?;

    let org = sqlx::query_as::<_, Org>(
        r#"
        UPDATE organizations
        SET logo_url = $2, updated_at = NOW()
        WHERE id = $1 AND deleted_at IS NULL
        RETURNING id, name, slug, logo_url, currency_code, tax_rate, receipt_footer, is_active
        "#,
    )
    .bind(*org_id)
    .bind(&new_logo_url)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Org not found".into()))?;

    if let Some(old_url) = existing.logo_url {
        delete_old_image(&old_url, &base_url, &uploads_dir).await;
    }

    Ok(HttpResponse::Ok().json(org))
}

// ── DELETE /orgs/:id  (super_admin only) ─────────────────────

#[utoipa::path(
    delete,
    path = "/orgs/{id}",
    tag = "orgs",
    params(
        ("id" = Uuid, Path, description = "Organization ID")
    ),
    responses(
        (status = 204, description = "Organization deleted (soft delete)"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn delete_org(
    req:    HttpRequest,
    pool:   web::Data<PgPool>,
    org_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orgs", "delete").await?;
    require_super_admin(&claims)?;

    let rows_affected = sqlx::query(
        "UPDATE organizations SET deleted_at = NOW(), is_active = false WHERE id = $1 AND deleted_at IS NULL"
    )
    .bind(*org_id)
    .execute(pool.get_ref())
    .await?
    .rows_affected();

    if rows_affected == 0 {
        return Err(AppError::NotFound("Org not found".into()));
    }

    Ok(HttpResponse::NoContent().finish())
}

// ── Helpers ───────────────────────────────────────────────────

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

async fn fetch_org(pool: &PgPool, id: Uuid) -> Result<Org, AppError> {
    sqlx::query_as::<_, Org>(
        "SELECT id, name, slug, logo_url, currency_code, tax_rate, receipt_footer, is_active
         FROM organizations
         WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Org not found".into()))
}

async fn drain_field(field: &mut actix_multipart::Field) -> Result<(), AppError> {
    while field.try_next().await.map_err(|e| AppError::BadRequest(e.to_string()))?.is_some() {}
    Ok(())
}

async fn text_field(field: &mut actix_multipart::Field) -> Result<Option<String>, AppError> {
    let mut buf = Vec::new();
    while let Some(chunk) = field.try_next().await.map_err(|e| AppError::BadRequest(e.to_string()))? {
        buf.extend_from_slice(chunk.as_ref());
    }
    Ok(if buf.is_empty() {
        None
    } else {
        Some(String::from_utf8(buf).map_err(|_| AppError::BadRequest("Invalid UTF-8 in field".into()))?)
    })
}