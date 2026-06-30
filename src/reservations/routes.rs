//! Reservations routes.
//!
//! - `/floor/*` (managers) — section + table geometry authoring, reservation
//!   settings, and live table status.
//! - `/reservations/*` (host/teller) — booking operations.
//! - `/public/reservations/*` (unauthenticated, per-IP rate-limited) — guest
//!   self-booking. The guest first verifies their phone via the existing
//!   `/public/otp/*` delivery endpoints, then posts a booking with the resulting
//!   device-trust token.

use actix_governor::{Governor, GovernorConfigBuilder};
use actix_web::{middleware::Condition, web};

use crate::auth::middleware::JwtMiddleware;
use crate::rate_limit::{PeerIpOrLocalhost, rate_limiting_enabled};
use crate::reservations::{bookings, floor, public};

pub fn configure(cfg: &mut web::ServiceConfig) {
    // Browsing (branch list, track): ~60/min sustained, burst 30.
    let browse_gov = GovernorConfigBuilder::default()
        .key_extractor(PeerIpOrLocalhost)
        .seconds_per_request(1)
        .burst_size(30)
        .finish()
        .expect("Invalid reservations browse rate limiter");
    // Intake (create booking): ~10/min per IP, burst 10.
    let intake_gov = GovernorConfigBuilder::default()
        .key_extractor(PeerIpOrLocalhost)
        .seconds_per_request(6)
        .burst_size(10)
        .finish()
        .expect("Invalid reservations intake rate limiter");
    let limited = rate_limiting_enabled();

    cfg
        // ── Floor authoring + settings + live status (managers/host) ─────
        .service(
            web::scope("/floor")
                .wrap(JwtMiddleware)
                .route("/sections", web::get().to(floor::list_sections))
                .route("/sections", web::post().to(floor::create_section))
                .route("/sections/{id}", web::patch().to(floor::update_section))
                .route("/sections/{id}", web::delete().to(floor::delete_section))
                .route("/tables", web::get().to(floor::list_tables))
                .route("/tables", web::post().to(floor::create_table))
                .route("/tables/{id}", web::patch().to(floor::update_table))
                .route("/tables/{id}", web::delete().to(floor::delete_table))
                .route(
                    "/tables/{id}/status",
                    web::patch().to(floor::set_table_status),
                )
                .route("/layout", web::put().to(floor::save_layout))
                .route(
                    "/reservation-settings",
                    web::get().to(floor::get_reservation_settings),
                )
                .route(
                    "/reservation-settings",
                    web::put().to(floor::put_reservation_settings),
                ),
        )
        // ── Booking host operations ──────────────────────────────────────
        .service(
            web::scope("/reservations")
                .wrap(JwtMiddleware)
                .route("", web::get().to(bookings::list_bookings))
                .route("", web::post().to(bookings::create_booking))
                .route("/{id}", web::patch().to(bookings::update_booking))
                .route("/{id}/assign", web::post().to(bookings::assign_tables))
                .route("/{id}/notify", web::post().to(bookings::notify_booking)),
        )
        // ── Public self-booking (unauthenticated, rate-limited) ──────────
        .service(
            web::resource("/public/reservations/branches")
                .wrap(Condition::new(limited, Governor::new(&browse_gov)))
                .route(web::get().to(public::public_branches)),
        )
        .service(
            web::resource("/public/reservations")
                .wrap(Condition::new(limited, Governor::new(&intake_gov)))
                .route(web::post().to(public::create_public_booking)),
        )
        .service(
            web::resource("/public/reservations/{id}")
                .wrap(Condition::new(limited, Governor::new(&browse_gov)))
                .route(web::get().to(public::track_public_booking)),
        );
}
