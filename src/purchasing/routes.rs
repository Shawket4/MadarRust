use actix_web::web;
use crate::{auth::middleware::JwtMiddleware, purchasing::handlers};

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/purchasing")
            .wrap(JwtMiddleware)
            // ── Suppliers ─────────────────────────────────────────────
            .route("/orgs/{org_id}/suppliers", web::post().to(handlers::create_supplier))
            .route("/orgs/{org_id}/suppliers", web::get().to(handlers::list_suppliers))
            .route("/suppliers/{id}",          web::patch().to(handlers::update_supplier))
            .route("/suppliers/{id}",          web::delete().to(handlers::delete_supplier))
            // ── Purchase orders ───────────────────────────────────────
            .route("/branches/{branch_id}/orders", web::post().to(handlers::create_order))
            .route("/branches/{branch_id}/orders", web::get().to(handlers::list_orders))
            .route("/branches/{branch_id}/reorder-suggestions", web::get().to(handlers::reorder_suggestions))
            .route("/branches/{branch_id}/returns", web::post().to(handlers::create_return))
            .route("/orders/{id}/receipts",         web::get().to(handlers::list_po_receipts))
            .route("/orgs/{org_id}/orders",        web::get().to(handlers::list_org_orders))
            .route("/orders/{id}",                 web::get().to(handlers::get_order))
            .route("/orders/{id}/submit",          web::post().to(handlers::submit_order))
            .route("/orders/{id}/receive",         web::post().to(handlers::receive_order))
            .route("/orders/{id}/cancel",          web::post().to(handlers::cancel_order)),
    );
}
