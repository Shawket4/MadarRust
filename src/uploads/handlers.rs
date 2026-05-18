use actix_multipart::Multipart;
use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
use futures::StreamExt;
use image::ImageReader;
use serde::Serialize;
use sqlx::PgPool;
use std::{io::Cursor, path::{Path, PathBuf}};
use uuid::Uuid;
use crate::{auth::jwt::Claims, errors::AppError, models::UserRole, permissions::checker::check_permission};

const ALLOWED_MIME: &[&str] = &[
    "image/jpeg","image/png","image/gif","image/webp",
    "image/bmp","image/x-bmp","image/x-ms-bmp",
];
const MAX_BYTES: usize = 2 * 1024 * 1024;

#[derive(Serialize)]
pub struct UploadResponse {
    #[serde(serialize_with = "serialize_url")]
    pub image_url: String,
}

pub async fn upload_menu_item_image(
    req:          HttpRequest,
    pool:         web::Data<PgPool>,
    menu_item_id: web::Path<Uuid>,
    mut payload:  Multipart,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let row: Option<(Uuid, Option<String>)> = sqlx::query_as(
        "SELECT org_id, image_url FROM menu_items WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(*menu_item_id)
    .fetch_optional(pool.get_ref())
    .await?;

    let (org_id, old_image_url) = row
        .ok_or_else(|| AppError::NotFound("Menu item not found".into()))?;

    if claims.role != UserRole::SuperAdmin
        && claims.org_id() != Some(org_id) {
            return Err(AppError::Forbidden("Menu item belongs to a different org".into()));
        }

    let uploads_dir = std::env::var("UPLOADS_DIR").map_err(|_| AppError::Internal)?;
    let base_url    = std::env::var("UPLOADS_BASE_URL").map_err(|_| AppError::Internal)?;

    let mut file_bytes: Option<Vec<u8>> = None;

    while let Some(item) = payload.next().await {
        let mut field = item.map_err(|_| AppError::BadRequest("Invalid multipart data".into()))?;
        let content_type = field.content_type().map(|m| m.to_string()).unwrap_or_default();
        let field_name   = field
            .content_disposition()
            .and_then(|cd| cd.get_name())
            .unwrap_or("")
            .to_string();

        if field_name != "image" { continue; }
        if !ALLOWED_MIME.contains(&content_type.as_str()) {
            return Err(AppError::BadRequest(format!("Unsupported image type: {}", content_type)));
        }

        let mut bytes = Vec::new();
        while let Some(chunk) = field.next().await {
            let chunk = chunk.map_err(|_| AppError::BadRequest("Failed reading upload".into()))?;
            bytes.extend_from_slice(&chunk);
            if bytes.len() > 20 * 1024 * 1024 {
                return Err(AppError::BadRequest("File too large (max 20 MB raw)".into()));
            }
        }
        file_bytes = Some(bytes);
        break;
    }

    let raw_bytes = file_bytes
        .ok_or_else(|| AppError::BadRequest("No image field found in upload".into()))?;

    let jpeg_bytes = compress_to_jpeg(&raw_bytes)?;

    let filename  = format!("{}.jpg", Uuid::new_v4());
    let dir_path  = Path::new(&uploads_dir).join(org_id.to_string()).join("menu-items");
    tokio::fs::create_dir_all(&dir_path).await.map_err(|e| {
        tracing::error!("Failed to create upload dir: {}", e); AppError::Internal
    })?;
    let file_path: PathBuf = dir_path.join(&filename);

    // 1. Write new file to disk first
    tokio::fs::write(&file_path, &jpeg_bytes).await.map_err(|e| {
        tracing::error!("Failed to write image: {}", e); AppError::Internal
    })?;

    let base      = base_url.trim_end_matches('/');
    let image_url = format!("{}/{}/menu-items/{}", base, org_id, filename);

    // 2. Update DB — if this fails, clean up the newly written file
    if let Err(e) = sqlx::query("UPDATE menu_items SET image_url = $1 WHERE id = $2")
        .bind(&image_url)
        .bind(*menu_item_id)
        .execute(pool.get_ref())
        .await
    {
        let _ = tokio::fs::remove_file(&file_path).await;
        tracing::error!("DB update failed, cleaned up new file: {}", e);
        return Err(AppError::from(e));
    }

    // 3. Delete old image ONLY after successful DB update
    if let Some(old_url) = old_image_url {
        delete_old_image(&old_url, &base_url, &uploads_dir).await;
    }

    tracing::info!("Uploaded image for menu_item {} → {} ({} KB)",
        menu_item_id, image_url, jpeg_bytes.len() / 1024);

    Ok(HttpResponse::Ok().json(UploadResponse { image_url }))
}

fn compress_to_jpeg(raw: &[u8]) -> Result<Vec<u8>, AppError> {
    let img = ImageReader::new(Cursor::new(raw))
        .with_guessed_format()
        .map_err(|_| AppError::BadRequest("Could not decode image".into()))?
        .decode()
        .map_err(|e| AppError::BadRequest(format!("Invalid image: {}", e)))?;

    let qualities: &[u8] = if raw.len() <= MAX_BYTES { &[85] } else { &[85, 75, 65, 50, 40] };
    for &quality in qualities {
        let mut buf = Cursor::new(Vec::new());
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
        enc.encode_image(&img)
            .map_err(|e| AppError::BadRequest(format!("Encoding failed: {}", e)))?;
        let bytes = buf.into_inner();
        if bytes.len() <= MAX_BYTES || quality == 40 { return Ok(bytes); }
    }
    Err(AppError::Internal)
}

pub async fn delete_old_image(old_url: &str, _base_url: &str, uploads_dir: &str) {
    let rel = extract_relative_path(old_url);
    let full = Path::new(uploads_dir).join(rel);
    if full.exists() {
        if let Err(e) = tokio::fs::remove_file(&full).await {
            tracing::warn!("Could not delete old image {:?}: {}", full, e);
        }
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
    let base_url = std::env::var("UPLOADS_BASE_URL").unwrap_or_else(|_| "https://sufrix-pos.ddns.net/api/uploads".to_string());
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
    req.extensions().get::<Claims>().cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

