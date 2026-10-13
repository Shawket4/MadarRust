//! `GET /orgs/{id}/plan` and `PUT /orgs/{id}/plan/active-till`: the plan an
//! organization is on, what it allows, and (on Free) which till each branch
//! sells on. The web dashboard's Settings > Plan and super admin read these.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, Utc};
use madar_plan::Plan;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::errors::{AppError, AppErrorResponse};
use crate::models::UserRole;
use crate::orgs::handlers::extract_claims;
use crate::permissions::checker::check_permission;

#[derive(Debug, Serialize, ToSchema)]
pub struct PlanTill {
    pub device_id: Uuid,
    /// The till's short code on receipts (`T1`).
    pub code: String,
    pub label: Option<String>,
    /// When the till last rang a sale, if it ever did.
    pub last_sale_at: Option<DateTime<Utc>>,
    /// When the till last talked to the server.
    pub last_seen_at: DateTime<Utc>,
    /// `false`: the plan switched this till off (not its branch's active one).
    /// Always `true` on Paid.
    pub active: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PlanBranch {
    pub branch_id: Uuid,
    pub name: String,
    /// The branch's POS tills, the active one first.
    pub tills: Vec<PlanTill>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PlanState {
    pub org_id: Uuid,
    /// `free` | `paid`.
    #[schema(example = "free")]
    pub plan: String,
    /// `admin` | `self`.
    #[schema(example = "self")]
    pub signup_source: String,
    /// What the plan allows: `{features: [..], limits: {branches,
    /// pos_devices_per_branch, kds_devices, waiter_devices}, realtime}`
    /// (madar-plan's `Entitlements`). A limit that is `null` is unlimited.
    #[schema(value_type = Object)]
    pub entitlements: madar_plan::Entitlements,
    /// Live selling branches (not deleted, not warehouses).
    pub live_branches: i64,
    /// The plan's branch limit, `null` for unlimited.
    pub branch_limit: Option<u32>,
    /// Past the plan's branch limit (a downgraded org keeps all its branches
    /// and cannot add one).
    pub over_branch_limit: bool,
    /// POS tills the plan has switched off.
    pub inactive_tills: i64,
    pub branches: Vec<PlanBranch>,
}

pub(crate) async fn plan_state(pool: &PgPool, org: Uuid) -> Result<PlanState, AppError> {
    let (plan, source): (String, String) =
        sqlx::query_as("SELECT plan, signup_source FROM organizations WHERE id = $1")
            .bind(org)
            .fetch_optional(pool)
            .await?
            .ok_or_else(|| AppError::NotFound("Org not found".into()))?;
    let plan = Plan::parse(&plan);
    let ent = madar_plan::entitlements(plan);
    let branches: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, name FROM branches
          WHERE org_id = $1 AND deleted_at IS NULL AND kind = 'branch' ORDER BY name, id",
    )
    .bind(org)
    .fetch_all(pool)
    .await?;
    #[allow(clippy::type_complexity)]
    let tills: Vec<(
        Uuid,
        Uuid,
        String,
        Option<String>,
        bool,
        DateTime<Utc>,
        Option<DateTime<Utc>>,
    )> = sqlx::query_as(
        "SELECT d.branch_id, d.id, d.code, d.label, d.plan_inactive, d.last_seen_at,
                    (SELECT max(o.created_at) FROM orders o WHERE o.device_id = d.id)
               FROM devices d
              WHERE d.org_id = $1 AND d.kind = 'pos' AND d.retired_at IS NULL
                AND d.branch_id IS NOT NULL
              ORDER BY d.plan_inactive, d.code, d.id",
    )
    .bind(org)
    .fetch_all(pool)
    .await?;
    let live = branches.len() as i64;
    let inactive_tills = tills.iter().filter(|t| t.4).count() as i64;
    let out = branches
        .into_iter()
        .map(|(id, name)| PlanBranch {
            branch_id: id,
            name,
            tills: tills
                .iter()
                .filter(|t| t.0 == id)
                .map(|t| PlanTill {
                    device_id: t.1,
                    code: t.2.clone(),
                    label: t.3.clone(),
                    last_sale_at: t.6,
                    last_seen_at: t.5,
                    active: !t.4,
                })
                .collect(),
        })
        .collect();
    Ok(PlanState {
        org_id: org,
        plan: plan.as_str().into(),
        signup_source: source,
        entitlements: ent,
        live_branches: live,
        branch_limit: ent.limits.branches,
        over_branch_limit: ent.limits.branches.is_some_and(|max| live > max as i64),
        inactive_tills,
        branches: out,
    })
}

/// The org's plan, what it allows, and each branch's tills.
#[utoipa::path(get, path = "/orgs/{id}/plan", tag = "orgs", operation_id = "get_org_plan",
    params(("id" = Uuid, Path, description = "Organization ID")),
    responses((status = 200, description = "The plan and its limits", body = PlanState), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn get_plan(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orgs", "read").await?;
    crate::auth::guards::require_same_org(&claims, Some(*org_id))?;
    Ok(HttpResponse::Ok().json(plan_state(pool.get_ref(), *org_id).await?))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ActiveTillRequest {
    pub branch_id: Uuid,
    /// The POS till (device) that stays active in the branch.
    pub device_id: Uuid,
}

/// Choose which till a branch sells on (Free plan). The chosen till is active,
/// every other POS till of the branch is switched off. The org's owner, or a
/// super admin; a paid org has no inactive tills (409 `PLAN_NOT_FREE`).
#[utoipa::path(put, path = "/orgs/{id}/plan/active-till", tag = "orgs",
    params(("id" = Uuid, Path, description = "Organization ID")),
    request_body = ActiveTillRequest,
    responses(
        (status = 200, description = "The plan as now stored", body = PlanState),
        (status = 404, description = "That till is not in this branch"),
        (status = 409, description = "PLAN_NOT_FREE"),
        AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn put_active_till(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
    body: web::Json<ActiveTillRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    crate::auth::guards::require_same_org(&claims, Some(*org_id))?;
    if claims.role != UserRole::SuperAdmin {
        let eff =
            crate::authz::require::effective_for_claims(pool.get_ref(), &claims, None).await?;
        if !eff.owner {
            return Err(AppError::Forbidden(
                "Only the organization's owner can choose the active till".into(),
            ));
        }
    }
    let mut tx = pool.get_ref().begin().await?;
    super::set_active_till(&mut tx, *org_id, body.branch_id, body.device_id).await?;
    tx.commit().await?;
    Ok(HttpResponse::Ok().json(plan_state(pool.get_ref(), *org_id).await?))
}
