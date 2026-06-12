use actix_governor::{Governor, GovernorConfigBuilder};
use actix_web::web;
use crate::{auth::middleware::JwtMiddleware, menu::handlers::*};
use crate::rate_limit::PeerIpOrLocalhost;

pub fn configure(cfg: &mut web::ServiceConfig) {
    // Public (unauthenticated) menu endpoint: limit per IP to deter scraping
    // and enumeration. ~60 req/min sustained with a burst of 30 — generous for
    // real customers refreshing a QR menu, tight enough to blunt abuse.
    let public_menu_gov = GovernorConfigBuilder::default()
        .key_extractor(PeerIpOrLocalhost)
        .seconds_per_request(1)
        .burst_size(30)
        .finish()
        .expect("Invalid public menu rate limiter configuration");

    cfg
        // ── Categories ────────────────────────────────────────────────────────
        .service(
            web::scope("/categories")
                .wrap(JwtMiddleware)
                .route("",      web::get().to(list_categories))
                .route("",      web::post().to(create_category))
                .route("/{id}", web::patch().to(update_category))
                .route("/{id}", web::delete().to(delete_category)),
        )

        // ── Menu items ────────────────────────────────────────────────────────
        .service(
            web::scope("/menu-items")
                .wrap(JwtMiddleware)
                .route("",      web::get().to(list_menu_items))
                .route("",      web::post().to(create_menu_item))
                .route("/{id}", web::get().to(get_menu_item))
                .route("/{id}", web::patch().to(update_menu_item))
                .route("/{id}", web::delete().to(delete_menu_item))

                // Sizes
                .route("/{id}/sizes",       web::post().to(upsert_size))
                .route("/{id}/sizes/{sid}", web::delete().to(delete_size))

                // Addon slots
                .route("/{id}/addon-slots",           web::get().to(list_addon_slots))
                .route("/{id}/addon-slots",           web::post().to(create_addon_slot))
                .route("/{id}/addon-slots/{slot_id}", web::patch().to(update_addon_slot))
                .route("/{id}/addon-slots/{slot_id}", web::delete().to(delete_addon_slot))

                // Optional fields
                .route("/{id}/optionals",              web::get().to(list_optional_fields))
                .route("/{id}/optionals",              web::post().to(create_optional_field))
                .route("/{id}/optionals/{field_id}",   web::patch().to(update_optional_field))
                .route("/{id}/optionals/{field_id}",   web::delete().to(delete_optional_field))

                // Addon overrides
                .route("/{id}/overrides",                web::get().to(list_addon_overrides))
                .route("/{id}/overrides",                web::post().to(upsert_addon_override))
                .route("/{id}/overrides/{override_id}",  web::delete().to(delete_addon_override)),
        )

        // ── Public Menu (unauthenticated, rate-limited) ───────────────────────
        .service(
            web::resource("/menu/public/{org_id}")
                .wrap(Governor::new(&public_menu_gov))
                .route(web::get().to(get_public_menu)),
        )

        // ── Addon items ───────────────────────────────────────────────────────
        .service(
            web::scope("/addon-items")
                .wrap(JwtMiddleware)
                .route("",      web::get().to(list_addon_items))
                .route("",      web::post().to(create_addon_item))
                .route("/{id}", web::patch().to(update_addon_item))
                .route("/{id}", web::delete().to(delete_addon_item)),
        );
}
