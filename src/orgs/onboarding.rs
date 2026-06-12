//! Organization onboarding — derived setup checklist + completion flag.
//!
//! Step progress is computed from data presence on every read instead of
//! being stored, so the checklist can never disagree with the actual state
//! of the org. Only the terminal `onboarding_completed` flag is persisted
//! (on `organizations`), because "the owner said we're done / skip this"
//! is a decision, not a derivable fact.

use actix_web::{web, HttpRequest, HttpResponse};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    auth::guards::require_same_org, errors::AppError, permissions::checker::check_permission,
};

use super::handlers::extract_claims;

/// One derived setup step.
#[derive(Debug, Serialize, ToSchema)]
pub struct OnboardingStep {
    /// Stable key the dashboard switches on — never localized.
    pub key: String,
    /// True when the underlying data exists.
    pub done: bool,
    /// Supporting count (branches created, items added, …).
    pub count: i64,
    /// Steps that are encouraged but not blocking (`required = false`
    /// never gates `can_complete`).
    pub required: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OnboardingStatus {
    pub org_id: Uuid,
    /// Persisted terminal flag — the dashboard routes into the wizard
    /// when this is false.
    pub completed: bool,
    pub completed_at: Option<DateTime<Utc>>,
    /// True when every `required` step is done (the Finish button enabler).
    pub can_complete: bool,
    /// Recipe coverage across active menu items (0..1) — drives the cost
    /// engine; surfaced separately because it's a percentage, not a bool.
    pub recipe_coverage: f64,
    pub steps: Vec<OnboardingStep>,
}

#[derive(sqlx::FromRow)]
struct CountsRow {
    onboarding_completed: bool,
    onboarding_completed_at: Option<DateTime<Utc>>,
    branches: i64,
    extra_users: i64,
    payment_methods: i64,
    categories: i64,
    menu_items: i64,
    ingredients: i64,
    items_with_recipes: i64,
    addon_items: i64,
    orders_placed: i64,
}

// ── GET /orgs/{id}/onboarding ─────────────────────────────────

#[utoipa::path(
    get,
    path = "/orgs/{id}/onboarding",
    tag = "orgs",
    params(("id" = Uuid, Path, description = "Organization ID")),
    responses((status = 200, description = "Derived onboarding checklist", body = OnboardingStatus), crate::errors::AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_onboarding(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let org_id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orgs", "read").await?;
    require_same_org(&claims, Some(org_id))?;

    let status = load_status(pool.get_ref(), org_id).await?;
    Ok(HttpResponse::Ok().json(status))
}

// ── POST /orgs/{id}/onboarding/complete ───────────────────────

#[utoipa::path(
    post,
    path = "/orgs/{id}/onboarding/complete",
    tag = "orgs",
    params(("id" = Uuid, Path, description = "Organization ID")),
    responses((status = 200, description = "Onboarding marked complete (idempotent)", body = OnboardingStatus), crate::errors::AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn complete_onboarding(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let org_id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orgs", "update").await?;
    require_same_org(&claims, Some(org_id))?;

    // Server-side gate: completion is only legal once every required step is
    // done. The dashboard disables Finish client-side, but the flag is
    // terminal (it never regresses), so the server must refuse to set it on
    // an org that isn't actually set up.
    let status = load_status(pool.get_ref(), org_id).await?;
    if status.completed {
        // idempotent: already completed orgs return their status unchanged
        return Ok(HttpResponse::Ok().json(status));
    }
    if !status.can_complete {
        let missing: Vec<&str> = status
            .steps
            .iter()
            .filter(|s| s.required && !s.done)
            .map(|s| s.key.as_str())
            .collect();
        return Err(AppError::Conflict(format!(
            "Onboarding cannot be completed: required steps missing ({})",
            missing.join(", ")
        )));
    }

    let updated = sqlx::query(
        "UPDATE organizations
         SET onboarding_completed = true,
             onboarding_completed_at = COALESCE(onboarding_completed_at, now())
         WHERE id = $1",
    )
    .bind(org_id)
    .execute(pool.get_ref())
    .await?;
    if updated.rows_affected() == 0 {
        return Err(AppError::NotFound("Organization not found".into()));
    }

    let status = load_status(pool.get_ref(), org_id).await?;
    Ok(HttpResponse::Ok().json(status))
}

async fn load_status(pool: &PgPool, org_id: Uuid) -> Result<OnboardingStatus, AppError> {
    let row: Option<CountsRow> = sqlx::query_as::<_, CountsRow>(
        r#"
        SELECT
            o.onboarding_completed,
            o.onboarding_completed_at,
            (SELECT COUNT(*) FROM branches b
              WHERE b.org_id = o.id)::bigint                          AS branches,
            (SELECT COUNT(*) FROM users u
              WHERE u.org_id = o.id
                AND u.role <> 'org_admin'::user_role)::bigint         AS extra_users,
            (SELECT COUNT(*) FROM org_payment_methods pm
              WHERE pm.org_id = o.id AND pm.is_active)::bigint        AS payment_methods,
            (SELECT COUNT(*) FROM categories c
              WHERE c.org_id = o.id)::bigint                          AS categories,
            (SELECT COUNT(*) FROM menu_items mi
              WHERE mi.org_id = o.id AND mi.deleted_at IS NULL
                AND mi.is_active)::bigint                             AS menu_items,
            (SELECT COUNT(*) FROM org_ingredients oi
              WHERE oi.org_id = o.id)::bigint                         AS ingredients,
            (SELECT COUNT(DISTINCT r.menu_item_id)
               FROM menu_item_recipes r
               JOIN menu_items mi ON mi.id = r.menu_item_id
              WHERE mi.org_id = o.id AND mi.deleted_at IS NULL
                AND mi.is_active)::bigint                             AS items_with_recipes,
            (SELECT COUNT(*) FROM addon_items a
              WHERE a.org_id = o.id AND a.is_active)::bigint          AS addon_items,
            (SELECT COUNT(*) FROM orders ord
               JOIN branches b ON b.id = ord.branch_id
              WHERE b.org_id = o.id)::bigint                          AS orders_placed
        FROM organizations o
        WHERE o.id = $1
        "#,
    )
    .bind(org_id)
    .fetch_optional(pool)
    .await?;

    let row = row.ok_or_else(|| AppError::NotFound("Organization not found".into()))?;

    let recipe_coverage = if row.menu_items > 0 {
        row.items_with_recipes as f64 / row.menu_items as f64
    } else {
        0.0
    };

    let step = |key: &str, done: bool, count: i64, required: bool| OnboardingStep {
        key: key.to_string(),
        done,
        count,
        required,
    };

    let steps = vec![
        step("branch", row.branches > 0, row.branches, true),
        step("payment_methods", row.payment_methods > 0, row.payment_methods, true),
        step("categories", row.categories > 0, row.categories, true),
        step("menu_items", row.menu_items > 0, row.menu_items, true),
        step("ingredients", row.ingredients > 0, row.ingredients, false),
        step(
            "recipes",
            row.items_with_recipes > 0,
            row.items_with_recipes,
            false,
        ),
        step("addons", row.addon_items > 0, row.addon_items, false),
        step("team", row.extra_users > 0, row.extra_users, false),
        step("first_order", row.orders_placed > 0, row.orders_placed, false),
    ];

    let can_complete = steps.iter().filter(|s| s.required).all(|s| s.done);

    Ok(OnboardingStatus {
        org_id,
        completed: row.onboarding_completed,
        completed_at: row.onboarding_completed_at,
        can_complete,
        recipe_coverage,
        steps,
    })
}
