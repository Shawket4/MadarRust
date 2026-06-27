use actix_governor::{Governor, GovernorConfigBuilder};
use actix_web::{middleware::Condition, web};

use crate::auth::{handlers, middleware::JwtMiddleware};
use crate::rate_limit::{rate_limiting_enabled, PeerIpOrLocalhost};

pub fn configure(cfg: &mut web::ServiceConfig) {
    // 10 req/min per IP, burst of 10.
    // seconds_per_request(6) = 1 token every 6 s → 10/min sustained.
    // In tests all requests share the 127.0.0.1 bucket; burst_size(10) means
    // the first 10 pass immediately — plenty for any single test.
    let gov = GovernorConfigBuilder::default()
        .key_extractor(PeerIpOrLocalhost)
        .seconds_per_request(6)
        .burst_size(10)
        .finish()
        .expect("Invalid rate limiter configuration");
    // Disabled wholesale by MADAR_DISABLE_RATE_LIMIT for local API fuzzing.
    let limited = rate_limiting_enabled();

    cfg.service(
        web::scope("/auth")
            // Each public endpoint gets its own rate-limited resource so it doesn't
            // shadow the JWT-protected scope below (both scopes having prefix "" would
            // cause the first scope to intercept all /auth/* requests).
            .service(
                web::resource("/login")
                    .wrap(Condition::new(limited, Governor::new(&gov)))
                    .route(web::post().to(handlers::login))
            )
            .service(
                web::resource("/resolve-branch")
                    .wrap(Condition::new(limited, Governor::new(&gov)))
                    .route(web::post().to(handlers::resolve_branch))
            )
            .service(
                web::scope("")
                    .wrap(JwtMiddleware)
                    .route("/me",          web::get().to(handlers::me))
                    .route("/permissions", web::get().to(handlers::permissions)),
            ),
    );
}