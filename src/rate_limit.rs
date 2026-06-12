//! Shared rate-limiting helpers.
//!
//! `PeerIpOrLocalhost` keys the `actix-governor` limiter by the client's peer
//! IP, falling back to 127.0.0.1 when no socket address is available (actix
//! test utilities don't supply a real peer addr). Shared so the auth and
//! public-menu endpoints limit on the same key type.

use actix_governor::{KeyExtractor, SimpleKeyExtractionError};
use actix_web::dev::ServiceRequest;
use std::net::{IpAddr, Ipv4Addr};

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
