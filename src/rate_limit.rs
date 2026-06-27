//! Shared rate-limiting helpers.
//!
//! `PeerIpOrLocalhost` keys the `actix-governor` limiter by the client's peer
//! IP, falling back to 127.0.0.1 when no socket address is available (actix
//! test utilities don't supply a real peer addr). Shared so the auth and
//! public-menu endpoints limit on the same key type.

use actix_governor::{KeyExtractor, SimpleKeyExtractionError};
use actix_web::dev::ServiceRequest;
use std::net::{IpAddr, Ipv4Addr};

/// Rate limiting is ON by default. Set `MADAR_DISABLE_RATE_LIMIT=1` (or `=true`)
/// to turn it off — used by the local API-fuzz harness (scripts/api-fuzz.sh) so
/// the fuzzer isn't throttled to a wall of 429s. Never set this in production.
pub fn rate_limiting_enabled() -> bool {
    !matches!(
        std::env::var("MADAR_DISABLE_RATE_LIMIT").as_deref(),
        Ok("1") | Ok("true")
    )
}

#[derive(Clone)]
pub struct PeerIpOrLocalhost;

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
