//! One browser session shared by the first-party web apps (the dashboard at
//! `madar-pos.cloud` and Rewards by Madar at `rewards.madar-pos.cloud`).
//!
//! Each app keeps signing in exactly as before (`POST /auth/login`, bearer
//! JWT). After a sign-in it hands that JWT to `POST /auth/session`, which keeps
//! it in an HttpOnly cookie on the API host; the other app, booting with no
//! session of its own, asks `GET /auth/session` and gets the same token back.
//! `DELETE /auth/session` ends that session on the server (`auth::sessions`),
//! so the other app's next call is refused with `SESSION_REVOKED` and it signs
//! out too.
//!
//! The cookie is the boundary, so it is narrow on purpose:
//!  * **host-only** on the API host (no `Domain`), `Path=/auth/session`: it is
//!    never sent to a shop's own hostname, a static host, or any other route;
//!  * **HttpOnly, Secure, SameSite=Strict**: no script reads it, and no other
//!    site's page can make the browser send it;
//!  * and every call must come from an allowlisted ORIGIN
//!    ([`session_origins`]). A shop subdomain is same-site, so the browser
//!    WOULD send the cookie from one; the origin check is what refuses it.
//!
//! Only a live session is shared or handed back: a revoked or expired one is
//! forgotten instead.
//!
//! Login itself is untouched, so the apps and the backend deploy in any order:
//! a frontend whose `POST /auth/session` fails just keeps its own session.

use actix_web::cookie::{Cookie, SameSite, time::Duration as CookieDuration};
use actix_web::{HttpRequest, HttpResponse, http::header, web};
use chrono::Utc;
use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;

use crate::auth::jwt::{Claims, JwtSecret, verify_token};
use crate::auth::sessions::{self, RequestMeta, Revocation};
use crate::errors::{AppError, AppErrorResponse};
use crate::models::UserRole;

/// The shared token. `__Secure-`: browsers refuse it without `Secure`.
pub const SESSION_COOKIE: &str = "__Secure-madar_session";
const COOKIE_PATH: &str = "/auth/session";

#[derive(Debug, Serialize, ToSchema)]
pub struct SessionTokenResponse {
    /// The JWT to send as `Authorization: Bearer <token>`.
    pub token: String,
}

/// The origins allowed to use the shared session: `MADAR_SESSION_ORIGINS`
/// (comma-separated), else `CORS_ALLOWED_ORIGINS`. With neither set (local
/// development) only `http(s)://localhost` and `127.0.0.1` on any port.
fn session_origins() -> Option<Vec<String>> {
    ["MADAR_SESSION_ORIGINS", "CORS_ALLOWED_ORIGINS"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|v| !v.trim().is_empty())
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().trim_end_matches('/').to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
}

fn is_local_origin(origin: &str) -> bool {
    let rest = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"));
    let host = rest.map(|r| r.split(':').next().unwrap_or(r));
    matches!(host, Some("localhost" | "127.0.0.1"))
}

/// Whether `origin` may use the shared session. Pure over the list for tests.
pub fn origin_allowed(origin: &str, list: Option<&[String]>) -> bool {
    match list {
        Some(list) => list.iter().any(|o| o == origin),
        None => is_local_origin(origin),
    }
}

/// The caller's `Origin`, refused unless allowlisted. A request with no
/// `Origin` is not one of the browser apps, and has no business with this
/// cookie.
fn check_origin(req: &HttpRequest) -> Result<(), AppError> {
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::Forbidden("session: no Origin".into()))?;
    if origin_allowed(origin, session_origins().as_deref()) {
        Ok(())
    } else {
        Err(AppError::Forbidden("session: origin not allowed".into()))
    }
}

/// Only people who sign in to a dashboard share it. A PIN session (teller,
/// waiter, kitchen) is a device's, never a browser-wide one.
fn shares_session(role: &UserRole) -> bool {
    matches!(
        role,
        UserRole::SuperAdmin | UserRole::OrgAdmin | UserRole::BranchManager
    )
}

fn cookie(value: String, max_age: CookieDuration) -> Cookie<'static> {
    Cookie::build(SESSION_COOKIE, value)
        .path(COOKIE_PATH)
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Strict)
        .max_age(max_age)
        .finish()
}

fn expired() -> Cookie<'static> {
    cookie(String::new(), CookieDuration::ZERO)
}

fn bearer(req: &HttpRequest) -> Option<&str> {
    req.headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

/// A token's claims when it verifies AND its session is live.
async fn live_claims(
    req: &HttpRequest,
    pool: &PgPool,
    secret: &JwtSecret,
    token: &str,
) -> Option<Claims> {
    let claims = verify_token(secret, token).ok()?;
    sessions::check(pool, &claims, token, || RequestMeta::of(req))
        .await
        .ok()?;
    Some(claims)
}

/// Share the caller's session with the other first-party apps.
#[utoipa::path(
    post,
    path = "/auth/session",
    tag = "auth",
    responses(
        (status = 204, description = "The browser now shares this session"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn share(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    secret: web::Data<JwtSecret>,
) -> Result<HttpResponse, AppError> {
    check_origin(&req)?;
    // Verified here rather than by `JwtMiddleware`: GET and DELETE on this
    // path are public, and one actix resource answers every method of it.
    let token = bearer(&req)
        .ok_or_else(|| AppError::Unauthorized("Missing Authorization header".into()))?
        .to_string();
    let claims = verify_token(&secret, &token)
        .map_err(|_| AppError::Unauthorized("Invalid or expired token".into()))?;
    sessions::check(&pool, &claims, &token, || RequestMeta::of(&req)).await?;
    if !shares_session(&claims.role) {
        return Err(AppError::Forbidden(
            "session: not a dashboard sign-in".into(),
        ));
    }
    let left = claims.exp as i64 - Utc::now().timestamp();
    if left <= 0 {
        return Err(AppError::Unauthorized("Invalid or expired token".into()));
    }
    Ok(HttpResponse::NoContent()
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .cookie(cookie(token, CookieDuration::seconds(left)))
        .finish())
}

/// The browser's shared session, for an app that has none of its own.
///
/// 401 `SESSION_NONE` when there is nothing live to share (the app shows its
/// sign-in); a dead cookie is forgotten on the way.
#[utoipa::path(
    get,
    path = "/auth/session",
    tag = "auth",
    responses(
        (status = 200, description = "The shared session's token", body = SessionTokenResponse),
        AppErrorResponse,
    )
)]
pub async fn adopt(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    secret: web::Data<JwtSecret>,
) -> Result<HttpResponse, AppError> {
    check_origin(&req)?;
    let none = AppError::Coded {
        status: 401,
        code: "SESSION_NONE",
        reason: "No shared session.".into(),
    };
    let Some(c) = req.cookie(SESSION_COOKIE) else {
        return Err(none);
    };
    let token = c.value().to_string();
    match live_claims(&req, &pool, &secret, &token).await {
        Some(claims) if shares_session(&claims.role) => Ok(HttpResponse::Ok()
            .insert_header((header::CACHE_CONTROL, "no-store"))
            .json(SessionTokenResponse { token })),
        // Expired, revoked or foreign: forget it, so the next boot does not ask
        // again.
        _ => {
            let mut resp = actix_web::ResponseError::error_response(&none);
            resp.add_removal_cookie(&expired())
                .map_err(|_| AppError::Internal)?;
            Ok(resp)
        }
    }
}

/// Sign this browser out of every first-party app: the session behind the
/// cookie, and behind the bearer token when one is sent, end on the server.
#[utoipa::path(
    delete,
    path = "/auth/session",
    tag = "auth",
    responses(
        (status = 204, description = "Signed out everywhere in this browser"),
        AppErrorResponse,
    )
)]
pub async fn end(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    secret: web::Data<JwtSecret>,
) -> Result<HttpResponse, AppError> {
    check_origin(&req)?;
    let mut tokens: Vec<String> = Vec::new();
    if let Some(c) = req.cookie(SESSION_COOKIE) {
        tokens.push(c.value().to_string());
    }
    if let Some(t) = bearer(&req) {
        tokens.push(t.to_string());
    }
    tokens.dedup();
    let signed_out = Revocation {
        by: None,
        reason: "signed out",
    };
    for token in &tokens {
        // A token that no longer verifies has no live session to end.
        let Ok(claims) = verify_token(&secret, token) else {
            continue;
        };
        if let Some(sid) =
            sessions::session_of(&pool, &claims, token, RequestMeta::of(&req)).await?
        {
            sessions::revoke_one(&pool, sid, &signed_out).await?;
        }
    }
    Ok(HttpResponse::NoContent()
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .cookie(expired())
        .finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listed_origins_only() {
        let list = vec![
            "https://madar-pos.cloud".to_string(),
            "https://rewards.madar-pos.cloud".to_string(),
        ];
        assert!(origin_allowed(
            "https://rewards.madar-pos.cloud",
            Some(&list)
        ));
        assert!(origin_allowed("https://madar-pos.cloud", Some(&list)));
        // A shop's own hostname is same-site, so it gets the cookie sent; the
        // origin check is what refuses it.
        assert!(!origin_allowed("https://rue.madar-pos.cloud", Some(&list)));
        assert!(!origin_allowed(
            "https://loyalty.madar-pos.cloud",
            Some(&list)
        ));
        assert!(!origin_allowed("http://localhost:5173", Some(&list)));
    }

    #[test]
    fn unconfigured_means_localhost_only() {
        assert!(origin_allowed("http://localhost:5173", None));
        assert!(origin_allowed("http://127.0.0.1:8081", None));
        assert!(!origin_allowed("https://evil.example", None));
        assert!(!origin_allowed("http://localhost.evil.example", None));
    }

    #[test]
    fn pin_sessions_are_not_shared() {
        assert!(shares_session(&UserRole::OrgAdmin));
        assert!(!shares_session(&UserRole::Teller));
        assert!(!shares_session(&UserRole::Waiter));
        assert!(!shares_session(&UserRole::Kitchen));
    }

    #[test]
    fn the_cookie_is_narrow() {
        let c = cookie("t".into(), CookieDuration::hours(1));
        assert_eq!(c.path(), Some(COOKIE_PATH));
        assert_eq!(c.domain(), None, "host-only: never a shop subdomain");
        assert_eq!(c.http_only(), Some(true));
        assert_eq!(c.secure(), Some(true));
        assert_eq!(c.same_site(), Some(SameSite::Strict));
    }
}
