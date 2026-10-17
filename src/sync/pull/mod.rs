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

use std::collections::{BTreeMap, HashMap};

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgConnection, PgPool, Row};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::errors::{AppError, AppErrorResponse};

/// Every wire type (`ALL_TYPES`), the ledger types (`LEDGER_TYPES`, never
/// checksummed, windowed in full snapshots — `staff_drink` among them), the
/// ledger window (`LEDGER_WINDOW_HOURS`) and `is_ledger`: one copy with the POS
/// core, in madar-shared (`madar_sync`).
pub use madar_sync::{ALL_TYPES, LEDGER_TYPES, LEDGER_WINDOW_HOURS, is_ledger};
const DEFAULT_LIMIT: i64 = 2000;
const MAX_LIMIT: i64 = 5000;

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
    /// Opt-in paging of a FULL snapshot's ledger rows (tills, orders, cash,
    /// refunds), 100..10000 rows a page. Absent = the whole snapshot in one
    /// response (what every older client gets).
    #[serde(default)]
    pub ledger_page_size: Option<i64>,
    /// The `snapshot_cursor` of the previous page of a paged full snapshot.
    #[serde(default)]
    pub snapshot_cursor: Option<SnapshotCursor>,
    /// The sync protocol the till speaks (`sync::protocol`). Absent = v1, and
    /// the response is exactly what v1 always got.
    #[serde(default)]
    pub protocol: Option<u16>,
}

/// Where a paged full snapshot stands. Every page reads the same horizon and
/// window, so the pages together are ONE snapshot: a ledger row that changes
/// while the pages are fetched moves past the horizon and arrives in the
/// incremental pull that follows (`since = next`), never twice and never lost.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, ToSchema)]
pub struct SnapshotCursor {
    pub horizon: i64,
    /// The ledger window start of this snapshot (RFC 3339).
    pub window_from: String,
    /// When the snapshot began (RFC 3339): a till that closes while the pages
    /// are fetched keeps its rows in the later pages.
    pub started_at: String,
    /// Ledger rows with `seq` above this come next.
    pub after_seq: i64,
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
    /// A paged full snapshot with more pages: send it back as `snapshot_cursor`.
    /// State types, checksums and the asset bundle come on the FIRST page only;
    /// `types` on each page lists what that page covers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_cursor: Option<SnapshotCursor>,
    /// The negotiated sync protocol; only when the request named one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<u16>,
    /// v2: how long (hours) a ledger row stays on the incremental feed; a
    /// cursor older than that is answered `resync_required`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ledger_retention_hours: Option<i64>,
}

fn coded(code: &'static str, reason: &str) -> AppError {
    AppError::Coded {
        status: 400,
        code,
        reason: reason.into(),
    }
}

#[utoipa::path(post, path = "/sync/pull", tag = "sync",
    params(PullQuery), request_body = PullRequest,
    responses((status = 200, description = "Changefeed page, resync marker or full snapshot. An idle incremental answer (nothing after `since`) carries an `ETag` when the request named a `protocol` or sent `If-None-Match`.", body = PullResponse),
              (status = 304, description = "Opt-in: `If-None-Match` held the idle answer's ETag and nothing changed since. No body."),
              AppErrorResponse),
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
    let org_id = crate::authz::scope::branch_org(pool.get_ref(), body.branch_id)
        .await?
        .ok_or_else(|| AppError::NotFound("Branch not found".into()))?;
    if claims.role != crate::models::UserRole::SuperAdmin && claims.org_id() != Some(org_id) {
        return Err(AppError::Forbidden(
            "Branch belongs to another organization".into(),
        ));
    }
    // OPT-A: a till already at the head of its feed is answered from memory
    // (`fastpath`), and a till that sent `If-None-Match` with the idle
    // answer's ETag gets a 304. Same body as the database path below.
    let plain_incremental = q.since.is_some()
        && body.types.is_none()
        && body.ledger_page_size.is_none()
        && body.snapshot_cursor.is_none();
    if plain_incremental
        && let Some(since) = q.since
        && let Some(idle) = crate::fastpath::idle(org_id, body.branch_id, since)
    {
        let mut resp = PullResponse {
            full: false,
            since: Some(since),
            next: Some(since),
            has_more: false,
            server_time: Utc::now().to_rfc3339(),
            checksums: idle.checksums,
            ..Default::default()
        };
        apply_protocol(&mut resp, body.protocol);
        return Ok(respond(&req, &body, resp));
    }
    let cap = if plain_incremental {
        crate::fastpath::capture(Some(body.branch_id), None)
    } else {
        None
    };
    let (resp, idle_until) =
        pull_core_ex(pool.get_ref(), org_id, &body, q.since, cap.is_some()).await?;
    if let (Some(cap), Some(until)) = (cap, idle_until) {
        crate::fastpath::put_idle(
            cap,
            org_id,
            body.branch_id,
            crate::fastpath::Idle {
                since: q.since.unwrap_or_default(),
                checksums: resp.checksums.clone(),
            },
            until,
        );
    }
    // A till the Free plan switched off has now been TOLD (its own device row
    // came back with `plan_inactive`): from here a new sale it queues is refused
    // on replay (`plan::replay_verdict`).
    let device = body
        .device_id
        .or_else(|| crate::devices::DeviceHeader::from_request_headers(&req));
    if let Some(device) = device
        && resp.data.get("device").is_some_and(|rows| {
            rows.iter().any(|r| {
                r.get("id").and_then(|v| v.as_str()) == Some(device.to_string().as_str())
                    && r.get("plan_inactive").and_then(|v| v.as_bool()) == Some(true)
            })
        })
    {
        crate::plan::mark_till_told(pool.get_ref(), device).await?;
    }
    Ok(respond(&req, &body, resp))
}

/// "Nothing after `since`": no change, no further page, the cursor stays.
fn is_idle(resp: &PullResponse, since: Option<i64>) -> bool {
    since.is_some()
        && !resp.full
        && !resp.resync_required
        && !resp.has_more
        && resp.changes.is_empty()
        && resp.next == since
}

/// The ETag of an idle answer: everything in it but `server_time`. Weak, as
/// the body is also served compressed.
fn idle_etag(resp: &PullResponse) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(format!(
        "{:?}|{:?}|{:?}|",
        resp.since, resp.protocol, resp.ledger_retention_hours
    ));
    for (ty, c) in &resp.checksums {
        h.update(format!("{ty}={}:{};", c.count, c.checksum));
    }
    let hex: String = h.finalize()[..12]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("W/\"p-{hex}\"")
}

/// The response, with the opt-in ETag / 304 for an idle answer. A till that
/// names no `protocol` and sends no `If-None-Match` gets exactly the old
/// response: no ETag header, never a 304.
fn respond(req: &HttpRequest, body: &PullRequest, resp: PullResponse) -> HttpResponse {
    let inm = req
        .headers()
        .get(actix_web::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    let opted_in = body.protocol.is_some() || inm.is_some();
    if !opted_in || !is_idle(&resp, resp.since) {
        return HttpResponse::Ok().json(resp);
    }
    let etag = idle_etag(&resp);
    let matches = inm.is_some_and(|v| v.split(',').any(|t| t.trim() == etag));
    if matches {
        return HttpResponse::NotModified()
            .insert_header((actix_web::http::header::ETAG, etag))
            .finish();
    }
    HttpResponse::Ok()
        .insert_header((actix_web::http::header::ETAG, etag))
        .json(resp)
}

/// Protocol fields only for a till that named a protocol: v1 is unchanged.
fn apply_protocol(resp: &mut PullResponse, protocol: Option<u16>) {
    if protocol.is_some() {
        let p = crate::sync::protocol::negotiate(protocol);
        resp.protocol = Some(p);
        if p >= crate::sync::protocol::V2 {
            resp.ledger_retention_hours = Some(sweeper::retention().num_hours());
        }
    }
}

/// Everything behind the route (tests call it directly).
pub async fn pull_core(
    pool: &PgPool,
    org_id: Uuid,
    body: &PullRequest,
    since: Option<i64>,
) -> Result<PullResponse, AppError> {
    Ok(pull_core_ex(pool, org_id, body, since, false).await?.0)
}

/// [`pull_core`], plus — when `want_idle` and the answer is idle — the
/// deadline its checksums hold until (`sync_idle_valid_until`, read in the
/// same snapshot): `Some(None)` = no clock-based row on the branch.
async fn pull_core_ex(
    pool: &PgPool,
    org_id: Uuid,
    body: &PullRequest,
    since: Option<i64>,
    want_idle: bool,
) -> Result<(PullResponse, Option<Option<chrono::DateTime<Utc>>>), AppError> {
    let types: Vec<String> = match &body.types {
        Some(t) => {
            if since.is_some() {
                return Err(coded(
                    "TYPES_REQUIRE_FULL",
                    "`types` is only valid for a full pull (no `since`)",
                ));
            }
            for ty in t {
                if !ALL_TYPES.contains(&ty.as_str()) {
                    return Err(coded(
                        "UNKNOWN_SYNC_TYPE",
                        &format!("Unknown sync type `{ty}`"),
                    ));
                }
            }
            ALL_TYPES
                .iter()
                .filter(|a| t.iter().any(|x| x == *a))
                .map(|s| s.to_string())
                .collect()
        }
        None => ALL_TYPES.iter().map(|s| s.to_string()).collect(),
    };
    let limit = body.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    if since.is_some() && (body.snapshot_cursor.is_some() || body.ledger_page_size.is_some()) {
        return Err(coded(
            "PAGING_REQUIRES_FULL",
            "`ledger_page_size` / `snapshot_cursor` are only valid for a full pull",
        ));
    }
    let mut idle_until = None;
    let mut resp = match since {
        Some(since) => {
            let (resp, until) =
                incremental(pool, org_id, body.branch_id, since, limit, want_idle).await?;
            idle_until = until;
            resp
        }
        None => match body.ledger_page_size {
            Some(size) => {
                full_paged(
                    pool,
                    org_id,
                    body.branch_id,
                    &types,
                    size.clamp(100, 10_000),
                    body.snapshot_cursor.clone(),
                )
                .await?
            }
            None => full(pool, org_id, body.branch_id, &types).await?,
        },
    };
    apply_protocol(&mut resp, body.protocol);
    Ok((resp, idle_until))
}

async fn incremental(
    pool: &PgPool,
    org_id: Uuid,
    branch: Uuid,
    since: i64,
    limit: i64,
    want_idle: bool,
) -> Result<(PullResponse, Option<Option<chrono::DateTime<Utc>>>), AppError> {
    let server_time = Utc::now().to_rfc3339();
    // ONE pooled connection for the whole pull (round trips: each acquire of
    // the shared tenant pool stamps the org on the connection first), and a
    // pull never holds a second one (a pool of 5 would otherwise deadlock
    // under 6 concurrent pulls).
    let mut conn = pool.acquire().await?;
    let (purged, head): (i64, i64) = sqlx::query_as(
        "SELECT COALESCE((SELECT purged_through_seq FROM sync_feed_watermarks WHERE branch_id = $1), 0), \
                COALESCE((SELECT max(seq) FROM sync_changes WHERE branch_id = $1), 0)",
    )
    .bind(branch)
    .fetch_one(&mut *conn)
    .await?;
    if since < purged || since > head {
        return Ok((
            PullResponse {
                resync_required: true,
                since: Some(since),
                server_time,
                ..Default::default()
            },
            None,
        ));
    }
    // Autocommit statement BEFORE reading (B1 amendment R-horizon).
    let horizon: i64 = sqlx::query_scalar("SELECT sync_safe_horizon($1, $2)")
        .bind(branch)
        .bind(since)
        .fetch_one(&mut *conn)
        .await?;
    let rows = sqlx::query(
        "SELECT seq, type, entity_id, op FROM sync_changes \
          WHERE branch_id = $1 AND seq > $2 AND seq <= $3 ORDER BY seq LIMIT $4",
    )
    .bind(branch)
    .bind(since)
    .bind(horizon)
    .bind(limit + 1)
    .fetch_all(&mut *conn)
    .await?;
    let has_more = rows.len() as i64 > limit;
    let rows: Vec<(i64, String, Uuid, String)> = rows
        .into_iter()
        .take(limit as usize)
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3)))
        .collect();
    let next = if has_more {
        rows.last().map(|r| r.0).unwrap_or(since)
    } else {
        horizon
    };
    let idle = rows.is_empty() && !has_more && next == since;

    // Projections and checksums read one snapshot on the same connection.
    let mut tx = sqlx::Acquire::begin(&mut *conn).await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
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
            let data = if op == "upsert" {
                projected.remove(&(ty.clone(), id))
            } else {
                None
            };
            let op = if data.is_some() { "upsert" } else { "delete" }.to_string();
            PullChange {
                seq,
                ty,
                id,
                op,
                data,
            }
        })
        .collect();
    let checksums = if has_more {
        BTreeMap::new()
    } else {
        state_checksums(&mut tx, branch, ALL_TYPES, horizon).await?
    };
    // The idle answer's clock deadline, in the checksums' own snapshot (the
    // live gates read `now()`, the transaction's start, in both).
    let idle_until = if want_idle && idle {
        let until: Option<chrono::DateTime<Utc>> =
            sqlx::query_scalar("SELECT sync_idle_valid_until($1)")
                .bind(branch)
                .fetch_one(&mut *tx)
                .await?;
        Some(until)
    } else {
        None
    };
    tx.commit().await?;
    Ok((
        PullResponse {
            full: false,
            since: Some(since),
            next: Some(next),
            has_more,
            server_time,
            changes,
            checksums,
            ..Default::default()
        },
        idle_until,
    ))
}

async fn full(
    pool: &PgPool,
    org_id: Uuid,
    branch: Uuid,
    types: &[String],
) -> Result<PullResponse, AppError> {
    let server_time = Utc::now().to_rfc3339();
    let horizon: i64 = sqlx::query_scalar("SELECT sync_safe_horizon($1, 0)")
        .bind(branch)
        .fetch_one(pool)
        .await?;
    let window_from = Utc::now() - chrono::Duration::hours(LEDGER_WINDOW_HOURS);

    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
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
        ledger_window: Some(LedgerWindow {
            from: window_from.to_rfc3339(),
        }),
        asset_bundle: Some(asset_bundle),
        ..Default::default()
    })
}

/// A full snapshot in pages (see [`SnapshotCursor`]). Page one is everything a
/// full pull returns except that ledger rows stop at `size`; later pages carry
/// only ledger rows, in seq order, of the same snapshot.
async fn full_paged(
    pool: &PgPool,
    org_id: Uuid,
    branch: Uuid,
    types: &[String],
    size: i64,
    cursor: Option<SnapshotCursor>,
) -> Result<PullResponse, AppError> {
    let server_time = Utc::now().to_rfc3339();
    let first = cursor.is_none();
    let cursor = match cursor {
        Some(c) => c,
        None => {
            let horizon: i64 = sqlx::query_scalar("SELECT sync_safe_horizon($1, 0)")
                .bind(branch)
                .fetch_one(pool)
                .await?;
            SnapshotCursor {
                horizon,
                window_from: (Utc::now() - chrono::Duration::hours(LEDGER_WINDOW_HOURS))
                    .to_rfc3339(),
                started_at: server_time.clone(),
                after_seq: 0,
            }
        }
    };
    let parse = |t: &str, what: &str| {
        chrono::DateTime::parse_from_rfc3339(t)
            .map(|d| d.with_timezone(&Utc))
            .map_err(|_| {
                coded(
                    "BAD_SNAPSHOT_CURSOR",
                    &format!("snapshot_cursor.{what} is not RFC 3339"),
                )
            })
    };
    let window_from = parse(&cursor.window_from, "window_from")?;
    let started_at = parse(&cursor.started_at, "started_at")?;
    let ledger: Vec<String> = types.iter().filter(|t| is_ledger(t)).cloned().collect();

    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    let mut data: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut page_types: Vec<String> = Vec::new();
    if first {
        for ty in types.iter().filter(|t| !is_ledger(t)) {
            let rows: Vec<(Uuid, i64)> = sqlx::query_as(
                "SELECT entity_id, seq FROM sync_changes \
                  WHERE branch_id = $1 AND type = $2 AND op = 'upsert' AND seq <= $3 ORDER BY seq",
            )
            .bind(branch)
            .bind(ty)
            .bind(cursor.horizon)
            .fetch_all(&mut *tx)
            .await?;
            let ids: Vec<Uuid> = rows.iter().map(|r| r.0).collect();
            let mut projected = projection::project(&mut tx, org_id, branch, ty, &ids).await?;
            data.insert(ty.clone(), with_seq(rows, &mut projected));
            page_types.push(ty.clone());
        }
    }
    let rows: Vec<(String, Uuid, i64)> = sqlx::query_as(LEDGER_PAGE_SQL)
        .bind(branch)
        .bind(&ledger)
        .bind(cursor.horizon)
        .bind(window_from)
        .bind(started_at)
        .bind(cursor.after_seq)
        .bind(size + 1)
        .fetch_all(&mut *tx)
        .await?;
    let has_more = rows.len() as i64 > size;
    let rows: Vec<(String, Uuid, i64)> = rows.into_iter().take(size as usize).collect();
    let last_seq = rows.last().map(|r| r.2).unwrap_or(cursor.after_seq);
    for ty in &ledger {
        let of_type: Vec<(Uuid, i64)> = rows
            .iter()
            .filter(|r| &r.0 == ty)
            .map(|r| (r.1, r.2))
            .collect();
        let ids: Vec<Uuid> = of_type.iter().map(|r| r.0).collect();
        let mut projected = projection::project(&mut tx, org_id, branch, ty, &ids).await?;
        data.insert(ty.clone(), with_seq(of_type, &mut projected));
        page_types.push(ty.clone());
    }
    let (checksums, asset_bundle) = if first {
        let type_refs: Vec<&str> = types.iter().map(String::as_str).collect();
        (
            state_checksums(&mut tx, branch, &type_refs, cursor.horizon).await?,
            Some(latest_asset_bundle(&mut tx, org_id, branch).await?),
        )
    } else {
        (BTreeMap::new(), None)
    };
    tx.commit().await?;
    Ok(PullResponse {
        full: true,
        next: Some(cursor.horizon),
        has_more,
        server_time,
        types: page_types,
        data,
        checksums,
        ledger_window: Some(LedgerWindow {
            from: cursor.window_from.clone(),
        }),
        asset_bundle,
        snapshot_cursor: has_more.then_some(SnapshotCursor {
            after_seq: last_seq,
            ..cursor
        }),
        ..Default::default()
    })
}

fn with_seq(rows: Vec<(Uuid, i64)>, projected: &mut HashMap<Uuid, Value>) -> Vec<Value> {
    rows.into_iter()
        .filter_map(|(id, seq)| {
            let mut v = projected.remove(&id)?;
            if let Value::Object(m) = &mut v {
                m.insert("seq".into(), Value::from(seq));
            }
            Some(v)
        })
        .collect()
}

async fn latest_asset_bundle(
    conn: &mut PgConnection,
    org_id: Uuid,
    branch: Uuid,
) -> Result<Option<AssetBundleRef>, AppError> {
    Ok(sqlx::query("SELECT seq, bytes, sha256 FROM asset_bundles WHERE branch_id = $1 ORDER BY seq DESC LIMIT 1")
        .bind(branch)
        .fetch_optional(&mut *conn)
        .await?
        .map(|r| {
            let seq: i64 = r.get(0);
            AssetBundleRef { url: format!("/sync/asset-bundles/{org_id}/assets-{branch}-{seq}.tar"), seq, bytes: r.get(1), sha256: r.get(2) }
        }))
}

/// A page of a paged snapshot's ledger rows: the window of [`LEDGER_WINDOW_SQL`]
/// over every ledger type at once, in seq order after the cursor. A till counts
/// as open if it was open when the snapshot began (`closed_at >= $5`).
const LEDGER_PAGE_SQL: &str = "\
SELECT c.type, c.entity_id, c.seq FROM sync_changes c
 WHERE c.branch_id = $1 AND c.type = ANY($2) AND c.op = 'upsert' AND c.seq <= $3 AND c.seq > $6
   AND (c.changed_at >= $4
        OR (c.type = 'till'          AND EXISTS (SELECT 1 FROM tills t WHERE t.id = c.entity_id
                                              AND (t.status = 'open' OR t.closed_at >= $5)))
        OR (c.type = 'cash_movement' AND EXISTS (SELECT 1 FROM till_cash_movements m JOIN tills t ON t.id = m.till_id
                                              WHERE m.id = c.entity_id AND (t.status = 'open' OR t.closed_at >= $5)))
        OR (c.type = 'order'         AND EXISTS (SELECT 1 FROM orders o JOIN tills t ON t.id = o.till_id
                                              WHERE o.id = c.entity_id AND (t.status = 'open' OR t.closed_at >= $5)))
        OR (c.type = 'refund'        AND EXISTS (SELECT 1 FROM order_refunds r JOIN tills t ON t.id = r.till_id
                                              WHERE r.id = c.entity_id AND (t.status = 'open' OR t.closed_at >= $5))))
 ORDER BY c.seq
 LIMIT $7";

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
    let rows: Vec<(String, Uuid, i64)> = sqlx::query_as(&sql)
        .bind(branch)
        .bind(horizon)
        .bind(&state)
        .fetch_all(&mut *conn)
        .await?;
    let mut rows_of: BTreeMap<String, Vec<(String, i64)>> =
        state.iter().map(|t| (t.to_string(), Vec::new())).collect();
    for (ty, id, seq) in rows {
        rows_of.entry(ty).or_default().push((id.to_string(), seq));
    }
    for (ty, rows) in rows_of {
        by_type.insert(
            ty,
            TypeChecksum {
                count: rows.len() as i64,
                checksum: checksum::checksum_of(&rows),
            },
        );
    }
    Ok(by_type)
}
