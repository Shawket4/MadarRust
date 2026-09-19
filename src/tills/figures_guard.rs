//! The server half of the widened `till.cash_spot_check` (owner, 2026-09-19).
//!
//! `till.cash_spot_check` means "may see this till's money figures". The POS
//! has hidden every shift and drawer aggregate without it since v0.7.10, but
//! the two routes that HAND OUT an open till's expected figures —
//! `GET /tills/{id}/report` and `GET /tills/{id}/close-preview` — served any
//! holder of till read access, so the rule was enforced on the tablet only.
//!
//! This is the gate, and it is deliberately narrow. Three things decide it:
//!
//! 1. **A CLOSED till is never gated.** Owner decision 1: a blind teller sees
//!    the finished report after closing, on screen and printed. It is also
//!    what the device's local mirror (`ledger_ops::fill_till`, which asks for
//!    the report of EVERY till it mirrors and carries no approval) and the
//!    dashboard's history need. Only a LIVE till's figures are the secret.
//!
//! 2. **Only a client that knows how to ask is held to it** — a `pos`/`kds`
//!    build at or after [`ENFORCED_FROM`]. Everything else is untouched, full
//!    stop: pre-0.7 tablets with no `X-Madar-Client`, the v0.7.10 build now in
//!    the field (which hides the figures itself but does not yet carry an
//!    unlock on a GET), and the dashboard, which is a browser and sends no
//!    client header. `X-Madar-Device-Id` could not have served here — the POS
//!    sends it on `/auth/login` only.
//!
//! 3. **The unlock is the one that already exists.** A teller without the
//!    grant gets the figures by carrying the same one-time manager-PIN
//!    approval that `POST /tills/{id}/spot-views` already verifies, in the
//!    `X-Madar-Approval` header, checked by the same
//!    [`crate::sync::handlers::verify_approval`]: the approver is an active
//!    person of this org, holds the capability now, and is not the caller.
//!    Every accepted unlock is recorded in `approvals`, so the owner's review
//!    queue sees a read exactly as it sees a spot view.
//!
//! A read is idempotent and a mobile GET is retried, so a one-shot that dies
//! on the first retry would be a worse bug than the leak it closes. Instead an
//! accepted approval covers re-reads BY THE SAME PERSON for
//! [`REUSE_WINDOW_MINUTES`] minutes and nothing else: it cannot be passed to
//! another teller, spent on another capability, or turned into a standing
//! grant that lasts the shift. The one-per-view rule on the audit artefact
//! itself is unchanged and still strict (`till_spot_views.approval_id`).

use actix_web::HttpRequest;
use uuid::Uuid;

use crate::{
    auth::jwt::Claims,
    authz::Cap,
    devices::ClientHeader,
    errors::AppError,
    sync::handlers::{ReplayApproval, record_approval, verify_approval},
    tills::handlers::Till,
};

/// The header a POS carries a one-time manager-PIN unlock in on these GETs
/// (the JSON of a `ReplayApproval`, the shape `/sync/replay` and the
/// spot-view route already take).
pub const APPROVAL_HEADER: &str = "X-Madar-Approval";

/// The first POS build that hides the figures AND carries an unlock on these
/// two GETs. Older tablets — including v0.7.10, which is in the field — are
/// served exactly as before.
pub const ENFORCED_FROM: (u64, u64, u64) = (0, 7, 11);

/// How long an accepted unlock covers re-reads by the same person.
pub const REUSE_WINDOW_MINUTES: i64 = 5;

/// `op` recorded on the `approvals` row for each gated route.
pub const OP_REPORT: &str = "TillLiveReportRead";
pub const OP_CLOSE_PREVIEW: &str = "TillCloseFiguresRead";

/// Is this caller a POS/KDS build new enough to be held to the rule?
pub fn enforced_client(headers: &actix_web::http::header::HeaderMap) -> bool {
    let c = ClientHeader::parse(
        headers
            .get(crate::devices::CLIENT_HEADER)
            .and_then(|v| v.to_str().ok()),
    );
    matches!(c.app.as_deref(), Some("pos" | "kds"))
        && c.version.is_some_and(|v| v >= ENFORCED_FROM)
}

/// The unlock carried on the request, when the header holds one.
fn carried_approval(req: &HttpRequest) -> Option<ReplayApproval> {
    let raw = req.headers().get(APPROVAL_HEADER)?.to_str().ok()?;
    serde_json::from_str::<ReplayApproval>(raw.trim()).ok()
}

/// 403 unless the caller may see this till's live money figures.
///
/// `Ok(())` for a closed till, for any client older than [`ENFORCED_FROM`],
/// for a holder of `till.cash_spot_check` at the till's branch, and for a
/// valid one-time unlock (which is recorded). See the module docs.
pub async fn require_live_figures(
    pool: &sqlx::PgPool,
    claims: &Claims,
    req: &HttpRequest,
    till: &Till,
    op: &str,
    device: Option<Uuid>,
) -> Result<(), AppError> {
    // A finished report is owner decision 1: never gated.
    if till.status != "open" {
        return Ok(());
    }
    if !enforced_client(req.headers()) {
        return Ok(());
    }
    if crate::authz::require::can(pool, claims, Cap::TillCashSpotCheck, Some(till.branch_id)).await?
    {
        return Ok(());
    }
    let denied = || crate::authz::require::denied(Cap::TillCashSpotCheck);
    let org = claims.org_id().ok_or_else(denied)?;
    let a = carried_approval(req).ok_or_else(denied)?;
    // Already accepted: a retry, a second read of the same look, or the print
    // that follows it. It must be the SAME person, still verified and still
    // inside the short window — an id minted for someone else, or one that
    // went stale, is refused rather than silently re-used.
    let seen: Option<(bool, Uuid, bool)> = sqlx::query_as(
        "SELECT verified, subject_user_id,
                occurred_at > now() - make_interval(mins => $2)
           FROM approvals
          WHERE id = $1",
    )
    .bind(a.id)
    .bind(REUSE_WINDOW_MINUTES as i32)
    .fetch_optional(pool)
    .await?;
    if let Some((verified, subject, fresh)) = seen {
        return if verified && fresh && subject == claims.user_id() {
            Ok(())
        } else {
            Err(denied())
        };
    }
    let verified = verify_approval(pool, &a, claims.user_id(), org, None, None).await;
    if !matches!(verified, Ok(Cap::TillCashSpotCheck)) {
        return Err(denied());
    }
    record_approval(
        pool,
        &a,
        org,
        Some(till.branch_id),
        device,
        claims.user_id(),
        op,
        chrono::Utc::now(),
        &verified,
    )
    .await;
    Ok(())
}
