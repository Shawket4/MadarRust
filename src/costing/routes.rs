use crate::{auth::middleware::JwtMiddleware, costing::handlers};
use actix_web::web;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/costing")
            .wrap(JwtMiddleware)
            .route("/menu-items", web::get().to(handlers::list_sku_costs))
            .route("/addon-items", web::get().to(handlers::list_addon_costs))
            // Paginated dashboard catalog (menu items + embedded per-SKU costs).
            // Lives here (not under /menu-items/{id}) so the static segment can't
            // be mis-parsed as a menu-item UUID.
            .route("/catalog", web::get().to(crate::menu::handlers::list_menu_catalog)),
    );
}
