//! Cash spot checks (owner design 2026-09-16 evening, item 5).
//!
//! A person holding `till.cash_spot_check` counts an open till's drawer against
//! the live expected figures. Someone without it may do ONE check when a holder
//! types their PIN on the till; that approval rides the replay envelope and is
//! verified there (accepted and flagged when it does not hold up, because the
//! count already happened).
//!
//! A spot check never changes the drawer: it records what was counted, what was
//! expected at that moment (the counter's snapshot) and the difference. It shows
//! on the Z report and the dashboard's till page.
//!
//! Split live / `*_inner` like every POS write, so `/sync/replay` lands a queued
//! check through the same code.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    authz::Cap,
    errors::{AppError, AppErrorResponse},
    realtime::hub::BranchEventHub,
    sync::ActingContext,
    tills::handlers::{
        compute_system_cash, extract_claims, fetch_till_or_404, publish, require_branch_access,
    },
};

/// One payment method on a spot check: what the system expected and, when the
/// counter counted it, what they found. Cash is always the first line.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, ToSchema)]
pub struct SpotCheckMethodLine {
    pub method: String,
    pub is_cash: bool,
    pub expected: i64,
    /// Null when this method was not counted.
    #[serde(default)]
    pub counted: Option<i64>,
    /// `counted - expected`, null when not counted.
    #[serde(default)]
    pub discrepancy: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize, Clone, ToSchema)]
pub struct SpotCheckMethodInput {
    pub method: String,
    #[serde(default)]
    pub is_cash: bool,
    /// The expected figure the counter saw. Absent → the server's own figure.
    #[serde(default)]
    pub expected: Option<i64>,
    #[serde(default)]
    pub counted: Option<i64>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CashSpotCheckRequest {
    /// Client-minted id; a retried or replayed check with the same id is one check.
    #[serde(default)]
    pub id: Option<Uuid>,
    /// The cash counted in the drawer, minor units.
    pub counted_cash: i64,
    /// The expected cash the counter saw. Absent → the server computes it now.
    #[serde(default)]
    pub expected_cash: Option<i64>,
    /// Per-method expected / counted figures. Absent → the server's own totals, uncounted.
    #[serde(default)]
    pub methods: Option<Vec<SpotCheckMethodInput>>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub checked_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub device_id: Option<Uuid>,
    /// Replay only: the person whose PIN unlocked this check (ignored live).
    #[serde(default)]
    pub approved_by: Option<Uuid>,
    /// Replay only: the approval id carried on the envelope (ignored live).
    #[serde(default)]
    pub approval_id: Option<Uuid>,
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct TillSpotCheck {
    pub id: Uuid,
    pub till_id: Uuid,
    pub branch_id: Uuid,
    pub counted_cash: i64,
    pub expected_cash: i64,
    /// `counted_cash - expected_cash`.
    pub cash_discrepancy: i64,
    #[schema(value_type = Vec<SpotCheckMethodLine>)]
    pub methods: sqlx::types::Json<Vec<SpotCheckMethodLine>>,
    pub note: Option<String>,
    pub checked_by: Uuid,
    pub checked_by_name: String,
    pub approved_by: Option<Uuid>,
    pub approved_by_name: Option<String>,
    pub approval_id: Option<Uuid>,
    pub device_id: Option<Uuid>,
    pub checked_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

/// Every column of [`TillSpotCheck`] from `till_spot_checks c`.
pub(crate) const SPOT_CHECK_COLUMNS: &str = "c.id, c.till_id, c.branch_id, c.counted_cash, c.expected_cash, \
    c.cash_discrepancy, c.methods, c.note, c.checked_by, \
    COALESCE((SELECT name FROM users WHERE id = c.checked_by), '') AS checked_by_name, \
    c.approved_by, (SELECT name FROM users WHERE id = c.approved_by) AS approved_by_name, \
    c.approval_id, c.device_id, c.checked_at, c.created_at";

/// A till's spot checks, oldest first (ties by id).
pub(crate) async fn spot_checks_for_till<'e, E: sqlx::PgExecutor<'e>>(
    exec: E,
    till_id: Uuid,
) -> Result<Vec<TillSpotCheck>, sqlx::Error> {
    sqlx::query_as::<_, TillSpotCheck>(&format!(
        "SELECT {SPOT_CHECK_COLUMNS} FROM till_spot_checks c WHERE c.till_id = $1 ORDER BY c.checked_at, c.id"
    ))
    .bind(till_id)
    .fetch_all(exec)
    .await
}

/// Spot checks of many tills (the till projection), each list oldest first.
pub(crate) async fn spot_checks_by_till(
    conn: &mut sqlx::PgConnection,
    till_ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, Vec<TillSpotCheck>>, AppError> {
    let rows = sqlx::query_as::<_, TillSpotCheck>(&format!(
        "SELECT {SPOT_CHECK_COLUMNS} FROM till_spot_checks c WHERE c.till_id = ANY($1) ORDER BY c.till_id, c.checked_at, c.id"
    ))
    .bind(till_ids)
    .fetch_all(&mut *conn)
    .await?;
    let mut out: std::collections::HashMap<Uuid, Vec<TillSpotCheck>> = Default::default();
    for r in rows {
        out.entry(r.till_id).or_default().push(r);
    }
    Ok(out)
}

/// Build the stored method lines: the counter's snapshot when sent, else the
/// server's own totals. Cash first; the cash line's counted figure is the
/// counted cash.
pub fn plan_methods(
    server: &[crate::tills::reconcile::MethodTotal],
    inputs: Option<&[SpotCheckMethodInput]>,
    counted_cash: i64,
    expected_cash: i64,
) -> Vec<SpotCheckMethodLine> {
    let line = |method: String, is_cash: bool, expected: i64, counted: Option<i64>| SpotCheckMethodLine {
        discrepancy: counted.map(|c| c - expected),
        method,
        is_cash,
        expected,
        counted,
    };
    let mut out: Vec<SpotCheckMethodLine> = Vec::new();
    match inputs {
        Some(inputs) if !inputs.is_empty() => {
            let cash_name = inputs
                .iter()
                .find(|i| i.is_cash)
                .map(|i| i.method.trim().to_string())
                .or_else(|| server.iter().find(|m| m.is_cash).map(|m| m.method.clone()))
                .unwrap_or_else(|| "cash".into());
            out.push(line(cash_name, true, expected_cash, Some(counted_cash)));
            for i in inputs.iter().filter(|i| !i.is_cash && !i.method.trim().is_empty()) {
                let expected = i.expected.unwrap_or_else(|| {
                    server
                        .iter()
                        .find(|m| m.method == i.method.trim())
                        .map(|m| m.system_total)
                        .unwrap_or(0)
                });
                out.push(line(i.method.trim().to_string(), false, expected, i.counted));
            }
        }
        _ => {
            let cash_name = server
                .iter()
                .find(|m| m.is_cash)
                .map(|m| m.method.clone())
                .unwrap_or_else(|| "cash".into());
            out.push(line(cash_name, true, expected_cash, Some(counted_cash)));
            for m in server.iter().filter(|m| !m.is_cash) {
                out.push(line(m.method.clone(), false, m.system_total, None));
            }
        }
    }
    out
}

// ── POST /tills/{till_id}/spot-checks ──────────────────────────

#[utoipa::path(post, path = "/tills/{till_id}/spot-checks", tag = "tills",
    params(("till_id" = Uuid, Path, description = "Till ID")), request_body = CashSpotCheckRequest,
    responses((status = 201, description = "Spot check recorded", body = TillSpotCheck),
              (status = 200, description = "Already recorded (same id)", body = TillSpotCheck), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn create_spot_check(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: Option<web::Data<BranchEventHub>>,
    till_id: web::Path<Uuid>,
    body: web::Bytes,
    device: crate::devices::DeviceHeader,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    // Permission first, before anything about the path or the body (the body is
    // taken raw so a malformed one cannot answer before the permission does).
    crate::authz::require::require(pool.get_ref(), &claims, Cap::TillCashSpotCheck, None).await?;
    let body: CashSpotCheckRequest = serde_json::from_slice(&body)
        .map_err(|e| AppError::BadRequest(format!("Json deserialize error: {e}")))?;
    let till = fetch_till_or_404(pool.get_ref(), *till_id).await?;
    require_branch_access(pool.get_ref(), &claims, till.branch_id).await?;
    crate::authz::require::require(
        pool.get_ref(),
        &claims,
        Cap::TillCashSpotCheck,
        Some(till.branch_id),
    )
    .await?;
    let mut body = body;
    body.device_id = body.device_id.or(device.0);
    // A live check is the caller's own grant; approvals only ride replay.
    body.approved_by = None;
    body.approval_id = None;
    create_spot_check_inner(
        pool.get_ref(),
        hub.as_ref().map(|h| h.get_ref()),
        till.id,
        body,
        ActingContext::live(&claims)?,
    )
    .await
}

pub(crate) async fn create_spot_check_inner(
    pool: &PgPool,
    hub: Option<&BranchEventHub>,
    till_id: Uuid,
    body: CashSpotCheckRequest,
    actor: ActingContext,
) -> Result<HttpResponse, AppError> {
    let till = fetch_till_or_404(pool, till_id).await?;
    if body.counted_cash < 0 {
        return Err(AppError::BadRequest("Counted cash cannot be negative".into()));
    }
    let checked_at = body.checked_at.unwrap_or_else(Utc::now);
    crate::clock::reject_if_future(checked_at, "checked_at")?;
    let note = body
        .note
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string);
    let id = body.id.unwrap_or_else(Uuid::new_v4);
    if let Some(existing) = fetch(pool, id).await? {
        return if existing.till_id == till_id {
            Ok(HttpResponse::Ok().json(existing))
        } else {
            Err(AppError::Conflict(
                "A spot check with this id belongs to another till".into(),
            ))
        };
    }
    // Live: only an open drawer is counted. Replay: the count happened while it
    // was open (the close is gated behind it on the device), so it is kept.
    if !actor.replay && till.status != "open" {
        return Err(AppError::Coded {
            status: 400,
            code: "TILL_NOT_OPEN",
            reason: "A spot check can only be taken on an open till".into(),
        });
    }

    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text))")
        .bind(till_id.to_string())
        .execute(&mut *tx)
        .await?;
    let expected_cash = match body.expected_cash {
        Some(e) => e,
        None => compute_system_cash(&mut *tx, till_id).await?,
    };
    let server_methods =
        crate::tills::reconcile::system_totals_by_method(&mut *tx, till_id, expected_cash).await?;
    let methods = plan_methods(
        &server_methods,
        body.methods.as_deref(),
        body.counted_cash,
        expected_cash,
    );
    let device_id = match body.device_id {
        Some(d) => {
            crate::devices::ensure_registered(&mut tx, actor.org_id, d, Some(till.branch_id), None)
                .await?
                .map(|_| d)
        }
        None => None,
    };
    // An approver who is not a person of this org is dropped, never stored.
    let approved_by = match body.approved_by {
        Some(a) => sqlx::query_scalar::<_, Uuid>("SELECT id FROM users WHERE id = $1 AND org_id = $2")
            .bind(a)
            .bind(actor.org_id)
            .fetch_optional(&mut *tx)
            .await?,
        None => None,
    };
    let inserted = sqlx::query(
        "INSERT INTO till_spot_checks (id, till_id, branch_id, counted_cash, expected_cash, cash_discrepancy,
                                       methods, note, checked_by, approved_by, approval_id, device_id, checked_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(id)
    .bind(till_id)
    .bind(till.branch_id)
    .bind(body.counted_cash)
    .bind(expected_cash)
    .bind(body.counted_cash - expected_cash)
    .bind(sqlx::types::Json(&methods))
    .bind(&note)
    .bind(actor.teller_id)
    .bind(approved_by)
    .bind(approved_by.and(body.approval_id))
    .bind(device_id)
    .bind(checked_at)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    let row = fetch(pool, id).await?.ok_or(AppError::Internal)?;
    if row.till_id != till_id {
        return Err(AppError::Conflict(
            "A spot check with this id belongs to another till".into(),
        ));
    }
    if inserted == 0 {
        return Ok(HttpResponse::Ok().json(row));
    }
    publish(
        hub,
        till.branch_id,
        "till.spot_check",
        serde_json::json!({
            "till_id": till_id, "branch_id": till.branch_id, "spot_check_id": row.id,
            "cash_discrepancy": row.cash_discrepancy,
        }),
    );
    Ok(HttpResponse::Created().json(row))
}

async fn fetch(pool: &PgPool, id: Uuid) -> Result<Option<TillSpotCheck>, AppError> {
    Ok(sqlx::query_as::<_, TillSpotCheck>(&format!(
        "SELECT {SPOT_CHECK_COLUMNS} FROM till_spot_checks c WHERE c.id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

// ── GET /tills/{till_id}/spot-checks ───────────────────────────

#[utoipa::path(get, path = "/tills/{till_id}/spot-checks", tag = "tills",
    params(("till_id" = Uuid, Path, description = "Till ID")),
    responses((status = 200, description = "The till's spot checks, oldest first", body = Vec<TillSpotCheck>), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn list_spot_checks(
    req: HttpRequest,
    pool: crate::db::Db,
    till_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    // Permission first: the same read the till page itself needs.
    crate::authz::require::require(pool.get_ref(), &claims, Cap::TillRead, None).await?;
    let till = fetch_till_or_404(pool.get_ref(), *till_id).await?;
    require_branch_access(pool.get_ref(), &claims, till.branch_id).await?;
    let rows = spot_checks_for_till(pool.get_ref(), till.id).await?;
    Ok(HttpResponse::Ok().json(rows))
}
