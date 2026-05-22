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
    
    // Check if there's already an active run
    if persistence::get_in_progress_run(pool.get_ref(), branch_id).await?.is_some() {
        return Err(AppError::BadRequest("A run is already in progress for this branch".into()));
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
    match adapter::load_inputs(&pool, org_id, branch_id, now, &config).await {
        Ok(inputs) => {
            let mut snaps_by_key = HashMap::new();
            for s in &inputs.snapshots {
                snaps_by_key.insert(s.key.clone(), (s.category_id, s.name.clone()));
            }

            match run_advisor(
                &inputs.snapshots,
                &inputs.sales,
                &inputs.baskets,
                now,
                &config,
                None, // We could load previous quadrants if needed, but not supported yet
                &inputs.price_changed_keys,
            ) {
                Ok(report) => {
                    if let Err(e) = persistence::save_completed_report(
                        &pool, run_id, branch_id, &snaps_by_key, &report,
                    ).await {
                        let _ = persistence::mark_run_failed(&pool, run_id, &e.to_string()).await;
                    }
                }
                Err(e) => {
                    let _ = persistence::mark_run_failed(&pool, run_id, &e.to_string()).await;
                }
            }
        }
        Err(e) => {
            let _ = persistence::mark_run_failed(&pool, run_id, &e.to_string()).await;
        }
    }
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

pub async fn get_latest_run_handler(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>, // branch_id
) -> Result<HttpResponse, AppError> {
    let branch_id = path.into_inner();
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    
    let run = persistence::get_latest_completed_run(pool.get_ref(), branch_id).await?;
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
