use actix_web::web;
use sqlx::PgPool;
use crate::{auth::middleware::JwtMiddleware, reports::handlers};

/// `read_pool` overrides the app-wide write pool for THIS scope only: every reports
/// handler extracts `web::Data<PgPool>`, and actix resolves scope-level `app_data`
/// ahead of app-level, so they all run against the read replica (when
/// `READ_DATABASE_URL` is set) with zero per-handler changes. Reports are read-only.
pub fn configure(cfg: &mut web::ServiceConfig, read_pool: web::Data<PgPool>) {
    cfg.service(
        web::scope("/reports")
            .app_data(read_pool)
            .wrap(JwtMiddleware)
            .route("/shifts/{shift_id}/summary",             web::get().to(handlers::shift_summary))
            .route("/shifts/{shift_id}/deductions",          web::get().to(handlers::shift_deductions))
            .route("/branches/{branch_id}/sales",            web::get().to(handlers::branch_sales))
            .route("/branches/{branch_id}/sales/timeseries", web::get().to(handlers::branch_sales_timeseries))
            .route("/branches/{branch_id}/sales/peak-hours", web::get().to(handlers::branch_sales_peak_hours))
            .route("/branches/{branch_id}/tellers",          web::get().to(handlers::branch_teller_stats))
            .route("/branches/{branch_id}/addons",           web::get().to(handlers::branch_addon_sales))
            .route("/branches/{branch_id}/stock",            web::get().to(handlers::branch_stock))
            .route("/branches/{branch_id}/bundles",          web::get().to(handlers::branch_bundle_sales))
            .route("/branches/{branch_id}/items-combined",   web::get().to(handlers::branch_combined_item_sales))
            .route("/branches/{branch_id}/menu-engineering",  web::get().to(handlers::branch_menu_engineering))
            .route("/branches/{branch_id}/inventory-valuation", web::get().to(handlers::branch_inventory_valuation))
            .route("/branches/{branch_id}/consumption",      web::get().to(handlers::branch_consumption))
            .route("/branches/{branch_id}/waste-report",     web::get().to(handlers::branch_waste_report))
            .route("/branches/{branch_id}/shrinkage",        web::get().to(handlers::branch_shrinkage))
            .route("/branches/{branch_id}/low-stock",        web::get().to(handlers::branch_low_stock))
            .route("/branches/{branch_id}/delivery-sales",   web::get().to(handlers::branch_delivery_sales))
            .route("/orgs/{org_id}/comparison",              web::get().to(handlers::org_branch_comparison))
            .route("/orgs/{org_id}/inventory-valuation",     web::get().to(handlers::org_inventory_valuation))
            .route("/orgs/{org_id}/low-stock",               web::get().to(handlers::org_low_stock))
            .route("/orgs/{org_id}/consumption",             web::get().to(handlers::org_consumption))
            .route("/orgs/{org_id}/waste-report",            web::get().to(handlers::org_waste_report))
            .route("/orgs/{org_id}/shrinkage",               web::get().to(handlers::org_shrinkage)),
    );
}
