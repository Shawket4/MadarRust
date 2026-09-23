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

/// Keys a limiter by the `{token}` segment of the matched route, so a budget
/// belongs to ONE card whatever address asks (design §4.2). The per-IP limiter
/// stops one machine; this stops a botnet working on one card — where every
/// address is fresh and the per-IP bucket is always full. Wrap it INSIDE the
/// per-IP limiter, so the addresses that can mint new keys here are themselves
/// bounded. A route with no `{token}` shares one bucket, which is a loud
/// misconfiguration rather than a silent hole.
#[derive(Clone)]
pub struct PathToken;

impl KeyExtractor for PathToken {
    type Key = String;
    type KeyExtractionError = SimpleKeyExtractionError<&'static str>;

    fn extract(&self, req: &ServiceRequest) -> Result<Self::Key, Self::KeyExtractionError> {
        let token = req.match_info().get("token").unwrap_or("");
        // A key is stored per distinct value: never let a caller choose how
        // much memory one costs. Real tokens are far shorter than this.
        let end = token.char_indices().nth(96).map_or(token.len(), |(i, _)| i);
        Ok(token[..end].to_string())
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
    std::sync::Mutex<std::collections::HashMap<String, (f64, std::time::Instant, f64)>>,
> = std::sync::LazyLock::new(Default::default);

/// The ceiling for one ADDRESS across every account behind it. The person
/// bucket is what an honest till spends; this one stops a single address from
/// multiplying the allowance by holding many accounts (N x 200).
///
/// **Fifty tills' worth** (locked owner decision, 2026-09-15). It was ten, on
/// the assumption that a shop behind one router is a shop with a few tills.
/// That is wrong for the customers this is being built for: fifty tablets on
/// one NAT is a real deployment, and they all leave through a single public
/// address. At ten tills' worth the fortieth tablet started getting 429s during
/// the morning rush — not because anything was runaway, but because the ceiling
/// was counting a whole branch as one client.
///
/// The PER-PERSON bucket is deliberately unchanged. That is the one that
/// actually catches a runaway client, and it is per account, so raising the
/// address ceiling does not loosen it: fifty honest tills each stay inside
/// their own 200, and one broken tablet is still stopped on its own.
const PER_ADDRESS_PER_MINUTE: f64 = 10_000.0;

fn per_address_per_minute() -> f64 {
    std::env::var("MADAR_RATE_LIMIT_PER_ADDRESS_PER_MINUTE")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|n| (1.0..=1_000_000.0).contains(n))
        .unwrap_or(PER_ADDRESS_PER_MINUTE)
}

/// Spend a token for `key` from the general bucket. False when it is empty.
fn take_token(key: &str) -> bool {
    take_token_at(key, global_per_minute())
}

/// Spend a token for `key` from a bucket of `per_minute`, refilling first.
fn take_token_at(key: &str, per_minute: f64) -> bool {
    let per_second = per_minute / 60.0;
    let now = std::time::Instant::now();
    let mut map = BUCKETS.lock().unwrap_or_else(|e| e.into_inner());

    // A bucket that has been full for a whole window is a client that has gone
    // away; dropping it is what keeps this map the size of the ACTIVE clients
    // rather than of everyone who has ever called.
    map.retain(|_, (tokens, last, cap)| {
        let refilled = *tokens + now.duration_since(*last).as_secs_f64() * (*cap / 60.0);
        refilled < *cap
    });

    let entry = map
        .entry(key.to_string())
        .or_insert((per_minute, now, per_minute));
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

/// The caller's address (`unknown` without a socket).
fn address_of(req: &actix_web::dev::ServiceRequest) -> String {
    req.peer_addr()
        .map(|s| s.ip().to_string())
        .unwrap_or_else(|| "unknown".into())
}

/// Who this request is, for limiting: the person if we know them, the address
/// if we do not.
///
/// This gate is mounted on the App, OUTSIDE the scopes' `JwtMiddleware`, so the
/// claims that middleware inserts are not there yet when it runs: reading only
/// the extensions keyed EVERY request by its peer address, and every till,
/// kitchen screen and phone behind one shop router spent one shared allowance
/// (found replaying a 1000-sale offline backlog, which paced the other tills'
/// sign-ins). The bearer is verified here instead — verified, not just decoded,
/// so a forged token cannot choose whose allowance it spends.
fn limiter_key(req: &actix_web::dev::ServiceRequest) -> String {
    crate::orgs::handlers::extract_claims(req.request())
        .ok()
        .or_else(|| {
            let token = req
                .headers()
                .get("Authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))?;
            let secret = req.app_data::<actix_web::web::Data<crate::auth::jwt::JwtSecret>>()?;
            crate::auth::jwt::verify_token(secret, token).ok()
        })
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
        } else if !take_token(&key)
            || (key != address_of(&req)
                && !take_token_at(
                    &format!("addr:{}", address_of(&req)),
                    per_address_per_minute(),
                ))
        {
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

    /// An anonymous caller has no person, so the ADDRESS is the key, through the
    /// whole middleware stack: one address runs dry, another does not.
    #[actix_web::test]
    async fn anonymous_requests_are_keyed_by_address() {
        use actix_web::{App, HttpResponse, test, web};
        let app = test::init_service(
            App::new()
                .wrap(actix_web::middleware::from_fn(throttle_exports))
                .route("/public/ping", web::get().to(HttpResponse::Ok)),
        )
        .await;
        let call = |addr: &str| {
            test::TestRequest::get()
                .uri("/public/ping")
                .peer_addr(addr.parse().unwrap())
                .to_request()
        };
        macro_rules! status {
            ($r:expr) => {
                match $r {
                    Ok(r) => r.status(),
                    Err(e) => e.error_response().status(),
                }
            };
        }
        for _ in 0..global_per_minute() as usize {
            assert!(
                status!(test::try_call_service(&app, call("10.9.0.1:4000")).await).is_success()
            );
        }
        assert_eq!(
            status!(test::try_call_service(&app, call("10.9.0.1:4001")).await),
            actix_web::http::StatusCode::TOO_MANY_REQUESTS,
            "a new port on the same address is the same caller"
        );
        assert!(
            status!(test::try_call_service(&app, call("10.9.0.2:4000")).await).is_success(),
            "another address is not"
        );
    }

    /// Many accounts behind one address cannot multiply the allowance past the
    /// address ceiling.
    #[actix_web::test]
    async fn many_accounts_share_one_address_ceiling() {
        use crate::auth::jwt::{JwtSecret, create_token};
        use crate::models::UserRole;
        use actix_web::{App, HttpResponse, test, web};
        let secret = JwtSecret("address-ceiling-test-secret".into());
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(JwtSecret(secret.0.clone())))
                .wrap(actix_web::middleware::from_fn(throttle_exports))
                .service(
                    web::scope("/api")
                        .wrap(crate::auth::middleware::JwtMiddleware)
                        .route("/ping", web::get().to(HttpResponse::Ok)),
                ),
        )
        .await;
        let per_person = global_per_minute() as usize;
        let ceiling = per_address_per_minute() as usize;
        // Enough extra accounts that the bucket's refill during a slow run
        // (~167 a second at 10,000 a minute) cannot cover them: with only +2
        // (400 requests) a run slower than ~2.4 s — a loaded CI box — let
        // every request through and failed "did not multiply".
        let accounts = ceiling / per_person + 10;
        let mut ok = 0usize;
        let mut paced = 0usize;
        let started = std::time::Instant::now();
        for _ in 0..accounts {
            let token = create_token(
                &secret,
                uuid::Uuid::new_v4(),
                None,
                UserRole::Teller,
                None,
                1,
            )
            .unwrap();
            for _ in 0..per_person {
                let req = test::TestRequest::get()
                    .uri("/api/ping")
                    .insert_header(("Authorization", format!("Bearer {token}")))
                    .peer_addr("10.9.9.9:5000".parse().unwrap())
                    .to_request();
                match test::try_call_service(&app, req).await {
                    Ok(r) if r.status().is_success() => ok += 1,
                    Ok(r) => assert_eq!(r.status(), actix_web::http::StatusCode::TOO_MANY_REQUESTS),
                    Err(e) => {
                        assert_eq!(
                            e.error_response().status(),
                            actix_web::http::StatusCode::TOO_MANY_REQUESTS
                        );
                        paced += 1;
                    }
                }
            }
        }
        // The address bucket refills continuously while the loop runs.
        let refilled =
            (started.elapsed().as_secs_f64() * per_address_per_minute() / 60.0).ceil() as usize;
        assert!(
            ok <= ceiling + refilled,
            "{ok} requests passed one address, ceiling {ceiling} (+{refilled} refilled)"
        );
        assert!(
            ok < accounts * per_person,
            "the accounts did not multiply the allowance"
        );
        assert!(
            ok >= ceiling - 1,
            "the ceiling is reached, not undercut ({ok})"
        );
        assert!(paced > 0);
    }

    /// Two people at one address each get their own allowance, through the
    /// real middleware stack order (the gate on the App, the JWT check inside a
    /// scope); an anonymous caller is still keyed by address.
    #[actix_web::test]
    async fn people_behind_one_address_do_not_share_an_allowance() {
        use crate::auth::jwt::{JwtSecret, create_token};
        use crate::models::UserRole;
        use actix_web::{App, HttpResponse, test, web};
        let secret = JwtSecret("limiter-key-test-secret".into());
        let token = |id| create_token(&secret, id, None, UserRole::Teller, None, 1).unwrap();
        let (alice, bob) = (token(uuid::Uuid::new_v4()), token(uuid::Uuid::new_v4()));
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(JwtSecret(secret.0.clone())))
                .wrap(actix_web::middleware::from_fn(throttle_exports))
                .service(
                    web::scope("/api")
                        .wrap(crate::auth::middleware::JwtMiddleware)
                        .route("/ping", web::get().to(HttpResponse::Ok)),
                ),
        )
        .await;
        let call = |bearer: String| {
            test::TestRequest::get()
                .uri("/api/ping")
                .insert_header(("Authorization", format!("Bearer {bearer}")))
                .peer_addr("10.0.0.7:5000".parse().unwrap())
                .to_request()
        };
        let per_minute = global_per_minute() as usize;
        for _ in 0..per_minute {
            let r = test::call_service(&app, call(alice.clone())).await;
            assert!(r.status().is_success());
        }
        let paced = test::try_call_service(&app, call(alice.clone())).await;
        let status = match paced {
            Ok(r) => r.status(),
            Err(e) => e.error_response().status(),
        };
        assert_eq!(
            status,
            actix_web::http::StatusCode::TOO_MANY_REQUESTS,
            "alice spent hers"
        );
        let r = test::call_service(&app, call(bob.clone())).await;
        assert!(
            r.status().is_success(),
            "bob, at the same address, has spent nothing"
        );
        // A forged bearer is not a person: it is keyed by the address.
        let forged = JwtSecret("not-the-secret".into());
        let fake = create_token(
            &forged,
            uuid::Uuid::new_v4(),
            None,
            UserRole::Teller,
            None,
            1,
        )
        .unwrap();
        let req = test::TestRequest::get()
            .uri("/api/ping")
            .insert_header(("Authorization", format!("Bearer {fake}")))
            .peer_addr("10.0.0.7:5000".parse().unwrap())
            .app_data(web::Data::new(JwtSecret(secret.0.clone())))
            .to_srv_request();
        assert_eq!(limiter_key(&req), "10.0.0.7");
    }

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
