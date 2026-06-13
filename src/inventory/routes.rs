use actix_web::web;
use crate::{auth::middleware::JwtMiddleware, inventory::handlers};

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/inventory")
            .wrap(JwtMiddleware)

            // ── Org-level catalog ─────────────────────────────────────
            .route("/orgs/{org_id}/catalog",     web::get().to(handlers::list_catalog))
            .route("/orgs/{org_id}/catalog",     web::post().to(handlers::create_catalog_item))
            .route("/orgs/{org_id}/catalog/{id}", web::patch().to(handlers::update_catalog_item))
            .route("/orgs/{org_id}/catalog/{id}", web::delete().to(handlers::delete_catalog_item))

            // ── Org-level inventory settings ──────────────────────────
            .route("/orgs/{org_id}/settings",    web::get().to(handlers::get_inventory_settings))
            .route("/orgs/{org_id}/settings",    web::put().to(handlers::update_inventory_settings))

            // ── Branch-level stock ────────────────────────────────────
            .route("/branches/{branch_id}/stock",     web::get().to(handlers::list_branch_stock))
            .route("/branches/{branch_id}/stock",     web::post().to(handlers::add_to_branch_stock))
            .route("/branches/{branch_id}/stock/{id}", web::patch().to(handlers::update_branch_stock))
            .route("/branches/{branch_id}/stock/{id}", web::delete().to(handlers::remove_from_branch_stock))

            // ── Adjustments ───────────────────────────────────────────
            .route("/branches/{branch_id}/adjustments", web::post().to(handlers::create_adjustment))
            .route("/branches/{branch_id}/adjustments", web::get().to(handlers::list_adjustments))

            // ── Movement ledger ───────────────────────────────────────
            .route("/branches/{branch_id}/movements", web::get().to(handlers::list_movements))

            // ── Waste ─────────────────────────────────────────────────
            .route("/branches/{branch_id}/waste", web::post().to(handlers::create_waste))
            .route("/branches/{branch_id}/waste", web::get().to(handlers::list_waste))

            // ── Transfers (always auto-applied) ───────────────────────
            .route("/transfers",                         web::post().to(handlers::create_transfer))
            .route("/transfers/{id}",                    web::patch().to(handlers::update_transfer))
            .route("/transfers/{id}",                    web::delete().to(handlers::delete_transfer))
            .route("/branches/{branch_id}/transfers",    web::get().to(handlers::list_transfers)),
    );
}