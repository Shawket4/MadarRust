//! The old onboarding checklist, frozen for released Flutter dashboards
//! (`dash-v0.1.0`–`0.5.x`), the only callers left.
//!
//! Readiness (`readiness.rs`, `GET /orgs/{id}/readiness`) replaced it. These
//! routes answer every org as completed with no steps, so an old build never
//! sends anyone into its wizard and its home card stays hidden. Registered in
//! `src/compat` as `onboarding_checklist`; each call counts a legacy hit.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, Utc};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    auth::guards::require_same_org, client_seen as cs, errors::AppError,
    permissions::checker::check_permission,
};

use super::handlers::extract_claims;

/// One step of the old checklist. The shim sends none.
#[derive(Debug, Serialize, ToSchema)]
pub struct OnboardingStep {
    pub key: String,
    pub done: bool,
    pub count: i64,
    pub required: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OnboardingStatus {
    pub org_id: Uuid,
    /// Always true: old builds never open their wizard.
    pub completed: bool,
    pub completed_at: Option<DateTime<Utc>>,
    pub can_complete: bool,
    pub recipe_coverage: f64,
    pub steps: Vec<OnboardingStep>,
}

fn done(org_id: Uuid) -> OnboardingStatus {
    OnboardingStatus {
        org_id,
        completed: true,
        completed_at: None,
        can_complete: true,
        recipe_coverage: 0.0,
        steps: Vec::new(),
    }
}

// ── GET /orgs/{id}/onboarding ─────────────────────────────────

#[utoipa::path(
    get,
    path = "/orgs/{id}/onboarding",
    tag = "orgs",
    params(("id" = Uuid, Path, description = "Organization ID")),
    responses((status = 200, description = "Deprecated: always completed. Use GET /orgs/{id}/readiness", body = OnboardingStatus), crate::errors::AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_onboarding(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let org_id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orgs", "read").await?;
    require_same_org(&claims, Some(org_id))?;
    cs::legacy_hit(cs::KIND_ONBOARDING_ROUTE);
    Ok(HttpResponse::Ok().json(done(org_id)))
}

// ── POST /orgs/{id}/onboarding/complete ───────────────────────

#[utoipa::path(
    post,
    path = "/orgs/{id}/onboarding/complete",
    tag = "orgs",
    params(("id" = Uuid, Path, description = "Organization ID")),
    responses((status = 200, description = "Deprecated: a no-op, always completed", body = OnboardingStatus), crate::errors::AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn complete_onboarding(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let org_id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orgs", "update").await?;
    require_same_org(&claims, Some(org_id))?;
    cs::legacy_hit(cs::KIND_ONBOARDING_ROUTE);
    Ok(HttpResponse::Ok().json(done(org_id)))
}
