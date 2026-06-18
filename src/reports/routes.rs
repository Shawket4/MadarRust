use actix_web::web;
use crate::{auth::middleware::JwtMiddleware, reports::handlers};

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/reports")
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
