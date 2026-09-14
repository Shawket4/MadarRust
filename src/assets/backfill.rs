//! Backfill of pre-asset-store files (decision 19, contract §11.6 + §11.10).
//!
//! Phases, each resumable and keyed by `asset_backfill_items`' primary key:
//! DISCOVER → INGEST → VERIFY+REWRITE → REPORT, plus a separate PRUNE. Legacy
//! URL columns are never modified; originals are deleted only by PRUNE, only
//! for items verified in a named run.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use super::AssetStore;
use super::ingest::{AssetPurpose, SourceKind, convert, ingest_bytes, sniff};
use crate::errors::AppError;

pub const MAX_ATTEMPTS: i32 = 5;

#[derive(Clone, Debug)]
pub struct BackfillOptions {
    /// `None` = all orgs (and the global preset library).
    pub org: Option<Uuid>,
    pub dry_run: bool,
    pub limit: Option<i64>,
    pub run_id: Uuid,
    pub verify_only: bool,
    pub store: AssetStore,
    pub step_animations_dir: Option<PathBuf>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq)]
pub struct Totals {
    pub discovered: i64,
    pub ingested: i64,
    pub verified: i64,
    pub deduped: i64,
    pub missing: i64,
    pub broken: i64,
    pub skipped: i64,
    pub failed: i64,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq)]
pub struct ByteTotals {
    pub source: i64,
    pub stored: i64,
    pub saved: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct FailedItem {
    pub table: String,
    pub id: String,
    pub field: String,
    pub legacy_url: String,
    pub status: String,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Report {
    pub run_id: Uuid,
    pub org_scope: String,
    pub dry_run: bool,
    pub totals: Totals,
    pub bytes: ByteTotals,
    pub items_failed: Vec<FailedItem>,
}

#[derive(Clone, Debug)]
struct Item {
    table: &'static str,
    field: &'static str,
    id: String,
    org_id: Option<Uuid>,
    legacy_url: String,
    legacy_path: Option<String>,
}

struct SourceDef {
    table: &'static str,
    field: &'static str,
    org_col: &'static str,
    url_col: &'static str,
    purpose: AssetPurpose,
    group_col: &'static str,
}

const SOURCES: &[SourceDef] = &[
    SourceDef {
        table: "menu_items",
        field: "image",
        org_col: "org_id",
        url_col: "image_url",
        purpose: AssetPurpose::MenuItemPhoto,
        group_col: "image_group_id",
    },
    SourceDef {
        table: "categories",
        field: "image",
        org_col: "org_id",
        url_col: "image_url",
        purpose: AssetPurpose::CategoryPhoto,
        group_col: "image_group_id",
    },
    SourceDef {
        table: "bundles",
        field: "image",
        org_col: "org_id",
        url_col: "image_url",
        purpose: AssetPurpose::BundlePhoto,
        group_col: "image_group_id",
    },
    SourceDef {
        table: "organizations",
        field: "logo",
        org_col: "id",
        url_col: "logo_url",
        purpose: AssetPurpose::OrgLogo,
        group_col: "logo_group_id",
    },
    SourceDef {
        table: "organizations",
        field: "brand_card_image",
        org_col: "id",
        url_col: "brand_card_image",
        purpose: AssetPurpose::LoyaltyCardImage,
        group_col: "brand_card_image_group_id",
    },
];

fn source_def(table: &str, field: &str) -> Option<&'static SourceDef> {
    SOURCES
        .iter()
        .find(|s| s.table == table && s.field == field)
}

async fn discover(pool: &PgPool, opts: &BackfillOptions) -> Result<Vec<Item>, AppError> {
    let mut items = Vec::new();
    for s in SOURCES {
        let rows = sqlx::query(&format!(
            "SELECT id, {org} AS org_id, {url} AS url FROM {t} WHERE {url} IS NOT NULL AND {url} <> '' \
             AND ($1::uuid IS NULL OR {org} = $1) ORDER BY id",
            org = s.org_col,
            url = s.url_col,
            t = s.table
        ))
        .bind(opts.org)
        .fetch_all(pool)
        .await?;
        for r in rows {
            let url: String = r.get("url");
            items.push(Item {
                table: s.table,
                field: s.field,
                id: r.get::<Uuid, _>("id").to_string(),
                org_id: Some(r.get("org_id")),
                legacy_path: super::legacy_rel_from_url(&url),
                legacy_url: url,
            });
        }
    }
    if opts.org.is_none()
        && let Some(dir) = &opts.step_animations_dir
    {
        let slugs: Vec<String> =
            sqlx::query_scalar("SELECT slug FROM recipe_step_presets ORDER BY slug")
                .fetch_all(pool)
                .await?;
        for slug in slugs {
            items.push(Item {
                table: "recipe_step_presets",
                field: "animation",
                id: slug.clone(),
                org_id: None,
                legacy_url: format!("{}/{slug}.json", crate::recipes::steps::STATIC_URL_PREFIX),
                legacy_path: Some(
                    dir.join(format!("{slug}.json"))
                        .to_string_lossy()
                        .to_string(),
                ),
            });
        }
    }
    Ok(items)
}

fn file_for(item: &Item, store: &AssetStore) -> Option<PathBuf> {
    let p = item.legacy_path.as_ref()?;
    if item.table == "recipe_step_presets" {
        return Some(PathBuf::from(p));
    }
    store.legacy_file(p)
}

fn purpose_of(item: &Item) -> AssetPurpose {
    source_def(item.table, item.field)
        .map(|s| s.purpose)
        .unwrap_or(AssetPurpose::StepAnimation)
}

enum Probe {
    Missing(String),
    Broken(String),
    External,
    Ok(Vec<u8>),
}

async fn probe(item: &Item, store: &AssetStore) -> Probe {
    let Some(path) = file_for(item, store) else {
        return if item.legacy_url.starts_with("https://") {
            Probe::External
        } else {
            Probe::Missing("unresolvable legacy path".into())
        };
    };
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(_) => {
            return if item.legacy_url.starts_with("https://") && item.legacy_path.is_none() {
                Probe::External
            } else {
                Probe::Missing(format!(
                    "file not found: {}",
                    item.legacy_path.as_deref().unwrap_or("")
                ))
            };
        }
    };
    match sniff(&bytes) {
        Ok(_) => Probe::Ok(bytes),
        Err(e) => Probe::Broken(e.to_string()),
    }
}

/// Run DISCOVER + INGEST + VERIFY (or VERIFY only) and produce the report.
pub async fn run(pool: &PgPool, opts: &BackfillOptions) -> Result<Report, AppError> {
    if opts.dry_run {
        return dry_run(pool, opts).await;
    }
    if !opts.verify_only {
        let items = discover(pool, opts).await?;
        for it in &items {
            sqlx::query(
                "INSERT INTO asset_backfill_items (source_table, source_id, source_field, org_id, legacy_url, legacy_path) \
                 VALUES ($1,$2,$3,$4,$5,$6) \
                 ON CONFLICT (source_table, source_id, source_field) DO UPDATE SET \
                    legacy_url = EXCLUDED.legacy_url, legacy_path = EXCLUDED.legacy_path, \
                    status = CASE WHEN asset_backfill_items.legacy_url <> EXCLUDED.legacy_url THEN 'pending' ELSE asset_backfill_items.status END, \
                    attempts = CASE WHEN asset_backfill_items.legacy_url <> EXCLUDED.legacy_url THEN 0 ELSE asset_backfill_items.attempts END, \
                    updated_at = CASE WHEN asset_backfill_items.legacy_url <> EXCLUDED.legacy_url THEN now() ELSE asset_backfill_items.updated_at END",
            )
            .bind(it.table)
            .bind(it.id.clone())
            .bind(it.field)
            .bind(it.org_id)
            .bind(&it.legacy_url)
            .bind(&it.legacy_path)
            .execute(pool)
            .await?;
        }
        ingest_phase(pool, opts).await?;
    }
    verify_phase(pool, opts).await?;
    report(pool, opts).await
}

async fn ingest_phase(pool: &PgPool, opts: &BackfillOptions) -> Result<(), AppError> {
    let rows = sqlx::query(
        "SELECT source_table, source_id, source_field, org_id, legacy_url, legacy_path FROM asset_backfill_items \
         WHERE (status = 'pending' OR (status = 'failed' AND attempts < $1)) AND ($2::uuid IS NULL OR org_id = $2) \
         ORDER BY source_table, source_id, source_field LIMIT $3",
    )
    .bind(MAX_ATTEMPTS)
    .bind(opts.org)
    .bind(opts.limit.unwrap_or(i64::MAX))
    .fetch_all(pool)
    .await?;
    for r in rows {
        let table: String = r.get("source_table");
        let field: String = r.get("source_field");
        let Some(item) = item_from_row(&r, &table, &field) else {
            continue;
        };
        let pk = (item.table, item.id.clone(), item.field);
        let set = |status: &'static str, err: Option<String>| {
            sqlx::query(
                "UPDATE asset_backfill_items SET status = $4, last_error = $5, attempts = attempts + 1, run_id = $6, updated_at = now() \
                 WHERE source_table = $1 AND source_id = $2 AND source_field = $3",
            )
            .bind(pk.0)
            .bind(pk.1.clone())
            .bind(pk.2)
            .bind(status)
            .bind(err)
            .bind(opts.run_id)
        };
        let bytes = match probe(&item, &opts.store).await {
            Probe::Missing(e) => {
                set("missing", Some(e)).execute(pool).await?;
                continue;
            }
            Probe::Broken(e) => {
                set("broken", Some(e)).execute(pool).await?;
                continue;
            }
            Probe::External => match url::Url::parse(&item.legacy_url) {
                Ok(u) => {
                    match super::ingest::load_source(super::ingest::IngestSource::Url(u)).await {
                        Ok(b) => b,
                        Err(e) => {
                            set("missing", Some(e.to_string())).execute(pool).await?;
                            continue;
                        }
                    }
                }
                Err(_) => {
                    set("broken", Some("unparseable url".into()))
                        .execute(pool)
                        .await?;
                    continue;
                }
            },
            Probe::Ok(b) => b,
        };
        let source_len = bytes.len() as i64;
        let res = ingest_bytes(
            pool,
            &opts.store,
            item.org_id,
            purpose_of(&item),
            bytes,
            SourceKind::Backfill,
            None,
            None,
        )
        .await;
        match res {
            Ok(outcome) => {
                sqlx::query(
                    "UPDATE asset_backfill_items SET status = 'ingested', group_id = $4, source_bytes = $5, stored_bytes = $6, \
                            deduped = $7, last_error = NULL, attempts = attempts + 1, run_id = $8, updated_at = now() \
                     WHERE source_table = $1 AND source_id = $2 AND source_field = $3",
                )
                .bind(pk.0)
                .bind(pk.1.clone())
                .bind(pk.2)
                .bind(outcome.group_id)
                .bind(source_len)
                // Bytes this item added to the store: nothing when it reused a group.
                .bind(if outcome.deduped { 0 } else { outcome.stored_bytes() })
                .bind(outcome.deduped)
                .bind(opts.run_id)
                .execute(pool)
                .await?;
            }
            Err(AppError::BadRequest(e)) => {
                set("broken", Some(e)).execute(pool).await?;
            }
            Err(e) => {
                set("failed", Some(e.to_string())).execute(pool).await?;
            }
        }
    }
    Ok(())
}

fn item_from_row(r: &sqlx::postgres::PgRow, table: &str, field: &str) -> Option<Item> {
    let (t, f): (&'static str, &'static str) = match source_def(table, field) {
        Some(s) => (s.table, s.field),
        None if table == "recipe_step_presets" => ("recipe_step_presets", "animation"),
        None => return None,
    };
    Some(Item {
        table: t,
        field: f,
        id: r.get("source_id"),
        org_id: r.get("org_id"),
        legacy_url: r.get("legacy_url"),
        legacy_path: r.get("legacy_path"),
    })
}

/// Re-read every stored variant, recompute sha256 and decode; then point the
/// row at the group (only when its slot is still empty or already this group,
/// and its legacy URL is still the one discovered) and map the legacy path.
async fn verify_phase(pool: &PgPool, opts: &BackfillOptions) -> Result<(), AppError> {
    let statuses: &[&str] = if opts.verify_only {
        &["ingested", "verified"]
    } else {
        &["ingested"]
    };
    let rows = sqlx::query(
        "SELECT source_table, source_id, source_field, org_id, legacy_url, legacy_path, group_id FROM asset_backfill_items \
         WHERE status = ANY($1) AND group_id IS NOT NULL AND ($2::uuid IS NULL OR org_id = $2) \
         ORDER BY source_table, source_id, source_field",
    )
    .bind(statuses)
    .bind(opts.org)
    .fetch_all(pool)
    .await?;
    for r in rows {
        let table: String = r.get("source_table");
        let field: String = r.get("source_field");
        let Some(item) = item_from_row(&r, &table, &field) else {
            continue;
        };
        let group_id: Uuid = r.get("group_id");
        let mark = |status: &'static str, err: Option<String>| {
            sqlx::query(
                "UPDATE asset_backfill_items SET status = $4, last_error = $5, run_id = $6, updated_at = now() \
                 WHERE source_table = $1 AND source_id = $2 AND source_field = $3",
            )
            .bind(item.table)
            .bind(item.id.clone())
            .bind(item.field)
            .bind(status)
            .bind(err)
            .bind(opts.run_id)
        };
        if let Err(e) = verify_group(pool, &opts.store, item.org_id, group_id).await {
            mark("failed", Some(e)).execute(pool).await?;
            continue;
        }
        let mut tx = pool.begin().await?;
        let applied = if item.table == "recipe_step_presets" {
            let slug = item.id.clone();
            sqlx::query(
                "UPDATE recipe_step_presets SET animation_group_id = $1 \
                 WHERE slug = $2 AND (animation_group_id IS NULL OR animation_group_id = $1)",
            )
            .bind(group_id)
            .bind(&slug)
            .execute(&mut *tx)
            .await?
            .rows_affected()
                > 0
        } else {
            let s = source_def(item.table, item.field).expect("known source");
            sqlx::query(&format!(
                "UPDATE {t} SET {gc} = $1 WHERE id = $2 AND ({gc} IS NULL OR {gc} = $1) AND {url} = $3",
                t = s.table,
                gc = s.group_col,
                url = s.url_col
            ))
            .bind(group_id)
            .bind(Uuid::parse_str(&item.id).map_err(|_| AppError::Internal)?)
            .bind(&item.legacy_url)
            .execute(&mut *tx)
            .await?
            .rows_affected()
                > 0
        };
        if !applied {
            drop(tx);
            mark(
                "skipped",
                Some("row changed since discovery (newer upload kept)".into()),
            )
            .execute(pool)
            .await?;
            continue;
        }
        if item.table != "recipe_step_presets"
            && let (Some(org), Some(rel)) = (item.org_id, item.legacy_path.as_deref())
        {
            let full: Option<Uuid> = sqlx::query_scalar(
                "SELECT id FROM assets WHERE group_id = $1 AND variant = 'full' AND org_id = $2",
            )
            .bind(group_id)
            .bind(org)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(full) = full {
                sqlx::query(
                    "INSERT INTO asset_legacy_paths (legacy_path, org_id, asset_id) VALUES ($1,$2,$3) \
                     ON CONFLICT (legacy_path) DO NOTHING",
                )
                .bind(rel)
                .bind(org)
                .bind(full)
                .execute(&mut *tx)
                .await?;
            }
            super::ingest::mark_org_bundles_dirty(&mut tx, org).await?;
        }
        sqlx::query(
            "UPDATE asset_backfill_items SET status = 'verified', last_error = NULL, run_id = $4, updated_at = now() \
             WHERE source_table = $1 AND source_id = $2 AND source_field = $3",
        )
        .bind(item.table)
        .bind(item.id.clone())
        .bind(item.field)
        .bind(opts.run_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
    }
    Ok(())
}

pub async fn verify_group(
    pool: &PgPool,
    store: &AssetStore,
    org_id: Option<Uuid>,
    group_id: Uuid,
) -> Result<(), String> {
    let rows = sqlx::query("SELECT org_id, hash, ext, bytes FROM assets WHERE group_id = $1 AND org_id IS NOT DISTINCT FROM $2")
        .bind(group_id)
        .bind(org_id)
        .fetch_all(pool)
        .await
        .map_err(|e| e.to_string())?;
    if rows.is_empty() {
        return Err("group has no stored variants".into());
    }
    for r in rows {
        let hash: String = r.get("hash");
        let ext: String = r.get("ext");
        let path = store.path_for_key(&AssetStore::key(r.get("org_id"), &hash, &ext));
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|_| format!("stored file missing: {hash}.{ext}"))?;
        if super::sha256_hex(&bytes) != hash {
            return Err(format!("hash mismatch: {hash}.{ext}"));
        }
        let decodes = if ext == "webp" {
            image::load_from_memory(&bytes).is_ok()
        } else {
            zstd::decode_all(&bytes[..])
                .ok()
                .and_then(|j| super::ingest::lottie_meta(&j))
                .is_some()
        };
        if !decodes {
            return Err(format!("stored file does not decode: {hash}.{ext}"));
        }
    }
    Ok(())
}

async fn report(pool: &PgPool, opts: &BackfillOptions) -> Result<Report, AppError> {
    let rows = sqlx::query(
        "SELECT source_table, source_id, source_field, legacy_url, status, last_error, deduped, \
                COALESCE(source_bytes,0) AS sb, COALESCE(stored_bytes,0) AS stb \
         FROM asset_backfill_items WHERE ($1::uuid IS NULL OR org_id = $1) ORDER BY source_table, source_id, source_field",
    )
    .bind(opts.org)
    .fetch_all(pool)
    .await?;
    let mut t = Totals::default();
    let mut b = ByteTotals::default();
    let mut failed = Vec::new();
    for r in rows {
        t.discovered += 1;
        let status: String = r.get("status");
        match status.as_str() {
            "ingested" => t.ingested += 1,
            "verified" => t.verified += 1,
            "missing" => t.missing += 1,
            "broken" => t.broken += 1,
            "skipped" => t.skipped += 1,
            "failed" => t.failed += 1,
            _ => {}
        }
        if r.get::<bool, _>("deduped") {
            t.deduped += 1;
        }
        if matches!(status.as_str(), "ingested" | "verified") {
            b.source += r.get::<i64, _>("sb");
        }
        if matches!(status.as_str(), "missing" | "broken" | "failed" | "skipped") {
            failed.push(FailedItem {
                table: r.get("source_table"),
                id: r.get("source_id"),
                field: r.get("source_field"),
                legacy_url: r.get("legacy_url"),
                status,
                error: r.get("last_error"),
            });
        }
    }
    // Stored = every distinct file (org, hash) behind the ingested/verified
    // items, counted once however many items, groups or variants share it.
    b.stored = sqlx::query_scalar(
        "SELECT COALESCE(sum(bytes), 0)::bigint FROM ( \
            SELECT DISTINCT a.org_id, a.hash, a.ext, a.bytes FROM assets a \
            WHERE a.group_id IN (SELECT group_id FROM asset_backfill_items \
                                 WHERE status IN ('ingested','verified') AND group_id IS NOT NULL \
                                   AND ($1::uuid IS NULL OR org_id = $1))) f",
    )
    .bind(opts.org)
    .fetch_one(pool)
    .await?;
    b.saved = b.source - b.stored;
    Ok(Report {
        run_id: opts.run_id,
        org_scope: opts
            .org
            .map(|o| o.to_string())
            .unwrap_or_else(|| "all".into()),
        dry_run: false,
        totals: t,
        bytes: b,
        items_failed: failed,
    })
}

/// Sniff/decode + size estimate only. Writes nothing (no ledger, no files, no rows).
async fn dry_run(pool: &PgPool, opts: &BackfillOptions) -> Result<Report, AppError> {
    let mut items = discover(pool, opts).await?;
    if let Some(l) = opts.limit {
        items.truncate(l.max(0) as usize);
    }
    let mut t = Totals {
        discovered: items.len() as i64,
        ..Default::default()
    };
    let mut b = ByteTotals::default();
    let mut failed = Vec::new();
    let mut seen_sources: BTreeMap<(Option<Uuid>, String), ()> = BTreeMap::new();
    let mut seen_files: std::collections::BTreeSet<(Option<Uuid>, String)> = Default::default();
    for it in &items {
        let (status, err) = match probe(it, &opts.store).await {
            Probe::Missing(e) => ("missing", Some(e)),
            Probe::Broken(e) => ("broken", Some(e)),
            Probe::External => (
                "skipped",
                Some("external url (fetched only in a real run)".into()),
            ),
            Probe::Ok(bytes) => {
                let hash = super::sha256_hex(&bytes);
                if seen_sources.insert((it.org_id, hash), ()).is_some() {
                    t.deduped += 1;
                }
                let purpose = purpose_of(it);
                let sniffed = sniff(&bytes).expect("probed");
                let len = bytes.len() as i64;
                match tokio::task::spawn_blocking(move || convert(&bytes, sniffed, purpose)).await {
                    Ok(Ok(vars)) => {
                        b.source += len;
                        for v in &vars {
                            let h = super::sha256_hex(&v.bytes);
                            if seen_files.insert((it.org_id, format!("{h}.{}", v.ext))) {
                                b.stored += v.bytes.len() as i64;
                            }
                        }
                        ("ingested", None)
                    }
                    Ok(Err(e)) => ("broken", Some(e.to_string())),
                    Err(_) => ("failed", Some("conversion panicked".into())),
                }
            }
        };
        match status {
            "ingested" => t.ingested += 1,
            "missing" => t.missing += 1,
            "broken" => t.broken += 1,
            "skipped" => t.skipped += 1,
            _ => t.failed += 1,
        }
        if status != "ingested" {
            failed.push(FailedItem {
                table: it.table.into(),
                id: it.id.clone(),
                field: it.field.into(),
                legacy_url: it.legacy_url.clone(),
                status: status.into(),
                error: err,
            });
        }
    }
    b.saved = b.source - b.stored;
    Ok(Report {
        run_id: opts.run_id,
        org_scope: opts
            .org
            .map(|o| o.to_string())
            .unwrap_or_else(|| "all".into()),
        dry_run: true,
        totals: t,
        bytes: b,
        items_failed: failed,
    })
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq)]
pub struct PruneReport {
    pub run_id: Uuid,
    pub deleted: i64,
    pub bytes_freed: i64,
    pub already_gone: i64,
}

/// The run ids that hold verified items in scope (what `--i-have-verified`
/// takes), newest first. A re-run that changes nothing verifies nothing, so its
/// own run id is not one of them.
pub async fn verified_runs(pool: &PgPool, org: Option<Uuid>) -> Result<Vec<(Uuid, i64)>, AppError> {
    Ok(sqlx::query_as(
        "SELECT run_id, count(*) FROM asset_backfill_items \
         WHERE status = 'verified' AND run_id IS NOT NULL AND ($1::uuid IS NULL OR org_id = $1) \
         GROUP BY run_id ORDER BY max(updated_at) DESC",
    )
    .bind(org)
    .fetch_all(pool)
    .await?)
}

/// Delete original legacy files for items verified in `verified_run` that are
/// mapped in `asset_legacy_paths`. Refuses when the run's org scope has any
/// failed/broken item unless `allow_partial`. A legacy file that another,
/// not-yet-verified item also points at is kept.
pub async fn prune(
    pool: &PgPool,
    store: &AssetStore,
    org: Option<Uuid>,
    verified_run: Uuid,
    allow_partial: bool,
) -> Result<PruneReport, AppError> {
    let bad: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM asset_backfill_items i \
         WHERE i.status IN ('failed','broken') AND ($1::uuid IS NULL OR i.org_id = $1) \
           AND i.org_id IN (SELECT DISTINCT org_id FROM asset_backfill_items WHERE run_id = $2)",
    )
    .bind(org)
    .bind(verified_run)
    .fetch_one(pool)
    .await?;
    let verified: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM asset_backfill_items WHERE run_id = $1 AND status = 'verified'",
    )
    .bind(verified_run)
    .fetch_one(pool)
    .await?;
    if verified == 0 {
        return Err(AppError::BadRequest(format!(
            "run {verified_run} has no verified items"
        )));
    }
    if bad > 0 && !allow_partial {
        return Err(AppError::BadRequest(format!(
            "{bad} failed/broken item(s) in scope; re-run or pass --allow-partial"
        )));
    }
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT i.legacy_path FROM asset_backfill_items i JOIN asset_legacy_paths l ON l.legacy_path = i.legacy_path \
         WHERE i.run_id = $1 AND i.status = 'verified' AND i.source_table <> 'recipe_step_presets' \
           AND ($2::uuid IS NULL OR i.org_id = $2) \
           AND NOT EXISTS (SELECT 1 FROM asset_backfill_items o WHERE o.legacy_path = i.legacy_path \
                             AND o.source_table <> 'recipe_step_presets' AND o.status <> 'verified')",
    )
    .bind(verified_run)
    .bind(org)
    .fetch_all(pool)
    .await?;
    let mut rep = PruneReport {
        run_id: verified_run,
        ..Default::default()
    };
    for (rel,) in rows {
        let Some(path) = store.legacy_file(&rel) else {
            continue;
        };
        match tokio::fs::metadata(&path).await {
            Ok(m) if m.is_file() => {
                if tokio::fs::remove_file(&path).await.is_ok() {
                    rep.deleted += 1;
                    rep.bytes_freed += m.len() as i64;
                }
            }
            _ => rep.already_gone += 1,
        }
    }
    Ok(rep)
}
