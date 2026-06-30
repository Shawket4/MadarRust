//! Demo route registration. Mounted only when `DEMO_MODE` is on (see main.rs).
//! `/demo/session` is unauthenticated, so it's rate-limited per-IP to blunt
//! org-flooding; the live-org cap in the handler is the second line of defence.

use actix_governor::{Governor, GovernorConfigBuilder};
use actix_web::middleware::Condition;
use actix_web::web;

use crate::rate_limit::{PeerIpOrLocalhost, rate_limiting_enabled};

use super::handlers;

pub fn configure(cfg: &mut web::ServiceConfig) {
    // ~1 request / 30s per IP, small burst — generous for a real visitor,
    // hostile to a flood. Disabled wholesale by MADAR_DISABLE_RATE_LIMIT.
    let gov = GovernorConfigBuilder::default()
        .key_extractor(PeerIpOrLocalhost)
        .seconds_per_request(30)
        .burst_size(5)
        .finish()
        .expect("Invalid demo rate limiter configuration");
    let limited = rate_limiting_enabled();

    cfg.service(
        web::scope("/demo").service(
            web::resource("/session")
                .wrap(Condition::new(limited, Governor::new(&gov)))
                .route(web::post().to(handlers::create_session)),
        ),
    );
}
