use crate::{auth::middleware::JwtMiddleware, menu::handlers::*};
use actix_web::web;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg
        // ── Categories ────────────────────────────────────────────────────────
        .service(
            web::scope("/categories")
                .wrap(JwtMiddleware)
                .route("", web::get().to(list_categories))
                .route("", web::post().to(create_category))
                .route("/{id}", web::patch().to(update_category))
                .route("/{id}", web::delete().to(delete_category)),
        )
        // ── Menu items ────────────────────────────────────────────────────────
        .service(
            web::scope("/menu-items")
                .wrap(JwtMiddleware)
                .route("", web::get().to(list_menu_items))
                .route("", web::post().to(create_menu_item))
                .route("/{id}", web::get().to(get_menu_item))
                .route("/{id}", web::patch().to(update_menu_item))
                .route("/{id}", web::delete().to(delete_menu_item))
                // Sizes
                .route("/{id}/sizes", web::post().to(upsert_size))
                .route("/{id}/sizes/{sid}", web::delete().to(delete_size))
                // Addon slots
                .route("/{id}/addon-slots", web::get().to(list_addon_slots))
                .route("/{id}/addon-slots", web::post().to(create_addon_slot))
                .route(
                    "/{id}/addon-slots/{slot_id}",
                    web::patch().to(update_addon_slot),
                )
                .route(
                    "/{id}/addon-slots/{slot_id}",
                    web::delete().to(delete_addon_slot),
                )
                // Optional fields
                .route("/{id}/optionals", web::get().to(list_optional_fields))
                .route("/{id}/optionals", web::post().to(create_optional_field))
                .route(
                    "/{id}/optionals/{field_id}",
                    web::patch().to(update_optional_field),
                )
                .route(
                    "/{id}/optionals/{field_id}",
                    web::delete().to(delete_optional_field),
                )
                // Addon overrides (per-item ingredient recipe overrides)
                .route("/{id}/overrides", web::get().to(list_addon_overrides))
                .route("/{id}/overrides", web::post().to(upsert_addon_override))
                .route(
                    "/{id}/overrides/{override_id}",
                    web::delete().to(delete_addon_override),
                )
                // Allowed addon list (per-item availability allowlist)
                .route("/{id}/allowed-addons", web::put().to(put_allowed_addons)),
        )
        // ── Branch menu overrides (per-branch price + availability) ───────────
        .service(
            web::scope("/branch-menu-overrides")
                .wrap(JwtMiddleware)
                .route("", web::get().to(list_branch_menu_overrides))
                .route("", web::put().to(upsert_branch_menu_override))
                .route("", web::delete().to(delete_branch_menu_override)),
        )
        // ── Branch addon overrides (per-branch addon price + availability) ────
        .service(
            web::scope("/branch-addon-overrides")
                .wrap(JwtMiddleware)
                .route("", web::get().to(list_branch_addon_overrides))
                .route("", web::put().to(upsert_branch_addon_override))
                .route("", web::delete().to(delete_branch_addon_override)),
        )
        // ── Addon items ───────────────────────────────────────────────────────
        .service(
            web::scope("/addon-items")
                .wrap(JwtMiddleware)
                .route("/catalog", web::get().to(list_addon_catalog))
                .route("", web::get().to(list_addon_items))
                .route("", web::post().to(create_addon_item))
                .route("/{id}", web::patch().to(update_addon_item))
                .route("/{id}", web::delete().to(delete_addon_item)),
        );
}
