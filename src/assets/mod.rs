//! Content-addressed, org-scoped assets (TILLS_DECISIONS 17-19, contract §11).
//!
//! ONE way in: every image or animation the backend stores goes through
//! [`ingest::stage`] (request path: cheap sniff + staging + a queued job) and
//! [`ingest::ingest`] (worker / backfill / preset reconcile: decode, strip,
//! resize, WebP/zstd, hash, store `<org_id>/<content_hash>.<ext>`, record rows).
//! Nothing else in `src/` writes image bytes to disk.
//!
//! Layout under `ASSETS_DIR` (default `${UPLOADS_DIR}/assets`):
//! - `<org_id>/<sha256>.webp`, `<org_id>/<sha256>.lottie.zst`, `global/<sha256>.lottie.zst`
//! - `<org_id>/bundles/assets-<branch_id>-<seq>.tar`
//! - `_staging/<job_id>` (server-generated names only)

pub mod backfill;
pub mod bundle;
pub mod handlers;
pub mod ingest;
pub mod refs;
pub mod routes;
pub mod tarball;
pub mod worker;

#[cfg(test)]
pub(crate) mod tests;

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Where asset files and legacy uploads live. Passed explicitly (never read
/// from a process-global in library code paths that tests run in parallel);
/// [`AssetStore::from_env`] is the production constructor.
#[derive(Clone, Debug)]
pub struct AssetStore {
    pub assets_dir: PathBuf,
    pub uploads_dir: PathBuf,
}

impl AssetStore {
    pub fn from_env() -> Self {
        let uploads_dir =
            PathBuf::from(std::env::var("UPLOADS_DIR").unwrap_or_else(|_| "./uploads".into()));
        let assets_dir = std::env::var("ASSETS_DIR")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| uploads_dir.join("assets"));
        Self {
            assets_dir,
            uploads_dir,
        }
    }

    pub fn new(assets_dir: impl Into<PathBuf>, uploads_dir: impl Into<PathBuf>) -> Self {
        Self {
            assets_dir: assets_dir.into(),
            uploads_dir: uploads_dir.into(),
        }
    }

    /// `<org_id>/<hash>.<ext>` or `global/<hash>.<ext>`.
    pub fn key(org_id: Option<Uuid>, hash: &str, ext: &str) -> String {
        match org_id {
            Some(o) => format!("{o}/{hash}.{ext}"),
            None => format!("global/{hash}.{ext}"),
        }
    }

    pub fn path_for_key(&self, key: &str) -> PathBuf {
        self.assets_dir.join(key)
    }

    pub fn staging_dir(&self) -> PathBuf {
        self.assets_dir.join("_staging")
    }

    pub fn bundle_key(org_id: Uuid, branch_id: Uuid, seq: i64) -> String {
        format!("{org_id}/bundles/assets-{branch_id}-{seq}.tar")
    }

    /// Resolve a legacy uploads-relative path (`<org>/menu-items/x.jpg`,
    /// `logos/x.png`) to a file under `uploads_dir`, refusing traversal and the
    /// asset store itself (hash names are not access control).
    pub fn legacy_file(&self, rel: &str) -> Option<PathBuf> {
        let rel = rel.trim_start_matches('/');
        if !safe_rel_path(rel) {
            return None;
        }
        // The asset store may live under UPLOADS_DIR; never expose it through
        // the legacy (unauthenticated) path.
        let candidate = self.uploads_dir.join(rel);
        if let (Ok(a), Ok(c)) = (
            std::fs::canonicalize(&self.assets_dir),
            std::fs::canonicalize(&candidate),
        ) && c.starts_with(&a)
        {
            return None;
        }
        Some(candidate)
    }
}

/// Serialize everyone who creates, references or deletes the file `key`
/// within the caller's transaction (released at commit/rollback). Take several
/// keys in sorted order.
pub async fn lock_file_key(conn: &mut sqlx::PgConnection, key: &str) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 7019))")
        .bind(key)
        .execute(conn)
        .await
        .map(|_| ())
}

/// A content-addressed file is shared by every `assets` row with the same
/// (org, hash): delete it only when no committed row references it. The caller
/// must hold [`lock_file_key`] for this key in `conn`'s transaction, so no
/// concurrent ingest can be between "the file exists, skip the write" and
/// committing its row. Returns whether the file was removed.
pub async fn remove_file_if_unreferenced(
    conn: &mut sqlx::PgConnection,
    store: &AssetStore,
    org_id: Option<Uuid>,
    hash: &str,
    ext: &str,
) -> Result<bool, sqlx::Error> {
    let referenced: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM assets WHERE org_id IS NOT DISTINCT FROM $1 AND hash = $2 AND ext = $3)",
    )
    .bind(org_id)
    .bind(hash)
    .bind(ext)
    .fetch_one(conn)
    .await?;
    if referenced {
        return Ok(false);
    }
    Ok(std::fs::remove_file(store.path_for_key(&AssetStore::key(org_id, hash, ext))).is_ok())
}

/// A relative path with no traversal, no absolute root, no backslashes and no
/// hidden segments.
pub fn safe_rel_path(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.starts_with('/')
        && !rel.contains('\\')
        && !rel.contains('\0')
        && rel
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != ".." && !seg.starts_with('.'))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn is_hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The HMAC key for signed asset URLs. `ASSET_URL_SECRET` (>= 32 bytes). Debug
/// and test builds fall back to a key derived from `JWT_SECRET` (or a fixed
/// development string) so local runs work; release builds refuse to start
/// without it (see [`require_secret`]).
pub fn url_secret() -> Vec<u8> {
    match std::env::var("ASSET_URL_SECRET") {
        Ok(s) if s.len() >= 32 => s.into_bytes(),
        _ => {
            let jwt = std::env::var("JWT_SECRET").unwrap_or_default();
            format!("madar-dev-asset-url-secret:{jwt}").into_bytes()
        }
    }
}

/// Called from the boot-time `spawn`s: a production binary without a real
/// secret would mint URLs anyone who has read this file can forge. `main`
/// already refused to start without it ([`crate::boot_config`]), before
/// migrating; this is the backstop for other entry points.
pub fn require_secret() {
    let ok = std::env::var("ASSET_URL_SECRET")
        .map(|s| s.len() >= 32)
        .unwrap_or(false);
    if !ok {
        if cfg!(debug_assertions) {
            tracing::warn!("ASSET_URL_SECRET unset/short — using a development key");
        } else {
            panic!("ASSET_URL_SECRET must be set (>= 32 bytes) in production builds");
        }
    }
}

/// Origin + prefix under which this API is reachable, used to make asset URLs
/// absolute: `API_PUBLIC_URL`, else `UPLOADS_BASE_URL` minus its `/uploads`
/// suffix, else empty (relative URLs).
pub fn api_base() -> String {
    if let Ok(b) = std::env::var("API_PUBLIC_URL")
        && !b.trim().is_empty()
    {
        return b.trim_end_matches('/').to_string();
    }
    std::env::var("UPLOADS_BASE_URL")
        .ok()
        .map(|u| {
            let u = u.trim_end_matches('/');
            u.strip_suffix("/uploads").unwrap_or(u).to_string()
        })
        .unwrap_or_default()
}

/// Uploads-relative path of a legacy image URL (`https://…/uploads/<org>/menu-items/x.jpg`
/// → `<org>/menu-items/x.jpg`). `None` for URLs that are not ours.
pub fn legacy_rel_from_url(url: &str) -> Option<String> {
    let url = url.split(['?', '#']).next().unwrap_or(url);
    if let Some(pos) = url.find("/uploads/") {
        return Some(url[pos + "/uploads/".len()..].to_string());
    }
    if let Ok(base) = std::env::var("UPLOADS_BASE_URL") {
        let base = base.trim_end_matches('/');
        if !base.is_empty()
            && let Some(rest) = url.strip_prefix(base)
        {
            return Some(rest.trim_start_matches('/').to_string());
        }
    }
    if url.starts_with("logos/") || url.starts_with("card/") {
        return Some(url.to_string());
    }
    if !url.contains("://") && !url.starts_with('/') && url.contains('/') {
        return Some(url.to_string());
    }
    None
}

/// Write bytes atomically: tmp file in the same dir, fsync, rename.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".tmp-{}", Uuid::new_v4()));
    let res = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}
