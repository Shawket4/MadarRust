//! HTTP handlers for the Menu Advisor.
//!
//! Every handler enforces, in order: claims extraction → `menu_items`
//! permission → org/branch OWNERSHIP. Run- and suggestion-scoped routes
//! resolve the record's branch first and gate on it — a UUID alone never
//! grants access (the pre-rebuild handlers skipped this, allowing any
//! authenticated user to read any org's advisor data).

use std::collections::HashMap;

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    auth::jwt::Claims,
    errors::{AppError, AppErrorResponse},
    models::UserRole,
    permissions::checker::check_permission,
};

use super::{
    adapter,
    dto::{
        AnalysisConfig, BundleSuggestionFilter, BundleSuggestionRecord, CalibrationSummary,
        CreateRunBody, CreateRunResponse, Decision, DecisionRecord, ItemKey, ItemKpiPath,
        LatestRunQuery, ListDecisionsQuery, ListRunsQuery, PersistedRun, PriceSuggestionFilter,
        PriceSuggestionRecord, PromoteBundleBody, RecordDecisionBody, RemovalScenarioFilter,
        RemovalScenarioRecord,
    },
    engine, persistence,
};

/// Runs in_progress longer than this are presumed dead (panicked task or
/// process restart) and get taken over by the next POST.
const STALE_RUN_TAKEOVER_MINUTES: i64 = 15;

// ═══════════════════════════════════════════════════════════════════
// Access-control helpers
// ═══════════════════════════════════════════════════════════════════

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    use actix_web::HttpMessage;
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

/// Standard branch ownership gate (same semantics as the reports module):
/// branch must exist (404) and belong to the caller's org (403); org admins
/// pass, everyone else needs a `user_branch_assignments` row. Returns the
/// branch's org id.
async fn require_branch_access(
    pool: &PgPool,
    claims: &Claims,
    branch_id: Uuid,
) -> Result<Uuid, AppError> {
    let branch_org: Option<Uuid> =
        sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL")
            .bind(branch_id)
            .fetch_optional(pool)
            .await?
            .flatten();

    let branch_org = branch_org.ok_or_else(|| AppError::NotFound("Branch not found".into()))?;

    if claims.role == UserRole::SuperAdmin {
        return Ok(branch_org);
    }
    if claims.org_id() != Some(branch_org) {
        return Err(AppError::Forbidden(
            "Branch belongs to a different org".into(),
        ));
    }
    if claims.role == UserRole::OrgAdmin {
        return Ok(branch_org);
    }

    let assigned: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM user_branch_assignments \
         WHERE user_id = $1 AND branch_id = $2)",
    )
    .bind(claims.user_id())
    .bind(branch_id)
    .fetch_one(pool)
    .await?;

    if !assigned {
        return Err(AppError::Forbidden("Not assigned to this branch".into()));
    }
    Ok(branch_org)
}

async fn get_run_checked(
    pool: &PgPool,
    claims: &Claims,
    run_id: Uuid,
) -> Result<PersistedRun, AppError> {
    let run = persistence::get_run(pool, run_id)
        .await?
        .ok_or_else(|| AppError::NotFound("Run not found".into()))?;
    require_branch_access(pool, claims, run.branch_id).await?;
    Ok(run)
}

async fn get_price_suggestion_checked(
    pool: &PgPool,
    claims: &Claims,
    id: Uuid,
) -> Result<PriceSuggestionRecord, AppError> {
    let record = persistence::get_price_suggestion(pool, id)
        .await?
        .ok_or_else(|| AppError::NotFound("Price suggestion not found".into()))?;
    require_branch_access(pool, claims, record.branch_id).await?;
    Ok(record)
}

/// Returns the record plus its branch's org id (the promote flow compares
/// it against the target bundle's org).
async fn get_bundle_suggestion_checked(
    pool: &PgPool,
    claims: &Claims,
    id: Uuid,
) -> Result<(BundleSuggestionRecord, Uuid), AppError> {
    let record = persistence::get_bundle_suggestion(pool, id)
        .await?
        .ok_or_else(|| AppError::NotFound("Bundle suggestion not found".into()))?;
    let org = require_branch_access(pool, claims, record.branch_id).await?;
    Ok((record, org))
}

async fn get_removal_scenario_checked(
    pool: &PgPool,
    claims: &Claims,
    id: Uuid,
) -> Result<RemovalScenarioRecord, AppError> {
    let record = persistence::get_removal_scenario(pool, id)
        .await?
        .ok_or_else(|| AppError::NotFound("Removal scenario not found".into()))?;
    require_branch_access(pool, claims, record.branch_id).await?;
    Ok(record)
}

// ═══════════════════════════════════════════════════════════════════
// Runs
// ═══════════════════════════════════════════════════════════════════

#[utoipa::path(
    post,
    path = "/menu-advisor/branches/{branch_id}/runs",
    tag = "menu_advisor",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    request_body = CreateRunBody,
    responses(
        (status = 202, description = "Analysis run started", body = CreateRunResponse),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn create_run_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>, // branch_id
    body: web::Json<CreateRunBody>,
) -> Result<HttpResponse, AppError> {
    let branch_id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;
    let org_id = require_branch_access(pool.get_ref(), &claims, branch_id).await?;

    let config = body.into_inner().config.unwrap_or_default();

    // A fresh active run blocks; a stale one (panicked task / process
    // restart) is taken over so a single crash can't block the branch
    // forever. The DB's partial unique index closes the remaining race.
    if let Some(active) = persistence::get_in_progress_run(pool.get_ref(), branch_id).await? {
        let age = Utc::now() - active.started_at;
        if age < chrono::Duration::minutes(STALE_RUN_TAKEOVER_MINUTES) {
            return Err(AppError::Conflict(
                "A run is already in progress for this branch".into(),
            ));
        }
        tracing::warn!(
            run_id = %active.id,
            branch_id = %branch_id,
            age_minutes = age.num_minutes(),
            "Taking over stale in-progress advisor run"
        );
        persistence::mark_run_failed(
            pool.get_ref(),
            active.id,
            &format!(
                "Run abandoned: still in_progress after {} minutes (process restart or \
                 panic). Superseded by a new run.",
                age.num_minutes()
            ),
        )
        .await?;
    }

    let run_id = persistence::create_run(pool.get_ref(), org_id, branch_id, &config).await?;

    let pool_clone = pool.get_ref().clone();
    tokio::spawn(async move {
        run_advisor_task(pool_clone, run_id, org_id, branch_id, config).await;
    });

    Ok(HttpResponse::Accepted().json(CreateRunResponse { run_id }))
}

async fn run_advisor_task(
    pool: PgPool,
    run_id: Uuid,
    org_id: Uuid,
    branch_id: Uuid,
    config: AnalysisConfig,
) {
    let now = Utc::now();

    async fn fail(pool: &PgPool, run_id: Uuid, stage: &str, msg: String) {
        tracing::error!(run_id = %run_id, stage = %stage, error = %msg, "Menu advisor run failed");
        if let Err(e) =
            persistence::mark_run_failed(pool, run_id, &format!("[{stage}] {msg}")).await
        {
            tracing::error!(run_id = %run_id, error = %e, "Could not mark advisor run failed");
        }
    }

    let inputs = match adapter::load_inputs(&pool, org_id, branch_id, now, &config).await {
        Ok(i) => i,
        Err(e) => return fail(&pool, run_id, "adapter", e.to_string()).await,
    };
    let previous = match persistence::load_latest_classifications(&pool, branch_id).await {
        Ok(p) => p,
        Err(e) => return fail(&pool, run_id, "adapter", e.to_string()).await,
    };
    tracing::info!(
        run_id = %run_id,
        snapshots = inputs.snapshots.len(),
        sales = inputs.sales.len(),
        baskets = inputs.baskets.len(),
        has_previous = previous.is_some(),
        "Menu advisor inputs loaded"
    );

    let category_by_key: HashMap<ItemKey, Option<Uuid>> = inputs
        .snapshots
        .iter()
        .map(|s| (s.key.clone(), s.category_id))
        .collect();

    // The engine is panic-free by construction; catch_unwind stays as a
    // backstop so no failure mode can strand the run in `in_progress`.
    let engine_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        engine::run_advisor(
            &inputs.snapshots,
            &inputs.sales,
            &inputs.baskets,
            now,
            &config,
            previous.as_ref(),
            &inputs.price_changed_keys,
        )
    }));

    let report = match engine_result {
        Ok(Ok(report)) => report,
        Ok(Err(e)) => return fail(&pool, run_id, "engine", e.to_string()).await,
        Err(panic) => {
            let msg = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "engine panicked (non-string payload)".into());
            return fail(&pool, run_id, "engine-panic", msg).await;
        }
    };

    if let Err(e) =
        persistence::save_completed_report(&pool, run_id, branch_id, &category_by_key, &report)
            .await
    {
        return fail(&pool, run_id, "persistence", e.to_string()).await;
    }
    tracing::info!(
        run_id = %run_id,
        price_suggestions = report.price_suggestions.len(),
        bundle_suggestions = report.bundle_suggestions.len(),
        removal_scenarios = report.removal_scenarios.len(),
        "Menu advisor run completed"
    );
}

#[utoipa::path(
    get,
    path = "/menu-advisor/branches/{branch_id}/runs",
    tag = "menu_advisor",
    params(("branch_id" = Uuid, Path, description = "Branch ID"), ListRunsQuery),
    responses(
        (status = 200, description = "Runs, newest first", body = Vec<PersistedRun>),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn list_runs_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>, // branch_id
    query: web::Query<ListRunsQuery>,
) -> Result<HttpResponse, AppError> {
    let branch_id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    require_branch_access(pool.get_ref(), &claims, branch_id).await?;

    let limit = query.limit.unwrap_or(20).clamp(1, 100);
    let runs = persistence::list_runs(pool.get_ref(), branch_id, limit, query.before).await?;
    Ok(HttpResponse::Ok().json(runs))
}

#[utoipa::path(
    get,
    path = "/menu-advisor/branches/{branch_id}/runs/latest",
    tag = "menu_advisor",
    params(("branch_id" = Uuid, Path, description = "Branch ID"), LatestRunQuery),
    responses(
        (status = 200, description = "Latest run (completed unless any_status), or JSON null when none", body = Option<PersistedRun>),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn get_latest_run_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>, // branch_id
    query: web::Query<LatestRunQuery>,
) -> Result<HttpResponse, AppError> {
    let branch_id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    require_branch_access(pool.get_ref(), &claims, branch_id).await?;

    let run = persistence::get_latest_run(pool.get_ref(), branch_id, query.any_status).await?;
    Ok(HttpResponse::Ok().json(run))
}

#[utoipa::path(
    get,
    path = "/menu-advisor/branches/{branch_id}/runs/active",
    tag = "menu_advisor",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    responses(
        (status = 200, description = "In-progress run, or JSON null when none", body = Option<PersistedRun>),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn get_active_run_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>, // branch_id
) -> Result<HttpResponse, AppError> {
    let branch_id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    require_branch_access(pool.get_ref(), &claims, branch_id).await?;

    let run = persistence::get_in_progress_run(pool.get_ref(), branch_id).await?;
    Ok(HttpResponse::Ok().json(run))
}

#[utoipa::path(
    get,
    path = "/menu-advisor/runs/{id}",
    tag = "menu_advisor",
    params(("id" = Uuid, Path, description = "Run ID")),
    responses(
        (status = 200, description = "Run", body = PersistedRun),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn get_run_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>, // run_id
) -> Result<HttpResponse, AppError> {
    let run_id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;

    let run = get_run_checked(pool.get_ref(), &claims, run_id).await?;
    Ok(HttpResponse::Ok().json(run))
}

// ═══════════════════════════════════════════════════════════════════
// Suggestions (read)
// ═══════════════════════════════════════════════════════════════════

#[utoipa::path(
    get,
    path = "/menu-advisor/runs/{id}/price-suggestions",
    tag = "menu_advisor",
    params(("id" = Uuid, Path, description = "Run ID"), PriceSuggestionFilter),
    responses(
        (status = 200, description = "Price suggestions with latest decision joined", body = Vec<PriceSuggestionRecord>),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn list_price_suggestions_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>, // run_id
    query: web::Query<PriceSuggestionFilter>,
) -> Result<HttpResponse, AppError> {
    let run_id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    get_run_checked(pool.get_ref(), &claims, run_id).await?;

    let suggestions =
        persistence::list_price_suggestions(pool.get_ref(), run_id, &query.into_inner()).await?;
    Ok(HttpResponse::Ok().json(suggestions))
}

#[utoipa::path(
    get,
    path = "/menu-advisor/runs/{id}/bundle-suggestions",
    tag = "menu_advisor",
    params(("id" = Uuid, Path, description = "Run ID"), BundleSuggestionFilter),
    responses(
        (status = 200, description = "Bundle suggestions with latest decision joined", body = Vec<BundleSuggestionRecord>),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn list_bundle_suggestions_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>, // run_id
    query: web::Query<BundleSuggestionFilter>,
) -> Result<HttpResponse, AppError> {
    let run_id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    get_run_checked(pool.get_ref(), &claims, run_id).await?;

    let suggestions =
        persistence::list_bundle_suggestions(pool.get_ref(), run_id, &query.into_inner()).await?;
    Ok(HttpResponse::Ok().json(suggestions))
}

#[utoipa::path(
    get,
    path = "/menu-advisor/runs/{id}/removal-scenarios",
    tag = "menu_advisor",
    params(("id" = Uuid, Path, description = "Run ID"), RemovalScenarioFilter),
    responses(
        (status = 200, description = "Removal scenarios with latest decision joined", body = Vec<RemovalScenarioRecord>),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn list_removal_scenarios_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>, // run_id
    query: web::Query<RemovalScenarioFilter>,
) -> Result<HttpResponse, AppError> {
    let run_id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    get_run_checked(pool.get_ref(), &claims, run_id).await?;

    let scenarios =
        persistence::list_removal_scenarios(pool.get_ref(), run_id, &query.into_inner()).await?;
    Ok(HttpResponse::Ok().json(scenarios))
}

#[utoipa::path(
    get,
    path = "/menu-advisor/price-suggestions/{id}",
    tag = "menu_advisor",
    params(("id" = Uuid, Path, description = "Price suggestion ID")),
    responses(
        (status = 200, description = "Price suggestion", body = PriceSuggestionRecord),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn get_price_suggestion_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;

    let suggestion = get_price_suggestion_checked(pool.get_ref(), &claims, id).await?;
    Ok(HttpResponse::Ok().json(suggestion))
}

#[utoipa::path(
    get,
    path = "/menu-advisor/bundle-suggestions/{id}",
    tag = "menu_advisor",
    params(("id" = Uuid, Path, description = "Bundle suggestion ID")),
    responses(
        (status = 200, description = "Bundle suggestion", body = BundleSuggestionRecord),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn get_bundle_suggestion_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;

    let (suggestion, _) = get_bundle_suggestion_checked(pool.get_ref(), &claims, id).await?;
    Ok(HttpResponse::Ok().json(suggestion))
}

#[utoipa::path(
    get,
    path = "/menu-advisor/removal-scenarios/{id}",
    tag = "menu_advisor",
    params(("id" = Uuid, Path, description = "Removal scenario ID")),
    responses(
        (status = 200, description = "Removal scenario", body = RemovalScenarioRecord),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn get_removal_scenario_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;

    let scenario = get_removal_scenario_checked(pool.get_ref(), &claims, id).await?;
    Ok(HttpResponse::Ok().json(scenario))
}

// ═══════════════════════════════════════════════════════════════════
// Decisions & calibration
// ═══════════════════════════════════════════════════════════════════

#[utoipa::path(
    post,
    path = "/menu-advisor/decisions",
    tag = "menu_advisor",
    request_body = RecordDecisionBody,
    responses(
        (status = 200, description = "Decision recorded", body = DecisionRecord),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn record_decision_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<RecordDecisionBody>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let decision = Decision::parse(&body.decision)
        .ok_or_else(|| AppError::BadRequest("Invalid decision".into()))?;

    // The suggestion is the source of truth for the branch — the body's
    // branch_id is validated against it, never trusted.
    let branch_id = persistence::get_suggestion_branch(
        pool.get_ref(),
        body.suggestion_kind,
        body.suggestion_id,
    )
    .await?
    .ok_or_else(|| AppError::NotFound("Suggestion not found".into()))?;
    if body.branch_id != branch_id {
        return Err(AppError::BadRequest(
            "branch_id does not match the suggestion".into(),
        ));
    }
    require_branch_access(pool.get_ref(), &claims, branch_id).await?;

    let decided_by = claims.user_id_safe()?;
    let record = persistence::record_decision(
        pool.get_ref(),
        body.suggestion_id,
        body.suggestion_kind,
        branch_id,
        decision,
        body.notes.clone(),
        decided_by,
    )
    .await?;

    Ok(HttpResponse::Ok().json(record))
}

#[utoipa::path(
    get,
    path = "/menu-advisor/branches/{branch_id}/decisions",
    tag = "menu_advisor",
    params(("branch_id" = Uuid, Path, description = "Branch ID"), ListDecisionsQuery),
    responses(
        (status = 200, description = "Decisions, newest first", body = Vec<DecisionRecord>),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn list_decisions_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>, // branch_id
    query: web::Query<ListDecisionsQuery>,
) -> Result<HttpResponse, AppError> {
    let branch_id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    require_branch_access(pool.get_ref(), &claims, branch_id).await?;

    let decisions = persistence::list_decisions(pool.get_ref(), branch_id, query.since).await?;
    Ok(HttpResponse::Ok().json(decisions))
}

#[utoipa::path(
    post,
    path = "/menu-advisor/bundle-suggestions/{id}/promote",
    tag = "menu_advisor",
    params(("id" = Uuid, Path, description = "Bundle suggestion ID")),
    request_body = PromoteBundleBody,
    responses(
        (status = 200, description = "Suggestion linked to the created bundle"),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn set_bundle_promoted_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>, // suggestion_id
    body: web::Json<PromoteBundleBody>,
) -> Result<HttpResponse, AppError> {
    let suggestion_id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let (_, suggestion_org) =
        get_bundle_suggestion_checked(pool.get_ref(), &claims, suggestion_id).await?;

    // The linked bundle must exist in the same org as the suggestion.
    let bundle_org: Option<Uuid> = sqlx::query_scalar("SELECT org_id FROM bundles WHERE id = $1")
        .bind(body.bundle_id)
        .fetch_optional(pool.get_ref())
        .await?;
    let bundle_org = bundle_org.ok_or_else(|| AppError::NotFound("Bundle not found".into()))?;
    if bundle_org != suggestion_org {
        return Err(AppError::Forbidden(
            "Bundle belongs to a different org".into(),
        ));
    }

    persistence::set_bundle_promoted(pool.get_ref(), suggestion_id, body.bundle_id).await?;
    Ok(HttpResponse::Ok().finish())
}

#[utoipa::path(
    get,
    path = "/menu-advisor/branches/{branch_id}/calibration",
    tag = "menu_advisor",
    params(("branch_id" = Uuid, Path, description = "Branch ID"), ListDecisionsQuery),
    responses(
        (status = 200, description = "Predicted vs realized price-move calibration", body = CalibrationSummary),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn get_calibration_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>, // branch_id
    query: web::Query<ListDecisionsQuery>,
) -> Result<HttpResponse, AppError> {
    let branch_id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    require_branch_access(pool.get_ref(), &claims, branch_id).await?;

    let calib = persistence::get_calibration(pool.get_ref(), branch_id, query.since).await?;
    Ok(HttpResponse::Ok().json(calib))
}

// ═══════════════════════════════════════════════════════════════════
// Item-level integration
// ═══════════════════════════════════════════════════════════════════

#[utoipa::path(
    get,
    path = "/menu-advisor/branches/{branch_id}/items/{menu_item_id}/sizes/{size_label}/latest-kpi",
    tag = "menu_advisor",
    params(
        ("branch_id" = Uuid, Path, description = "Branch ID"),
        ("menu_item_id" = Uuid, Path, description = "Menu item ID"),
        ("size_label" = String, Path, description = "Size label, e.g. one_size")
    ),
    responses(
        (status = 200, description = "Latest completed-run price suggestion for the SKU, or JSON null", body = PriceSuggestionRecord),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn get_latest_item_kpi_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<ItemKpiPath>,
) -> Result<HttpResponse, AppError> {
    let path = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    require_branch_access(pool.get_ref(), &claims, path.branch_id).await?;

    let kpi = persistence::get_latest_item_kpi(
        pool.get_ref(),
        path.branch_id,
        path.menu_item_id,
        &path.size_label,
    )
    .await?;
    Ok(HttpResponse::Ok().json(kpi))
}
