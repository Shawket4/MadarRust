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

// ── Per-route limiters ───────────────────────────────────────────────────────

/// One per-route governor's defaults: `burst` requests at once, then one more
/// every `ms_per_request`. Each is overridable with
/// `MADAR_RL_<AREA>_<LIMITER>_BURST` and `MADAR_RL_<AREA>_<LIMITER>_MS_PER_REQUEST`.
#[derive(Debug, Clone, Copy)]
pub struct RouteLimitDef {
    pub area: &'static str,
    pub limiter: &'static str,
    pub burst: u32,
    pub ms_per_request: u64,
    /// What it guards (the `.env.example` line).
    pub guards: &'static str,
}

/// Every per-route governor in the backend, in one place, so the routes, the
/// tests and the docs read the same numbers. The owner doubled every
/// allowance on 2026-09-25 (burst ×2, refill twice as fast) and wanted each
/// one set from the environment.
pub const ROUTE_LIMITS: &[RouteLimitDef] = &[
    RouteLimitDef {
        area: "AUTH",
        limiter: "LOGIN",
        burst: 120,
        ms_per_request: 500,
        guards: "per IP: password/PIN login, staff OTP request + verify, staff session refresh",
    },
    RouteLimitDef {
        area: "AUTH",
        limiter: "ACTIVATION",
        burst: 20,
        ms_per_request: 3_000,
        guards: "per IP: device activation codes and branch resolution",
    },
    RouteLimitDef {
        area: "CUSTOMERS",
        limiter: "BROWSE",
        burst: 60,
        ms_per_request: 500,
        guards: "per IP: opening the order-now page from a card",
    },
    RouteLimitDef {
        area: "CUSTOMERS",
        limiter: "IDENTITY",
        burst: 10,
        ms_per_request: 3_000,
        guards: "per IP: changing who an order-now profile belongs to",
    },
    RouteLimitDef {
        area: "CUSTOMERS",
        limiter: "BROWSE_TOKEN",
        burst: 40,
        ms_per_request: 1_000,
        guards: "per card token: opening the order-now page",
    },
    RouteLimitDef {
        area: "CUSTOMERS",
        limiter: "IDENTITY_TOKEN",
        burst: 6,
        ms_per_request: 60_000,
        guards: "per card token: replace/combine identity",
    },
    RouteLimitDef {
        area: "LOYALTY",
        limiter: "BROWSE",
        burst: 60,
        ms_per_request: 500,
        guards: "per IP: loyalty join pages and cards",
    },
    RouteLimitDef {
        area: "LOYALTY",
        limiter: "JOIN",
        burst: 10,
        ms_per_request: 3_000,
        guards: "per IP: loyalty sign-up",
    },
    RouteLimitDef {
        area: "DEMO",
        limiter: "REQUEST",
        burst: 10,
        ms_per_request: 15_000,
        guards: "per IP: the public demo request form",
    },
    RouteLimitDef {
        area: "TICKETS",
        limiter: "TABLE_BROWSE",
        burst: 60,
        ms_per_request: 500,
        guards: "per IP: a table's QR menu",
    },
    RouteLimitDef {
        area: "TICKETS",
        limiter: "TABLE_INTAKE",
        burst: 20,
        ms_per_request: 3_000,
        guards: "per IP: orders placed from a table's QR",
    },
    RouteLimitDef {
        area: "INTEGRATIONS",
        limiter: "PARTNER",
        burst: 60,
        ms_per_request: 1_000,
        guards: "per IP: the password-authenticated partner analytics API",
    },
    RouteLimitDef {
        area: "DELIVERY",
        limiter: "BROWSE",
        burst: 60,
        ms_per_request: 500,
        guards: "per IP: the public delivery menu",
    },
    RouteLimitDef {
        area: "DELIVERY",
        limiter: "QUOTE",
        burst: 20,
        ms_per_request: 3_000,
        guards: "per IP: delivery quotes",
    },
    RouteLimitDef {
        area: "DELIVERY",
        limiter: "OTP",
        burst: 6,
        ms_per_request: 15_000,
        guards: "per IP: delivery OTP send and verify",
    },
    RouteLimitDef {
        area: "DELIVERY",
        limiter: "INTAKE",
        burst: 20,
        ms_per_request: 3_000,
        guards: "per IP: delivery order intake",
    },
    RouteLimitDef {
        area: "BOOKINGS",
        limiter: "BROWSE",
        burst: 120,
        ms_per_request: 500,
        guards: "per IP: public booking pages and availability",
    },
    RouteLimitDef {
        area: "BOOKINGS",
        limiter: "WRITE",
        burst: 20,
        ms_per_request: 3_000,
        guards: "per IP: creating or changing a public booking",
    },
];

/// A route governor's numbers after the environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteLimit {
    pub burst: u32,
    pub ms_per_request: u64,
}

/// `MADAR_RL_<AREA>_<LIMITER>_BURST` / `_MS_PER_REQUEST`, else the defaults.
/// A missing, unparsable or out-of-range value falls back, as the global
/// helpers do.
pub fn route_limit(area: &str, limiter: &str, default_burst: u32, default_ms: u64) -> RouteLimit {
    let var = |what: &str| std::env::var(format!("MADAR_RL_{area}_{limiter}_{what}")).ok();
    RouteLimit {
        burst: var("BURST")
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|n| (1..=1_000_000).contains(n))
            .unwrap_or(default_burst),
        ms_per_request: var("MS_PER_REQUEST")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|n| (1..=86_400_000).contains(n))
            .unwrap_or(default_ms),
    }
}

/// The numbers of a limiter in [`ROUTE_LIMITS`], after the environment.
pub fn limit_of(area: &str, limiter: &str) -> RouteLimit {
    let d = ROUTE_LIMITS
        .iter()
        .find(|d| d.area == area && d.limiter == limiter)
        .unwrap_or_else(|| panic!("rate limiter {area}/{limiter} is not in ROUTE_LIMITS"));
    route_limit(area, limiter, d.burst, d.ms_per_request)
}

/// One governor config from `(area, limiter, default_burst, default_ms)`.
pub fn governor<K: KeyExtractor>(
    key: K,
    area: &str,
    limiter: &str,
    default_burst: u32,
    default_ms: u64,
) -> actix_governor::GovernorConfig<K, actix_governor::governor::middleware::NoOpMiddleware> {
    let l = route_limit(area, limiter, default_burst, default_ms);
    actix_governor::GovernorConfigBuilder::default()
        .key_extractor(key)
        .milliseconds_per_request(l.ms_per_request)
        .burst_size(l.burst)
        .finish()
        .unwrap_or_else(|| panic!("invalid rate limiter {area}/{limiter}: {l:?}"))
}

/// The governor for a limiter listed in [`ROUTE_LIMITS`]: what every route uses.
pub fn route_governor<K: KeyExtractor>(
    key: K,
    area: &str,
    limiter: &str,
) -> actix_governor::GovernorConfig<K, actix_governor::governor::middleware::NoOpMiddleware> {
    let d = ROUTE_LIMITS
        .iter()
        .find(|d| d.area == area && d.limiter == limiter)
        .unwrap_or_else(|| panic!("rate limiter {area}/{limiter} is not in ROUTE_LIMITS"));
    governor(key, area, limiter, d.burst, d.ms_per_request)
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

/// Exports per user per window (`MADAR_EXPORT_MAX_PER_MINUTE`; doubled from
/// 5 on the owner's request, 2026-09-25).
const EXPORT_MAX: usize = 10;
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
///
/// `MADAR_RATE_LIMIT_PER_MINUTE`; doubled from 200 on the owner's request
/// (2026-09-25).
const GLOBAL_PER_MINUTE: f64 = 400.0;

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
///
/// `MADAR_RATE_LIMIT_PER_ADDRESS_PER_MINUTE`; doubled from 10,000 on the
/// owner's request (2026-09-25).
const PER_ADDRESS_PER_MINUTE: f64 = 20_000.0;

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
    let bearer = || {
        req.headers()
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
    };
    let secret = || req.app_data::<actix_web::web::Data<crate::auth::jwt::JwtSecret>>();
    // A staff-app phone is its own device (a verified staff token), as a
    // signed-in till is its person: keyed by address, every phone on one
    // shop's Wi-Fi shared one allowance.
    if let Some(c) = bearer()
        .zip(secret())
        .and_then(|(t, s)| crate::staff::principal::verify(s, t).ok())
    {
        return format!("staff:{}", c.dev);
    }
    crate::orgs::handlers::extract_claims(req.request())
        .ok()
        .or_else(|| {
            let token = bearer()?;
            crate::auth::jwt::verify_token(secret()?, token).ok()
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
                return Ok(too_many(
                    req,
                    format!(
                        "That is {} exports in a minute. Give it a moment and try again — \
                         each one reads the whole filtered dataset.",
                        export_max()
                    ),
                ));
            }
        } else if !take_token(&key)
            || (key != address_of(&req)
                && !take_token_at(
                    &format!("addr:{}", address_of(&req)),
                    per_address_per_minute(),
                ))
        {
            return Ok(too_many(
                req,
                "Too many requests just now. This will clear in a moment.".into(),
            ));
        }
    }
    next.call(req).await.map(|r| r.map_into_left_body())
}

/// The 429, as a RESPONSE rather than an error: an `Err` from a middleware
/// passes actix-cors without its headers, so the browser hid it and the
/// dashboard said "Network error" (E2E B-TEAM-8). Same body and code as
/// `AppError::TooManyRequests` everywhere else.
fn too_many<B>(
    req: actix_web::dev::ServiceRequest,
    why: String,
) -> actix_web::dev::ServiceResponse<actix_web::body::EitherBody<B>> {
    use actix_web::ResponseError;
    req.into_response(crate::errors::AppError::TooManyRequests(why).error_response())
        .map_into_right_body()
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
        // Small allowances, so the ceiling is reached in a few hundred calls
        // however slow the box: at the defaults (20,000 a minute, 333 a
        // second back) a loaded run could serve no faster than the refill
        // and never reach it.
        // SAFETY: nextest runs each test in its own process; nothing else reads these.
        unsafe {
            std::env::set_var("MADAR_RATE_LIMIT_PER_MINUTE", "20");
            std::env::set_var("MADAR_RATE_LIMIT_PER_ADDRESS_PER_MINUTE", "200");
        }
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
        let mut ok = 0usize;
        let started = std::time::Instant::now();
        // New accounts at one address until a FRESH account's very first
        // request is refused: its own bucket is full, so only the address
        // ceiling can refuse it. Stopping on that event rather than after a
        // fixed number of accounts keeps the test independent of how fast the
        // box runs (a fixed count failed under the full parallel run, when
        // the address bucket's refill covered the margin).
        let mut refused_fresh = false;
        for _ in 0..(ceiling / per_person) * 4 + 10 {
            let token = create_token(
                &secret,
                uuid::Uuid::new_v4(),
                None,
                UserRole::Teller,
                None,
                1,
            )
            .unwrap();
            for n in 0..per_person {
                let req = test::TestRequest::get()
                    .uri("/api/ping")
                    .insert_header(("Authorization", format!("Bearer {token}")))
                    .peer_addr("10.9.9.9:5000".parse().unwrap())
                    .to_request();
                let status = match test::try_call_service(&app, req).await {
                    Ok(r) => r.status(),
                    // The 429 is a response (B-TEAM-8), so it can carry CORS.
                    Err(e) => e.error_response().status(),
                };
                if status.is_success() {
                    ok += 1;
                    continue;
                }
                assert_eq!(status, actix_web::http::StatusCode::TOO_MANY_REQUESTS);
                if n == 0 {
                    refused_fresh = true;
                }
                break;
            }
            if refused_fresh {
                break;
            }
        }
        assert!(
            refused_fresh,
            "a fresh account at a full address is refused: the accounts did not multiply the allowance"
        );
        // The address bucket refills continuously while the loop runs.
        let refilled =
            (started.elapsed().as_secs_f64() * per_address_per_minute() / 60.0).ceil() as usize;
        assert!(
            ok <= ceiling + refilled,
            "{ok} requests passed one address, ceiling {ceiling} (+{refilled} refilled)"
        );
        assert!(
            ok >= ceiling - 1,
            "the ceiling is reached, not undercut ({ok})"
        );
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
        // Her bucket refills while the loop runs (a token every 150 ms at
        // 400 a minute), so on a loaded box the next call or two may still
        // pass; far faster than the refill, she is refused.
        let mut refused = false;
        for _ in 0..per_minute {
            let status = match test::try_call_service(&app, call(alice.clone())).await {
                Ok(r) => r.status(),
                Err(e) => e.error_response().status(),
            };
            if status == actix_web::http::StatusCode::TOO_MANY_REQUESTS {
                refused = true;
                break;
            }
        }
        assert!(refused, "alice spent hers");
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

    /// A staff-app phone is keyed by its own device, like a signed-in till,
    /// never by the address: every phone on a shop's Wi-Fi (or one mobile
    /// carrier's NAT) shared ONE allowance, and a few pull-to-refreshes got
    /// 429 — worded "can't reach the server" (owner, Android, 2026-09-25).
    #[test]
    fn a_staff_phone_is_keyed_by_its_device_not_the_address() {
        use crate::auth::jwt::JwtSecret;
        use actix_web::{test, web};
        let secret = JwtSecret("limiter-key-test-secret".into());
        let device = uuid::Uuid::new_v4();
        let (token, _) = crate::staff::principal::mint(
            &secret,
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            None,
            device,
        )
        .unwrap();
        let req = test::TestRequest::get()
            .uri("/staff/me/context")
            .insert_header(("Authorization", format!("Bearer {token}")))
            .peer_addr("10.0.0.7:5000".parse().unwrap())
            .app_data(web::Data::new(JwtSecret(secret.0.clone())))
            .to_srv_request();
        assert_eq!(limiter_key(&req), format!("staff:{device}"));
        // A staff token signed with another secret is nobody: the address.
        let (fake, _) = crate::staff::principal::mint(
            &JwtSecret("not-the-secret".into()),
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            None,
            device,
        )
        .unwrap();
        let req = test::TestRequest::get()
            .uri("/staff/me/context")
            .insert_header(("Authorization", format!("Bearer {fake}")))
            .peer_addr("10.0.0.7:5000".parse().unwrap())
            .app_data(web::Data::new(JwtSecret(secret.0.clone())))
            .to_srv_request();
        assert_eq!(limiter_key(&req), "10.0.0.7");
    }

    /// The owner's request (2026-09-25): every allowance doubled.
    #[test]
    fn the_defaults_are_the_doubled_allowances() {
        assert_eq!(GLOBAL_PER_MINUTE, 400.0);
        assert_eq!(PER_ADDRESS_PER_MINUTE, 20_000.0);
        assert_eq!(EXPORT_MAX, 10);
        // (area, limiter, burst before, seconds per request before)
        let before: &[(&str, &str, u32, u64)] = &[
            ("AUTH", "LOGIN", 60, 1),
            ("AUTH", "ACTIVATION", 10, 6),
            ("CUSTOMERS", "BROWSE", 30, 1),
            ("CUSTOMERS", "IDENTITY", 5, 6),
            ("CUSTOMERS", "BROWSE_TOKEN", 20, 2),
            ("CUSTOMERS", "IDENTITY_TOKEN", 3, 120),
            ("LOYALTY", "BROWSE", 30, 1),
            ("LOYALTY", "JOIN", 5, 6),
            ("DEMO", "REQUEST", 5, 30),
            ("TICKETS", "TABLE_BROWSE", 30, 1),
            ("TICKETS", "TABLE_INTAKE", 10, 6),
            ("INTEGRATIONS", "PARTNER", 30, 2),
            ("DELIVERY", "BROWSE", 30, 1),
            ("DELIVERY", "QUOTE", 10, 6),
            ("DELIVERY", "OTP", 3, 30),
            ("DELIVERY", "INTAKE", 10, 6),
            ("BOOKINGS", "BROWSE", 60, 1),
            ("BOOKINGS", "WRITE", 10, 6),
        ];
        assert_eq!(before.len(), ROUTE_LIMITS.len(), "every limiter is listed");
        for (area, limiter, burst, secs) in before {
            let d = ROUTE_LIMITS
                .iter()
                .find(|d| d.area == *area && d.limiter == *limiter)
                .unwrap_or_else(|| panic!("{area}/{limiter}"));
            assert_eq!(d.burst, burst * 2, "{area}/{limiter} burst");
            assert_eq!(d.ms_per_request, secs * 1000 / 2, "{area}/{limiter} refill");
        }
    }

    /// Every variable is documented in `.env.example` with its default.
    #[test]
    fn every_limit_is_documented_with_its_default() {
        let doc = include_str!("../.env.example");
        let mut want = vec![
            format!("MADAR_RATE_LIMIT_PER_MINUTE={GLOBAL_PER_MINUTE}"),
            format!("MADAR_RATE_LIMIT_PER_ADDRESS_PER_MINUTE={PER_ADDRESS_PER_MINUTE}"),
            format!("MADAR_EXPORT_MAX_PER_MINUTE={EXPORT_MAX}"),
        ];
        for d in ROUTE_LIMITS {
            want.push(format!(
                "MADAR_RL_{}_{}_BURST={}",
                d.area, d.limiter, d.burst
            ));
            want.push(format!(
                "MADAR_RL_{}_{}_MS_PER_REQUEST={}",
                d.area, d.limiter, d.ms_per_request
            ));
        }
        for w in want {
            assert!(doc.contains(&w), ".env.example lacks {w}");
        }
    }

    /// Each limiter is set from the environment; a garbage value falls back.
    /// (nextest runs each test in its own process, so these variables reach
    /// no other test.)
    #[test]
    fn every_limit_is_read_from_the_environment() {
        // SAFETY: this test's own process; nothing else reads these.
        unsafe {
            std::env::set_var("MADAR_RATE_LIMIT_PER_MINUTE", "33");
            std::env::set_var("MADAR_RATE_LIMIT_PER_ADDRESS_PER_MINUTE", "4444");
            std::env::set_var("MADAR_EXPORT_MAX_PER_MINUTE", "3");
            std::env::set_var("MADAR_RL_DELIVERY_OTP_BURST", "2");
            std::env::set_var("MADAR_RL_DELIVERY_OTP_MS_PER_REQUEST", "90000");
            std::env::set_var("MADAR_RL_BOOKINGS_WRITE_BURST", "lots");
            std::env::set_var("MADAR_RL_BOOKINGS_WRITE_MS_PER_REQUEST", "0");
        }
        assert_eq!(global_per_minute(), 33.0);
        assert_eq!(per_address_per_minute(), 4444.0);
        assert_eq!(export_max(), 3);
        assert_eq!(
            limit_of("DELIVERY", "OTP"),
            RouteLimit {
                burst: 2,
                ms_per_request: 90_000
            }
        );
        assert_eq!(
            limit_of("BOOKINGS", "WRITE"),
            RouteLimit {
                burst: 20,
                ms_per_request: 3_000
            },
            "garbage falls back to the default"
        );
        unsafe {
            std::env::set_var("MADAR_RATE_LIMIT_PER_MINUTE", "not a number");
            std::env::set_var("MADAR_EXPORT_MAX_PER_MINUTE", "-1");
        }
        assert_eq!(global_per_minute(), GLOBAL_PER_MINUTE);
        assert_eq!(export_max(), EXPORT_MAX);
    }

    /// An override reaches a real route governor: a burst of 2 on the delivery
    /// OTP limiter refuses the third call from one address.
    #[actix_web::test]
    async fn a_route_governor_takes_its_numbers_from_the_environment() {
        use actix_web::{App, HttpResponse, test, web};
        // SAFETY: this test's own process; nothing else reads it.
        unsafe {
            std::env::set_var("MADAR_RL_DELIVERY_OTP_BURST", "2");
        }
        let gov = route_governor(PeerIpOrLocalhost, "DELIVERY", "OTP");
        let app = test::init_service(
            App::new().service(
                web::resource("/otp")
                    .wrap(actix_governor::Governor::new(&gov))
                    .route(web::post().to(HttpResponse::Ok)),
            ),
        )
        .await;
        let call = || {
            test::TestRequest::post()
                .uri("/otp")
                .peer_addr("10.1.2.3:5000".parse().unwrap())
                .to_request()
        };
        for _ in 0..2 {
            assert!(test::call_service(&app, call()).await.status().is_success());
        }
        let third = match test::try_call_service(&app, call()).await {
            Ok(r) => r.status(),
            Err(e) => e.error_response().status(),
        };
        assert_eq!(third, actix_web::http::StatusCode::TOO_MANY_REQUESTS);
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

        // One and a half tokens' time later there is one token, not a whole
        // window's.
        let token_ms = 60_000.0 / per_minute;
        BUCKETS.lock().unwrap().get_mut(key).unwrap().1 -=
            std::time::Duration::from_millis((token_ms * 1.5) as u64);
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

    /// E2E B-TEAM-8: a 429 reaches the browser WITH its CORS headers, so the
    /// dashboard can say "slow down" instead of "Network error" — from the
    /// general limiter and from a route's own governor alike. This is main's
    /// stack: the limiter inside CORS.
    #[actix_web::test]
    async fn a_rate_limited_answer_carries_cors_headers() {
        use actix_governor::{Governor, GovernorConfigBuilder};
        use actix_web::{App, HttpResponse, http::StatusCode, test, web};
        let gov = GovernorConfigBuilder::default()
            .key_extractor(PeerIpOrLocalhost)
            .seconds_per_request(60)
            .burst_size(1)
            .finish()
            .unwrap();
        let app = test::init_service(
            App::new()
                .wrap(actix_web::middleware::from_fn(throttle_exports))
                .wrap(
                    actix_cors::Cors::default()
                        .allow_any_origin()
                        .allow_any_method()
                        .allow_any_header(),
                )
                .route("/public/ping", web::get().to(HttpResponse::Ok))
                .service(
                    web::resource("/login")
                        .wrap(Governor::new(&gov))
                        .route(web::post().to(HttpResponse::Ok)),
                ),
        )
        .await;
        let origin = "https://dash.example";
        let req = |m: actix_web::http::Method, uri: &str, addr: &str| {
            test::TestRequest::default()
                .method(m)
                .uri(uri)
                .insert_header(("Origin", origin))
                .peer_addr(addr.parse().unwrap())
                .to_request()
        };
        fn allow<B>(r: &actix_web::dev::ServiceResponse<B>) -> Option<String> {
            r.headers()
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        }
        // The general limiter.
        for _ in 0..global_per_minute() as usize {
            let r = test::call_service(
                &app,
                req(
                    actix_web::http::Method::GET,
                    "/public/ping",
                    "10.9.8.1:4000",
                ),
            )
            .await;
            assert!(r.status().is_success());
        }
        let r = test::call_service(
            &app,
            req(
                actix_web::http::Method::GET,
                "/public/ping",
                "10.9.8.1:4000",
            ),
        )
        .await;
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            allow(&r).as_deref(),
            Some(origin),
            "the general limiter's 429"
        );
        // A route's own governor.
        let r = test::call_service(
            &app,
            req(actix_web::http::Method::POST, "/login", "10.9.8.2:4000"),
        )
        .await;
        assert!(r.status().is_success());
        let r = test::call_service(
            &app,
            req(actix_web::http::Method::POST, "/login", "10.9.8.2:4000"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(allow(&r).as_deref(), Some(origin), "a route governor's 429");
    }
}
