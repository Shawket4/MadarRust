//! Asset references attached to dashboard-facing responses (§11.10). Never
//! bytes, never original-size URLs by default: variant URLs, signed.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use utoipa::ToSchema;
use uuid::Uuid;

use super::ingest::{AssetTarget, DASHBOARD_TTL, signed_url};
use crate::errors::AppError;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, ToSchema)]
pub struct VariantRef {
    pub url: String,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub bytes: i64,
    pub content_hash: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default, ToSchema)]
pub struct VariantSet {
    pub thumb: Option<VariantRef>,
    pub tile: Option<VariantRef>,
    pub full: Option<VariantRef>,
    pub original: Option<VariantRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub animation: Option<VariantRef>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, ToSchema)]
pub struct ReadyGroupRef {
    pub group_id: Uuid,
    pub label: Option<String>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub has_alpha: bool,
    pub variants: VariantSet,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, ToSchema)]
pub struct ProcessingGroupRef {
    /// Always null while processing.
    pub group_id: Option<Uuid>,
    /// `processing`
    pub status: String,
    pub job_id: Uuid,
}

/// `image` / `logo` / `brand_card_image` on entity responses.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, ToSchema)]
#[serde(untagged)]
pub enum AssetGroupRef {
    Processing(ProcessingGroupRef),
    Ready(ReadyGroupRef),
}

impl AssetGroupRef {
    pub fn group_id(&self) -> Option<Uuid> {
        match self {
            Self::Ready(r) => Some(r.group_id),
            Self::Processing(_) => None,
        }
    }
    pub fn ready(&self) -> Option<&ReadyGroupRef> {
        match self {
            Self::Ready(r) => Some(r),
            _ => None,
        }
    }
}

fn build(rows: &[sqlx::postgres::PgRow], ttl: Duration) -> HashMap<Uuid, AssetGroupRef> {
    let mut out: HashMap<Uuid, ReadyGroupRef> = HashMap::new();
    for r in rows {
        let gid: Uuid = r.get("group_id");
        let org_id: Option<Uuid> = r.get("org_id");
        let hash: String = r.get("hash");
        let ext: String = r.get("ext");
        let variant: String = r.get("variant");
        let v = VariantRef {
            url: signed_url(org_id, &hash, &ext, ttl),
            width: r.get("width"),
            height: r.get("height"),
            bytes: r.get("bytes"),
            content_hash: hash,
        };
        let e = out.entry(gid).or_insert_with(|| ReadyGroupRef {
            group_id: gid,
            label: r.get("label"),
            width: None,
            height: None,
            has_alpha: false,
            variants: VariantSet::default(),
        });
        e.has_alpha |= r.get::<bool, _>("has_alpha");
        match variant.as_str() {
            "thumb" => e.variants.thumb = Some(v),
            "tile" => e.variants.tile = Some(v),
            "full" => {
                e.width = v.width;
                e.height = v.height;
                e.variants.full = Some(v)
            }
            "original" => e.variants.original = Some(v),
            "animation" => {
                e.width = v.width;
                e.height = v.height;
                e.variants.animation = Some(v)
            }
            _ => {}
        }
    }
    out.into_iter()
        .map(|(k, mut v)| {
            if v.variants.tile.is_none() {
                v.variants.tile = v.variants.full.clone();
            }
            if v.variants.thumb.is_none() {
                v.variants.thumb = v.variants.tile.clone();
            }
            (k, AssetGroupRef::Ready(v))
        })
        .collect()
}

/// Batch: one query for any number of groups (list endpoints).
pub async fn group_refs(
    pool: &PgPool,
    org_id: Uuid,
    group_ids: &[Uuid],
) -> Result<HashMap<Uuid, AssetGroupRef>, AppError> {
    if group_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query(
        "SELECT a.group_id, a.org_id, a.hash, a.ext, a.variant, a.width, a.height, a.bytes, a.has_alpha, g.label \
         FROM assets a JOIN asset_groups g ON g.id = a.group_id \
         WHERE a.group_id = ANY($1) AND (a.org_id = $2 OR a.org_id IS NULL)",
    )
    .bind(group_ids)
    .bind(org_id)
    .fetch_all(pool)
    .await?;
    Ok(build(&rows, DASHBOARD_TTL))
}

pub async fn group_ref(
    pool: &PgPool,
    org_id: Uuid,
    group_id: Option<Uuid>,
    ttl: Duration,
) -> Result<Option<AssetGroupRef>, AppError> {
    let Some(gid) = group_id else { return Ok(None) };
    let rows = sqlx::query(
        "SELECT a.group_id, a.org_id, a.hash, a.ext, a.variant, a.width, a.height, a.bytes, a.has_alpha, g.label \
         FROM assets a JOIN asset_groups g ON g.id = a.group_id \
         WHERE a.group_id = $1 AND (a.org_id = $2 OR a.org_id IS NULL)",
    )
    .bind(gid)
    .bind(org_id)
    .fetch_all(pool)
    .await?;
    Ok(build(&rows, ttl).remove(&gid))
}

/// Refs for a slot on many rows in ONE query: the row's group when attached,
/// a `processing` ref when a newer job for the slot is still queued/running.
pub async fn slot_refs(
    pool: &PgPool,
    org_id: Uuid,
    table: super::ingest::AssetTable,
    field: super::ingest::AssetField,
    row_ids: &[Uuid],
) -> Result<HashMap<Uuid, AssetGroupRef>, AppError> {
    if row_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let slot = AssetTarget::new(table, Uuid::nil(), field).slot()?;
    let rows = sqlx::query(&format!(
        "WITH t AS (SELECT id AS row_id, {gc} AS gid FROM {tbl} WHERE id = ANY($1)), \
              j AS (SELECT DISTINCT ON (target_id) target_id, id AS job_id FROM asset_jobs \
                     WHERE target_table = $3 AND target_field = $4 AND target_id = ANY($1) \
                       AND status IN ('queued','running') ORDER BY target_id, created_at DESC) \
         SELECT t.row_id, j.job_id, a.group_id, a.org_id, a.hash, a.ext, a.variant, a.width, a.height, a.bytes, a.has_alpha, g.label \
         FROM t LEFT JOIN j ON j.target_id = t.row_id \
                LEFT JOIN asset_groups g ON g.id = t.gid \
                LEFT JOIN assets a ON a.group_id = t.gid AND (a.org_id = $2 OR a.org_id IS NULL)",
        gc = slot.group_col,
        tbl = slot.table
    ))
    .bind(row_ids)
    .bind(org_id)
    .bind(slot.table)
    .bind(slot.field)
    .fetch_all(pool)
    .await?;
    let mut out = HashMap::new();
    let mut by_group: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    let mut asset_rows = Vec::new();
    for r in rows {
        let row_id: Uuid = r.get("row_id");
        if let Some(job_id) = r.get::<Option<Uuid>, _>("job_id") {
            out.insert(
                row_id,
                AssetGroupRef::Processing(ProcessingGroupRef {
                    group_id: None,
                    status: "processing".into(),
                    job_id,
                }),
            );
            continue;
        }
        if let Some(gid) = r.get::<Option<Uuid>, _>("group_id") {
            let e = by_group.entry(gid).or_default();
            if !e.contains(&row_id) {
                e.push(row_id);
            }
            asset_rows.push(r);
        }
    }
    let groups = build(&asset_rows, DASHBOARD_TTL);
    for (gid, rows) in by_group {
        if let Some(g) = groups.get(&gid) {
            for row_id in rows {
                out.entry(row_id).or_insert_with(|| g.clone());
            }
        }
    }
    Ok(out)
}
