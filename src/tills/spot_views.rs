//! Cash spot report views (owner design 2026-09-16 evening item 5, corrected
//! 2026-09-17).
//!
//! The cash spot is the FULL live till report of an open till (the old X
//! report), shown on the POS and printable. Nothing is counted here: counting
//! happens only at close. What the server keeps is the AUDIT TRAIL: who viewed
//! the spot report, when, whether it was printed, and whose PIN unlocked it
//! when the viewer does not hold `till.cash_spot_check`.
//!
//! Split live / `*_inner` like every POS write, so `/sync/replay` lands a queued
//! view through the same code. A print marks the same row (same client id).

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
    tills::handlers::{extract_claims, fetch_till_or_404, publish, require_branch_access},
};

/// A one-time unlock minted on the till: someone holding
/// `till.cash_spot_check` typed their PIN.
#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct SpotViewApproval {
    pub id: Uuid,
    /// Always `till.cash_spot_check`.
    pub capability: String,
    pub approver_id: Uuid,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct SpotViewRequest {
    /// Client-minted id; a retry, a replay or the print of the same view is one row.
    #[serde(default)]
    pub id: Option<Uuid>,
    /// The spot report was printed.
    #[serde(default)]
    pub printed: bool,
    #[serde(default)]
    pub viewed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub printed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub device_id: Option<Uuid>,
    /// Live route: the one-time unlock, when the caller does not hold
    /// `till.cash_spot_check`. (Replay carries it on the envelope.)
    #[serde(default)]
    pub approval: Option<SpotViewApproval>,
    /// Set by replay from a verified envelope approval (ignored live).
    #[serde(default)]
    pub approved_by: Option<Uuid>,
    /// Set by replay from a verified envelope approval (ignored live).
    #[serde(default)]
    pub approval_id: Option<Uuid>,
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct TillSpotView {
    pub id: Uuid,
    pub till_id: Uuid,
    pub branch_id: Uuid,
    pub viewed_by: Uuid,
    pub viewed_by_name: String,
    pub printed: bool,
    pub printed_at: Option<DateTime<Utc>>,
    pub approved_by: Option<Uuid>,
    pub approved_by_name: Option<String>,
    pub approval_id: Option<Uuid>,
    pub device_id: Option<Uuid>,
    pub viewed_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

/// Every column of [`TillSpotView`] from `till_spot_views v`.
pub(crate) const SPOT_VIEW_COLUMNS: &str = "v.id, v.till_id, v.branch_id, v.viewed_by, \
    COALESCE((SELECT name FROM users WHERE id = v.viewed_by), '') AS viewed_by_name, \
    v.printed, v.printed_at, v.approved_by, (SELECT name FROM users WHERE id = v.approved_by) AS approved_by_name, \
    v.approval_id, v.device_id, v.viewed_at, v.created_at";

/// A till's spot views, oldest first (ties by id).
pub(crate) async fn spot_views_for_till<'e, E: sqlx::PgExecutor<'e>>(
    exec: E,
    till_id: Uuid,
) -> Result<Vec<TillSpotView>, sqlx::Error> {
    sqlx::query_as::<_, TillSpotView>(&format!(
        "SELECT {SPOT_VIEW_COLUMNS} FROM till_spot_views v WHERE v.till_id = $1 ORDER BY v.viewed_at, v.id"
    ))
    .bind(till_id)
    .fetch_all(exec)
    .await
}

/// Spot views of many tills (the till projection), each list oldest first.
pub(crate) async fn spot_views_by_till(
    conn: &mut sqlx::PgConnection,
    till_ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, Vec<TillSpotView>>, AppError> {
    let rows = sqlx::query_as::<_, TillSpotView>(&format!(
        "SELECT {SPOT_VIEW_COLUMNS} FROM till_spot_views v WHERE v.till_id = ANY($1) ORDER BY v.till_id, v.viewed_at, v.id"
    ))
    .bind(till_ids)
    .fetch_all(&mut *conn)
    .await?;
    let mut out: std::collections::HashMap<Uuid, Vec<TillSpotView>> = Default::default();
    for r in rows {
        out.entry(r.till_id).or_default().push(r);
    }
    Ok(out)
}

// ── POST /tills/{till_id}/spot-views ───────────────────────────

#[utoipa::path(post, path = "/tills/{till_id}/spot-views", tag = "tills",
    params(("till_id" = Uuid, Path, description = "Till ID")), request_body = SpotViewRequest,
    responses((status = 201, description = "Spot report view recorded", body = TillSpotView),
              (status = 200, description = "Already recorded (same id); a print marks it printed", body = TillSpotView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn create_spot_view(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: Option<web::Data<BranchEventHub>>,
    till_id: web::Path<Uuid>,
    body: web::Bytes,
    device: crate::devices::DeviceHeader,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let org = claims
        .org_id()
        .ok_or_else(|| crate::authz::require::denied(Cap::TillCashSpotCheck))?;
    // Permission first, before the path or the body are validated: the grant,
    // or a one-time unlock for it that verifies (the body is read leniently for
    // it only; anything else about the body answers after this).
    let holds =
        crate::authz::require::can(pool.get_ref(), &claims, Cap::TillCashSpotCheck, None).await?;
    let mut unlock: Option<crate::sync::handlers::ReplayApproval> = None;
    if !holds {
        let a = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("approval").cloned())
            .and_then(|a| serde_json::from_value::<crate::sync::handlers::ReplayApproval>(a).ok())
            .ok_or_else(|| crate::authz::require::denied(Cap::TillCashSpotCheck))?;
        match crate::sync::handlers::verify_approval(
            pool.get_ref(),
            &a,
            claims.user_id(),
            org,
            None,
            None,
        )
        .await
        {
            Ok(Cap::TillCashSpotCheck) => unlock = Some(a),
            _ => return Err(crate::authz::require::denied(Cap::TillCashSpotCheck)),
        }
    }
    let mut body: SpotViewRequest = serde_json::from_slice(&body)
        .map_err(|e| AppError::BadRequest(format!("Json deserialize error: {e}")))?;
    let till = fetch_till_or_404(pool.get_ref(), *till_id).await?;
    require_branch_access(pool.get_ref(), &claims, till.branch_id).await?;
    if holds {
        crate::authz::require::require(
            pool.get_ref(),
            &claims,
            Cap::TillCashSpotCheck,
            Some(till.branch_id),
        )
        .await?;
    }
    body.device_id = body.device_id.or(device.0);
    body.approved_by = None;
    body.approval_id = None;
    if let Some(a) = &unlock {
        // One unlock, one view: an approval already used by another view is spent.
        let used: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM till_spot_views WHERE approval_id = $1 AND id IS DISTINCT FROM $2)",
        )
        .bind(a.id)
        .bind(body.id)
        .fetch_one(pool.get_ref())
        .await?;
        if used {
            return Err(crate::authz::require::denied(Cap::TillCashSpotCheck));
        }
        body.approved_by = Some(a.approver_id);
        body.approval_id = Some(a.id);
    }
    let viewed_at = body.viewed_at.unwrap_or_else(Utc::now);
    let out = create_spot_view_inner(
        pool.get_ref(),
        hub.as_ref().map(|h| h.get_ref()),
        till.id,
        body,
        ActingContext::live(&claims)?,
    )
    .await?;
    if let Some(a) = &unlock {
        crate::sync::handlers::record_approval(
            pool.get_ref(),
            a,
            org,
            Some(till.branch_id),
            device.0,
            claims.user_id(),
            "SpotReportView",
            viewed_at,
            &Ok(Cap::TillCashSpotCheck),
        )
        .await;
    }
    Ok(out)
}

pub(crate) async fn create_spot_view_inner(
    pool: &PgPool,
    hub: Option<&BranchEventHub>,
    till_id: Uuid,
    body: SpotViewRequest,
    actor: ActingContext,
) -> Result<HttpResponse, AppError> {
    let till = fetch_till_or_404(pool, till_id).await?;
    let viewed_at = body.viewed_at.unwrap_or_else(Utc::now);
    crate::clock::reject_if_future(viewed_at, "viewed_at")?;
    let printed_at = body
        .printed
        .then(|| body.printed_at.unwrap_or_else(Utc::now));
    let id = body.id.unwrap_or_else(Uuid::new_v4);
    let existing = fetch(pool, id).await?;
    if let Some(e) = &existing {
        if e.till_id != till_id {
            return Err(AppError::Conflict(
                "A spot view with this id belongs to another till".into(),
            ));
        }
    }
    // Live: only an open till's live report is a spot report. Replay: it was
    // open when viewed (the close is gated behind it on the device).
    if existing.is_none() && !actor.replay && till.status != "open" {
        return Err(AppError::Coded {
            status: 400,
            code: "TILL_NOT_OPEN",
            reason: "The spot report is only for an open till".into(),
        });
    }
    let mut tx = pool.begin().await?;
    let device_id = match body.device_id {
        Some(d) => {
            crate::devices::ensure_registered(&mut tx, actor.org_id, d, Some(till.branch_id), None)
                .await?
                .map(|_| d)
        }
        None => None,
    };
    let approved_by = match body.approved_by {
        Some(a) => {
            sqlx::query_scalar::<_, Uuid>("SELECT id FROM users WHERE id = $1 AND org_id = $2")
                .bind(a)
                .bind(actor.org_id)
                .fetch_optional(&mut *tx)
                .await?
        }
        None => None,
    };
    let inserted: bool = sqlx::query_scalar(
        "INSERT INTO till_spot_views (id, till_id, branch_id, viewed_by, printed, printed_at,
                                      approved_by, approval_id, device_id, viewed_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         ON CONFLICT (id) DO UPDATE SET
             printed = till_spot_views.printed OR EXCLUDED.printed,
             printed_at = COALESCE(till_spot_views.printed_at, EXCLUDED.printed_at),
             approved_by = COALESCE(till_spot_views.approved_by, EXCLUDED.approved_by),
             approval_id = COALESCE(till_spot_views.approval_id, EXCLUDED.approval_id)
         RETURNING (xmax = 0)",
    )
    .bind(id)
    .bind(till_id)
    .bind(till.branch_id)
    .bind(actor.teller_id)
    .bind(body.printed)
    .bind(printed_at)
    .bind(approved_by)
    .bind(approved_by.and(body.approval_id))
    .bind(device_id)
    .bind(viewed_at)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    let row = fetch(pool, id).await?.ok_or(AppError::Internal)?;
    if inserted || (body.printed && !existing.as_ref().is_some_and(|e| e.printed)) {
        publish(
            hub,
            till.branch_id,
            "till.spot_view",
            serde_json::json!({
                "till_id": till_id, "branch_id": till.branch_id, "spot_view_id": row.id,
                "printed": row.printed,
            }),
        );
    }
    Ok(if inserted {
        HttpResponse::Created().json(row)
    } else {
        HttpResponse::Ok().json(row)
    })
}

async fn fetch(pool: &PgPool, id: Uuid) -> Result<Option<TillSpotView>, AppError> {
    Ok(sqlx::query_as::<_, TillSpotView>(&format!(
        "SELECT {SPOT_VIEW_COLUMNS} FROM till_spot_views v WHERE v.id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

// ── GET /tills/{till_id}/spot-views ────────────────────────────

#[utoipa::path(get, path = "/tills/{till_id}/spot-views", tag = "tills",
    params(("till_id" = Uuid, Path, description = "Till ID")),
    responses((status = 200, description = "Who viewed / printed the till's spot report, oldest first", body = Vec<TillSpotView>), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn list_spot_views(
    req: HttpRequest,
    pool: crate::db::Db,
    till_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    // Permission first: the same read the till page itself needs.
    crate::authz::require::require(pool.get_ref(), &claims, Cap::TillRead, None).await?;
    let till = fetch_till_or_404(pool.get_ref(), *till_id).await?;
    require_branch_access(pool.get_ref(), &claims, till.branch_id).await?;
    let rows = spot_views_for_till(pool.get_ref(), till.id).await?;
    Ok(HttpResponse::Ok().json(rows))
}
