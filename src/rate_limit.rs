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

// ── Exports ──────────────────────────────────────────────────────────────────

/// The header a client sets on a read it is making in order to EXPORT.
///
/// An export is not one request. The dashboard builds its workbook in the
/// browser, so it pages the whole filtered dataset — dozens of reads for one
/// file — and a limiter counting requests would either kill a legitimate export
/// halfway or be so loose it protected nothing. Counting the export INTENT
/// instead lets one file cost one unit however many pages it took to assemble.
///
/// It is a hint from the client, and that is fine: the throttle exists to stop
/// an honest dashboard from pulling the whole database repeatedly, not to stop
/// an attacker, who would simply not send the header and be limited by every
/// other control instead.
pub const EXPORT_HEADER: &str = "X-Madar-Export";

/// Exports per user per window.
const EXPORT_MAX: usize = 5;
const EXPORT_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

fn export_max() -> usize {
    std::env::var("MADAR_EXPORT_MAX_PER_MINUTE")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| (1..=1000).contains(n))
        .unwrap_or(EXPORT_MAX)
}

/// Who has exported what, lately. Pruned as it is read, so it cannot grow past
/// the number of people who exported in the last minute.
static EXPORTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, Vec<std::time::Instant>>>,
> = std::sync::LazyLock::new(Default::default);

/// Record an export for `key` and say whether it is within the allowance.
fn allow_export(key: &str) -> bool {
    let now = std::time::Instant::now();
    let mut map = EXPORTS.lock().unwrap_or_else(|e| e.into_inner());
    map.retain(|_, times| {
        times.retain(|t| now.duration_since(*t) < EXPORT_WINDOW);
        !times.is_empty()
    });
    let times = map.entry(key.to_string()).or_default();
    if times.len() >= export_max() {
        return false;
    }
    times.push(now);
    true
}

// ── The global bucket ────────────────────────────────────────────────────────

/// Requests a client may make per minute, and the size of the bucket.
///
/// A token bucket rather than a fixed window, because a fixed window has a
/// cliff: everything is fine, and then the same request that worked a second
/// ago does not, for up to a minute, with no way to tell how long. Tokens
/// return continuously — a client that has spent its allowance can make one
/// more request roughly every third of a second rather than waiting out a
/// window it cannot see.
///
/// The bucket's capacity is a minute's worth, which is deliberately permissive:
/// this is here to stop a runaway client from taking the box down, not to pace
/// anyone's honest use of the dashboard. A page that fires forty queries on
/// load is a page we wrote, and it should not be punished for it.
const GLOBAL_PER_MINUTE: f64 = 200.0;

fn global_per_minute() -> f64 {
    std::env::var("MADAR_RATE_LIMIT_PER_MINUTE")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|n| (1.0..=100_000.0).contains(n))
        .unwrap_or(GLOBAL_PER_MINUTE)
}

/// `(tokens, last refill)` per client.
static BUCKETS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, (f64, std::time::Instant)>>,
> = std::sync::LazyLock::new(Default::default);

/// Spend a token for `key`, refilling first. False when the bucket is empty.
fn take_token(key: &str) -> bool {
    let per_minute = global_per_minute();
    let per_second = per_minute / 60.0;
    let now = std::time::Instant::now();
    let mut map = BUCKETS.lock().unwrap_or_else(|e| e.into_inner());

    // A bucket that has been full for a whole window is a client that has gone
    // away; dropping it is what keeps this map the size of the ACTIVE clients
    // rather than of everyone who has ever called.
    map.retain(|_, (tokens, last)| {
        let refilled = *tokens + now.duration_since(*last).as_secs_f64() * per_second;
        refilled < per_minute
    });

    let entry = map.entry(key.to_string()).or_insert((per_minute, now));
    let refilled =
        (entry.0 + now.duration_since(entry.1).as_secs_f64() * per_second).min(per_minute);
    entry.1 = now;
    if refilled < 1.0 {
        entry.0 = refilled;
        return false;
    }
    entry.0 = refilled - 1.0;
    true
}

/// Who this request is, for limiting: the person if we know them, the address
/// if we do not.
fn limiter_key(req: &actix_web::dev::ServiceRequest) -> String {
    crate::orgs::handlers::extract_claims(req.request())
        .ok()
        .and_then(|c| c.user_id_safe().ok())
        .map(|id| id.to_string())
        .unwrap_or_else(|| {
            req.peer_addr()
                .map(|s| s.ip().to_string())
                .unwrap_or_else(|| "unknown".into())
        })
}

/// One gate for every request: a general token bucket, and a much tighter
/// allowance for whole-dataset reads which opt out of it.
///
/// Mounted once for the whole app rather than wrapped around each route. An
/// export can come from any of them, and thirty wrappers is thirty chances to
/// add the thirty-first endpoint and forget.
pub async fn throttle_exports(
    req: actix_web::dev::ServiceRequest,
    next: actix_web::middleware::Next<impl actix_web::body::MessageBody + 'static>,
) -> Result<actix_web::dev::ServiceResponse<impl actix_web::body::MessageBody>, actix_web::Error> {
    let exporting = req
        .headers()
        .get(EXPORT_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| matches!(v, "1" | "true"));

    if rate_limiting_enabled() {
        // Per PERSON, not per address: a shop behind one office router is one
        // address and several people, and throttling them as one would make the
        // whole system feel broken for everyone but the first through the door.
        let key = limiter_key(&req);
        if exporting {
            // Exports OPT OUT of the general bucket and are counted their own
            // way. One file is dozens of paged reads, so the general limit would
            // cut a legitimate export in half; and five whole-dataset reads a
            // minute is a far tighter leash than two hundred ordinary ones,
            // which is the point.
            if !allow_export(&key) {
                return Err(crate::errors::AppError::TooManyRequests(format!(
                    "That is {} exports in a minute. Give it a moment and try again — \
                     each one reads the whole filtered dataset.",
                    export_max()
                ))
                .into());
            }
        } else if !take_token(&key) {
            return Err(crate::errors::AppError::TooManyRequests(
                "Too many requests just now. This will clear in a moment.".into(),
            )
            .into());
        }
    }
    next.call(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bucket_refills_rather_than_opening_on_the_minute() {
        // A fixed window has a cliff — the same request that worked a second
        // ago fails for up to a minute, with no way to tell how long. Tokens
        // come back continuously instead.
        let key = "bucket-test";
        let per_minute = global_per_minute();
        for _ in 0..per_minute as usize {
            assert!(take_token(key));
        }
        assert!(!take_token(key), "the bucket is empty");

        // A third of a second later there is one token, not a whole window's.
        BUCKETS.lock().unwrap().get_mut(key).unwrap().1 -= std::time::Duration::from_millis(400);
        assert!(take_token(key), "one token has come back");
        assert!(!take_token(key), "and only one");
    }

    #[test]
    fn the_allowance_is_per_person_and_recovers() {
        // Distinct keys do not spend each other's allowance — a shop behind one
        // office router is one address and several people.
        for i in 0..export_max() {
            assert!(allow_export("alice"), "alice's export {i} should pass");
        }
        assert!(!allow_export("alice"), "and the next one should not");
        assert!(allow_export("bob"), "bob has spent nothing");

        // The window is a sliding one, so a key that has aged out is forgotten
        // entirely rather than kept forever.
        EXPORTS
            .lock()
            .unwrap()
            .get_mut("alice")
            .unwrap()
            .iter_mut()
            .for_each(|t| *t -= EXPORT_WINDOW);
        assert!(
            allow_export("alice"),
            "a minute later, alice may export again"
        );
    }
}
