use crate::{
    auth::middleware::JwtMiddleware,
    inventory::{handlers, transfers},
};
use actix_web::web;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/inventory")
            .wrap(JwtMiddleware)
            // ── Org-level categories ───────────────────────────────────
            .route(
                "/orgs/{org_id}/categories",
                web::get().to(handlers::list_ingredient_categories),
            )
            .route(
                "/orgs/{org_id}/categories",
                web::post().to(handlers::create_ingredient_category),
            )
            .route(
                "/orgs/{org_id}/categories/{id}",
                web::patch().to(handlers::update_ingredient_category),
            )
            .route(
                "/orgs/{org_id}/categories/{id}",
                web::delete().to(handlers::delete_ingredient_category),
            )
            // ── Org-level catalog ─────────────────────────────────────
            .route(
                "/orgs/{org_id}/catalog",
                web::get().to(handlers::list_catalog),
            )
            .route(
                "/orgs/{org_id}/catalog",
                web::post().to(handlers::create_catalog_item),
            )
            .route(
                "/orgs/{org_id}/catalog/{id}",
                web::patch().to(handlers::update_catalog_item),
            )
            .route(
                "/orgs/{org_id}/catalog/{id}",
                web::delete().to(handlers::delete_catalog_item),
            )
            // ── Org-level inventory settings ──────────────────────────
            .route(
                "/orgs/{org_id}/settings",
                web::get().to(handlers::get_inventory_settings),
            )
            .route(
                "/orgs/{org_id}/settings",
                web::put().to(handlers::update_inventory_settings),
            )
            // ── Branch-level stock (read-only balances + par levels) ──
            .route(
                "/branches/{branch_id}/stock",
                web::get().to(handlers::list_branch_stock),
            )
            .route(
                "/branches/{branch_id}/stock/{org_ingredient_id}/par",
                web::put().to(handlers::set_par_levels),
            )
            // ── Movement ledger ───────────────────────────────────────
            .route(
                "/branches/{branch_id}/movements",
                web::get().to(handlers::list_movements),
            )
            // ── Waste ─────────────────────────────────────────────────
            .route(
                "/waste",
                web::post().to(crate::inventory::waste::record_waste),
            )
            .route(
                "/branches/{branch_id}/waste",
                web::post().to(handlers::create_waste),
            )
            .route(
                "/branches/{branch_id}/waste",
                web::get().to(handlers::list_waste),
            )
            // ── Transfers: requested → draft → dispatched → received ──
            .route("/transfers", web::post().to(transfers::create_transfer))
            .route("/transfers/{id}", web::get().to(transfers::get_transfer))
            .route(
                "/transfers/{id}",
                web::patch().to(transfers::update_transfer),
            )
            .route(
                "/transfers/{id}/accept",
                web::post().to(transfers::accept_transfer),
            )
            .route(
                "/transfers/{id}/decline",
                web::post().to(transfers::decline_transfer),
            )
            .route(
                "/transfers/{id}/dispatch",
                web::post().to(transfers::dispatch_transfer),
            )
            .route(
                "/transfers/{id}/receive",
                web::post().to(transfers::receive_transfer),
            )
            .route(
                "/transfers/{id}/cancel",
                web::post().to(transfers::cancel_transfer),
            )
            .route(
                "/branches/{branch_id}/transfers",
                web::get().to(transfers::list_transfers),
            )
            .route(
                "/warehouses/{warehouse_id}/replenishment",
                web::get().to(transfers::replenishment),
            )
            .route(
                "/orgs/{org_id}/transfer-differences",
                web::get().to(transfers::transfer_differences),
            ),
    );
}
