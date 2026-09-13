//! `POST /sync/pull` — the one changefeed pull (TILLS_CONTRACT §10.2, decisions
//! 15 + 16). The POS core (`madar-core/src/sync_pull.rs`) is the consumer; the
//! wire shapes here match its `PullResponse` / `ChangeWire` / `TypeChecksum`.
//!
//! * Incremental (`?since=`): rows of `sync_changes` with `since < seq <= horizon`,
//!   paged by `limit`; `horizon` comes from `sync_safe_horizon()` (never skips a
//!   seq held by an in-flight emitter). Each change carries the entity's CURRENT
//!   lean projection; an entity that no longer projects is sent as `delete`.
//! * Resync: `since` older than the purge watermark or ahead of the head.
//! * Full (no `since`): one REPEATABLE READ snapshot of every live state row
//!   (each with `seq`) plus the ledger window (last 48 h + every open till's
//!   history), per-type checksums and the latest asset bundle.
pub mod checksum;
pub mod listener;
pub mod projection;
pub mod sweeper;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap};

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgConnection, PgPool, Row};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::errors::{AppError, AppErrorResponse};

/// Every wire type, in the order the POS lists them (`ALL_TYPES`).
pub const ALL_TYPES: &[&str] = &[
    "category", "menu_item", "bundle", "ingredient", "payment_method", "payment_availability",
    "discount", "branch_settings", "device", "teller", "floor_section", "floor_table",
    "table_occupancy", "table_transfer", "open_ticket", "kitchen_ticket", "delivery", "booking",
    "till", "cash_movement", "order", "refund",
];
/// Ledger types: never checksummed; windowed in full snapshots.
pub const LEDGER_TYPES: &[&str] = &["till", "cash_movement", "order", "refund"];
/// How far back a full snapshot's ledger window reaches.
pub const LEDGER_WINDOW_HOURS: i64 = 48;
const DEFAULT_LIMIT: i64 = 2000;
const MAX_LIMIT: i64 = 5000;

pub fn is_ledger(ty: &str) -> bool {
    LEDGER_TYPES.contains(&ty)
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PullQuery {
    /// Cursor from the previous response's `next`. Absent = full snapshot.
    pub since: Option<i64>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct PullRequest {
    pub branch_id: Uuid,
    #[serde(default)]
    pub device_id: Option<Uuid>,
    /// Full-fetch ONLY these types (checksum self-heal). Invalid with `since`.
    #[serde(default)]
    pub types: Option<Vec<String>>,
    /// Page size for incremental pulls, 1..5000 (default 2000).
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, ToSchema)]
pub struct TypeChecksum {
    pub count: i64,
    pub checksum: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct PullChange {
    pub seq: i64,
    #[serde(rename = "type")]
    pub ty: String,
    pub id: Uuid,
    /// `upsert` | `delete`.
    pub op: String,
    #[schema(value_type = Object)]
    pub data: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct LedgerWindow {
    pub from: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct AssetBundleRef {
    pub url: String,
    pub seq: i64,
    pub bytes: i64,
    pub sha256: String,
}

/// Any `/sync/pull` response (incremental, resync or full).
#[derive(Debug, Serialize, Deserialize, Clone, Default, ToSchema)]
pub struct PullResponse {
    pub full: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub resync_required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<i64>,
    pub has_more: bool,
    pub server_time: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<PullChange>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    #[schema(value_type = Object)]
    pub data: BTreeMap<String, Vec<Value>>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub checksums: BTreeMap<String, TypeChecksum>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ledger_window: Option<LedgerWindow>,
    /// Full responses only: the latest built base bundle, or null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset_bundle: Option<Option<AssetBundleRef>>,
}

fn coded(code: &'static str, reason: &str) -> AppError {
    AppError::Coded { status: 400, code, reason: reason.into() }
}

#[utoipa::path(post, path = "/sync/pull", tag = "sync",
    params(PullQuery), request_body = PullRequest,
    responses((status = 200, description = "Changefeed page, resync marker or full snapshot", body = PullResponse), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn pull(
    req: HttpRequest,
    pool: crate::db::Db,
    q: web::Query<PullQuery>,
    body: web::Json<PullRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = crate::tills::handlers::extract_claims(&req)?;
    let body = body.into_inner();
    crate::tills::handlers::require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;
    let org_id: Uuid = sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL")
        .bind(body.branch_id)
        .fetch_optional(pool.get_ref())
        .await?
        .ok_or_else(|| AppError::NotFound("Branch not found".into()))?;
    if claims.role != crate::models::UserRole::SuperAdmin && claims.org_id() != Some(org_id) {
        return Err(AppError::Forbidden("Branch belongs to another organization".into()));
    }
    let resp = pull_core(pool.get_ref(), org_id, &body, q.since).await?;
    Ok(HttpResponse::Ok().json(resp))
}

/// Everything behind the route (tests call it directly).
pub async fn pull_core(pool: &PgPool, org_id: Uuid, body: &PullRequest, since: Option<i64>) -> Result<PullResponse, AppError> {
    let types: Vec<String> = match &body.types {
        Some(t) => {
            if since.is_some() {
                return Err(coded("TYPES_REQUIRE_FULL", "`types` is only valid for a full pull (no `since`)"));
            }
            for ty in t {
                if !ALL_TYPES.contains(&ty.as_str()) {
                    return Err(coded("UNKNOWN_SYNC_TYPE", &format!("Unknown sync type `{ty}`")));
                }
            }
            ALL_TYPES.iter().filter(|a| t.iter().any(|x| x == *a)).map(|s| s.to_string()).collect()
        }
        None => ALL_TYPES.iter().map(|s| s.to_string()).collect(),
    };
    let limit = body.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    match since {
        Some(since) => incremental(pool, org_id, body.branch_id, since, limit).await,
        None => full(pool, org_id, body.branch_id, &types).await,
    }
}

async fn incremental(pool: &PgPool, org_id: Uuid, branch: Uuid, since: i64, limit: i64) -> Result<PullResponse, AppError> {
    let server_time = Utc::now().to_rfc3339();
    let (purged, head): (i64, i64) = sqlx::query_as(
        "SELECT COALESCE((SELECT purged_through_seq FROM sync_feed_watermarks WHERE branch_id = $1), 0), \
                COALESCE((SELECT max(seq) FROM sync_changes WHERE branch_id = $1), 0)",
    )
    .bind(branch)
    .fetch_one(pool)
    .await?;
    if since < purged || since > head {
        return Ok(PullResponse { resync_required: true, since: Some(since), server_time, ..Default::default() });
    }
    // Autocommit statement BEFORE reading (B1 amendment R-horizon).
    let horizon: i64 = sqlx::query_scalar("SELECT sync_safe_horizon($1, $2)")
        .bind(branch)
        .bind(since)
        .fetch_one(pool)
        .await?;
    let rows = sqlx::query(
        "SELECT seq, type, entity_id, op FROM sync_changes \
          WHERE branch_id = $1 AND seq > $2 AND seq <= $3 ORDER BY seq LIMIT $4",
    )
    .bind(branch)
    .bind(since)
    .bind(horizon)
    .bind(limit + 1)
    .fetch_all(pool)
    .await?;
    let has_more = rows.len() as i64 > limit;
    let rows: Vec<(i64, String, Uuid, String)> = rows
        .into_iter()
        .take(limit as usize)
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3)))
        .collect();
    let next = if has_more { rows.last().map(|r| r.0).unwrap_or(since) } else { horizon };

    // Projections and checksums read one snapshot on ONE connection: a pull
    // never holds a second pooled connection (a pool of 5 would otherwise
    // deadlock under 6 concurrent pulls).
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY").execute(&mut *tx).await?;
    let mut wanted: BTreeMap<String, Vec<Uuid>> = BTreeMap::new();
    for (_, ty, id, op) in &rows {
        if op == "upsert" {
            wanted.entry(ty.clone()).or_default().push(*id);
        }
    }
    let mut projected: HashMap<(String, Uuid), Value> = HashMap::new();
    for (ty, ids) in &wanted {
        for (id, v) in projection::project(&mut tx, org_id, branch, ty, ids).await? {
            projected.insert((ty.clone(), id), v);
        }
    }
    let changes = rows
        .into_iter()
        .map(|(seq, ty, id, op)| {
            let data = if op == "upsert" { projected.remove(&(ty.clone(), id)) } else { None };
            let op = if data.is_some() { "upsert" } else { "delete" }.to_string();
            PullChange { seq, ty, id, op, data }
        })
        .collect();
    let checksums = if has_more {
        BTreeMap::new()
    } else {
        state_checksums(&mut tx, branch, ALL_TYPES, horizon).await?
    };
    tx.commit().await?;
    Ok(PullResponse {
        full: false,
        since: Some(since),
        next: Some(next),
        has_more,
        server_time,
        changes,
        checksums,
        ..Default::default()
    })
}

async fn full(pool: &PgPool, org_id: Uuid, branch: Uuid, types: &[String]) -> Result<PullResponse, AppError> {
    let server_time = Utc::now().to_rfc3339();
    let horizon: i64 = sqlx::query_scalar("SELECT sync_safe_horizon($1, 0)").bind(branch).fetch_one(pool).await?;
    let window_from = Utc::now() - chrono::Duration::hours(LEDGER_WINDOW_HOURS);

    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY").execute(&mut *tx).await?;
    let mut data = BTreeMap::new();
    for ty in types {
        let rows: Vec<(Uuid, i64)> = if is_ledger(ty) {
            sqlx::query_as(LEDGER_WINDOW_SQL)
                .bind(branch)
                .bind(ty)
                .bind(horizon)
                .bind(window_from)
                .fetch_all(&mut *tx)
                .await?
        } else {
            sqlx::query_as(
                "SELECT entity_id, seq FROM sync_changes \
                  WHERE branch_id = $1 AND type = $2 AND op = 'upsert' AND seq <= $3 ORDER BY seq",
            )
            .bind(branch)
            .bind(ty)
            .bind(horizon)
            .fetch_all(&mut *tx)
            .await?
        };
        let ids: Vec<Uuid> = rows.iter().map(|r| r.0).collect();
        let mut projected = projection::project(&mut tx, org_id, branch, ty, &ids).await?;
        let out: Vec<Value> = rows
            .into_iter()
            .filter_map(|(id, seq)| {
                let mut v = projected.remove(&id)?;
                if let Value::Object(m) = &mut v {
                    m.insert("seq".into(), Value::from(seq));
                }
                Some(v)
            })
            .collect();
        data.insert(ty.clone(), out);
    }
    let type_refs: Vec<&str> = types.iter().map(String::as_str).collect();
    let checksums = state_checksums(&mut tx, branch, &type_refs, horizon).await?;
    let asset_bundle: Option<AssetBundleRef> = sqlx::query(
        "SELECT seq, bytes, sha256 FROM asset_bundles WHERE branch_id = $1 ORDER BY seq DESC LIMIT 1",
    )
    .bind(branch)
    .fetch_optional(&mut *tx)
    .await?
    .map(|r| {
        let seq: i64 = r.get(0);
        AssetBundleRef {
            url: format!("/sync/asset-bundles/{org_id}/assets-{branch}-{seq}.tar"),
            seq,
            bytes: r.get(1),
            sha256: r.get(2),
        }
    });
    tx.commit().await?;
    Ok(PullResponse {
        full: true,
        next: Some(horizon),
        has_more: false,
        server_time,
        types: types.to_vec(),
        data,
        checksums,
        ledger_window: Some(LedgerWindow { from: window_from.to_rfc3339() }),
        asset_bundle: Some(asset_bundle),
        ..Default::default()
    })
}

/// Ledger rows of a full snapshot: changed inside the window, or belonging to
/// a till that is still open (its whole history).
const LEDGER_WINDOW_SQL: &str = "\
SELECT c.entity_id, c.seq FROM sync_changes c
 WHERE c.branch_id = $1 AND c.type = $2 AND c.op = 'upsert' AND c.seq <= $3
   AND (c.changed_at >= $4
        OR ($2 = 'till'          AND EXISTS (SELECT 1 FROM tills t WHERE t.id = c.entity_id AND t.status = 'open'))
        OR ($2 = 'cash_movement' AND EXISTS (SELECT 1 FROM till_cash_movements m JOIN tills t ON t.id = m.till_id
                                              WHERE m.id = c.entity_id AND t.status = 'open'))
        OR ($2 = 'order'         AND EXISTS (SELECT 1 FROM orders o JOIN tills t ON t.id = o.till_id
                                              WHERE o.id = c.entity_id AND t.status = 'open'))
        OR ($2 = 'refund'        AND EXISTS (SELECT 1 FROM order_refunds r JOIN tills t ON t.id = r.till_id
                                              WHERE r.id = c.entity_id AND t.status = 'open')))
 ORDER BY c.seq";

/// R-checksum per state type over exactly that type's projected set: the feed's
/// `upsert` rows whose entity passes [`projection::projects_sql`] — the same
/// gate `project` applies, so what a device holds after applying the feed (or a
/// full snapshot) is what is counted here.
async fn state_checksums(
    conn: &mut PgConnection,
    branch: Uuid,
    types: &[&str],
    horizon: i64,
) -> Result<BTreeMap<String, TypeChecksum>, AppError> {
    let state: Vec<&str> = types.iter().copied().filter(|t| !is_ledger(t)).collect();
    let mut by_type: BTreeMap<String, TypeChecksum> = BTreeMap::new();
    if state.is_empty() {
        return Ok(by_type);
    }
    let cases: String = state
        .iter()
        .map(|t| {
            let pred = projection::projects_sql(t).expect("state type has a projection gate");
            format!(" WHEN '{t}' THEN {}", pred.replace("$ID", "c.entity_id"))
        })
        .collect();
    let sql = format!(
        "SELECT c.type, c.entity_id, c.seq FROM sync_changes c \
          WHERE c.branch_id = $1 AND c.op = 'upsert' AND c.seq <= $2 AND c.type = ANY($3) \
            AND CASE c.type{cases} ELSE false END"
    );
    let rows: Vec<(String, Uuid, i64)> =
        sqlx::query_as(&sql).bind(branch).bind(horizon).bind(&state).fetch_all(&mut *conn).await?;
    let mut rows_of: BTreeMap<String, Vec<(String, i64)>> = state.iter().map(|t| (t.to_string(), Vec::new())).collect();
    for (ty, id, seq) in rows {
        rows_of.entry(ty).or_default().push((id.to_string(), seq));
    }
    for (ty, rows) in rows_of {
        by_type.insert(ty, TypeChecksum { count: rows.len() as i64, checksum: checksum::checksum_of(&rows) });
    }
    Ok(by_type)
}
