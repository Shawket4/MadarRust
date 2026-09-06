use crate::{auth::middleware::JwtMiddleware, recipes::handlers, recipes::steps};
use actix_web::web;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/recipes")
            .wrap(JwtMiddleware)
            // ── Preparation steps, and the preset library ─────────────
            .route("/step-presets", web::get().to(steps::list_step_presets))
            .route(
                "/steps/{menu_item_id}",
                web::get().to(steps::list_recipe_steps),
            )
            .route(
                "/steps/{menu_item_id}",
                web::put().to(steps::put_recipe_steps),
            )
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
