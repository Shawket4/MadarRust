//! `/bookings/*` (host, JWT) and `/public/…booking…` (guest, rate-limited).

use actix_governor::{Governor, GovernorConfigBuilder};
use actix_web::middleware::Condition;
use actix_web::web;

use super::{handlers, public, settings};
use crate::auth::middleware::JwtMiddleware;
use crate::rate_limit::{PeerIpOrLocalhost, rate_limiting_enabled};

pub fn configure(cfg: &mut web::ServiceConfig) {
    // Browse (info / slots / manage page): ~60/min per IP.
    let browse_gov = GovernorConfigBuilder::default()
        .key_extractor(PeerIpOrLocalhost)
        .seconds_per_request(1)
        .burst_size(60)
        .finish()
        .expect("Invalid bookings browse rate limiter");
    // Writes (book / change / cancel): ~10/min per IP.
    let write_gov = GovernorConfigBuilder::default()
        .key_extractor(PeerIpOrLocalhost)
        .seconds_per_request(6)
        .burst_size(10)
        .finish()
        .expect("Invalid bookings write rate limiter");
    let limited = rate_limiting_enabled();

    cfg.service(
        web::scope("/bookings")
            .wrap(JwtMiddleware)
            // Literal paths first so `{id}` never swallows them.
            .route("/settings", web::get().to(settings::get_settings))
            .route("/settings", web::put().to(settings::put_settings))
            .route("/availability", web::get().to(handlers::availability))
            .route("/stats", web::get().to(handlers::stats))
            .route("", web::get().to(handlers::list_bookings))
            .route("", web::post().to(handlers::create_booking))
            .route("/{id}", web::get().to(handlers::get_booking))
            .route("/{id}", web::patch().to(handlers::update_booking))
            .route("/{id}/cancel", web::post().to(handlers::cancel_booking))
            .route("/{id}/no-show", web::post().to(handlers::no_show_booking))
            .route("/{id}/seat", web::post().to(handlers::seat_booking))
            .route("/{id}/complete", web::post().to(handlers::complete_booking)),
    )
    .service(
        web::resource("/public/booking-branches")
            .wrap(Condition::new(limited, Governor::new(&browse_gov)))
            .route(web::get().to(public::booking_branches)),
    )
    .service(
        web::resource("/public/branches/{id}/booking-info")
            .wrap(Condition::new(limited, Governor::new(&browse_gov)))
            .route(web::get().to(public::booking_info)),
    )
    .service(
        web::resource("/public/branches/{id}/booking-slots")
            .wrap(Condition::new(limited, Governor::new(&browse_gov)))
            .route(web::get().to(public::booking_slots)),
    )
    .service(
        web::resource("/public/bookings")
            .wrap(Condition::new(limited, Governor::new(&write_gov)))
            .route(web::post().to(public::create_public_booking)),
    )
    .service(
        web::resource("/public/bookings/{token}")
            .wrap(Condition::new(limited, Governor::new(&browse_gov)))
            .route(web::get().to(public::get_public_booking))
            .route(web::patch().to(public::update_public_booking)),
    )
    .service(
        web::resource("/public/bookings/{token}/cancel")
            .wrap(Condition::new(limited, Governor::new(&write_gov)))
            .route(web::post().to(public::cancel_public_booking)),
    );
}
