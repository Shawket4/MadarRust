use crate::{auth::middleware::JwtMiddleware, costing::handlers};
use actix_web::web;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/costing")
            .wrap(JwtMiddleware)
            .route("/menu-items", web::get().to(handlers::list_sku_costs))
            .route("/addon-items", web::get().to(handlers::list_addon_costs)),
    );
}
