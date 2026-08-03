use actix_governor::{Governor, GovernorConfigBuilder};
use actix_web::{middleware::Condition, web};

use crate::auth::middleware::JwtMiddleware;
use crate::integrations::handlers;
use crate::rate_limit::{PeerIpOrLocalhost, rate_limiting_enabled};

pub fn configure(cfg: &mut web::ServiceConfig) {
    // The partner endpoint takes a password on every request, so it is
    // brute-forceable in a way JWT-protected routes are not. bcrypt's work
    // factor already makes guessing expensive; this caps the attempt rate on
    // top. 30/min sustained (a token every 2 s) with a burst of 30 is far more
    // than any analytics pull needs — partners poll hourly, not per second.
    let gov = GovernorConfigBuilder::default()
        .key_extractor(PeerIpOrLocalhost)
        .seconds_per_request(2)
        .burst_size(30)
        .finish()
        .expect("Invalid rate limiter configuration");
    let limited = rate_limiting_enabled();

    cfg.service(
        web::scope("/integrations")
            // Distinct, non-empty prefixes: a scope with prefix "" would
            // swallow every /integrations/* request before the sibling scope
            // could match (see the same note in auth/routes.rs).
            .service(
                web::scope("/analytics")
                    .wrap(Condition::new(limited, Governor::new(&gov)))
                    .route("/orders", web::get().to(handlers::analytics_orders)),
            )
            .service(
                web::scope("/credentials")
                    .wrap(JwtMiddleware)
                    .route("", web::get().to(handlers::list_credentials))
                    .route("", web::post().to(handlers::create_credential))
                    .route("/{id}", web::delete().to(handlers::revoke_credential))
                    .route("/{id}/rotate", web::post().to(handlers::rotate_credential)),
            ),
    );
}
