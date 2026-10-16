//! The minimum-client gate (DB_ARCHITECTURE_AUDIT_2026-10-16 §7.2).
//!
//! **Inert unless the owner inserts a `client_version_policy` row.** No row is
//! seeded, no default minimum exists in code or env, and with no row every
//! request passes through untouched, whatever its `X-Madar-Client` says (owner,
//! 2026-10-16: tills are never forced to update by a deploy).
//!
//! With a row for the client's app (`pos` / `kds`), a request whose
//! `X-Madar-Client` names a version:
//! * below `min_recommended` — answered as normal, plus the header
//!   `X-Madar-Upgrade: recommended`;
//! * below `min_supported`:
//!   * every WRITE still goes through (`/sync/replay`, orders, cash
//!     movements, refunds, till close — anything that is not a gated read),
//!     with `X-Madar-Upgrade: required`: a till holding offline sales must
//!     always be able to flush them, and keeps selling offline;
//!   * the reads that keep an old build running — `POST /sync/pull` and the
//!     catalog reads (`GET /menu-items`, `/catalog/sync`, `/addon-items`,
//!     `/categories`) — answer 426 `CLIENT_UPGRADE_REQUIRED` with
//!     `{min_supported, download_url}`.
//!
//! A request with no `X-Madar-Client`, a browser, or a client whose version
//! cannot be read is never gated. Policies are read with the owner pool and
//! cached for 60 s; a failed read gates nothing.
//!
//! Row matching: the most specific row wins — the caller's org over a global
//! row (`org_id` NULL), a platform row over an all-platform one (`platform`
//! NULL). The org comes from the bearer token, verified here.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use actix_web::body::{EitherBody, MessageBody};
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::http::header::{HeaderName, HeaderValue};
use actix_web::middleware::Next;
use actix_web::{HttpResponse, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::devices::{CLIENT_HEADER, ClientHeader};

/// The response header naming an upgrade: `recommended` or `required`.
pub const UPGRADE_HEADER: &str = "x-madar-upgrade";

/// How long the policy rows are cached.
pub const CACHE_TTL: Duration = Duration::from_secs(60);

type Semver = (u64, u64, u64);

/// One `client_version_policy` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub app: String,
    pub platform: Option<String>,
    pub org_id: Option<Uuid>,
    pub min_supported: Option<Semver>,
    pub min_recommended: Option<Semver>,
    pub download_url: Option<String>,
}

/// What the gate does with one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Untouched: the request and its response exactly as without the gate.
    Pass,
    /// Served as normal, with `X-Madar-Upgrade: recommended|required`.
    Nudge(&'static str),
    /// 426 Upgrade Required, without running the handler.
    Refuse {
        min_supported: String,
        download_url: Option<String>,
    },
}

fn parse_semver(v: &str) -> Option<Semver> {
    ClientHeader::parse(Some(&format!("x/{v}"))).version
}

fn show(v: Semver) -> String {
    format!("{}.{}.{}", v.0, v.1, v.2)
}

/// The reads a below-minimum client is refused: what keeps an old build's
/// catalogue and changefeed going. Everything else (every write above all) is
/// never refused.
pub fn is_gated_read(method: &actix_web::http::Method, path: &str) -> bool {
    use actix_web::http::Method;
    let path = path.trim_end_matches('/');
    if *method == Method::POST {
        return path == "/sync/pull";
    }
    *method == Method::GET
        && matches!(
            path,
            "/menu-items" | "/catalog/sync" | "/addon-items" | "/addon-items/catalog" | "/categories"
        )
}

/// The gate's decision, a pure function of the policies and the request.
pub fn decide(
    policies: &[Policy],
    client: &ClientHeader,
    org: Option<Uuid>,
    gated_read: bool,
) -> Verdict {
    if policies.is_empty() {
        return Verdict::Pass;
    }
    let (Some(app), Some(version)) = (client.app.as_deref(), client.version) else {
        return Verdict::Pass;
    };
    let platform = client.platform.as_deref().map(str::to_ascii_lowercase);
    let best = policies
        .iter()
        .filter(|p| p.app == app)
        .filter(|p| p.org_id.is_none() || p.org_id == org)
        .filter(|p| {
            p.platform.is_none()
                || platform
                    .as_deref()
                    .is_some_and(|pl| pl.contains(&p.platform.as_deref().unwrap().to_ascii_lowercase()))
        })
        .max_by_key(|p| (p.org_id.is_some(), p.platform.is_some()));
    let Some(p) = best else {
        return Verdict::Pass;
    };
    if let Some(min) = p.min_supported
        && version < min
    {
        return if gated_read {
            Verdict::Refuse {
                min_supported: show(min),
                download_url: p.download_url.clone(),
            }
        } else {
            Verdict::Nudge("required")
        };
    }
    if let Some(rec) = p.min_recommended
        && version < rec
    {
        return Verdict::Nudge("recommended");
    }
    Verdict::Pass
}

struct Cached {
    at: Instant,
    rows: Vec<Policy>,
}

fn cache() -> &'static Mutex<Option<Cached>> {
    static C: OnceLock<Mutex<Option<Cached>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

/// Forget the cached policies (tests; an operator change shows within 60 s).
pub fn forget_cache() {
    if let Ok(mut c) = cache().lock() {
        *c = None;
    }
}

/// Every policy row, cached for [`CACHE_TTL`]. A failed read is "no policy".
pub async fn policies(pool: &PgPool) -> Vec<Policy> {
    if let Ok(c) = cache().lock()
        && let Some(c) = c.as_ref()
        && c.at.elapsed() < CACHE_TTL
    {
        return c.rows.clone();
    }
    let rows = match sqlx::query!(
        "SELECT app, platform, org_id, min_supported, min_recommended, download_url
           FROM client_version_policy"
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|r| Policy {
                app: r.app,
                platform: r.platform,
                org_id: r.org_id,
                min_supported: r.min_supported.as_deref().and_then(parse_semver),
                min_recommended: r.min_recommended.as_deref().and_then(parse_semver),
                download_url: r.download_url,
            })
            .collect(),
        Err(e) => {
            tracing::debug!(target: "madar.version_gate", error = %e, "client_version_policy read failed; gating nothing");
            Vec::new()
        }
    };
    if let Ok(mut c) = cache().lock() {
        *c = Some(Cached {
            at: Instant::now(),
            rows: rows.clone(),
        });
    }
    rows
}

fn bearer_org(req: &ServiceRequest) -> Option<Uuid> {
    let secret = req.app_data::<web::Data<crate::auth::jwt::JwtSecret>>()?;
    let token = req
        .headers()
        .get(actix_web::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")?;
    crate::auth::jwt::verify_token(secret, token.trim())
        .ok()?
        .org_id()
}

/// App-level middleware. With no policy row it does nothing at all.
pub async fn gate(
    req: ServiceRequest,
    next: Next<impl MessageBody + 'static>,
) -> Result<ServiceResponse<EitherBody<impl MessageBody>>, actix_web::Error> {
    let client = ClientHeader::parse(
        req.headers()
            .get(CLIENT_HEADER)
            .and_then(|v| v.to_str().ok()),
    );
    // Only a native client that names its version can ever be gated: skip the
    // policy read entirely for everyone else (browsers, the dashboard).
    let candidate = matches!(client.app.as_deref(), Some("pos" | "kds")) && client.version.is_some();
    let verdict = match (candidate, req.app_data::<web::Data<PgPool>>()) {
        (true, Some(pool)) => {
            let rows = policies(pool.get_ref()).await;
            if rows.is_empty() {
                Verdict::Pass
            } else {
                let org = bearer_org(&req);
                decide(&rows, &client, org, is_gated_read(req.method(), req.path()))
            }
        }
        _ => Verdict::Pass,
    };
    match verdict {
        Verdict::Pass => Ok(next.call(req).await?.map_into_left_body()),
        Verdict::Nudge(level) => {
            let mut res = next.call(req).await?;
            res.headers_mut().insert(
                HeaderName::from_static(UPGRADE_HEADER),
                HeaderValue::from_static(level),
            );
            Ok(res.map_into_left_body())
        }
        Verdict::Refuse {
            min_supported,
            download_url,
        } => {
            crate::client_seen::legacy_hit_at(KIND_UPGRADE_REFUSED, "426");
            let body = serde_json::json!({
                "error": format!("This app version is no longer supported. Update to {min_supported} or later."),
                "code": "CLIENT_UPGRADE_REQUIRED",
                "min_supported": min_supported,
                "download_url": download_url,
            });
            let res = HttpResponse::build(actix_web::http::StatusCode::UPGRADE_REQUIRED)
                .insert_header((UPGRADE_HEADER, "required"))
                .json(body);
            Ok(req.into_response(res).map_into_right_body())
        }
    }
}

/// A gated read answered 426 (counted like any legacy path).
pub const KIND_UPGRADE_REFUSED: &str = "client_upgrade_refused";
