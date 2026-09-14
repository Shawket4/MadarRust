//! Debounced per-branch base bundle builder (§11.4, §11.10).
//!
//! Every 60 s: branches whose `asset_bundle_dirty.dirty_since` is older than the
//! debounce (10 min after the LAST asset change) get a fresh
//! `<org_id>/bundles/assets-<branch_id>-<seq>.tar` holding exactly the POS
//! variants (`tile` + `animation`) their live rows reference. The tar is
//! deterministic, so an unchanged set produces the same sha256 and nothing is
//! written. The latest 2 bundles per branch are kept.

use std::collections::BTreeMap;
use std::time::Duration;

use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use super::AssetStore;
use super::tarball::{TarFile, sort_files, write_tar};
use crate::errors::AppError;

pub const DEBOUNCE: Duration = Duration::from_secs(10 * 60);
const TICK: Duration = Duration::from_secs(60);
const KEEP: i64 = 2;

pub fn spawn(pool: PgPool) {
    let store = AssetStore::from_env();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(TICK);
        loop {
            tick.tick().await;
            if let Err(e) = run_due(&pool, &store, DEBOUNCE).await {
                tracing::warn!(error = %e, "asset bundle tick failed");
            }
        }
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltBundle {
    pub branch_id: Uuid,
    pub org_id: Uuid,
    pub seq: i64,
    pub file_key: String,
    pub bytes: i64,
    pub sha256: String,
    pub file_count: i32,
    /// false when the content equalled the latest bundle (nothing written).
    pub written: bool,
}

/// Build every branch dirty for longer than `debounce`. Returns what was built.
pub async fn run_due(
    pool: &PgPool,
    store: &AssetStore,
    debounce: Duration,
) -> Result<Vec<BuiltBundle>, AppError> {
    let due: Vec<(Uuid, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT branch_id, dirty_since FROM asset_bundle_dirty \
         WHERE dirty_since <= now() - make_interval(secs => $1) ORDER BY dirty_since",
    )
    .bind(debounce.as_secs() as f64)
    .fetch_all(pool)
    .await?;
    let mut out = Vec::new();
    for (branch_id, since) in due {
        match build_branch(pool, store, branch_id).await {
            Ok(b) => {
                sqlx::query("DELETE FROM asset_bundle_dirty WHERE branch_id = $1 AND dirty_since <= $2")
                    .bind(branch_id)
                    .bind(since)
                    .execute(pool)
                    .await?;
                if let Some(b) = b {
                    out.push(b);
                }
            }
            Err(e) => tracing::warn!(%branch_id, error = %e, "asset bundle build failed"),
        }
    }
    Ok(out)
}

/// The POS-variant files a branch references, sorted, de-duplicated.
pub async fn referenced_files(
    pool: &PgPool,
    store: &AssetStore,
    branch_id: Uuid,
) -> Result<(Uuid, Vec<TarFile>), AppError> {
    let org_id: Uuid = sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1")
        .bind(branch_id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| AppError::NotFound("Branch not found".into()))?;
    let rows = sqlx::query(
        "WITH g AS ( \
            SELECT image_group_id AS gid FROM menu_items WHERE org_id = $1 AND deleted_at IS NULL AND image_group_id IS NOT NULL \
            UNION SELECT image_group_id FROM categories WHERE org_id = $1 AND deleted_at IS NULL AND image_group_id IS NOT NULL \
            UNION SELECT image_group_id FROM bundles WHERE org_id = $1 AND image_group_id IS NOT NULL \
            UNION SELECT logo_group_id FROM organizations WHERE id = $1 AND logo_group_id IS NOT NULL \
            UNION SELECT p.animation_group_id FROM recipe_step_presets p \
                   JOIN menu_item_recipe_steps s ON s.preset_slug = p.slug \
                  WHERE s.org_id = $1 AND p.is_active AND p.animation_group_id IS NOT NULL) \
         SELECT DISTINCT a.org_id, a.hash, a.ext, a.bytes FROM assets a JOIN g ON a.group_id = g.gid \
         WHERE (a.variant IN ('tile','animation') OR (a.variant = 'full' AND NOT EXISTS (SELECT 1 FROM assets t WHERE t.group_id = a.group_id AND t.variant = 'tile'))) AND (a.org_id = $1 OR a.org_id IS NULL)",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await?;
    let mut uniq: BTreeMap<(String, String), TarFile> = BTreeMap::new();
    for r in rows {
        let o: Option<Uuid> = r.get("org_id");
        let hash: String = r.get("hash");
        let ext: String = r.get("ext");
        let path = store.path_for_key(&AssetStore::key(o, &hash, &ext));
        uniq.entry((hash.clone(), ext.clone())).or_insert(TarFile {
            hash,
            ext,
            bytes: r.get::<i64, _>("bytes") as u64,
            path,
        });
    }
    let mut files: Vec<TarFile> = uniq.into_values().collect();
    sort_files(&mut files);
    Ok((org_id, files))
}

async fn feed_seq(pool: &PgPool, branch_id: Uuid) -> i64 {
    // The changefeed (§10.1) may not exist on an older schema; fall back to 0.
    sqlx::query_scalar::<_, Option<i64>>("SELECT max(seq) FROM sync_changes WHERE branch_id = $1")
        .bind(branch_id)
        .fetch_one(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(0)
}

pub async fn build_branch(
    pool: &PgPool,
    store: &AssetStore,
    branch_id: Uuid,
) -> Result<Option<BuiltBundle>, AppError> {
    let (org_id, files) = referenced_files(pool, store, branch_id).await?;
    // Drop files missing on disk rather than failing the whole bundle; the
    // top-up path reports them as missing.
    let files: Vec<TarFile> = files
        .into_iter()
        .filter(|f| std::fs::metadata(&f.path).map(|m| m.len() == f.bytes).unwrap_or(false))
        .collect();

    let latest: Option<(i64, String)> = sqlx::query_as(
        "SELECT seq, sha256 FROM asset_bundles WHERE branch_id = $1 ORDER BY seq DESC LIMIT 1",
    )
    .bind(branch_id)
    .fetch_optional(pool)
    .await?;

    let staging = store.staging_dir();
    let tmp = staging.join(format!("bundle-{}", Uuid::new_v4()));
    let files_c = files.clone();
    let tmp_c = tmp.clone();
    let (sha, bytes) = tokio::task::spawn_blocking(move || -> std::io::Result<(String, u64)> {
        std::fs::create_dir_all(tmp_c.parent().expect("staging parent"))?;
        let f = std::fs::File::create(&tmp_c)?;
        let mut w = HashingWriter {
            inner: std::io::BufWriter::new(f),
            hasher: Sha256::new(),
            n: 0,
        };
        write_tar(&mut w, &files_c, None)?;
        use std::io::Write;
        w.inner.flush()?;
        w.inner.get_ref().sync_all()?;
        Ok((format!("{:x}", w.hasher.finalize()), w.n))
    })
    .await
    .map_err(|_| AppError::Internal)?
    .map_err(|e| {
        tracing::error!(error = %e, "bundle write failed");
        AppError::Internal
    })?;

    if let Some((_, latest_sha)) = &latest
        && *latest_sha == sha
    {
        let _ = std::fs::remove_file(&tmp);
        return Ok(None);
    }
    let seq = feed_seq(pool, branch_id)
        .await
        .max(latest.as_ref().map(|l| l.0 + 1).unwrap_or(1));
    let key = AssetStore::bundle_key(org_id, branch_id, seq);
    let dest = store.path_for_key(&key);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|_| AppError::Internal)?;
    }
    std::fs::rename(&tmp, &dest).map_err(|_| AppError::Internal)?;
    sqlx::query(
        "INSERT INTO asset_bundles (branch_id, org_id, seq, file_key, bytes, sha256, file_count) \
         VALUES ($1,$2,$3,$4,$5,$6,$7)",
    )
    .bind(branch_id)
    .bind(org_id)
    .bind(seq)
    .bind(&key)
    .bind(bytes as i64)
    .bind(&sha)
    .bind(files.len() as i32)
    .execute(pool)
    .await?;

    // Keep the latest KEEP bundles (a device mid-download of the previous one
    // can still finish it).
    let old: Vec<(i64, String)> = sqlx::query_as(
        "SELECT seq, file_key FROM asset_bundles WHERE branch_id = $1 ORDER BY seq DESC OFFSET $2",
    )
    .bind(branch_id)
    .bind(KEEP)
    .fetch_all(pool)
    .await?;
    for (s, k) in old {
        sqlx::query("DELETE FROM asset_bundles WHERE branch_id = $1 AND seq = $2")
            .bind(branch_id)
            .bind(s)
            .execute(pool)
            .await?;
        let _ = std::fs::remove_file(store.path_for_key(&k));
    }

    Ok(Some(BuiltBundle {
        branch_id,
        org_id,
        seq,
        file_key: key,
        bytes: bytes as i64,
        sha256: sha,
        file_count: files.len() as i32,
        written: true,
    }))
}

struct HashingWriter<W: std::io::Write> {
    inner: W,
    hasher: Sha256,
    n: u64,
}

impl<W: std::io::Write> std::io::Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.n += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
