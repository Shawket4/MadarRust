use crate::{auth::middleware::JwtMiddleware, uploads::handlers};
use actix_web::web;

pub fn configure(cfg: &mut web::ServiceConfig) {
    // Upload POSTs first; the GET/HEAD legacy fallback is method-guarded so it
    // never shadows them.
    for (scope, handler) in [
        ("/uploads/menu-items", "menu"),
        ("/uploads/categories", "cat"),
        ("/uploads/bundles", "bundle"),
    ] {
        let s = web::scope(scope).wrap(JwtMiddleware);
        let s = match handler {
            "menu" => s.route("/{id}", web::post().to(handlers::upload_menu_item_image)),
            "cat" => s.route("/{id}", web::post().to(handlers::upload_category_image)),
            _ => s.route("/{id}", web::post().to(handlers::upload_bundle_image)),
        };
        cfg.service(s);
    }
    crate::assets::routes::legacy_uploads(cfg);
}
