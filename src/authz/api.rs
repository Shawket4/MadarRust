//! The owner-facing permissions API (PERMISSIONS_ARCHITECTURE §4.11).
//!
//! - `GET  /authz/me`                        what I may do (UI gating)
//! - `GET  /authz/roles`                     the org's roles and their grants
//! - `POST /authz/roles`                     a custom role
//! - `PATCH /authz/roles/{id}`               rename
//! - `PUT  /authz/roles/{id}/grants`         grant / remove one capability
//! - `DELETE /authz/roles/{id}`              a custom role nobody holds
//! - `GET  /authz/users/{id}`                a person's access, per capability
//! - `PUT  /authz/users/{id}/overrides`      inherit / allow / deny one capability
//! - `PUT  /authz/users/{id}/assignments`    which roles, where
//! - `GET  /authz/explain`                   why a person can or cannot
//! - `GET|PUT /authz/policy`                 "ask a manager" per capability
//! - `GET  /authz/flags`                     offline acts accepted and flagged
//! - `POST /authz/flags/{id}/review`         acknowledge one
//!
//! Every write goes through `madar_authz::guard`: no self-edit, dominate the
//! target, hold what you grant, owners protected, core grants kept.

use std::collections::BTreeMap;

use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::guard::{self as g, GuardError};
use super::{
    Cap, CapSet, EffectiveSet, Kinds, Limits, RoleKind, Scope, Tier, is_core_for, resolve,
};
use crate::auth::jwt::Claims;
use crate::errors::{AppError, AppErrorResponse};
use crate::models::UserRole;

// ── wire types ──────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, ToSchema, Clone, Copy, Debug, Default, PartialEq)]
pub struct LimitsView {
    /// Money, minor units.
    pub max_amount: Option<i64>,
    /// Basis points (1000 = 10%).
    pub max_percent: Option<i64>,
    /// Stock value, minor units.
    pub max_value: Option<i64>,
    /// How old the thing acted on may be, in minutes.
    #[serde(default)]
    pub max_age_minutes: Option<i64>,
    /// Only the person's own work. Absent means unrestricted, so a dashboard
    /// that predates the field keeps meaning what it always meant.
    #[serde(default)]
    pub own: Option<bool>,
}

impl From<Limits> for LimitsView {
    fn from(l: Limits) -> Self {
        LimitsView {
            max_amount: l.max_amount,
            max_percent: l.max_percent,
            max_value: l.max_value,
            max_age_minutes: l.max_age_minutes,
            own: l.own.then_some(true),
        }
    }
}

impl From<LimitsView> for Limits {
    fn from(l: LimitsView) -> Self {
        Limits {
            max_amount: l.max_amount,
            max_percent: l.max_percent,
            max_value: l.max_value,
            max_age_minutes: l.max_age_minutes,
            own: l.own.unwrap_or(false),
        }
    }
}

/// What the signed-in person may do. The dashboard and POS gate on this.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct MyAuthz {
    pub user_id: Uuid,
    pub branch_id: Option<Uuid>,
    pub epoch: i64,
    pub spec_version: u32,
    pub owner: bool,
    pub platform: bool,
    /// Role kinds held here (org_admin, branch_manager, teller, waiter, kitchen).
    pub role_kinds: Vec<String>,
    /// Capability keys held.
    pub capabilities: Vec<String>,
    /// Capabilities not held that show "ask a manager" instead of nothing.
    pub ask_manager: Vec<String>,
    /// Limits on held capabilities, by key; absent = unlimited.
    pub limits: BTreeMap<String, LimitsView>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BranchQuery {
    pub branch_id: Option<Uuid>,
}

#[derive(Serialize, Deserialize, ToSchema, Clone)]
pub struct GrantView {
    pub capability: String,
    pub limits: LimitsView,
    /// "template" or "custom" (an owner edited it).
    pub source: String,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct RoleView {
    pub id: Uuid,
    pub key: String,
    pub name_en: String,
    pub name_ar: String,
    /// What the role behaves like on older tablets, and its core grants.
    pub kind: String,
    pub is_system: bool,
    /// The owner role holds everything and is not editable.
    pub editable: bool,
    pub members: i64,
    pub grants: Vec<GrantView>,
}

#[derive(Deserialize, ToSchema)]
pub struct CreateRoleRequest {
    pub name_en: String,
    pub name_ar: String,
    /// branch_manager | teller | waiter | kitchen
    pub kind: String,
    /// Start from this role's grants; otherwise from the default template.
    pub copy_from: Option<Uuid>,
}

#[derive(Deserialize, ToSchema)]
pub struct RenameRoleRequest {
    pub name_en: Option<String>,
    pub name_ar: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct SetGrantRequest {
    pub capability: String,
    pub granted: bool,
    pub limits: Option<LimitsView>,
}

#[derive(Serialize, Deserialize, ToSchema, Clone)]
pub struct AssignmentView {
    pub id: Uuid,
    pub role_id: Uuid,
    pub role_name_en: String,
    pub role_name_ar: String,
    pub kind: String,
    pub all_branches: bool,
    pub branch_ids: Vec<Uuid>,
}

#[derive(Serialize, Deserialize, ToSchema, Clone)]
pub struct OverrideView {
    pub effect: String,
    pub branch_id: Option<Uuid>,
    pub limits: Option<LimitsView>,
    pub reason: Option<String>,
    pub valid_to: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct CapabilityAccess {
    pub capability: String,
    /// Held here after everything.
    pub effective: bool,
    /// Where the answer comes from: owner | core | allow | deny | role | none.
    pub source: String,
    /// Role names granting it (for "Inherits from …").
    pub from_roles: Vec<String>,
    pub overrides: Vec<OverrideView>,
    pub limits: Option<LimitsView>,
    /// Can the caller change this row for this person?
    pub editable: bool,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct UserAccess {
    pub user_id: Uuid,
    pub name: String,
    pub is_owner: bool,
    pub branch_id: Option<Uuid>,
    /// Can the caller edit this person's access at all?
    pub can_edit: bool,
    /// Why not, when not (self | owner | not_dominant | not_above | missing_authority).
    pub locked_reason: Option<String>,
    pub assignments: Vec<AssignmentView>,
    pub capabilities: Vec<CapabilityAccess>,
}

#[derive(Deserialize, ToSchema)]
pub struct SetOverrideRequest {
    pub capability: String,
    /// inherit | allow | deny
    pub effect: String,
    pub branch_id: Option<Uuid>,
    pub limits: Option<LimitsView>,
    pub reason: Option<String>,
    pub valid_to: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Deserialize, ToSchema)]
pub struct AssignmentInput {
    pub role_id: Uuid,
    pub all_branches: bool,
    #[serde(default)]
    pub branch_ids: Vec<Uuid>,
}

#[derive(Deserialize, ToSchema)]
pub struct SetAssignmentsRequest {
    pub assignments: Vec<AssignmentInput>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ExplainQuery {
    pub user_id: Uuid,
    pub capability: String,
    pub branch_id: Option<Uuid>,
}

#[derive(Serialize, Deserialize, ToSchema, Default)]
pub struct ExplainStep {
    /// owner | inactive | assignment | core | override_allow | override_deny |
    /// protected | not_held | limit | ask_manager
    pub kind: String,
    pub role_name: Option<String>,
    pub branch_id: Option<Uuid>,
    pub detail: Option<String>,
    /// For an assignment step: does the assignment cover the branch asked about?
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applies_here: Option<bool>,
    /// For an assignment step: does the role grant the capability?
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grants: Option<bool>,
    /// The role's Arabic name, beside `role_name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_name_ar: Option<String>,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct Explanation {
    pub capability: String,
    pub label_en: String,
    pub label_ar: String,
    pub effective: bool,
    pub ask_manager: bool,
    pub steps: Vec<ExplainStep>,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct PolicyEntry {
    pub capability: String,
    pub ask_manager: bool,
}

/// One offline act that was accepted despite failing the permission re-check
/// (PERMISSIONS_ARCHITECTURE §4.4.5). The money already moved; this is the
/// owner's notice, not a rollback.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct ReplayFlag {
    pub id: i64,
    pub branch_id: Option<Uuid>,
    /// The replayed op, e.g. `CashMovement`.
    pub op: String,
    pub author_id: Uuid,
    pub author_name: Option<String>,
    /// The `resource:action` cell the author did not hold.
    pub capability: String,
    /// `stale_snapshot` — they held it when they acted and the device had not
    /// heard the revocation yet. `unauthorized_offline` — nothing explains it.
    /// `pin_wrong_branch` — their correct PIN was typed at a branch they may
    /// not sign in at (`op` = `PinSignIn`, `details.attempts` counts the tries).
    pub reason: String,
    /// When the act happened on the device.
    pub occurred_at: chrono::DateTime<chrono::Utc>,
    /// When it reached us. The gap is the offline window.
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub reviewed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub reviewed_by: Option<Uuid>,
}

#[derive(Deserialize, IntoParams)]
pub struct FlagQuery {
    /// Include flags already reviewed. Default false: the queue is what is left
    /// to look at.
    #[serde(default)]
    pub include_reviewed: bool,
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn claims_of(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

fn org_of(req: &HttpRequest, claims: &Claims) -> Result<Uuid, AppError> {
    claims
        .scope_org(crate::auth::middleware::header_org_id(req))
        .ok_or_else(|| AppError::Forbidden("No organization selected".into()))
}

fn cap_of(key: &str) -> Result<Cap, AppError> {
    let cap = Cap::from_key(key)
        .ok_or_else(|| AppError::BadRequest(format!("Unknown capability {key}")))?;
    if cap.meta().tier == Tier::Legacy {
        return Err(AppError::BadRequest(format!(
            "{key} is kept for older tablets and cannot be changed"
        )));
    }
    Ok(cap)
}

fn guard_err(e: GuardError) -> AppError {
    let msg = match &e {
        GuardError::SelfEdit => "You cannot change your own access".to_string(),
        GuardError::MissingAuthority { cap } => {
            format!("You don't have permission to change access ({cap})")
        }
        GuardError::NotDominant { caps } => format!(
            "This person can do things you can't ({}), so you can't change their access",
            caps.join(", ")
        ),
        GuardError::NotHeld { cap } => format!("You can only grant what you hold yourself ({cap})"),
        GuardError::LimitAbove { cap } => {
            format!("You can't set a limit above your own ({cap})")
        }
        GuardError::CoreRemoval { cap } => {
            format!("{cap} is always on for this role and can't be removed")
        }
        GuardError::OwnerProtected => "Only an owner can change an owner's access".to_string(),
        GuardError::NotAbove => "You can only change people whose role is below yours".to_string(),
    };
    AppError::Forbidden(msg)
}

fn kind_str(k: RoleKind) -> String {
    k.as_str().to_string()
}

async fn actor_eff(
    pool: &sqlx::PgPool,
    claims: &Claims,
    branch: Option<Uuid>,
) -> Result<EffectiveSet, AppError> {
    super::require::effective_for_claims(pool, claims, branch).await
}

fn actor_id(claims: &Claims) -> String {
    claims.user_id().to_string()
}

async fn target_row(
    pool: &sqlx::PgPool,
    org: Uuid,
    user_id: Uuid,
) -> Result<(String, bool), AppError> {
    sqlx::query_as(
        "SELECT name, is_owner FROM users WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL",
    )
    .bind(user_id)
    .bind(org)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("User not found".into()))
}

fn scope_of(b: &Option<String>) -> Scope<'_> {
    match b {
        Some(b) => Scope::Branch(b),
        None => Scope::Anywhere,
    }
}

// ── /authz/me ───────────────────────────────────────────────────────────────

#[utoipa::path(get, path = "/authz/me", tag = "authz", params(BranchQuery),
    responses((status = 200, description = "The signed-in person's effective capabilities", body = MyAuthz), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn get_my_authz(
    req: HttpRequest,
    pool: crate::db::Db,
    q: web::Query<BranchQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = claims_of(&req)?;
    let branch = q.branch_id.or_else(|| claims.branch_id());
    let eff = actor_eff(pool.get_ref(), &claims, branch).await?;
    let mut conn = pool.acquire().await?;
    let epoch = super::load::epoch_of(&mut conn, claims.user_id()).await?;
    Ok(HttpResponse::Ok().json(my_authz(
        claims.user_id(),
        branch,
        epoch,
        &eff,
        claims.role == UserRole::SuperAdmin,
    )))
}

pub fn my_authz(
    user_id: Uuid,
    branch_id: Option<Uuid>,
    epoch: i64,
    eff: &EffectiveSet,
    platform: bool,
) -> MyAuthz {
    MyAuthz {
        user_id,
        branch_id,
        epoch,
        spec_version: super::SPEC_VERSION,
        owner: eff.owner,
        platform,
        role_kinds: eff.kinds.iter().map(kind_str).collect(),
        capabilities: eff
            .caps
            .iter()
            .filter(|c| c.meta().tier != Tier::Legacy)
            .map(|c| c.key().to_string())
            .collect(),
        ask_manager: eff
            .ask_manager
            .minus(&eff.caps)
            .iter()
            .map(|c| c.key().to_string())
            .collect(),
        limits: eff
            .limits
            .iter()
            .filter_map(|(id, l)| Cap::from_id(*id).map(|c| (c.key().to_string(), (*l).into())))
            .collect(),
    }
}

// ── roles ───────────────────────────────────────────────────────────────────

async fn load_roles(pool: &sqlx::PgPool, org: Uuid) -> Result<Vec<RoleView>, AppError> {
    #[allow(clippy::type_complexity)]
    let roles: Vec<(Uuid, String, String, String, String, bool, i64)> = sqlx::query_as(
        "SELECT r.id, r.key, r.name_en, r.name_ar, r.kind::text, r.is_system,
                (SELECT COUNT(DISTINCT ra.user_id) FROM role_assignments ra
                   JOIN users u ON u.id = ra.user_id AND u.deleted_at IS NULL
                  WHERE ra.org_role_id = r.id AND ra.revoked_at IS NULL)
           FROM org_roles r
          WHERE r.org_id = $1 AND r.deleted_at IS NULL
          ORDER BY r.is_system DESC,
                   array_position(ARRAY['org_admin','branch_manager','teller','waiter','kitchen'], r.kind::text),
                   r.name_en",
    )
    .bind(org)
    .fetch_all(pool)
    .await?;
    let grants: Vec<(Uuid, i16, serde_json::Value, String)> = sqlx::query_as(
        "SELECT g.org_role_id, g.capability_id, g.limits, g.source
           FROM org_role_grants g JOIN org_roles r ON r.id = g.org_role_id
          WHERE r.org_id = $1 AND r.deleted_at IS NULL",
    )
    .bind(org)
    .fetch_all(pool)
    .await?;
    Ok(roles
        .into_iter()
        .map(|(id, key, en, ar, kind, is_system, members)| RoleView {
            editable: !(is_system && kind == "org_admin"),
            grants: grants
                .iter()
                .filter(|g| g.0 == id)
                .filter_map(|(_, cap, limits, source)| {
                    let c = Cap::from_id(*cap as u16)?;
                    (c.meta().tier != Tier::Legacy).then(|| GrantView {
                        capability: c.key().to_string(),
                        limits: serde_json::from_value::<Limits>(limits.clone())
                            .unwrap_or_default()
                            .into(),
                        source: source.clone(),
                    })
                })
                .collect(),
            id,
            key,
            name_en: en,
            name_ar: ar,
            kind,
            is_system,
            members,
        })
        .collect())
}

#[utoipa::path(get, path = "/authz/roles", tag = "authz",
    responses((status = 200, description = "The organization's roles", body = Vec<RoleView>), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn list_roles(req: HttpRequest, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let claims = claims_of(&req)?;
    let org = org_of(&req, &claims)?;
    let eff = actor_eff(pool.get_ref(), &claims, None).await?;
    if !(eff.can(Cap::StaffPermissionsRead) || eff.can(Cap::StaffRolesManage)) {
        return Err(super::require::denied(Cap::StaffPermissionsRead));
    }
    Ok(HttpResponse::Ok().json(load_roles(pool.get_ref(), org).await?))
}

async fn one_role(pool: &sqlx::PgPool, org: Uuid, id: Uuid) -> Result<RoleView, AppError> {
    load_roles(pool, org)
        .await?
        .into_iter()
        .find(|r| r.id == id)
        .ok_or_else(|| AppError::NotFound("Role not found".into()))
}

fn role_caps(r: &RoleView) -> CapSet {
    r.grants
        .iter()
        .filter_map(|g| Cap::from_key(&g.capability))
        .collect()
}

#[utoipa::path(post, path = "/authz/roles", tag = "authz", request_body = CreateRoleRequest,
    responses((status = 201, description = "Role created", body = RoleView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn create_role(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CreateRoleRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = claims_of(&req)?;
    let org = org_of(&req, &claims)?;
    // Permission before validation (route guard): a caller who may not manage roles
    // learns nothing from how their request is malformed or what exists.
    super::require::require(pool.get_ref(), &claims, Cap::StaffRolesManage, None).await?;
    let kind = RoleKind::parse(&body.kind)
        .filter(|k| *k != RoleKind::OrgAdmin)
        .ok_or_else(|| {
            AppError::BadRequest("kind must be branch_manager, teller, waiter or kitchen".into())
        })?;
    let (en, ar) = (body.name_en.trim(), body.name_ar.trim());
    if en.is_empty() || ar.is_empty() || en.len() > 80 || ar.len() > 80 {
        return Err(AppError::BadRequest(
            "A role needs an English and an Arabic name".into(),
        ));
    }
    let actor = actor_eff(pool.get_ref(), &claims, None).await?;
    let grants: CapSet = match body.copy_from {
        Some(src) => role_caps(&one_role(pool.get_ref(), org, src).await?),
        None => super::template_grants("restaurant", kind)
            .unwrap_or_default()
            .minus(&super::core_set(kind)),
    };
    let grants: CapSet = grants
        .iter()
        .filter(|c| c.meta().tier != Tier::Legacy)
        .collect();
    g::may_edit_role(&actor, Kinds(kind.bit()), &CapSet::EMPTY, &grants).map_err(guard_err)?;

    let slug: String = en
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(40)
        .collect();
    let key = format!(
        "{}_{}",
        slug.trim_matches('_'),
        &Uuid::new_v4().simple().to_string()[..6]
    );
    let mut tx = pool.begin().await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO org_roles (org_id, key, name_en, name_ar, kind, is_system, created_by)
         VALUES ($1, $2, $3, $4, $5::user_role, false, $6) RETURNING id",
    )
    .bind(org)
    .bind(&key)
    .bind(en)
    .bind(ar)
    .bind(kind.as_str())
    .bind(claims.user_id())
    .fetch_one(&mut *tx)
    .await?;
    for c in grants.iter() {
        sqlx::query(
            "INSERT INTO org_role_grants (org_role_id, org_id, capability_id, source, updated_by)
             VALUES ($1, $2, $3, 'custom', $4)",
        )
        .bind(id)
        .bind(org)
        .bind(c.id() as i16)
        .bind(claims.user_id())
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(HttpResponse::Created().json(one_role(pool.get_ref(), org, id).await?))
}

#[utoipa::path(patch, path = "/authz/roles/{id}", tag = "authz", request_body = RenameRoleRequest,
    params(("id" = Uuid, Path, description = "Role ID")),
    responses((status = 200, description = "Role renamed", body = RoleView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn rename_role(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<RenameRoleRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = claims_of(&req)?;
    let org = org_of(&req, &claims)?;
    let actor = actor_eff(pool.get_ref(), &claims, None).await?;
    if !actor.can(Cap::StaffRolesManage) {
        return Err(super::require::denied(Cap::StaffRolesManage));
    }
    let role = one_role(pool.get_ref(), org, *id).await?;
    let en = body
        .name_en
        .as_deref()
        .map(str::trim)
        .unwrap_or(&role.name_en)
        .to_string();
    let ar = body
        .name_ar
        .as_deref()
        .map(str::trim)
        .unwrap_or(&role.name_ar)
        .to_string();
    if en.is_empty() || ar.is_empty() {
        return Err(AppError::BadRequest(
            "A role needs an English and an Arabic name".into(),
        ));
    }
    sqlx::query(
        "UPDATE org_roles SET name_en = $2, name_ar = $3, updated_at = now() WHERE id = $1",
    )
    .bind(*id)
    .bind(&en)
    .bind(&ar)
    .execute(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(one_role(pool.get_ref(), org, *id).await?))
}

#[utoipa::path(put, path = "/authz/roles/{id}/grants", tag = "authz", request_body = SetGrantRequest,
    params(("id" = Uuid, Path, description = "Role ID")),
    responses((status = 200, description = "Grant changed", body = RoleView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn set_role_grant(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<SetGrantRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = claims_of(&req)?;
    let org = org_of(&req, &claims)?;
    // Permission before validation (route guard): a caller who may not manage roles
    // learns nothing from how their request is malformed or what exists.
    super::require::require(pool.get_ref(), &claims, Cap::StaffRolesManage, None).await?;
    let cap = cap_of(&body.capability)?;
    let role = one_role(pool.get_ref(), org, *id).await?;
    if !role.editable {
        return Err(AppError::Conflict(
            "The owner role holds everything and cannot be edited".into(),
        ));
    }
    let kind = RoleKind::parse(&role.kind).ok_or(AppError::Internal)?;
    let actor = actor_eff(pool.get_ref(), &claims, None).await?;
    let before = role_caps(&role).union(&super::core_set(kind));
    let mut after = before;
    if body.granted {
        after.insert(cap);
    } else {
        after.remove(cap);
    }
    g::may_edit_role(&actor, Kinds(kind.bit()), &before, &after).map_err(guard_err)?;
    if body.granted {
        let limits: Limits = body.limits.map(Into::into).unwrap_or_default();
        let limits = limits.restricted_to(cap.meta().limits);
        if !limits.within(&actor.limits_of(cap)) {
            return Err(guard_err(GuardError::LimitAbove {
                cap: cap.key().into(),
            }));
        }
        if is_core_for(cap, Kinds(kind.bit())) && limits.is_unlimited() {
            // Always on already; nothing to store.
            return Ok(HttpResponse::Ok().json(role));
        }
        sqlx::query(
            "INSERT INTO org_role_grants (org_role_id, org_id, capability_id, limits, source, updated_by)
             VALUES ($1, $2, $3, $4, 'custom', $5)
             ON CONFLICT (org_role_id, capability_id)
             DO UPDATE SET limits = EXCLUDED.limits, source = 'custom', updated_by = EXCLUDED.updated_by, updated_at = now()",
        )
        .bind(*id)
        .bind(org)
        .bind(cap.id() as i16)
        .bind(serde_json::to_value(limits).unwrap_or_default())
        .bind(claims.user_id())
        .execute(pool.get_ref())
        .await?;
    } else {
        sqlx::query("DELETE FROM org_role_grants WHERE org_role_id = $1 AND capability_id = $2")
            .bind(*id)
            .bind(cap.id() as i16)
            .execute(pool.get_ref())
            .await?;
    }
    Ok(HttpResponse::Ok().json(one_role(pool.get_ref(), org, *id).await?))
}

#[utoipa::path(delete, path = "/authz/roles/{id}", tag = "authz",
    params(("id" = Uuid, Path, description = "Role ID")),
    responses((status = 204, description = "Role deleted"), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn delete_role(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = claims_of(&req)?;
    let org = org_of(&req, &claims)?;
    let actor = actor_eff(pool.get_ref(), &claims, None).await?;
    if !actor.can(Cap::StaffRolesManage) {
        return Err(super::require::denied(Cap::StaffRolesManage));
    }
    let role = one_role(pool.get_ref(), org, *id).await?;
    if role.is_system {
        return Err(AppError::Conflict(
            "Built-in roles cannot be deleted".into(),
        ));
    }
    if role.members > 0 {
        return Err(AppError::Conflict(
            "People still hold this role. Move them to another role first.".into(),
        ));
    }
    sqlx::query("UPDATE org_roles SET deleted_at = now() WHERE id = $1")
        .bind(*id)
        .execute(pool.get_ref())
        .await?;
    Ok(HttpResponse::NoContent().finish())
}

// ── a person's access ───────────────────────────────────────────────────────

#[utoipa::path(get, path = "/authz/users/{id}", tag = "authz", params(("id" = Uuid, Path, description = "User ID"), BranchQuery),
    responses((status = 200, description = "A person's access, per capability", body = UserAccess), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn user_access(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    q: web::Query<BranchQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = claims_of(&req)?;
    let org = org_of(&req, &claims)?;
    let actor = actor_eff(pool.get_ref(), &claims, q.branch_id).await?;
    if claims.user_id() != *id
        && !(actor.can(Cap::StaffPermissionsRead) || actor.can(Cap::StaffPermissionsEdit))
    {
        return Err(super::require::denied(Cap::StaffPermissionsRead));
    }
    let (name, is_owner) = target_row(pool.get_ref(), org, *id).await?;
    let roles = load_roles(pool.get_ref(), org).await?;
    let mut conn = pool.acquire().await?;
    let loaded = super::load::load(&mut conn, *id)
        .await?
        .ok_or_else(|| AppError::NotFound("User not found".into()))?;
    let b = q.branch_id.map(|b| b.to_string());
    let now = chrono::Utc::now().timestamp();
    let eff = resolve(&loaded.principal, scope_of(&b), now, &loaded.policy);

    let role_name = |rid: &str| {
        roles
            .iter()
            .find(|r| r.id.to_string() == rid)
            .map(|r| (r.name_en.clone(), r.name_ar.clone()))
            .unwrap_or_default()
    };
    let assignment_rows: Vec<(Uuid, Uuid, String, bool)> = sqlx::query_as(
        "SELECT ra.id, ra.org_role_id, r.kind::text, ra.all_branches
           FROM role_assignments ra JOIN org_roles r ON r.id = ra.org_role_id
          WHERE ra.user_id = $1 AND ra.revoked_at IS NULL ORDER BY ra.created_at",
    )
    .bind(*id)
    .fetch_all(&mut *conn)
    .await?;
    let branch_rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT rab.assignment_id, rab.branch_id FROM role_assignment_branches rab
           JOIN role_assignments ra ON ra.id = rab.assignment_id
          WHERE ra.user_id = $1 AND ra.revoked_at IS NULL",
    )
    .bind(*id)
    .fetch_all(&mut *conn)
    .await?;
    let assignments: Vec<AssignmentView> = assignment_rows
        .into_iter()
        .map(|(aid, rid, kind, all)| {
            let (en, ar) = role_name(&rid.to_string());
            AssignmentView {
                id: aid,
                role_id: rid,
                role_name_en: en,
                role_name_ar: ar,
                kind,
                all_branches: all,
                branch_ids: branch_rows
                    .iter()
                    .filter(|(a, _)| *a == aid)
                    .map(|(_, b)| *b)
                    .collect(),
            }
        })
        .collect();

    let can_edit = if claims.user_id() == *id {
        Err("self")
    } else {
        g::may_touch(&actor, &actor_id(&claims), &eff, &id.to_string()).map_err(|e| match e {
            GuardError::OwnerProtected => "owner",
            GuardError::NotDominant { .. } => "not_dominant",
            GuardError::NotAbove => "not_above",
            _ => "missing_authority",
        })
    }
    .and_then(|_| {
        if actor.can(Cap::StaffPermissionsEdit) {
            Ok(())
        } else {
            Err("missing_authority")
        }
    });

    let capabilities = super::CAPS
        .iter()
        .filter(|m| m.tier != Tier::Legacy)
        .map(|m| {
            let c = m.cap;
            let covering: Vec<&super::AssignmentDef> = loaded
                .principal
                .assignments
                .iter()
                .filter(|a| match &b {
                    Some(b) => a.all_branches || a.branches.iter().any(|x| x == b),
                    None => true,
                })
                .collect();
            let from_roles: Vec<String> = covering
                .iter()
                .filter(|a| a.role.grants.contains(c))
                .map(|a| role_name(&a.role.id).0)
                .collect();
            let overrides: Vec<OverrideView> = loaded
                .principal
                .overrides
                .iter()
                .filter(|o| o.cap == c.id())
                .map(|o| OverrideView {
                    effect: if o.allow { "allow" } else { "deny" }.into(),
                    branch_id: o.branch.as_deref().and_then(|x| Uuid::parse_str(x).ok()),
                    limits: o.limits.map(Into::into),
                    reason: None,
                    valid_to: o
                        .valid_to
                        .and_then(|t| chrono::DateTime::from_timestamp(t, 0)),
                })
                .collect();
            let is_core = is_core_for(c, eff.kinds);
            let source = if eff.owner && super::owner_set().contains(c) {
                "owner"
            } else if is_core {
                "core"
            } else if let Some(o) = overrides
                .iter()
                .find(|o| o.branch_id.map(|x| x.to_string()) == b || o.branch_id.is_none())
            {
                if o.effect == "allow" { "allow" } else { "deny" }
            } else if !from_roles.is_empty() {
                "role"
            } else {
                "none"
            };
            let row_editable = can_edit.is_ok()
                && !is_core
                && !(eff.owner && m.protected)
                && (actor.can(c) || eff.can(c));
            CapabilityAccess {
                capability: c.key().to_string(),
                effective: eff.can(c),
                source: source.into(),
                from_roles,
                overrides,
                limits: eff.limits.get(&c.id()).map(|l| (*l).into()),
                editable: row_editable,
            }
        })
        .collect();

    Ok(HttpResponse::Ok().json(UserAccess {
        user_id: *id,
        name,
        is_owner,
        branch_id: q.branch_id,
        can_edit: can_edit.is_ok(),
        locked_reason: can_edit.err().map(str::to_string),
        assignments,
        capabilities,
    }))
}

#[utoipa::path(put, path = "/authz/users/{id}/overrides", tag = "authz", request_body = SetOverrideRequest,
    params(("id" = Uuid, Path, description = "User ID")),
    responses((status = 200, description = "Override set; the person's access now", body = UserAccess), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn set_override(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<SetOverrideRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = claims_of(&req)?;
    let org = org_of(&req, &claims)?;
    // Permission before validation (route guard): a caller who may not edit permissions
    // learns nothing from how their request is malformed or what exists.
    super::require::require(pool.get_ref(), &claims, Cap::StaffPermissionsEdit, None).await?;
    let cap = cap_of(&body.capability)?;
    target_row(pool.get_ref(), org, *id).await?;
    if let Some(b) = body.branch_id {
        branch_in_org(pool.get_ref(), org, b).await?;
    }
    let actor = actor_eff(pool.get_ref(), &claims, body.branch_id).await?;
    let target = super::require::effective(pool.get_ref(), *id, body.branch_id).await?;
    let limits: Option<Limits> = body
        .limits
        .map(|l| Limits::from(l).restricted_to(cap.meta().limits));
    match body.effect.as_str() {
        "inherit" => {
            g::may_touch(&actor, &actor_id(&claims), &target, &id.to_string())
                .map_err(guard_err)?;
            if !actor.can(Cap::StaffPermissionsEdit) {
                return Err(super::require::denied(Cap::StaffPermissionsEdit));
            }
            // Removing an allow narrows; removing a deny re-exposes the role grant,
            // which the actor must hold.
            let was_deny: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM user_overrides WHERE user_id = $1 AND capability_id = $2
                   AND branch_id IS NOT DISTINCT FROM $3 AND revoked_at IS NULL AND effect = 'deny')",
            )
            .bind(*id)
            .bind(cap.id() as i16)
            .bind(body.branch_id)
            .fetch_one(pool.get_ref())
            .await?;
            if was_deny && !actor.can(cap) {
                return Err(guard_err(GuardError::NotHeld {
                    cap: cap.key().into(),
                }));
            }
        }
        "allow" | "deny" => {
            let allow = body.effect == "allow";
            if matches!(cap.meta().risk, super::Risk::Money | super::Risk::Admin)
                && body.reason.as_deref().is_none_or(|r| r.trim().is_empty())
            {
                return Err(AppError::BadRequest(
                    "Say why: a reason is required for money and admin permissions".into(),
                ));
            }
            g::may_set_override(
                &actor,
                &actor_id(&claims),
                &target,
                &id.to_string(),
                target.kinds,
                cap,
                allow,
                limits.as_ref(),
            )
            .map_err(guard_err)?;
        }
        _ => {
            return Err(AppError::BadRequest(
                "effect must be inherit, allow or deny".into(),
            ));
        }
    }

    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE user_overrides SET revoked_at = now()
          WHERE user_id = $1 AND capability_id = $2 AND branch_id IS NOT DISTINCT FROM $3 AND revoked_at IS NULL",
    )
    .bind(*id)
    .bind(cap.id() as i16)
    .bind(body.branch_id)
    .execute(&mut *tx)
    .await?;
    if body.effect != "inherit" {
        sqlx::query(
            "INSERT INTO user_overrides (org_id, user_id, capability_id, effect, branch_id, limits, valid_to, reason, granted_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(org)
        .bind(*id)
        .bind(cap.id() as i16)
        .bind(&body.effect)
        .bind(body.branch_id)
        .bind(limits.map(|l| serde_json::to_value(l).unwrap_or_default()))
        .bind(body.valid_to)
        .bind(body.reason.as_deref().map(str::trim))
        .bind(claims.user_id())
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    user_access(
        req,
        pool,
        id,
        web::Query(BranchQuery {
            branch_id: body.branch_id,
        }),
    )
    .await
}

async fn branch_in_org(pool: &sqlx::PgPool, org: Uuid, branch: Uuid) -> Result<(), AppError> {
    let ok: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM branches WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL)",
    )
    .bind(branch)
    .bind(org)
    .fetch_one(pool)
    .await?;
    if ok {
        Ok(())
    } else {
        Err(AppError::BadRequest("Unknown branch".into()))
    }
}

#[utoipa::path(put, path = "/authz/users/{id}/assignments", tag = "authz", request_body = SetAssignmentsRequest,
    params(("id" = Uuid, Path, description = "User ID")),
    responses((status = 200, description = "Assignments replaced; the person's access now", body = UserAccess), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn set_assignments(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<SetAssignmentsRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = claims_of(&req)?;
    let org = org_of(&req, &claims)?;
    // Permission before validation (route guard): a caller who may not edit people
    // learns nothing from how their request is malformed or what exists.
    super::require::require(pool.get_ref(), &claims, Cap::StaffUsersEdit, None).await?;
    let (_, target_is_owner) = target_row(pool.get_ref(), org, *id).await?;
    if body.assignments.is_empty() {
        return Err(AppError::BadRequest(
            "A person needs at least one role".into(),
        ));
    }
    let actor = actor_eff(pool.get_ref(), &claims, None).await?;
    let target = super::require::effective(pool.get_ref(), *id, None).await?;
    let roles = load_roles(pool.get_ref(), org).await?;
    let mut kinds = vec![];
    for a in &body.assignments {
        let role = roles
            .iter()
            .find(|r| r.id == a.role_id)
            .ok_or_else(|| AppError::BadRequest("Unknown role".into()))?;
        let kind = RoleKind::parse(&role.kind).ok_or(AppError::Internal)?;
        g::may_give_kind(&actor, kind).map_err(guard_err)?;
        let caps = if kind == RoleKind::OrgAdmin {
            super::owner_set()
        } else {
            role_caps(role).union(&super::core_set(kind))
        };
        g::may_assign(&actor, &actor_id(&claims), &target, &id.to_string(), &caps)
            .map_err(guard_err)?;
        if !a.all_branches && a.branch_ids.is_empty() {
            return Err(AppError::BadRequest(
                "Choose the branches, or all branches".into(),
            ));
        }
        for b in &a.branch_ids {
            branch_in_org(pool.get_ref(), org, *b).await?;
        }
        kinds.push(kind);
    }
    let stays_owner = kinds.contains(&RoleKind::OrgAdmin);
    let mut tx = pool.begin().await?;
    if target_is_owner
        && !stays_owner
        && crate::permissions::guard::is_last_active_owner(&mut tx, org, *id).await?
    {
        return Err(crate::permissions::guard::last_owner_error());
    }
    sqlx::query(
        "UPDATE role_assignments SET revoked_at = now() WHERE user_id = $1 AND revoked_at IS NULL",
    )
    .bind(*id)
    .execute(&mut *tx)
    .await?;
    for a in &body.assignments {
        let aid: Uuid = sqlx::query_scalar(
            "INSERT INTO role_assignments (org_id, user_id, org_role_id, all_branches, granted_by, managed)
             VALUES ($1, $2, $3, $4, $5, false) RETURNING id",
        )
        .bind(org)
        .bind(*id)
        .bind(a.role_id)
        .bind(a.all_branches)
        .bind(claims.user_id())
        .fetch_one(&mut *tx)
        .await?;
        if !a.all_branches {
            for b in &a.branch_ids {
                sqlx::query(
                    "INSERT INTO role_assignment_branches (assignment_id, branch_id, org_id) VALUES ($1, $2, $3)
                     ON CONFLICT DO NOTHING",
                )
                .bind(aid)
                .bind(*b)
                .bind(org)
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    // Project the branch allow-list back onto `user_branch_assignments`, which
    // is now only that: a projection for pre-0.8 readers (the offline bundle,
    // schedules, attendance). Role assignments are the truth
    // (POS_SIGNIN_OVERHAUL.md §5.2 "A"). The legacy table's own convention is
    // that no rows means org-wide for a till worker, so an `all_branches`
    // assignment projects to no rows — which is exactly how it reads back in.
    let projected: Vec<Uuid> = if body.assignments.iter().any(|a| a.all_branches) {
        vec![]
    } else {
        let mut v: Vec<Uuid> = vec![];
        for a in &body.assignments {
            for b in &a.branch_ids {
                if !v.contains(b) {
                    v.push(*b);
                }
            }
        }
        v
    };
    sqlx::query(
        "DELETE FROM user_branch_assignments WHERE user_id = $1 AND NOT (branch_id = ANY($2))",
    )
    .bind(*id)
    .bind(&projected)
    .execute(&mut *tx)
    .await?;
    for b in &projected {
        sqlx::query(
            "INSERT INTO user_branch_assignments (user_id, branch_id, assigned_by)
             VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(*id)
        .bind(*b)
        .bind(claims.user_id())
        .execute(&mut *tx)
        .await?;
    }

    // The label older tablets and the JWT read: the most senior kind held.
    let primary = [
        RoleKind::OrgAdmin,
        RoleKind::BranchManager,
        RoleKind::Teller,
        RoleKind::Waiter,
        RoleKind::Kitchen,
    ]
    .into_iter()
    .find(|k| kinds.contains(k))
    .unwrap_or(RoleKind::Teller);
    sqlx::query("UPDATE users SET role = $2::user_role, updated_at = now() WHERE id = $1 AND role::text <> $2")
        .bind(*id)
        .bind(primary.as_str())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    user_access(req, pool, id, web::Query(BranchQuery { branch_id: None })).await
}

// ── explain ─────────────────────────────────────────────────────────────────

#[utoipa::path(get, path = "/authz/explain", tag = "authz", params(ExplainQuery),
    responses((status = 200, description = "Why a person can or cannot", body = Explanation), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn explain(
    req: HttpRequest,
    pool: crate::db::Db,
    q: web::Query<ExplainQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = claims_of(&req)?;
    let org = org_of(&req, &claims)?;
    if claims.user_id() != q.user_id {
        super::require::require(pool.get_ref(), &claims, Cap::StaffPermissionsRead, None).await?;
    }
    target_row(pool.get_ref(), org, q.user_id).await?;
    let cap = Cap::from_key(&q.capability)
        .ok_or_else(|| AppError::BadRequest("Unknown capability".into()))?;
    let names: BTreeMap<String, (String, String)> = load_roles(pool.get_ref(), org)
        .await?
        .into_iter()
        .map(|r| (r.id.to_string(), (r.name_en, r.name_ar)))
        .collect();
    let mut conn = pool.acquire().await?;
    let loaded = super::load::load(&mut conn, q.user_id)
        .await?
        .ok_or_else(|| AppError::NotFound("User not found".into()))?;
    drop(conn);
    let b = q.branch_id.map(|b| b.to_string());
    let now = chrono::Utc::now().timestamp();
    let eff = resolve(&loaded.principal, scope_of(&b), now, &loaded.policy);

    let mut steps = vec![];
    let p = &loaded.principal;
    if !p.active {
        steps.push(ExplainStep {
            kind: "inactive".into(),
            role_name: None,
            branch_id: None,
            detail: None,
            ..Default::default()
        });
    }
    if p.is_owner {
        steps.push(ExplainStep {
            kind: "owner".into(),
            role_name: None,
            branch_id: None,
            detail: None,
            ..Default::default()
        });
    }
    for a in &p.assignments {
        let covers = match &b {
            Some(b) => a.all_branches || a.branches.iter().any(|x| x == b),
            None => true,
        };
        let grants = a.role.grants.contains(cap);
        let core = is_core_for(cap, Kinds(a.role.kind.bit()));
        steps.push(ExplainStep {
            kind: if core { "core" } else { "assignment" }.into(),
            role_name: names.get(&a.role.id).map(|n| n.0.clone()),
            role_name_ar: names.get(&a.role.id).map(|n| n.1.clone()),
            applies_here: Some(covers),
            grants: Some(grants || core),
            branch_id: None,
            detail: Some(format!(
                "{}{}",
                if covers {
                    "applies here"
                } else {
                    "not at this branch"
                },
                if grants || core {
                    ", grants it"
                } else {
                    ", does not grant it"
                }
            )),
        });
    }
    for o in p.overrides.iter().filter(|o| o.cap == cap.id()) {
        steps.push(ExplainStep {
            kind: if o.allow {
                "override_allow"
            } else {
                "override_deny"
            }
            .into(),
            role_name: None,
            branch_id: o.branch.as_deref().and_then(|x| Uuid::parse_str(x).ok()),
            detail: None,
            ..Default::default()
        });
    }
    if eff.owner && cap.meta().protected {
        steps.push(ExplainStep {
            kind: "protected".into(),
            role_name: None,
            branch_id: None,
            detail: None,
            ..Default::default()
        });
    }
    if let Some(l) = eff.limits.get(&cap.id()) {
        steps.push(ExplainStep {
            kind: "limit".into(),
            role_name: None,
            branch_id: None,
            detail: serde_json::to_string(l).ok(),
            ..Default::default()
        });
    }
    let ask = !eff.can(cap) && eff.ask_manager.contains(cap);
    if ask {
        steps.push(ExplainStep {
            kind: "ask_manager".into(),
            role_name: None,
            branch_id: None,
            detail: None,
            ..Default::default()
        });
    }
    if !eff.can(cap) {
        steps.push(ExplainStep {
            kind: "not_held".into(),
            role_name: None,
            branch_id: None,
            detail: None,
            ..Default::default()
        });
    }
    Ok(HttpResponse::Ok().json(Explanation {
        capability: cap.key().into(),
        label_en: cap.meta().en.into(),
        label_ar: cap.meta().ar.into(),
        effective: eff.can(cap),
        ask_manager: ask,
        steps,
    }))
}

// ── policy ──────────────────────────────────────────────────────────────────

#[utoipa::path(get, path = "/authz/policy", tag = "authz",
    responses((status = 200, description = "Capabilities that show \"ask a manager\"", body = Vec<PolicyEntry>), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn get_policy(req: HttpRequest, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let claims = claims_of(&req)?;
    let org = org_of(&req, &claims)?;
    let rows: Vec<(i16, bool)> = sqlx::query_as(
        "SELECT capability_id, ask_manager FROM org_capability_policy WHERE org_id = $1",
    )
    .bind(org)
    .fetch_all(pool.get_ref())
    .await?;
    let out: Vec<PolicyEntry> = super::CAPS
        .iter()
        .filter(|m| m.approval)
        .map(|m| PolicyEntry {
            capability: m.key.into(),
            ask_manager: rows.iter().any(|(c, a)| *c as u16 == m.cap.id() && *a),
        })
        .collect();
    Ok(HttpResponse::Ok().json(out))
}

#[utoipa::path(put, path = "/authz/policy", tag = "authz", request_body = PolicyEntry,
    responses((status = 200, description = "Policy updated", body = Vec<PolicyEntry>), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn set_policy(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<PolicyEntry>,
) -> Result<HttpResponse, AppError> {
    let claims = claims_of(&req)?;
    let org = org_of(&req, &claims)?;
    super::require::require(pool.get_ref(), &claims, Cap::StaffRolesManage, None).await?;
    let cap = cap_of(&body.capability)?;
    if !cap.meta().approval {
        return Err(AppError::BadRequest(format!(
            "{} cannot be approved by a manager",
            cap.key()
        )));
    }
    sqlx::query(
        "INSERT INTO org_capability_policy (org_id, capability_id, ask_manager, updated_by)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (org_id, capability_id) DO UPDATE SET ask_manager = EXCLUDED.ask_manager,
             updated_by = EXCLUDED.updated_by, updated_at = now()",
    )
    .bind(org)
    .bind(cap.id() as i16)
    .bind(body.ask_manager)
    .bind(claims.user_id())
    .execute(pool.get_ref())
    .await?;
    get_policy(req, pool).await
}

#[utoipa::path(get, path = "/authz/flags", tag = "authz", params(FlagQuery),
    responses((status = 200, description = "Flagged offline actions", body = Vec<ReplayFlag>), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn list_flags(
    req: HttpRequest,
    pool: crate::db::Db,
    q: web::Query<FlagQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = claims_of(&req)?;
    let org = org_of(&req, &claims)?;
    super::require::require(pool.get_ref(), &claims, Cap::ApprovalsReview, None).await?;
    let rows: Vec<ReplayFlag> = sqlx::query_as::<
        _,
        (
            i64,
            Option<Uuid>,
            String,
            Uuid,
            Option<String>,
            String,
            String,
            chrono::DateTime<chrono::Utc>,
            chrono::DateTime<chrono::Utc>,
            Option<chrono::DateTime<chrono::Utc>>,
            Option<Uuid>,
        ),
    >(
        "SELECT f.id, f.branch_id, f.op, f.author_id, u.name, f.capability, f.reason,
                f.occurred_at, f.created_at, f.reviewed_at, f.reviewed_by
           FROM authz_replay_flags f
           LEFT JOIN users u ON u.id = f.author_id
          WHERE f.org_id = $1 AND ($2 OR f.reviewed_at IS NULL)
          ORDER BY f.created_at DESC
          LIMIT 500",
    )
    .bind(org)
    .bind(q.include_reviewed)
    .fetch_all(pool.get_ref())
    .await?
    .into_iter()
    .map(
        |(
            id,
            branch_id,
            op,
            author_id,
            author_name,
            capability,
            reason,
            occurred_at,
            created_at,
            reviewed_at,
            reviewed_by,
        )| {
            ReplayFlag {
                id,
                branch_id,
                op,
                author_id,
                author_name,
                capability,
                reason,
                occurred_at,
                created_at,
                reviewed_at,
                reviewed_by,
            }
        },
    )
    .collect();
    Ok(HttpResponse::Ok().json(rows))
}

/// Mark one flag as looked at. It is an acknowledgement, not an approval: the
/// act is already on the books either way, so there is nothing here to undo or
/// let through.
#[utoipa::path(post, path = "/authz/flags/{id}/review", tag = "authz",
    responses((status = 200, description = "Flag reviewed", body = ReplayFlag), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn review_flag(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<i64>,
) -> Result<HttpResponse, AppError> {
    let claims = claims_of(&req)?;
    let org = org_of(&req, &claims)?;
    super::require::require(pool.get_ref(), &claims, Cap::ApprovalsReview, None).await?;
    let me = claims.user_id();
    let updated = sqlx::query(
        "UPDATE authz_replay_flags
            SET reviewed_at = now(), reviewed_by = $3
          WHERE id = $1 AND org_id = $2 AND reviewed_at IS NULL",
    )
    .bind(path.into_inner())
    .bind(org)
    .bind(me)
    .execute(pool.get_ref())
    .await?;
    if updated.rows_affected() == 0 {
        return Err(AppError::NotFound("No such open flag".into()));
    }
    Ok(HttpResponse::Ok().json(serde_json::json!({ "reviewed": true })))
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
pub struct BulkReviewRequest {
    /// Every open flag to resolve at once — a till, a day, or a hand-picked
    /// selection. Order does not matter; each id is its own transaction.
    pub flag_ids: Vec<i64>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct BulkReviewResult {
    /// Ids that are now reviewed (already reviewed counts as resolved too —
    /// resubmitting the same batch never fails or double-records).
    pub resolved: Vec<i64>,
    /// An id this call could not resolve, and why. Never silently dropped.
    pub pending: Vec<BulkReviewPending>,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct BulkReviewPending {
    pub id: i64,
    pub reason: String,
}

/// Resolve many flags at once — "select many" or "everything for this till or
/// day" from the dashboard's review queue (owner, 2026-09-17). Extends
/// [`review_flag`] rather than duplicating it: same capability, same
/// semantics (an acknowledgement, not an approval), now with an optional note
/// and one id at a time so a bad id among many never loses the rest.
#[utoipa::path(post, path = "/authz/flags/bulk-review", tag = "authz",
    request_body = BulkReviewRequest,
    responses((status = 200, description = "Per-id result", body = BulkReviewResult), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn bulk_review_flags(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<BulkReviewRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = claims_of(&req)?;
    let org = org_of(&req, &claims)?;
    // Permission FIRST, before the id list is even looked at.
    super::require::require(pool.get_ref(), &claims, Cap::ApprovalsReview, None).await?;
    let me = claims.user_id();
    let mut resolved = Vec::new();
    let mut pending = Vec::new();
    for id in body.flag_ids.iter().copied() {
        // Idempotent per item: an already-reviewed flag is a no-op success,
        // never a second row and never an error on resubmit.
        let already: Option<bool> = sqlx::query_scalar(
            "SELECT true FROM authz_replay_flags WHERE id = $1 AND org_id = $2 AND reviewed_at IS NOT NULL",
        )
        .bind(id)
        .bind(org)
        .fetch_optional(pool.get_ref())
        .await?;
        if already.is_some() {
            resolved.push(id);
            continue;
        }
        let updated = sqlx::query(
            "UPDATE authz_replay_flags
                SET reviewed_at = now(), reviewed_by = $3, review_note = $4
              WHERE id = $1 AND org_id = $2 AND reviewed_at IS NULL",
        )
        .bind(id)
        .bind(org)
        .bind(me)
        .bind(body.note.as_deref())
        .execute(pool.get_ref())
        .await;
        match updated {
            Ok(r) if r.rows_affected() > 0 => resolved.push(id),
            Ok(_) => pending.push(BulkReviewPending {
                id,
                reason: "no such open flag in this org".into(),
            }),
            Err(e) => pending.push(BulkReviewPending {
                id,
                reason: e.to_string(),
            }),
        }
    }
    Ok(HttpResponse::Ok().json(BulkReviewResult { resolved, pending }))
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    use crate::auth::middleware::JwtMiddleware;
    cfg.service(
        web::scope("/authz")
            .wrap(JwtMiddleware)
            .route("/me", web::get().to(get_my_authz))
            .route("/roles", web::get().to(list_roles))
            .route("/roles", web::post().to(create_role))
            .route("/roles/{id}", web::patch().to(rename_role))
            .route("/roles/{id}", web::delete().to(delete_role))
            .route("/roles/{id}/grants", web::put().to(set_role_grant))
            .route("/users/{id}", web::get().to(user_access))
            .route("/users/{id}/overrides", web::put().to(set_override))
            .route("/users/{id}/assignments", web::put().to(set_assignments))
            .route("/explain", web::get().to(explain))
            .route("/policy", web::get().to(get_policy))
            .route("/policy", web::put().to(set_policy))
            .route("/flags", web::get().to(list_flags))
            .route("/flags/{id}/review", web::post().to(review_flag))
            .route("/flags/bulk-review", web::post().to(bulk_review_flags)),
    );
}
