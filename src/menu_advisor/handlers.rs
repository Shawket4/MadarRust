//! HTTP handlers for the Menu Advisor.
//!
//! One endpoint:  GET /menu-advisor/report?org_id=<uuid>[&window_days=30]
//!
//! Permissions: reuses `menu_items` resource (read).

use actix_web::{web, HttpRequest, HttpResponse};
use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;
use serde::Deserialize;

use crate::{
    auth::guards::require_same_org,
    errors::AppError,
    permissions::checker::check_permission,
};
use super::{adapter, engine::{AnalysisConfig, run_advisor}};

fn extract_claims(req: &HttpRequest) -> Result<crate::auth::jwt::Claims, AppError> {
    use actix_web::HttpMessage;
    req.extensions()
        .get::<crate::auth::jwt::Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

#[derive(Deserialize)]
pub struct AdvisorQuery {
    pub org_id:      Uuid,
    pub window_days: Option<f64>,
}

pub async fn get_report(
    req:   HttpRequest,
    pool:  web::Data<PgPool>,
    query: web::Query<AdvisorQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    require_same_org(&claims, Some(query.org_id))?;

    let window_days = query.window_days
        .unwrap_or(30.0)
        .clamp(7.0, 365.0);

    let config = AnalysisConfig {
        analysis_window_days: window_days,
        ..AnalysisConfig::default()
    };

    let now = Utc::now();

    let inputs = adapter::load_inputs(pool.get_ref(), query.org_id, now, &config).await?;

    let report = run_advisor(
        &inputs.snapshots,
        &inputs.sales,
        &inputs.baskets,
        now,
        &config,
        None,                      // no previous quadrant state stored server-side
        &inputs.price_changed_keys,
    ).map_err(|_e| AppError::Internal)?;

    Ok(HttpResponse::Ok().json(report))
}
