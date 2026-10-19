use crate::{auth::middleware::JwtMiddleware, orgs::handlers, qr_card::handlers as qr_handlers};
use actix_governor::Governor;
use actix_web::{middleware::Condition, web};

use crate::rate_limit::{PeerIpOrLocalhost, rate_limiting_enabled};

pub fn configure(cfg: &mut web::ServiceConfig) {
    // Outside the JWT scope on purpose: this is the first request a customer's
    // browser makes, before any session exists, and everything it returns is
    // already on the shopfront.
    cfg.route("/public/orgs/brand", web::get().to(super::public::brand));
    cfg.route(
        "/public/orgs/links",
        web::get().to(super::links_page::public_links),
    );
    // The menu without prices: every plan, so per-IP limited like the other
    // public menus (a table card in a busy shop is scanned all evening).
    let menu_display_gov =
        crate::rate_limit::route_governor(PeerIpOrLocalhost, "MENU_DISPLAY", "BROWSE");
    cfg.service(
        web::resource("/public/menu-display")
            .wrap(Condition::new(
                rate_limiting_enabled(),
                Governor::new(&menu_display_gov),
            ))
            .route(web::get().to(super::menu_display::public_menu_display)),
    );
    cfg.route(
        "/public/orgs/favicon",
        web::get().to(super::public::favicon),
    );
    cfg.service(
        web::scope("/orgs")
            .wrap(JwtMiddleware)
            .route("", web::post().to(handlers::create_org))
            .route("", web::get().to(handlers::list_orgs))
            .route(
                "/provision",
                web::post().to(super::provision::provision_org),
            )
            .route(
                "/templates",
                web::get().to(super::provision::list_templates),
            )
            .route("/{id}", web::get().to(handlers::get_org))
            .route("/{id}/modules", web::get().to(handlers::get_org_modules))
            .route("/{id}/plan", web::get().to(crate::plan::handlers::get_plan))
            .route(
                "/{id}/plan/active-till",
                web::put().to(crate::plan::handlers::put_active_till),
            )
            .route("/{id}", web::patch().to(handlers::update_org))
            .route("/{id}", web::delete().to(handlers::delete_org))
            .route("/{id}/logo", web::put().to(handlers::upload_org_logo))
            .route(
                "/{id}/card-image",
                web::put().to(handlers::upload_org_card_image),
            )
            .route(
                "/{id}/offline-auth-bundle",
                web::get().to(handlers::offline_auth_bundle),
            )
            .route(
                "/{id}/readiness",
                web::get().to(crate::orgs::readiness::get_readiness),
            )
            .route(
                "/{id}/onboarding",
                web::get().to(crate::orgs::onboarding::get_onboarding),
            )
            .route(
                "/{id}/onboarding/complete",
                web::post().to(crate::orgs::onboarding::complete_onboarding),
            )
            .route("/{id}/qr", web::get().to(qr_handlers::org_qr))
            .route(
                "/{id}/booking-qr",
                web::get().to(qr_handlers::org_booking_qr),
            )
            .route(
                "/{id}/loyalty-qr",
                web::get().to(qr_handlers::org_loyalty_qr),
            )
            .route(
                "/{id}/links-page",
                web::get().to(super::links_page::get_links_page),
            )
            .route(
                "/{id}/links-page",
                web::put().to(super::links_page::put_links_page),
            )
            .route(
                "/{id}/links-page/menu-display",
                web::get().to(super::menu_display::get_menu_display),
            )
            .route(
                "/{id}/links-page/menu-display",
                web::put().to(super::menu_display::put_menu_display),
            )
            .route("/{id}/links-qr", web::get().to(qr_handlers::org_links_qr)),
    );
}
