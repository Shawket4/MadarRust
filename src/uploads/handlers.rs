use crate::{
    assets::ingest::{AssetField, AssetPurpose, AssetTable, AssetTarget, IngestSource, MAX_RAW_BYTES, stage},
    auth::jwt::Claims,
    errors::{AppError, AppErrorResponse},
    models::UserRole,
    permissions::checker::check_permission,
};
use actix_multipart::Multipart;
use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// Upload accepted: the picture is converted by the background asset worker.
/// `image_url` is the row's current legacy URL (unchanged until the job is
/// done); poll `GET /assets/jobs/{asset_job_id}`.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct UploadResponse {
    #[serde(serialize_with = "serialize_opt_url")]
    pub image_url: Option<String>,
    pub asset_job_id: Uuid,
    /// `processing`
    pub status: String,
}

#[derive(ToSchema)]
#[allow(dead_code)]
pub struct UploadImageMultipart {
    /// Image file (JPEG, PNG, WebP, GIF still, BMP). Type is sniffed from bytes.
    #[schema(format = Binary, content_media_type = "image/*")]
    pub image: String,
}

/// Read the `image` multipart field (capped). Client MIME/filename are ignored.
pub async fn read_image_field(payload: &mut Multipart) -> Result<(Vec<u8>, Option<String>), AppError> {
    while let Some(item) = payload.next().await {
        let mut field = item.map_err(|_| AppError::BadRequest("Invalid multipart data".into()))?;
        let name = field.content_disposition().and_then(|cd| cd.get_name()).unwrap_or("").to_string();
        let label = field.content_disposition().and_then(|cd| cd.get_filename()).map(str::to_string);
        if name != "image" {
            while let Some(c) = field.next().await {
                c.map_err(|_| AppError::BadRequest("Failed reading upload".into()))?;
            }
            continue;
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = field.next().await {
            let chunk = chunk.map_err(|_| AppError::BadRequest("Failed reading upload".into()))?;
            bytes.extend_from_slice(&chunk);
            if bytes.len() > MAX_RAW_BYTES {
                return Err(AppError::BadRequest("File too large (max 20 MB raw)".into()));
            }
        }
        if bytes.is_empty() {
            return Err(AppError::BadRequest("No image field found in upload".into()));
        }
        return Ok((bytes, label));
    }
    Err(AppError::BadRequest("No image field found in upload".into()))
}

async fn upload_for(
    req: HttpRequest,
    pool: crate::db::Db,
    table: AssetTable,
    id: Uuid,
    mut payload: Multipart,
) -> Result<HttpResponse, AppError> {
    if crate::demo::config::demo_mode() {
        return Err(AppError::BadRequest("Image uploads are disabled in the demo.".into()));
    }
    let claims = extract_claims(&req)?;
    let (resource, sql, purpose, what) = match table {
        AssetTable::MenuItems => ("menu_items", "SELECT org_id, image_url FROM menu_items WHERE id = $1 AND deleted_at IS NULL", AssetPurpose::MenuItemPhoto, "Menu item"),
        AssetTable::Categories => ("categories", "SELECT org_id, image_url FROM categories WHERE id = $1 AND deleted_at IS NULL", AssetPurpose::CategoryPhoto, "Category"),
        AssetTable::Bundles => ("menu_items", "SELECT org_id, image_url FROM bundles WHERE id = $1", AssetPurpose::BundlePhoto, "Bundle"),
        _ => return Err(AppError::BadRequest("unsupported upload target".into())),
    };
    check_permission(pool.get_ref(), &claims, resource, "update").await?;
    let row: Option<(Uuid, Option<String>)> = sqlx::query_as(sql).bind(id).fetch_optional(pool.get_ref()).await?;
    let (org_id, image_url) = row.ok_or_else(|| AppError::NotFound(format!("{what} not found")))?;
    if claims.role != UserRole::SuperAdmin && claims.org_id() != Some(org_id) {
        return Err(AppError::Forbidden(format!("{what} belongs to a different org")));
    }
    let (bytes, label) = read_image_field(&mut payload).await?;
    let job = stage(
        pool.get_ref(),
        Some(org_id),
        purpose,
        IngestSource::Bytes(bytes.into()),
        label.as_deref(),
        AssetTarget::new(table, id, AssetField::Image),
        claims.user_id_safe().ok(),
    )
    .await?;
    Ok(HttpResponse::Ok().json(UploadResponse { image_url, asset_job_id: job, status: "processing".into() }))
}

#[utoipa::path(
    post,
    path = "/uploads/menu-items/{menu_item_id}",
    tag = "uploads",
    params(("menu_item_id" = Uuid, Path, description = "Menu item ID")),
    request_body(content = UploadImageMultipart, content_type = "multipart/form-data",
        description = "Multipart form with a single `image` file field."),
    responses((status = 200, description = "Image accepted for processing", body = UploadResponse), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn upload_menu_item_image(req: HttpRequest, pool: crate::db::Db, id: web::Path<Uuid>, payload: Multipart) -> Result<HttpResponse, AppError> {
    upload_for(req, pool, AssetTable::MenuItems, *id, payload).await
}

#[utoipa::path(
    post,
    path = "/uploads/categories/{category_id}",
    tag = "uploads",
    params(("category_id" = Uuid, Path, description = "Category ID")),
    request_body(content = UploadImageMultipart, content_type = "multipart/form-data"),
    responses((status = 200, description = "Image accepted for processing", body = UploadResponse), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn upload_category_image(req: HttpRequest, pool: crate::db::Db, id: web::Path<Uuid>, payload: Multipart) -> Result<HttpResponse, AppError> {
    upload_for(req, pool, AssetTable::Categories, *id, payload).await
}

#[utoipa::path(
    post,
    path = "/uploads/bundles/{bundle_id}",
    tag = "uploads",
    params(("bundle_id" = Uuid, Path, description = "Bundle ID")),
    request_body(content = UploadImageMultipart, content_type = "multipart/form-data"),
    responses((status = 200, description = "Image accepted for processing", body = UploadResponse), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn upload_bundle_image(req: HttpRequest, pool: crate::db::Db, id: web::Path<Uuid>, payload: Multipart) -> Result<HttpResponse, AppError> {
    upload_for(req, pool, AssetTable::Bundles, *id, payload).await
}

/// A URL saved into an image field by a JSON handler: our own legacy upload
/// path → staged from the uploads dir (org-scoped); an external https URL →
/// fetched by the worker (SSRF-guarded). Anything else is ignored.
pub async fn stage_image_url(
    pool: &sqlx::PgPool,
    org_id: Uuid,
    table: AssetTable,
    id: Uuid,
    url: &str,
    actor: Option<Uuid>,
) -> Result<Option<Uuid>, AppError> {
    let target = AssetTarget::new(table, id, AssetField::Image);
    let purpose = match table {
        AssetTable::MenuItems => AssetPurpose::MenuItemPhoto,
        AssetTable::Categories => AssetPurpose::CategoryPhoto,
        AssetTable::Bundles => AssetPurpose::BundlePhoto,
        _ => return Ok(None),
    };
    let store = crate::assets::AssetStore::from_env();
    if let Some(rel) = crate::assets::legacy_rel_from_url(url) {
        // Only this org's own files (or pre-org legacy dirs) may be re-staged.
        let first = rel.split('/').next().unwrap_or("");
        if Uuid::parse_str(first).is_ok_and(|o| o != org_id) {
            return Ok(None);
        }
        // Already an asset-minted URL: nothing to do.
        if rel.ends_with(".webp") && sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM asset_legacy_paths WHERE legacy_path = $1)").bind(&rel).fetch_one(pool).await? {
            return Ok(None);
        }
        let Some(path) = store.legacy_file(&rel) else { return Ok(None) };
        if tokio::fs::metadata(&path).await.is_err() {
            return Ok(None);
        }
        return stage(pool, Some(org_id), purpose, IngestSource::LegacyFile(path), None, target, actor).await.map(Some);
    }
    match url::Url::parse(url) {
        Ok(u) if u.scheme() == "https" => stage(pool, Some(org_id), purpose, IngestSource::Url(u), None, target, actor).await.map(Some),
        _ => Ok(None),
    }
}

pub fn extract_relative_path(url: &str) -> &str {
    if let Some(pos) = url.find("/uploads/") {
        &url[pos + 9..]
    } else if let Some(pos) = url.find("/logos/") {
        &url[pos + 1..]
    } else if url.starts_with("logos/") {
        url
    } else if let Some(pos) = url.find("/menu-items/") {
        let before = &url[..pos];
        if let Some(last_slash) = before.rfind('/') {
            &url[last_slash + 1..]
        } else {
            url
        }
    } else {
        url
    }
}

pub fn normalize_upload_url(url: &str) -> String {
    // Signed asset URLs are already absolute and must not be rewritten.
    if url.contains("/assets/") && url.contains("sig=") {
        return url.to_string();
    }
    let base_url = std::env::var("UPLOADS_BASE_URL")
        .unwrap_or_else(|_| "https://madar-pos.cloud/api/uploads".to_string());
    let base = base_url.trim_end_matches('/');
    let rel = extract_relative_path(url);
    format!("{}/{}", base, rel)
}

pub fn serialize_opt_url<S>(url: &Option<String>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match url {
        Some(u) => {
            let normalized = normalize_upload_url(u);
            serializer.serialize_some(&normalized)
        }
        None => serializer.serialize_none(),
    }
}

pub fn serialize_url<S>(url: &str, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    let normalized = normalize_upload_url(url);
    serializer.serialize_str(&normalized)
}

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}
