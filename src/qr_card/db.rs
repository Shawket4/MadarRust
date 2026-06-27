//! Database helpers: short-link dedup and branch-table CRUD.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::errors::AppError;

use super::shlink::ShortLinkProvider;

// ── Short-link dedup ──────────────────────────────────────────────────────────

#[derive(Debug, sqlx::FromRow)]
pub struct ShortLinkRow {
    pub id: Uuid,
    pub org_id: Uuid,
    pub branch_id: Option<Uuid>,
    pub kind: String,
    pub target_ref: String,
    pub long_url: String,
    pub short_code: String,
    pub short_url: String,
    pub label: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Look up `(org_id, kind, target_ref)` in `qr_short_links`; reuse the stored
/// `short_url` if found, otherwise create a new one via Shlink + insert.
/// `findIfExists: true` on the Shlink side means the call is idempotent even
/// when our DB row is missing (e.g. after a data wipe).
pub async fn get_or_create_short_link(
    pool: &PgPool,
    provider: &dyn ShortLinkProvider,
    org_id: Uuid,
    branch_id: Option<Uuid>,
    kind: &str,
    target_ref: &str,
    long_url: &str,
    custom_slug: Option<&str>,
    label: Option<&str>,
) -> Result<ShortLinkRow, AppError> {
    // Fast path: already in DB — but only reuse if the long_url still matches.
    // A format change (e.g. path segment vs query param) must produce a fresh
    // Shlink entry; stale rows are deleted so the slow path recreates them.
    if let Some(row) = sqlx::query_as::<_, ShortLinkRow>(
        "SELECT id, org_id, branch_id, kind, target_ref, long_url,
                short_code, short_url, label, created_at
         FROM qr_short_links
         WHERE org_id = $1 AND kind = $2 AND target_ref = $3",
    )
    .bind(org_id)
    .bind(kind)
    .bind(target_ref)
    .fetch_optional(pool)
    .await?
    {
        if row.long_url == long_url {
            return Ok(row);
        }
        // Stale URL — drop the row so the slow path creates a fresh Shlink entry.
        sqlx::query("DELETE FROM qr_short_links WHERE id = $1")
            .bind(row.id)
            .execute(pool)
            .await?;
    }

    // Slow path: create via Shlink, then cache.
    let tags = vec![format!("madar"), format!("kind:{kind}")];
    let short = provider
        .create_short_url(long_url, custom_slug, &tags)
        .await?;

    let row = sqlx::query_as::<_, ShortLinkRow>(
        "INSERT INTO qr_short_links
             (org_id, branch_id, kind, target_ref, long_url, short_code, short_url, label)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (org_id, kind, target_ref) DO UPDATE
             SET short_url = EXCLUDED.short_url, short_code = EXCLUDED.short_code
         RETURNING id, org_id, branch_id, kind, target_ref, long_url,
                   short_code, short_url, label, created_at",
    )
    .bind(org_id)
    .bind(branch_id)
    .bind(kind)
    .bind(target_ref)
    .bind(long_url)
    .bind(&short.short_code)
    .bind(&short.short_url)
    .bind(label)
    .fetch_one(pool)
    .await?;

    Ok(row)
}

// ── Branch tables ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct BranchTable {
    pub id: Uuid,
    pub org_id: Uuid,
    pub branch_id: Uuid,
    #[schema(example = "Table 5")]
    pub label: String,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateTableRequest {
    #[schema(example = "Table 5")]
    pub label: String,
}

pub async fn fetch_table(pool: &PgPool, table_id: Uuid) -> Result<BranchTable, AppError> {
    sqlx::query_as::<_, BranchTable>(
        "SELECT id, org_id, branch_id, label, is_active, created_at, updated_at
         FROM branch_tables
         WHERE id = $1",
    )
    .bind(table_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Table not found".into()))
}
