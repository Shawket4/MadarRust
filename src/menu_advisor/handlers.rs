use std::collections::HashMap;

use actix_web::{web, HttpRequest, HttpResponse};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    auth::guards::require_same_org,
    errors::AppError,
    permissions::checker::check_permission,
};

use super::{
    adapter,
    engine::{AnalysisConfig, run_advisor},
    persistence::{self, *},
};

fn extract_claims(req: &HttpRequest) -> Result<crate::auth::jwt::Claims, AppError> {
    use actix_web::HttpMessage;
    req.extensions()
        .get::<crate::auth::jwt::Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

// ═══════════════════════════════════════════════════════════════════
// Runs
// ═══════════════════════════════════════════════════════════════════

#[derive(Serialize, Deserialize)]
pub struct CreateRunBody {
    pub config: Option<AnalysisConfig>,
}

pub async fn create_run_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>, // branch_id
    body: web::Json<CreateRunBody>,
) -> Result<HttpResponse, AppError> {
    let branch_id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;
    
    // We assume there's a way to get org_id from branch_id, but here we can just query it.
    // However, it's safer to just lookup the branch org_id.
    let branch_org_id = sqlx::query_scalar::<_, Uuid>("SELECT org_id FROM branches WHERE id = $1")
        .bind(branch_id)
        .fetch_one(pool.get_ref())
        .await?;
    
    require_same_org(&claims, Some(branch_org_id))?;

    let config = body.into_inner().config.unwrap_or_default();

    // Check if there's already an active run. Runs older than the takeover
    // threshold are presumed dead (panicked task / process restart) — mark
    // them failed and proceed, otherwise a single crash blocks the branch
    // forever.
    const STALE_RUN_TAKEOVER_MINUTES: i64 = 15;
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
                "Run abandoned: still in_progress after {} minutes (process restart or panic). \
                 Superseded by a new run.",
                age.num_minutes()
            ),
        )
        .await?;
    }

    let run_id = persistence::create_run(pool.get_ref(), branch_org_id, branch_id, &config).await?;

    let pool_clone = pool.get_ref().clone();
    tokio::spawn(async move {
        run_advisor_task(pool_clone, run_id, branch_org_id, branch_id, config).await;
    });

    #[derive(Serialize)]
    struct CreateRunRes {
        run_id: Uuid,
    }

    Ok(HttpResponse::Accepted().json(CreateRunRes { run_id }))
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
        if let Err(e) = persistence::mark_run_failed(pool, run_id, &format!("[{stage}] {msg}")).await {
            tracing::error!(run_id = %run_id, error = %e, "Could not mark advisor run failed");
        }
    }

    let inputs = match adapter::load_inputs(&pool, org_id, branch_id, now, &config).await {
        Ok(i) => i,
        Err(e) => return fail(&pool, run_id, "adapter", e.to_string()).await,
    };
    tracing::info!(
        run_id = %run_id,
        snapshots = inputs.snapshots.len(),
        sales = inputs.sales.len(),
        baskets = inputs.baskets.len(),
        "Menu advisor inputs loaded"
    );

    let mut snaps_by_key = HashMap::new();
    for s in &inputs.snapshots {
        snaps_by_key.insert(s.key.clone(), (s.category_id, s.name.clone()));
    }

    // The engine is pure CPU — catch panics so a math edge case can never
    // strand the run in `in_progress` (the old failure mode: silent forever).
    let engine_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_advisor(
            &inputs.snapshots,
            &inputs.sales,
            &inputs.baskets,
            now,
            &config,
            None, // previous quadrants: not loaded yet (hysteresis input)
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
        persistence::save_completed_report(&pool, run_id, branch_id, &snaps_by_key, &report).await
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

#[derive(Deserialize)]
pub struct ListRunsQuery {
    pub limit: Option<i64>,
    pub before: Option<chrono::DateTime<Utc>>,
}

pub async fn list_runs_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>, // branch_id
    query: web::Query<ListRunsQuery>,
) -> Result<HttpResponse, AppError> {
    let branch_id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    let limit = query.limit.unwrap_or(20).clamp(1, 100);
    
    let runs = persistence::list_runs(pool.get_ref(), branch_id, limit, query.before).await?;
    Ok(HttpResponse::Ok().json(runs))
}

#[derive(Deserialize)]
pub struct LatestRunQuery {
    /// When true, return the latest run regardless of status so the client
    /// can show failed runs (error_message) instead of an empty state.
    #[serde(default)]
    pub any_status: bool,
}

pub async fn get_latest_run_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>, // branch_id
    query: web::Query<LatestRunQuery>,
) -> Result<HttpResponse, AppError> {
    let branch_id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;

    let run = if query.any_status {
        persistence::get_latest_run_any(pool.get_ref(), branch_id).await?
    } else {
        persistence::get_latest_completed_run(pool.get_ref(), branch_id).await?
    };
    Ok(HttpResponse::Ok().json(run))
}

pub async fn get_active_run_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>, // branch_id
) -> Result<HttpResponse, AppError> {
    let branch_id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    
    let run = persistence::get_in_progress_run(pool.get_ref(), branch_id).await?;
    Ok(HttpResponse::Ok().json(run))
}

pub async fn get_run_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>, // run_id
) -> Result<HttpResponse, AppError> {
    let run_id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    
    let run = persistence::get_run(pool.get_ref(), run_id).await?;
    Ok(HttpResponse::Ok().json(run))
}

// ═══════════════════════════════════════════════════════════════════
// Suggestions (Read)
// ═══════════════════════════════════════════════════════════════════

pub async fn list_price_suggestions_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>, // run_id
    query: web::Query<PriceSuggestionFilter>,
) -> Result<HttpResponse, AppError> {
    let run_id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    
    let filter = query.into_inner();
    let suggestions = persistence::list_price_suggestions(pool.get_ref(), run_id, &filter).await?;
    Ok(HttpResponse::Ok().json(suggestions))
}

pub async fn list_bundle_suggestions_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>, // run_id
    query: web::Query<BundleSuggestionFilter>,
) -> Result<HttpResponse, AppError> {
    let run_id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    
    let filter = query.into_inner();
    let suggestions = persistence::list_bundle_suggestions(pool.get_ref(), run_id, &filter).await?;
    Ok(HttpResponse::Ok().json(suggestions))
}

pub async fn list_removal_scenarios_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>, // run_id
    query: web::Query<RemovalScenarioFilter>,
) -> Result<HttpResponse, AppError> {
    let run_id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    
    let filter = query.into_inner();
    let scenarios = persistence::list_removal_scenarios(pool.get_ref(), run_id, &filter).await?;
    Ok(HttpResponse::Ok().json(scenarios))
}

pub async fn get_price_suggestion_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>, // id
) -> Result<HttpResponse, AppError> {
    let id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    
    let suggestion = persistence::get_price_suggestion(pool.get_ref(), id).await?;
    Ok(HttpResponse::Ok().json(suggestion))
}

pub async fn get_bundle_suggestion_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>, // id
) -> Result<HttpResponse, AppError> {
    let id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    
    let suggestion = persistence::get_bundle_suggestion(pool.get_ref(), id).await?;
    Ok(HttpResponse::Ok().json(suggestion))
}

pub async fn get_removal_scenario_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>, // id
) -> Result<HttpResponse, AppError> {
    let id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    
    let scenario = persistence::get_removal_scenario(pool.get_ref(), id).await?;
    Ok(HttpResponse::Ok().json(scenario))
}

// ═══════════════════════════════════════════════════════════════════
// Decisions & Calibration
// ═══════════════════════════════════════════════════════════════════

#[derive(Serialize, Deserialize)]
pub struct RecordDecisionBody {
    pub suggestion_id: Uuid,
    pub suggestion_kind: SuggestionKind,
    pub branch_id: Uuid,
    pub decision: String,
    pub notes: Option<String>,
}

pub async fn record_decision_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<RecordDecisionBody>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;
    
    let decision = Decision::parse(&body.decision)
        .ok_or_else(|| AppError::BadRequest("Invalid decision".into()))?;

    let decided_by = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid user UUID".into()))?;

    let record = persistence::record_decision(
        pool.get_ref(),
        body.suggestion_id,
        body.suggestion_kind,
        body.branch_id,
        decision,
        body.notes.clone(),
        decided_by,
    ).await?;
    
    Ok(HttpResponse::Ok().json(record))
}

#[derive(Deserialize)]
pub struct ListDecisionsQuery {
    pub since: Option<chrono::DateTime<Utc>>,
}

pub async fn list_decisions_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>, // branch_id
    query: web::Query<ListDecisionsQuery>,
) -> Result<HttpResponse, AppError> {
    let branch_id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    
    let decisions = persistence::list_decisions(pool.get_ref(), branch_id, query.since).await?;
    Ok(HttpResponse::Ok().json(decisions))
}

#[derive(Serialize, Deserialize)]
pub struct PromoteBundleBody {
    pub bundle_id: Uuid,
}

pub async fn set_bundle_promoted_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>, // suggestion_id
    body: web::Json<PromoteBundleBody>,
) -> Result<HttpResponse, AppError> {
    let suggestion_id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;
    
    persistence::set_bundle_promoted(pool.get_ref(), suggestion_id, body.bundle_id).await?;
    Ok(HttpResponse::Ok().finish())
}

pub async fn get_calibration_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>, // branch_id
    query: web::Query<ListDecisionsQuery>, // Reuse query for `since`
) -> Result<HttpResponse, AppError> {
    let branch_id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    
    let calib = persistence::get_calibration(pool.get_ref(), branch_id, query.since).await?;
    Ok(HttpResponse::Ok().json(calib))
}

// ═══════════════════════════════════════════════════════════════════
// Item Level Integration
// ═══════════════════════════════════════════════════════════════════

#[derive(Deserialize)]
pub struct ItemKpiPath {
    pub branch_id: Uuid,
    pub menu_item_id: Uuid,
    pub size_label: String,
}

pub async fn get_latest_item_kpi_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<ItemKpiPath>,
) -> Result<HttpResponse, AppError> {
    let path = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    
    let kpi = persistence::get_latest_item_kpi(
        pool.get_ref(), 
        path.branch_id, 
        path.menu_item_id, 
        &path.size_label
    ).await?;
    
    Ok(HttpResponse::Ok().json(kpi))
}
