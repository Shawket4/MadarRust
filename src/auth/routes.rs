use actix_governor::{Governor, GovernorConfigBuilder, KeyExtractor, SimpleKeyExtractionError};
use actix_web::{dev::ServiceRequest, web};
use std::net::{IpAddr, Ipv4Addr};

use crate::auth::{handlers, middleware::JwtMiddleware};

/// Rate-limit by peer IP, falling back to 127.0.0.1 when no socket
/// address is available (actix test utils don't supply a real peer addr).
#[derive(Clone)]
struct PeerIpOrLocalhost;

impl KeyExtractor for PeerIpOrLocalhost {
    type Key = IpAddr;
    type KeyExtractionError = SimpleKeyExtractionError<&'static str>;

    fn extract(&self, req: &ServiceRequest) -> Result<Self::Key, Self::KeyExtractionError> {
        Ok(req
            .peer_addr()
            .map(|s| s.ip())
            .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)))
    }
}

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

    cfg.service(
        web::scope("/auth")
            // Each public endpoint gets its own rate-limited resource so it doesn't
            // shadow the JWT-protected scope below (both scopes having prefix "" would
            // cause the first scope to intercept all /auth/* requests).
            .service(
                web::resource("/login")
                    .wrap(Governor::new(&gov))
                    .route(web::post().to(handlers::login))
            )
            .service(
                web::resource("/resolve-branch")
                    .wrap(Governor::new(&gov))
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