//! Background ingest worker (§11.3). Claims `asset_jobs` with
//! `FOR UPDATE SKIP LOCKED`, runs [`ingest`] then [`attach`] in one
//! transaction, retries up to 5 times with 30 s × attempt backoff, and deletes
//! the staged file once the job is `done` (or finally `failed`).
//!
//! Heavy work is serialized by the process-wide conversion semaphore in
//! `ingest` (default 1), so a weak VPS converts one picture at a time and the
//! request path never decodes.
//!
//! Backoff: a `queued` row is claimable when `updated_at <= now() - 30 s × attempts`.

use std::time::Duration;

use sqlx::{PgPool, Row};
use uuid::Uuid;

use super::AssetStore;
use super::ingest::{AssetPurpose, AssetTarget, IngestSource, SourceKind, attach, ingest_with};
use crate::errors::AppError;

pub const MAX_ATTEMPTS: i32 = 5;
pub const BACKOFF_STEP: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_secs(2);
/// A `running` job this old belonged to a process that died.
const STALE_RUNNING: Duration = Duration::from_secs(15 * 60);

pub fn spawn(pool: PgPool) {
    super::require_secret();
    let store = AssetStore::from_env();
    tokio::spawn(async move {
        loop {
            match run_one(&pool, &store).await {
                Ok(Some(_)) => continue,
                Ok(None) => tokio::time::sleep(POLL).await,
                Err(e) => {
                    tracing::warn!(error = %e, "asset worker poll failed");
                    tokio::time::sleep(POLL * 5).await;
                }
            }
        }
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobResult {
    Done(Uuid),
    Retry(Uuid),
    Failed(Uuid),
}

/// Claim and process at most one job. `Ok(None)` when nothing is due.
pub async fn run_one(pool: &PgPool, store: &AssetStore) -> Result<Option<JobResult>, AppError> {
    // Recover jobs orphaned by a crash.
    sqlx::query(
        "UPDATE asset_jobs SET status = 'queued', updated_at = now() \
         WHERE status = 'running' AND updated_at < now() - make_interval(secs => $1)",
    )
    .bind(STALE_RUNNING.as_secs() as f64)
    .execute(pool)
    .await?;

    let Some(job) = sqlx::query(
        "UPDATE asset_jobs SET status = 'running', attempts = attempts + 1, updated_at = now() \
         WHERE id = (SELECT id FROM asset_jobs WHERE status = 'queued' \
                       AND updated_at <= now() - make_interval(secs => $1 * attempts) \
                     ORDER BY created_at FOR UPDATE SKIP LOCKED LIMIT 1) \
         RETURNING id, org_id, purpose, source_kind, staged_path, source_url, label, target_table, \
                   target_id, target_field, attempts, created_by, created_at",
    )
    .bind(BACKOFF_STEP.as_secs() as f64)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };

    let id: Uuid = job.get("id");
    let attempts: i32 = job.get("attempts");
    let staged: Option<String> = job.get("staged_path");
    match process(pool, store, &job).await {
        Ok(asset_id) => {
            if let Some(p) = &staged {
                let _ = tokio::fs::remove_file(p).await;
            }
            tracing::info!(job = %id, "asset job done");
            let _ = asset_id;
            Ok(Some(JobResult::Done(id)))
        }
        Err(e) => {
            let msg = e.to_string();
            let permanent = matches!(
                e,
                AppError::BadRequest(_) | AppError::NotFound(_) | AppError::Forbidden(_)
            );
            if attempts >= MAX_ATTEMPTS || permanent {
                sqlx::query(
                    "UPDATE asset_jobs SET status = 'failed', last_error = $2, updated_at = now() WHERE id = $1",
                )
                .bind(id)
                .bind(&msg)
                .execute(pool)
                .await?;
                if let Some(p) = &staged {
                    let _ = tokio::fs::remove_file(p).await;
                }
                tracing::warn!(job = %id, error = %msg, "asset job failed");
                Ok(Some(JobResult::Failed(id)))
            } else {
                // Backoff: claimable again once updated_at (set to now() by
                // the table trigger) is 30 s × attempts in the past.
                sqlx::query(
                    "UPDATE asset_jobs SET status = 'queued', last_error = $2 WHERE id = $1",
                )
                .bind(id)
                .bind(&msg)
                .execute(pool)
                .await?;
                Ok(Some(JobResult::Retry(id)))
            }
        }
    }
}

async fn process(
    pool: &PgPool,
    store: &AssetStore,
    job: &sqlx::postgres::PgRow,
) -> Result<Uuid, AppError> {
    let id: Uuid = job.get("id");
    let org_id: Option<Uuid> = job.get("org_id");
    let purpose = AssetPurpose::parse(job.get::<&str, _>("purpose"))
        .ok_or_else(|| AppError::BadRequest("unknown purpose".into()))?;
    let source_kind =
        SourceKind::parse(job.get::<&str, _>("source_kind")).unwrap_or(SourceKind::Upload);
    let target = AssetTarget::from_db(
        job.get::<&str, _>("target_table"),
        job.get("target_id"),
        job.get::<&str, _>("target_field"),
    )
    .ok_or_else(|| AppError::BadRequest("unknown target".into()))?;
    let source = match (
        job.get::<Option<String>, _>("staged_path"),
        job.get::<Option<String>, _>("source_url"),
    ) {
        (Some(p), _) => IngestSource::StagedFile(p.into()),
        (None, Some(u)) => IngestSource::Url(
            url::Url::parse(&u).map_err(|_| AppError::BadRequest("bad source url".into()))?,
        ),
        _ => return Err(AppError::BadRequest("job has no source".into())),
    };
    let label: Option<String> = job.get("label");
    let actor: Option<Uuid> = job.get("created_by");
    let created_at: chrono::DateTime<chrono::Utc> = job.get("created_at");

    let outcome = ingest_with(
        pool,
        store,
        org_id,
        purpose,
        source,
        source_kind,
        label.as_deref(),
        actor,
    )
    .await?;
    let pos_id = outcome.group_id;

    let mut tx = pool.begin().await?;
    // A newer upload for the same slot supersedes this one: record the result
    // but never clobber the newer picture.
    let superseded: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM asset_jobs WHERE target_table = $1 AND target_id = $2 \
                          AND target_field = $3 AND created_at > $4 AND id <> $5 AND status <> 'failed')",
    )
    .bind(job.get::<&str, _>("target_table"))
    .bind(target.id)
    .bind(job.get::<&str, _>("target_field"))
    .bind(created_at)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if !superseded {
        attach(&mut tx, &target, &outcome).await?;
    }
    sqlx::query(
        "UPDATE asset_jobs SET status = 'done', result_group_id = $2, last_error = NULL, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(pos_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    if !superseded && purpose == AssetPurpose::OrgLogo {
        post_attach_logo(pool, store, &outcome, target.id).await;
    }
    Ok(pos_id)
}

/// The brand palette and `is_mark` flag are derived from the stored pixels
/// (the lossless `original`), never from the upload.
async fn post_attach_logo(
    pool: &PgPool,
    store: &AssetStore,
    outcome: &super::ingest::IngestOutcome,
    org_id: Uuid,
) {
    let Some(src) = outcome.variant("original").or_else(|| outcome.full()) else {
        return;
    };
    let Ok((_, bytes)) =
        super::ingest::read_asset_bytes_with(pool, store, outcome.org_id, src.id).await
    else {
        return;
    };
    let Ok(img) = image::load_from_memory(&bytes) else {
        return;
    };
    let palette = crate::orgs::branding::palette_from_image(&img);
    let is_mark = crate::orgs::branding::is_mark(&img);
    let _ = sqlx::query(
        "UPDATE organizations SET brand_background = $2, brand_foreground = $3, brand_accent = $4, \
                brand_logo_is_mark = $5, updated_at = now() WHERE id = $1",
    )
    .bind(org_id)
    .bind(palette.as_ref().map(|p| p.background.clone()))
    .bind(palette.as_ref().map(|p| p.foreground.clone()))
    .bind(palette.as_ref().map(|p| p.accent.clone()))
    .bind(is_mark)
    .execute(pool)
    .await;
}
