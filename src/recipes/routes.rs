use crate::{auth::middleware::JwtMiddleware, recipes::handlers};
use actix_web::web;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/recipes")
            .wrap(JwtMiddleware)
            // ── Drink recipes (per item + size) ───────────────────────
            .route(
                "/drinks/{menu_item_id}",
                web::get().to(handlers::list_drink_recipes),
            )
            .route(
                "/drinks/{menu_item_id}",
                web::post().to(handlers::upsert_drink_recipe),
            )
            .route(
                "/drinks/{menu_item_id}/{size}",
                web::delete().to(handlers::delete_drink_recipe),
            )
            // ── Addon base ingredients ────────────────────────────────
            .route(
                "/addons/{addon_item_id}",
                web::get().to(handlers::list_addon_ingredients),
            )
            .route(
                "/addons/{addon_item_id}",
                web::post().to(handlers::upsert_addon_ingredient),
            )
            .route(
                "/addons/{addon_item_id}",
                web::delete().to(handlers::delete_addon_ingredient),
            ),
    );
}
