//! Server-side sessions behind every dashboard, till and kitchen sign-in
//! (migration `20261028090000_auth_sessions`).
//!
//! A sign-in inserts an `auth_sessions` row and mints a token that names it
//! (`Claims::sid`). Every authenticated request then asks [`check`]: a revoked
//! or expired row refuses the token at once with **401 `SESSION_REVOKED`**,
//! carrying the usual JSON error body. That shape is load-bearing: the POS
//! reads a bare 401 as a captive portal (offline) and a 403 as a refusal that
//! dead-letters a queued sale, while a coded 401 pauses its outbox until the
//! teller signs in again, losing nothing.
//!
//! A token from before sessions existed has no `sid`. It is adopted on first
//! use as a `legacy` row keyed by the token's SHA-256, so it can be revoked
//! like any other, and runs out at its own `exp`: the deploy signs nobody out.
//!
//! Answers are cached by [`crate::fastpath`] (no database read per request
//! while the listener is up). A revocation fires `authz_user` from its trigger,
//! which clears the cached answers on every instance at commit; the revoking
//! instance also clears its own at once.
//!
//! Only a super admin lists or revokes sessions (`auth::admin_sessions`).

use std::time::{Duration as StdDuration, Instant};

use actix_web::HttpRequest;
use actix_web::http::header::{HeaderMap, USER_AGENT};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::auth::jwt::{Claims, JwtSecret, create_session_token};
use crate::errors::AppError;
use crate::models::UserRole;

/// A till sign-in (PIN, branch-bound) and a kitchen screen's device session.
pub const TILL_HOURS: i64 = 24;
/// A dashboard sign-in (email and password) and a new owner's first session.
pub const DASHBOARD_HOURS: i64 = 48;

/// How a session began. Stored as text; the migration's CHECK lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Password,
    Pin,
    Device,
    Signup,
    Demo,
    /// A token issued before sessions existed, adopted on first use.
    Legacy,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Password => "password",
            Kind::Pin => "pin",
            Kind::Device => "device",
            Kind::Signup => "signup",
            Kind::Demo => "demo",
            Kind::Legacy => "legacy",
        }
    }
}

/// Where a request came from, as far as a session records it.
#[derive(Debug, Clone, Default)]
pub struct RequestMeta {
    pub device_id: Option<String>,
    pub client: Option<String>,
    pub ip: Option<std::net::IpAddr>,
    pub user_agent: Option<String>,
}

fn header(headers: &HeaderMap, name: &str, max: usize) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| v.chars().take(max).collect())
}

impl RequestMeta {
    pub fn from_parts(peer: Option<std::net::SocketAddr>, headers: &HeaderMap) -> Self {
        Self {
            // The till's registered device, else the per-install id a PIN
            // sign-in sends for its throttle.
            device_id: header(headers, crate::devices::DEVICE_HEADER, 128)
                .or_else(|| header(headers, crate::devices::LEGACY_DEVICE_HEADER, 128)),
            client: header(headers, crate::devices::CLIENT_HEADER, 128),
            ip: crate::rate_limit::client_ip_of(peer, headers),
            user_agent: header(headers, USER_AGENT.as_str(), 512),
        }
    }

    pub fn of(req: &HttpRequest) -> Self {
        Self::from_parts(req.peer_addr(), req.headers())
    }
}

/// A sign-in about to be issued.
pub struct NewSession<'a> {
    pub user_id: Uuid,
    pub org_id: Option<Uuid>,
    pub role: UserRole,
    pub branch_id: Option<Uuid>,
    pub kind: Kind,
    pub hours: i64,
    pub meta: &'a RequestMeta,
}

/// Insert the session row and mint its token. Runs on the caller's connection
/// (or transaction), so a sign-in that fails later leaves no row behind.
pub async fn issue(
    conn: &mut PgConnection,
    secret: &JwtSecret,
    s: NewSession<'_>,
) -> Result<String, AppError> {
    let sid = sqlx::query_scalar!(
        "INSERT INTO auth_sessions
             (user_id, org_id, branch_id, role, kind, device_id, client, ip, user_agent,
              expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8::text::inet, $9,
                 now() + make_interval(hours => $10))
         RETURNING id",
        s.user_id,
        s.org_id,
        s.branch_id,
        s.role.clone() as UserRole,
        s.kind.as_str(),
        s.meta.device_id.as_deref(),
        s.meta.client.as_deref(),
        s.meta.ip.map(|ip| ip.to_string()),
        s.meta.user_agent.as_deref(),
        s.hours as i32,
    )
    .fetch_one(&mut *conn)
    .await?;
    create_session_token(
        secret,
        s.user_id,
        s.org_id,
        s.role,
        s.branch_id,
        s.hours,
        Some(sid),
    )
    .map_err(|_| AppError::Internal)
}

/// The refusal every client branches on (see the module doc).
pub fn revoked() -> AppError {
    AppError::Coded {
        status: 401,
        code: "SESSION_REVOKED",
        reason: "Your session has ended. Please sign in again.".into(),
    }
}

/// SHA-256 of a token, hex: how an adopted legacy token is found again.
pub fn token_hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

/// The cache key of a token's session: its `sid`, or (legacy) the first 16
/// bytes of its hash.
fn key(claims: &Claims, token: &str) -> Uuid {
    claims.session_id().unwrap_or_else(|| {
        let d = Sha256::digest(token.as_bytes());
        Uuid::from_slice(&d[..16]).unwrap_or_default()
    })
}

/// May this (already signature-checked) token still be used?
pub async fn check(
    pool: &PgPool,
    claims: &Claims,
    token: &str,
    meta: impl FnOnce() -> RequestMeta,
) -> Result<(), AppError> {
    let k = key(claims, token);
    if let Some(live) = crate::fastpath::session_live(k) {
        return if live { Ok(()) } else { Err(revoked()) };
    }
    let cap = crate::fastpath::capture(None, None);
    let found = match claims.session_id() {
        Some(sid) => live_by_id(pool, sid).await?,
        None => adopt(pool, claims, token, meta()).await?,
    };
    // `None`: a token whose user has no row (fixtures, service tokens) is not
    // a session's to refuse; the permission check still applies to it.
    let Some((live, left_secs)) = found else {
        return Ok(());
    };
    if let Some(cap) = cap {
        let until = live.then(|| Instant::now() + StdDuration::from_secs(left_secs.max(0) as u64));
        crate::fastpath::put_session_live(cap, k, claims.org_id(), live, until);
    }
    if live { Ok(()) } else { Err(revoked()) }
}

/// (live?, seconds left) for a session row, bumping `last_seen_at` when it is
/// more than 5 minutes stale (at most one write per session per 5 minutes).
/// A missing row is dead: its user was deleted, or it was pruned long after
/// it expired.
async fn live_by_id(pool: &PgPool, sid: Uuid) -> Result<Option<(bool, i64)>, AppError> {
    let row = sqlx::query!(
        r#"WITH seen AS (
               UPDATE auth_sessions SET last_seen_at = now()
                WHERE id = $1 AND revoked_at IS NULL
                  AND last_seen_at < now() - interval '5 minutes'
           )
           SELECT (revoked_at IS NULL AND expires_at > now()) AS "live!",
                  floor(extract(epoch FROM expires_at - now()))::bigint AS "left_secs!"
             FROM auth_sessions WHERE id = $1"#,
        sid
    )
    .fetch_optional(pool)
    .await?;
    Ok(Some(row.map_or((false, 0), |r| (r.live, r.left_secs))))
}

/// A token without a `sid`: its adopted row, made on first sight.
async fn adopt(
    pool: &PgPool,
    claims: &Claims,
    token: &str,
    meta: RequestMeta,
) -> Result<Option<(bool, i64)>, AppError> {
    let Ok(user_id) = claims.user_id_safe() else {
        return Ok(None);
    };
    let hash = token_hash(token);
    if let Some(found) = live_by_hash(pool, &hash).await? {
        return Ok(Some(found));
    }
    // Not seen yet. A user-wide sign-out (`sessions_valid_after`) refuses it
    // outright, so adopting can never revive a token that was ended before it
    // was first seen. A disabled account is not adopted either, and not refused
    // here: the permission check answers it (403, terminal for every client),
    // exactly as before sessions existed.
    let account = sqlx::query!(
        r#"SELECT (is_active AND deleted_at IS NULL) AS "active!",
                  floor(extract(epoch FROM sessions_valid_after))::bigint AS valid_after
             FROM users WHERE id = $1"#,
        user_id
    )
    .fetch_optional(pool)
    .await?;
    let Some((active, valid_after)) = account.map(|a| (a.active, a.valid_after)) else {
        return Ok(None);
    };
    if !active {
        return Ok(None);
    }
    if valid_after.is_some_and(|after| (claims.iat as i64) < after) {
        return Ok(Some((false, 0)));
    }
    // The org and branch only when they still exist: a row must never fail
    // its foreign keys for a token that is otherwise good.
    sqlx::query!(
        "INSERT INTO auth_sessions
             (user_id, org_id, branch_id, role, kind, device_id, client, ip, user_agent,
              legacy_token_hash, created_at, expires_at)
         VALUES ($1, (SELECT id FROM organizations WHERE id = $2),
                 (SELECT id FROM branches WHERE id = $3), $4, 'legacy', $5, $6,
                 $7::text::inet, $8, $9, to_timestamp($10), to_timestamp($11))
         ON CONFLICT (legacy_token_hash) WHERE legacy_token_hash IS NOT NULL DO NOTHING",
        user_id,
        claims.org_id(),
        claims.branch_id(),
        claims.role.clone() as UserRole,
        meta.device_id.as_deref(),
        meta.client.as_deref(),
        meta.ip.map(|ip| ip.to_string()),
        meta.user_agent.as_deref(),
        hash,
        claims.iat as f64,
        claims.exp as f64,
    )
    .execute(pool)
    .await?;
    // Read back whichever insert won (two first requests at once make one row).
    Ok(Some(live_by_hash(pool, &hash).await?.unwrap_or((false, 0))))
}

async fn live_by_hash(pool: &PgPool, hash: &str) -> Result<Option<(bool, i64)>, AppError> {
    Ok(sqlx::query!(
        r#"WITH seen AS (
               UPDATE auth_sessions SET last_seen_at = now()
                WHERE legacy_token_hash = $1 AND revoked_at IS NULL
                  AND last_seen_at < now() - interval '5 minutes'
           )
           SELECT (revoked_at IS NULL AND expires_at > now()) AS "live!",
                  floor(extract(epoch FROM expires_at - now()))::bigint AS "left_secs!"
             FROM auth_sessions WHERE legacy_token_hash = $1"#,
        hash
    )
    .fetch_optional(pool)
    .await?
    .map(|r| (r.live, r.left_secs)))
}

/// The session id behind a token, adopting a legacy one if need be. `None`
/// for a token whose user has no row.
pub async fn session_of(
    pool: &PgPool,
    claims: &Claims,
    token: &str,
    meta: RequestMeta,
) -> Result<Option<Uuid>, AppError> {
    if let Some(sid) = claims.session_id() {
        return Ok(Some(sid));
    }
    if adopt(pool, claims, token, meta).await?.is_none() {
        return Ok(None);
    }
    Ok(sqlx::query_scalar!(
        "SELECT id FROM auth_sessions WHERE legacy_token_hash = $1",
        token_hash(token)
    )
    .fetch_optional(pool)
    .await?)
}

/// Who revokes, and why. `by = None` is the person signing themselves out.
pub struct Revocation<'a> {
    pub by: Option<Uuid>,
    pub reason: &'a str,
}

/// Drop this instance's cached answers now; the trigger's notification does
/// the same on every instance at commit.
fn forget_cached() {
    crate::fastpath::on_authz();
}

/// Revoke one session. Returns whether it was live.
pub async fn revoke_one(pool: &PgPool, sid: Uuid, r: &Revocation<'_>) -> Result<bool, AppError> {
    let n = sqlx::query!(
        "UPDATE auth_sessions
            SET revoked_at = now(), revoked_by = $2, revoke_reason = $3
          WHERE id = $1 AND revoked_at IS NULL",
        sid,
        r.by,
        r.reason
    )
    .execute(pool)
    .await?
    .rows_affected();
    forget_cached();
    Ok(n > 0)
}

/// Sign a person out everywhere: every live row, and (through
/// `sessions_valid_after`) any older token of theirs not adopted yet.
pub async fn revoke_user(
    pool: &PgPool,
    user_id: Uuid,
    r: &Revocation<'_>,
) -> Result<u64, AppError> {
    let mut tx = pool.begin().await?;
    let n = sqlx::query!(
        "UPDATE auth_sessions
            SET revoked_at = now(), revoked_by = $2, revoke_reason = $3
          WHERE user_id = $1 AND revoked_at IS NULL",
        user_id,
        r.by,
        r.reason
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    sqlx::query!(
        "UPDATE users SET sessions_valid_after = now() WHERE id = $1",
        user_id
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    forget_cached();
    Ok(n)
}

/// Sign out a whole org, or only its sessions of the given kinds (every till:
/// `["pin", "device"]`). Older tokens of the org's people go too when no kind
/// filter is given.
pub async fn revoke_org(
    pool: &PgPool,
    org_id: Uuid,
    kinds: Option<&[String]>,
    r: &Revocation<'_>,
) -> Result<u64, AppError> {
    let mut tx = pool.begin().await?;
    let n = sqlx::query!(
        "UPDATE auth_sessions
            SET revoked_at = now(), revoked_by = $3, revoke_reason = $4
          WHERE org_id = $1 AND revoked_at IS NULL
            AND ($2::text[] IS NULL OR kind = ANY($2))",
        org_id,
        kinds,
        r.by,
        r.reason
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if kinds.is_none() {
        sqlx::query!(
            "UPDATE users SET sessions_valid_after = now()
              WHERE org_id = $1 AND deleted_at IS NULL",
            org_id
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    forget_cached();
    Ok(n)
}

/// Prune [`prune`] hourly, for the life of the process.
pub fn spawn_pruner(pool: PgPool) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(StdDuration::from_secs(3600));
        loop {
            ticker.tick().await;
            match prune(&pool).await {
                Ok(n) if n > 0 => tracing::info!(target: "auth.sessions", pruned = n),
                Ok(_) => {}
                Err(e) => tracing::warn!(target: "auth.sessions", error = %e, "prune failed"),
            }
        }
    });
}

/// Delete rows that ended more than 30 days ago.
pub async fn prune(pool: &PgPool) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query!(
        "DELETE FROM auth_sessions
          WHERE expires_at < now() - interval '30 days'
             OR revoked_at < now() - interval '30 days'"
    )
    .execute(pool)
    .await?
    .rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_match_the_migration_check() {
        for k in [
            Kind::Password,
            Kind::Pin,
            Kind::Device,
            Kind::Signup,
            Kind::Demo,
            Kind::Legacy,
        ] {
            assert!(
                include_str!("../../migrations/20261028090000_auth_sessions.sql")
                    .contains(&format!("'{}'", k.as_str())),
                "{k:?}"
            );
        }
    }

    #[test]
    fn the_revoked_answer_is_a_coded_401() {
        match revoked() {
            AppError::Coded { status, code, .. } => {
                assert_eq!(status, 401);
                assert_eq!(code, "SESSION_REVOKED");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_legacy_key_is_stable_per_token() {
        let c = Claims {
            sub: Uuid::nil().to_string(),
            org_id: None,
            role: UserRole::OrgAdmin,
            branch_id: None,
            exp: 0,
            iat: 0,
            sid: None,
        };
        assert_eq!(key(&c, "a"), key(&c, "a"));
        assert_ne!(key(&c, "a"), key(&c, "b"));
        let sid = Uuid::new_v4();
        let with = Claims {
            sid: Some(sid.to_string()),
            ..c
        };
        assert_eq!(key(&with, "a"), sid);
    }
}
