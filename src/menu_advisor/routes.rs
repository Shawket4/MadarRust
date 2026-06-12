//! Route registration for the Menu Advisor module.
//!
//! The route table is the wire contract — paths must not change.

use actix_web::web;
use super::handlers::*;
use crate::auth::middleware::JwtMiddleware;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/menu-advisor")
            .wrap(JwtMiddleware)

            // Runs
            .route("/branches/{branch_id}/runs", web::post().to(create_run_handler))
            .route("/branches/{branch_id}/runs", web::get().to(list_runs_handler))
            .route("/branches/{branch_id}/runs/latest", web::get().to(get_latest_run_handler))
            .route("/branches/{branch_id}/runs/active", web::get().to(get_active_run_handler))
            .route("/runs/{id}", web::get().to(get_run_handler))

            // Suggestions (read)
            .route("/runs/{id}/price-suggestions", web::get().to(list_price_suggestions_handler))
            .route("/runs/{id}/bundle-suggestions", web::get().to(list_bundle_suggestions_handler))
            .route("/runs/{id}/removal-scenarios", web::get().to(list_removal_scenarios_handler))
            .route("/price-suggestions/{id}", web::get().to(get_price_suggestion_handler))
            .route("/bundle-suggestions/{id}", web::get().to(get_bundle_suggestion_handler))
            .route("/removal-scenarios/{id}", web::get().to(get_removal_scenario_handler))

            // Decisions & calibration
            .route("/decisions", web::post().to(record_decision_handler))
            .route("/branches/{branch_id}/decisions", web::get().to(list_decisions_handler))
            .route("/branches/{branch_id}/calibration", web::get().to(get_calibration_handler))
            .route("/bundle-suggestions/{id}/promote", web::post().to(set_bundle_promoted_handler))

            // Item-level integration
            .route(
                "/branches/{branch_id}/items/{menu_item_id}/sizes/{size_label}/latest-kpi",
                web::get().to(get_latest_item_kpi_handler),
            ),
    );
}
