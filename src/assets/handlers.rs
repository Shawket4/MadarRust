//! Asset HTTP handlers (§11.4, §11.10).
//!
//! Every "not allowed" is a 404 — never 403 — so a hash's existence in an org
//! the caller cannot see is not observable.

use std::path::{Path, PathBuf};
use std::time::Duration;

use actix_web::http::{StatusCode, header};
use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use utoipa::ToSchema;
use uuid::Uuid;

use super::AssetStore;
use super::ingest::{AssetRef, DASHBOARD_TTL, asset_ref_from_row, verify_signature};
use super::tarball::{TarFile, stream_tar};
use crate::auth::jwt::{Claims, JwtSecret, verify_token};
use crate::errors::{AppError, AppErrorResponse, ErrorBody};
use crate::models::UserRole;

pub const MAX_TOPUP_HASHES: usize = 2000;
/// Signed URLs valid this far into the future may be cached publicly.
const PUBLIC_CACHE_MIN: i64 = 30 * 24 * 3600;

pub(crate) fn store(req: &HttpRequest) -> AssetStore {
    req.app_data::<web::Data<AssetStore>>()
        .map(|s| s.get_ref().clone())
        .unwrap_or_else(AssetStore::from_env)
}

pub(crate) fn base_pool(req: &HttpRequest) -> Result<PgPool, AppError> {
    req.app_data::<web::Data<PgPool>>()
        .map(|p| p.get_ref().clone())
        .ok_or(AppError::Internal)
}

fn not_found() -> AppError {
    AppError::NotFound("Not found".into())
}

/// Bearer claims when a valid token is present; `None` otherwise (the asset
/// route also accepts signed URLs, so a missing token is not an error).
fn optional_claims(req: &HttpRequest) -> Option<Claims> {
    if let Some(c) = req.extensions().get::<Claims>() {
        return Some(c.clone());
    }
    let token = req
        .headers()
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")?;
    let secret = req.app_data::<web::Data<JwtSecret>>()?;
    verify_token(secret, token).ok()
}

fn claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

/// Split `<hash>.webp` / `<hash>.lottie.zst`.
pub fn parse_file_name(file: &str) -> Option<(String, &'static str)> {
    let (hash, ext) = if let Some(h) = file.strip_suffix(".lottie.zst") {
        (h, "lottie.zst")
    } else if let Some(h) = file.strip_suffix(".webp") {
        (h, "webp")
    } else {
        return None;
    };
    super::is_hash(hash).then(|| (hash.to_string(), ext))
}

#[derive(Deserialize)]
pub struct SigQuery {
    pub exp: Option<i64>,
    pub sig: Option<String>,
}

/// `GET /assets/{org_id|global}/{hash}.{ext}`
pub async fn serve_asset(
    req: HttpRequest,
    path: web::Path<(String, String)>,
    q: web::Query<SigQuery>,
) -> Result<HttpResponse, AppError> {
    let (scope, file) = path.into_inner();
    let (hash, ext) = parse_file_name(&file).ok_or_else(not_found)?;
    let org_id: Option<Uuid> = if scope == "global" {
        if ext != "lottie.zst" {
            return Err(not_found());
        }
        None
    } else {
        Some(Uuid::parse_str(&scope).map_err(|_| not_found())?)
    };
    let key = AssetStore::key(org_id, &hash, ext);
    let now = chrono::Utc::now().timestamp();

    let signed = match (q.exp, q.sig.as_deref()) {
        (Some(exp), Some(sig)) => exp > now && verify_signature(&key, exp, sig),
        _ => false,
    };
    let jwt_ok = !signed
        && optional_claims(&req).is_some_and(|c| match org_id {
            None => true,
            Some(o) => c.org_id() == Some(o) || (c.role == UserRole::SuperAdmin && c.org_id.is_none()),
        });
    if !signed && !jwt_ok {
        return Err(not_found());
    }

    let pool = base_pool(&req)?;
    let row = sqlx::query(
        "SELECT content_type FROM assets WHERE org_id IS NOT DISTINCT FROM $1 AND hash = $2 AND ext = $3 LIMIT 1",
    )
    .bind(org_id)
    .bind(&hash)
    .bind(ext)
    .fetch_optional(&pool)
    .await?
    .ok_or_else(not_found)?;
    let content_type: String = row.get("content_type");
    let cache = if signed && q.exp.unwrap_or(0) - now >= PUBLIC_CACHE_MIN {
        "public, max-age=31536000, immutable"
    } else {
        "private, max-age=31536000, immutable"
    };
    let path = store(&req).path_for_key(&key);
    serve_ranged(&req, &path, &content_type, &format!("\"{hash}\""), cache).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeSpec {
    Full,
    Partial(u64, u64),
    Unsatisfiable,
}

/// One `bytes=` range (multi-range requests are served whole).
pub fn parse_range(h: Option<&str>, len: u64) -> RangeSpec {
    let Some(v) = h.and_then(|v| v.trim().strip_prefix("bytes=")) else {
        return RangeSpec::Full;
    };
    if v.contains(',') {
        return RangeSpec::Full;
    }
    let Some((a, b)) = v.split_once('-') else {
        return RangeSpec::Full;
    };
    let (a, b) = (a.trim(), b.trim());
    if len == 0 {
        return RangeSpec::Unsatisfiable;
    }
    match (a.parse::<u64>().ok(), b.parse::<u64>().ok()) {
        (Some(s), Some(e)) if s <= e && s < len => RangeSpec::Partial(s, e.min(len - 1)),
        (Some(s), None) if b.is_empty() && s < len => RangeSpec::Partial(s, len - 1),
        (None, Some(n)) if a.is_empty() && n > 0 => RangeSpec::Partial(len.saturating_sub(n), len - 1),
        (Some(_), _) | (None, Some(_)) => RangeSpec::Unsatisfiable,
        _ => RangeSpec::Full,
    }
}

/// Serve a file with `Accept-Ranges`, single `Range`, `If-Range` (ETag),
/// `If-None-Match`, and HEAD.
pub async fn serve_ranged(
    req: &HttpRequest,
    path: &Path,
    content_type: &str,
    etag: &str,
    cache_control: &str,
) -> Result<HttpResponse, AppError> {
    let meta = tokio::fs::metadata(path).await.map_err(|_| not_found())?;
    if !meta.is_file() {
        return Err(not_found());
    }
    let len = meta.len();
    let builder = |status: StatusCode| {
        let mut b = HttpResponse::build(status);
        b.insert_header((header::CONTENT_TYPE, content_type.to_string()))
            .insert_header((header::CACHE_CONTROL, cache_control.to_string()))
            .insert_header((header::ETAG, etag.to_string()))
            .insert_header((header::ACCEPT_RANGES, "bytes"))
            .insert_header(("X-Content-Type-Options", "nosniff"));
        b
    };
    if let Some(inm) = req.headers().get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok())
        && inm.split(',').any(|t| t.trim() == etag || t.trim() == "*")
    {
        return Ok(builder(StatusCode::NOT_MODIFIED).finish());
    }
    let if_range_ok = req
        .headers()
        .get(header::IF_RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim() == etag)
        .unwrap_or(true);
    let range = if if_range_ok {
        parse_range(
            req.headers().get(header::RANGE).and_then(|v| v.to_str().ok()),
            len,
        )
    } else {
        RangeSpec::Full
    };
    let head = req.method() == actix_web::http::Method::HEAD;
    let (status, start, end) = match range {
        RangeSpec::Full => (StatusCode::OK, 0, len.saturating_sub(1)),
        RangeSpec::Partial(s, e) => (StatusCode::PARTIAL_CONTENT, s, e),
        RangeSpec::Unsatisfiable => {
            return Ok(builder(StatusCode::RANGE_NOT_SATISFIABLE)
                .insert_header((header::CONTENT_RANGE, format!("bytes */{len}")))
                .finish());
        }
    };
    let count = if len == 0 { 0 } else { end - start + 1 };
    let mut b = builder(status);
    if status == StatusCode::PARTIAL_CONTENT {
        b.insert_header((header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}")));
    }
    b.no_chunking(count);
    if head || count == 0 {
        return Ok(b.body(Bytes::new()));
    }
    let mut file = tokio::fs::File::open(path).await.map_err(|_| not_found())?;
    file.seek(std::io::SeekFrom::Start(start))
        .await
        .map_err(|_| AppError::Internal)?;
    let stream = futures::stream::unfold((file, count), |(mut f, left)| async move {
        if left == 0 {
            return None;
        }
        let mut buf = vec![0u8; (64 * 1024).min(left as usize)];
        match f.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<_, std::io::Error>(Bytes::from(buf)), (f, left - n as u64)))
            }
            Err(e) => Some((Err(e), (f, 0))),
        }
    });
    Ok(b.streaming(stream))
}

// ── Jobs ────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, ToSchema)]
pub struct AssetJobResult {
    pub pos: AssetRef,
    pub full: Option<AssetRef>,
    pub variants: Vec<AssetRef>,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct AssetJobView {
    pub id: Uuid,
    /// queued | running | done | failed
    pub status: String,
    pub result: Option<AssetJobResult>,
    pub error: Option<String>,
}

#[utoipa::path(get, path = "/assets/jobs/{id}", tag = "assets",
    params(("id" = Uuid, Path, description = "Asset job id")),
    responses((status = 200, body = AssetJobView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn get_job(req: HttpRequest, id: web::Path<Uuid>) -> Result<HttpResponse, AppError> {
    let c = claims(&req)?;
    let pool = base_pool(&req)?;
    let row = sqlx::query("SELECT id, org_id, status, last_error, result_group_id FROM asset_jobs WHERE id = $1")
        .bind(*id)
        .fetch_optional(&pool)
        .await?
        .ok_or_else(not_found)?;
    let org_id: Option<Uuid> = row.get("org_id");
    let allowed = c.role == UserRole::SuperAdmin || (org_id.is_some() && c.org_id() == org_id);
    if !allowed {
        return Err(not_found());
    }
    let status: String = row.get("status");
    let mut result = None;
    if let Some(aid) = row.get::<Option<Uuid>, _>("result_group_id") {
        let rows = sqlx::query(&format!(
            "SELECT {} FROM assets a WHERE a.group_id = $1 \
             ORDER BY CASE a.variant WHEN 'thumb' THEN 0 WHEN 'tile' THEN 1 WHEN 'full' THEN 2 ELSE 3 END",
            super::ingest::ASSET_COLS
        ))
        .bind(aid)
        .fetch_all(&pool)
        .await?;
        let variants: Vec<AssetRef> = rows.iter().map(|r| asset_ref_from_row(r, DASHBOARD_TTL)).collect();
        let pos = variants
            .iter()
            .find(|v| v.variant == "tile" || v.variant == "animation")
            .or_else(|| variants.iter().find(|v| v.variant == "full"))
            .cloned();
        let full = variants.iter().find(|v| v.variant == "full").cloned();
        if let Some(pos) = pos {
            result = Some(AssetJobResult { pos, full, variants });
        }
    }
    Ok(HttpResponse::Ok().json(AssetJobView {
        id: *id,
        status,
        result,
        error: row.get("last_error"),
    }))
}

// ── Bundles ─────────────────────────────────────────────────────────────────

/// `assets-<branch_uuid>-<seq>.tar` → (branch, seq)
pub fn parse_bundle_name(name: &str) -> Option<(Uuid, i64)> {
    let rest = name.strip_prefix("assets-")?.strip_suffix(".tar")?;
    if rest.len() < 38 {
        return None;
    }
    let (b, s) = rest.split_at(36);
    let seq = s.strip_prefix('-')?;
    if seq.is_empty() || !seq.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((Uuid::parse_str(b).ok()?, seq.parse().ok()?))
}

fn branch_allowed(c: &Claims, org_id: Uuid, branch_id: Uuid) -> bool {
    if c.role == UserRole::SuperAdmin && c.org_id.is_none() {
        return true;
    }
    if c.org_id() != Some(org_id) {
        return false;
    }
    match c.branch_id() {
        Some(b) => b == branch_id,
        None => true,
    }
}

#[utoipa::path(get, path = "/sync/asset-bundles/{org_id}/{file_name}", tag = "assets",
    params(("org_id" = Uuid, Path), ("file_name" = String, Path, description = "assets-<branch_id>-<seq>.tar")),
    responses((status = 200, description = "Tar (Range/If-Range supported)"), (status = 206, description = "Partial"), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn get_bundle(
    req: HttpRequest,
    path: web::Path<(Uuid, String)>,
) -> Result<HttpResponse, AppError> {
    let c = claims(&req)?;
    let (org_id, name) = path.into_inner();
    let (branch_id, seq) = parse_bundle_name(&name).ok_or_else(not_found)?;
    if !branch_allowed(&c, org_id, branch_id) {
        return Err(not_found());
    }
    let pool = base_pool(&req)?;
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT b.file_key, b.sha256 FROM asset_bundles b JOIN branches br ON br.id = b.branch_id \
         WHERE b.branch_id = $1 AND b.seq = $2 AND b.org_id = $3 AND br.org_id = $3",
    )
    .bind(branch_id)
    .bind(seq)
    .bind(org_id)
    .fetch_optional(&pool)
    .await?;
    let (key, sha) = row.ok_or_else(not_found)?;
    let path = store(&req).path_for_key(&key);
    serve_ranged(
        &req,
        &path,
        "application/x-tar",
        &format!("\"{sha}\""),
        "private, max-age=31536000, immutable",
    )
    .await
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct TopUpRequest {
    pub branch_id: Uuid,
    pub hashes: Vec<String>,
}

#[utoipa::path(post, path = "/sync/assets", tag = "assets", request_body = TopUpRequest,
    responses((status = 200, description = "application/x-tar: index.json then <hash>.<ext> entries"), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn top_up(
    req: HttpRequest,
    body: web::Json<TopUpRequest>,
) -> Result<HttpResponse, AppError> {
    let c = claims(&req)?;
    if body.hashes.len() > MAX_TOPUP_HASHES {
        return Ok(HttpResponse::BadRequest().json(ErrorBody {
            error: format!("At most {MAX_TOPUP_HASHES} hashes per request"),
            code: Some("TOO_MANY_HASHES".into()),
            till: None,
        }));
    }
    let pool = base_pool(&req)?;
    let org_id: Uuid = sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1")
        .bind(body.branch_id)
        .fetch_optional(&pool)
        .await?
        .ok_or_else(not_found)?;
    if !branch_allowed(&c, org_id, body.branch_id) {
        return Err(not_found());
    }
    let mut wanted: Vec<String> = body
        .hashes
        .iter()
        .map(|h| h.to_ascii_lowercase())
        .filter(|h| super::is_hash(h))
        .collect();
    wanted.sort();
    wanted.dedup();
    let rows = sqlx::query(
        "SELECT DISTINCT a.org_id, a.hash, a.ext, a.bytes FROM assets a \
         WHERE a.hash = ANY($1) AND ((a.org_id = $2 AND (a.variant IN ('tile','animation') OR (a.variant = 'full' AND NOT EXISTS (SELECT 1 FROM assets t WHERE t.group_id = a.group_id AND t.variant = 'tile')))) \
                                   OR (a.org_id IS NULL AND a.variant = 'animation'))",
    )
    .bind(&wanted)
    .bind(org_id)
    .fetch_all(&pool)
    .await?;
    let st = store(&req);
    let mut files: Vec<TarFile> = Vec::new();
    for r in rows {
        let o: Option<Uuid> = r.get("org_id");
        let hash: String = r.get("hash");
        let ext: String = r.get("ext");
        let bytes = r.get::<i64, _>("bytes") as u64;
        let path: PathBuf = st.path_for_key(&AssetStore::key(o, &hash, &ext));
        let on_disk = tokio::fs::metadata(&path)
            .await
            .map(|m| m.len() == bytes)
            .unwrap_or(false);
        if on_disk && !files.iter().any(|f| f.hash == hash && f.ext == ext) {
            files.push(TarFile { hash, ext, bytes, path });
        }
    }
    super::tarball::sort_files(&mut files);
    let missing: Vec<String> = wanted
        .into_iter()
        .filter(|h| !files.iter().any(|f| &f.hash == h))
        .collect();
    Ok(HttpResponse::Ok()
        .content_type("application/x-tar")
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .streaming(stream_tar(files, missing)))
}

// ── Legacy uploads (old clients) ────────────────────────────────────────────

/// `GET /uploads/{tail}`: the original file while it exists; after prune (or
/// for URLs minted by `attach`) a 302 to a 7-day signed URL of the `full`
/// variant via `asset_legacy_paths`; otherwise 404.
pub async fn legacy_upload(req: HttpRequest, tail: web::Path<String>) -> Result<HttpResponse, AppError> {
    let rel = tail.into_inner();
    let st = store(&req);
    let file = st.legacy_file(&rel).ok_or_else(not_found)?;
    if let Ok(m) = tokio::fs::metadata(&file).await
        && m.is_file()
    {
        let ct = mime_for_legacy(&rel);
        let etag = format!("\"{:x}-{:x}\"", m.len(), m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0));
        return serve_ranged(&req, &file, ct, &etag, "public, max-age=86400").await;
    }
    let pool = base_pool(&req)?;
    let row = sqlx::query(
        "SELECT a.org_id, a.hash, a.ext FROM asset_legacy_paths l JOIN assets a ON a.id = l.asset_id \
         WHERE l.legacy_path = $1 AND a.org_id = l.org_id",
    )
    .bind(&rel)
    .fetch_optional(&pool)
    .await?
    .ok_or_else(not_found)?;
    let url = super::ingest::signed_url(
        row.get("org_id"),
        &row.get::<String, _>("hash"),
        &row.get::<String, _>("ext"),
        Duration::from_secs(7 * 24 * 3600),
    );
    Ok(HttpResponse::Found()
        .insert_header((header::LOCATION, url))
        .insert_header((header::CACHE_CONTROL, "private, max-age=3600"))
        .finish())
}

fn mime_for_legacy(rel: &str) -> &'static str {
    let lower = rel.to_ascii_lowercase();
    match lower.rsplit('.').next().unwrap_or("") {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}
