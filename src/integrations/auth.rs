//! HTTP Basic authentication for partner analytics pulls.
//!
//! Partners send `Authorization: Basic base64(username:secret)`. The username
//! carries no tenant hint, so the credential lookup necessarily runs on the
//! OWNER pool (RLS bypass) — there is no org to scope by until the row is
//! found. Everything after that point runs on the org's RLS-scoped pool, so a
//! partner cannot reach another merchant's data even if a handler forgets a
//! `WHERE` clause.
//!
//! A credential grants read access to exactly one branch and carries no role:
//! it is not a `Claims`, cannot be turned into one, and the only routes behind
//! it are the read-only `/integrations/analytics/*` handlers.

use actix_web::{FromRequest, HttpRequest, dev::Payload, web};
use base64::Engine as _;
use sqlx::PgPool;
use std::future::Future;
use std::pin::Pin;
use uuid::Uuid;

use crate::errors::AppError;

/// A bcrypt hash of a value no caller can produce. Verified against when the
/// username is unknown so an attacker cannot distinguish "no such user" from
/// "wrong password" by response time — without it, the miss path would skip
/// the ~100 ms hash and return conspicuously faster.
const DUMMY_HASH: &str = "$2b$12$C6UzMDM.H6dfI/f/IKcEeO3Zx1kK1KpVvHqCJcSGP7ZTjhJ0Hn0nS";

/// A partner authenticated by HTTP Basic against `integration_credentials`.
#[derive(Debug, Clone, Copy)]
pub struct IntegrationCaller {
    pub credential_id: Uuid,
    pub org_id: Uuid,
    /// The single branch this credential may read. There is no branch
    /// parameter on the request at all — scope comes from here and nowhere
    /// else, so there is nothing for a partner to pass or to tamper with.
    pub branch_id: Uuid,
}

/// Split `Authorization: Basic <b64>` into (username, secret).
///
/// Per RFC 7617 the decoded payload splits on the FIRST colon: a colon is
/// legal inside a password but not inside a username.
fn parse_basic(header: &str) -> Option<(String, String)> {
    let b64 = header
        .strip_prefix("Basic ")
        .or_else(|| header.strip_prefix("basic "))?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (user, secret) = decoded.split_once(':')?;
    Some((user.to_string(), secret.to_string()))
}

impl FromRequest for IntegrationCaller {
    type Error = AppError;
    type Future = Pin<Box<dyn Future<Output = Result<IntegrationCaller, AppError>>>>;

    fn from_request(req: &HttpRequest, _payload: &mut Payload) -> Self::Future {
        let header = req
            .headers()
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let pool = req.app_data::<web::Data<PgPool>>().cloned();

        Box::pin(async move {
            let pool = pool.ok_or(AppError::Internal)?;
            let header =
                header.ok_or_else(|| AppError::Unauthorized("Missing credentials".into()))?;
            let (username, secret) = parse_basic(&header)
                .ok_or_else(|| AppError::Unauthorized("Malformed Basic credentials".into()))?;

            // Owner pool on purpose: no tenant is known yet (see module docs).
            let row: Option<(Uuid, Uuid, Uuid, String)> = sqlx::query_as(
                "SELECT id, org_id, branch_id, secret_hash
                   FROM integration_credentials
                  WHERE lower(username) = lower($1)
                    AND revoked_at IS NULL",
            )
            .bind(&username)
            .fetch_optional(pool.get_ref())
            .await?;

            // bcrypt is deliberately slow (~100 ms), so it must not run on the
            // actix worker thread — a handful of concurrent pulls would stall
            // every other request the worker is serving.
            let hash = row
                .as_ref()
                .map(|(_, _, _, h)| h.clone())
                .unwrap_or_else(|| DUMMY_HASH.to_string());
            let ok = web::block(move || bcrypt::verify(&secret, &hash).unwrap_or(false))
                .await
                .map_err(|_| AppError::Internal)?;

            // One message for every failure mode (unknown user, wrong secret,
            // revoked): never tell a prober which username exists.
            let (credential_id, org_id, branch_id, _) = match row {
                Some(r) if ok => r,
                _ => return Err(AppError::Unauthorized("Invalid credentials".into())),
            };

            // Best-effort usage stamp. A failure here must never fail an
            // otherwise-valid pull, so the result is deliberately discarded.
            let _ = sqlx::query(
                "UPDATE integration_credentials SET last_used_at = now() WHERE id = $1",
            )
            .bind(credential_id)
            .execute(pool.get_ref())
            .await;

            Ok(IntegrationCaller {
                credential_id,
                org_id,
                branch_id,
            })
        })
    }
}

#[cfg(test)]
mod parse_tests {
    use super::parse_basic;
    use base64::Engine as _;

    fn header(raw: &str) -> String {
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(raw)
        )
    }

    #[test]
    fn parses_username_and_secret() {
        let (u, s) = parse_basic(&header("rue:hunter2")).unwrap();
        assert_eq!(u, "rue");
        assert_eq!(s, "hunter2");
    }

    #[test]
    fn splits_on_first_colon_so_secrets_may_contain_colons() {
        let (u, s) = parse_basic(&header("rue:a:b:c")).unwrap();
        assert_eq!(u, "rue");
        assert_eq!(s, "a:b:c");
    }

    #[test]
    fn rejects_non_basic_and_garbage() {
        assert!(parse_basic("Bearer abc.def.ghi").is_none());
        assert!(parse_basic("Basic !!!not-base64!!!").is_none());
        // No colon at all — not a credential pair.
        assert!(parse_basic(&header("nocolon")).is_none());
    }
}
