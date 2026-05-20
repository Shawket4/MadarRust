use actix_web::web;
use crate::{auth::middleware::JwtMiddleware, reports::handlers};

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/reports")
            .wrap(JwtMiddleware)
            .route("/shifts/{shift_id}/summary",             web::get().to(handlers::shift_summary))
            .route("/shifts/{shift_id}/inventory",           web::get().to(handlers::shift_inventory_discrepancies))
            .route("/shifts/{shift_id}/deductions",          web::get().to(handlers::shift_deductions))
            .route("/branches/{branch_id}/sales",            web::get().to(handlers::branch_sales))
            .route("/branches/{branch_id}/sales/timeseries", web::get().to(handlers::branch_sales_timeseries))
            .route("/branches/{branch_id}/tellers",          web::get().to(handlers::branch_teller_stats))
            .route("/branches/{branch_id}/addons",           web::get().to(handlers::branch_addon_sales))
            .route("/branches/{branch_id}/stock",            web::get().to(handlers::branch_stock))
            .route("/branches/{branch_id}/bundles",          web::get().to(handlers::branch_bundle_sales))
            .route("/branches/{branch_id}/items-combined",   web::get().to(handlers::branch_combined_item_sales))
            .route("/orgs/{org_id}/comparison",              web::get().to(handlers::org_branch_comparison)),
    );
}
