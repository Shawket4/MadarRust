//! POS installations (`devices`) and the two request headers that name them.
//!
//! A device is one POS / KDS / waiter install. Its identity is the UUID the
//! core minted once (`lan_device_id`); its human code (`36B`) prefixes every
//! order number it rings (`36B-12`). Devices register on first contact
//! (`POST /devices/register`) or implicitly when a replayed op names one — a
//! till that sold offline for a week must never have its backlog refused
//! because the device was not known yet. Codes are NOT unique: two devices
//! sharing a code offline is reported (`code_conflict`), never refused.

pub mod activation;
pub mod handlers;
pub mod routes;


use std::future::{Ready, ready};

use actix_web::{FromRequest, HttpRequest, dev::Payload, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;

/// The header a new POS/KDS sends on every request (its `lan_device_id`).
pub const DEVICE_HEADER: &str = "X-Madar-Device";
/// Legacy spelling read as a fallback (`tickets::DEVICE_ID_HEADER`).
pub const LEGACY_DEVICE_HEADER: &str = crate::tickets::DEVICE_ID_HEADER;
/// `<app>/<semver> (<platform>)`, e.g. `pos/0.7.0 (ios)`.
pub const CLIENT_HEADER: &str = "X-Madar-Client";

/// `^[A-Z0-9]{1,6}$`, the `devices.code` CHECK.
pub fn valid_code(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= 6
        && code
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// The code a device gets when it was never told one (auto-registered from a
/// request that named only its id): the first four hex digits of the id.
pub fn fallback_code(id: Uuid) -> String {
    id.simple().to_string()[..4].to_uppercase()
}

/// The device a request names, if any. Never fails the request: a missing or
/// malformed header is simply `None`. When present, the device's
/// `last_seen_at` is touched in the background (at most once per 5 minutes).
#[derive(Debug, Clone, Copy, Default)]
pub struct DeviceHeader(pub Option<Uuid>);

impl DeviceHeader {
    pub fn from_request_headers(req: &HttpRequest) -> Option<Uuid> {
        [DEVICE_HEADER, LEGACY_DEVICE_HEADER].iter().find_map(|h| {
            req.headers()
                .get(*h)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| Uuid::parse_str(v.trim()).ok())
        })
    }
}

impl FromRequest for DeviceHeader {
    type Error = AppError;
    type Future = Ready<Result<Self, AppError>>;

    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        let id = Self::from_request_headers(req);
        if let (Some(id), Some(pool)) = (id, req.app_data::<web::Data<PgPool>>()) {
            let pool = pool.get_ref().clone();
            tokio::spawn(async move {
                let _ = sqlx::query(
                    "UPDATE devices SET last_seen_at = now() \
                      WHERE id = $1 AND last_seen_at < now() - interval '5 minutes'",
                )
                .bind(id)
                .execute(&pool)
                .await;
            });
        }
        ready(Ok(DeviceHeader(id)))
    }
}

/// The parsed `X-Madar-Client` header. Only used for the legacy `/tills`
/// entity adapter and logging — never to reject a request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientHeader {
    pub app: Option<String>,
    pub version: Option<(u64, u64, u64)>,
    pub platform: Option<String>,
}

impl ClientHeader {
    pub fn parse(raw: Option<&str>) -> Self {
        let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
            return Self::default();
        };
        let (head, platform) = match raw.split_once('(') {
            Some((h, rest)) => (
                h.trim(),
                Some(rest.trim_end_matches(')').trim().to_string()),
            ),
            None => (raw, None),
        };
        let (app, version) = match head.split_once('/') {
            Some((a, v)) => (Some(a.trim().to_ascii_lowercase()), parse_semver(v.trim())),
            None => (Some(head.to_ascii_lowercase()), None),
        };
        Self {
            app,
            version,
            platform,
        }
    }

    pub fn from_request_headers(req: &HttpRequest) -> Self {
        Self::parse(
            req.headers()
                .get(CLIENT_HEADER)
                .and_then(|v| v.to_str().ok()),
        )
    }

    /// Header absent, or a `pos` build older than 0.7.0 (the tills rework).
    pub fn is_legacy_pos(&self) -> bool {
        match (self.app.as_deref(), self.version) {
            (None, _) => true,
            (Some("pos"), Some(v)) => v < (0, 7, 0),
            (Some("pos"), None) => true,
            _ => false,
        }
    }
}

fn parse_semver(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    let major = it.next()??;
    let minor = it.next().flatten().unwrap_or(0);
    let patch = it.next().flatten().unwrap_or(0);
    Some((major, minor, patch))
}

impl FromRequest for ClientHeader {
    type Error = AppError;
    type Future = Ready<Result<Self, AppError>>;

    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        ready(Ok(Self::from_request_headers(req)))
    }
}

/// The code + label snapshot a till / order takes of its device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSnapshot {
    pub code: String,
    pub label: Option<String>,
}

/// Look the device up, registering it (kind `pos`) when it is unknown. Used by
/// replay (§2.4 "replay also auto-registers") and the live till open. Never
/// fails on a code clash; an unusable `code` falls back to [`fallback_code`].
/// A device id that exists in ANOTHER org is not visible through the tenant
/// pool; the insert then conflicts and the function returns `None` (the
/// caller stores no device rather than refusing recorded history).
pub async fn ensure_registered(
    conn: &mut sqlx::PgConnection,
    org_id: Uuid,
    device_id: Uuid,
    branch_id: Option<Uuid>,
    code: Option<&str>,
) -> Result<Option<DeviceSnapshot>, AppError> {
    if let Some((code, label)) = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT code, label FROM devices WHERE id = $1 AND org_id = $2",
    )
    .bind(device_id)
    .bind(org_id)
    .fetch_optional(&mut *conn)
    .await?
    {
        return Ok(Some(DeviceSnapshot { code, label }));
    }
    let code = code
        .map(|c| c.trim().to_ascii_uppercase())
        .filter(|c| valid_code(c))
        .unwrap_or_else(|| fallback_code(device_id));
    let branch_id = match branch_id {
        Some(b) => {
            sqlx::query_scalar::<_, Uuid>("SELECT id FROM branches WHERE id = $1 AND org_id = $2")
                .bind(b)
                .bind(org_id)
                .fetch_optional(&mut *conn)
                .await?
        }
        None => None,
    };
    let inserted: Option<(String, Option<String>)> = sqlx::query_as(
        "INSERT INTO devices (id, org_id, branch_id, code, kind) VALUES ($1, $2, $3, $4, 'pos') \
         ON CONFLICT (id) DO NOTHING RETURNING code, label",
    )
    .bind(device_id)
    .bind(org_id)
    .bind(branch_id)
    .bind(&code)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(inserted.map(|(code, label)| DeviceSnapshot { code, label }))
}

#[cfg(test)]
mod header_tests {
    use super::*;

    #[test]
    fn client_header_legacy_detection() {
        assert!(ClientHeader::parse(None).is_legacy_pos());
        assert!(ClientHeader::parse(Some("pos/0.6.0 (android)")).is_legacy_pos());
        assert!(ClientHeader::parse(Some("pos/0.5.1")).is_legacy_pos());
        assert!(!ClientHeader::parse(Some("pos/0.7.0 (ios)")).is_legacy_pos());
        assert!(!ClientHeader::parse(Some("pos/1.2.3-beta (ios)")).is_legacy_pos());
        assert!(!ClientHeader::parse(Some("dashboard/2026.09")).is_legacy_pos());
        let h = ClientHeader::parse(Some("pos/0.7.0 (ios)"));
        assert_eq!(h.platform.as_deref(), Some("ios"));
        assert_eq!(h.version, Some((0, 7, 0)));
    }

    #[test]
    fn codes() {
        assert!(valid_code("36B"));
        assert!(!valid_code("36b"));
        assert!(!valid_code("1234567"));
        assert!(!valid_code(""));
        assert!(valid_code(&fallback_code(Uuid::new_v4())));
    }
}
