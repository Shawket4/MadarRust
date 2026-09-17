//! Device activation codes (POS_SIGNIN_OVERHAUL.md §4).
//!
//! Binding a tablet to an org and a branch used to need a manager's email and
//! password on the tablet, because a PIN sign-in needs a branch and an unbound
//! tablet has none. An activation code removes the person from that step: an
//! owner issues a short numeric code for a branch in the dashboard, the tablet
//! enters it, and the server binds the device and hands it its own long-lived
//! credential.
//!
//! - A code is **8 digits, single-use, expires after 24 hours, revocable**.
//! - Activation is unauthenticated, so a live code is unique across all orgs
//!   (partial unique index) and every failure — unknown, used, expired,
//!   revoked — gets the same 404, so the endpoint is no oracle. It sits behind
//!   the same per-address governor as `/auth/login`.
//! - The device credential is a random 32-byte token returned ONCE; only its
//!   SHA-256 is stored (`devices.credential_hash`). Retiring the device clears
//!   it. `verify_credential` is the check a device-authenticated endpoint uses.
//! - Old tablets keep binding through the manager email login, which stays.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::handlers::{Device, DeviceKind};
use crate::authz::Cap;
use crate::errors::{AppError, AppErrorResponse};
use crate::orgs::handlers::extract_claims;

/// How long an unused code stays valid.
pub const CODE_TTL_HOURS: i64 = 24;

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ActivationCodeState {
    /// Can still be entered on a tablet.
    Free,
    /// A device was activated with it.
    Used,
    /// Nobody used it within the day.
    Expired,
    /// Withdrawn by an admin.
    Revoked,
}

#[derive(Debug, Serialize, Clone, ToSchema)]
pub struct ActivationCode {
    pub id: Uuid,
    pub branch_id: Uuid,
    /// The 8 digits. Shown while free; kept afterwards so the list reads.
    pub code: String,
    pub label: Option<String>,
    #[schema(value_type = DeviceKind)]
    pub kind: String,
    pub state: ActivationCodeState,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub used_at: Option<DateTime<Utc>>,
    pub used_by_device: Option<Uuid>,
    pub revoked_at: Option<DateTime<Utc>>,
}

type CodeRow = (
    Uuid,
    Uuid,
    String,
    Option<String>,
    String,
    DateTime<Utc>,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
    Option<Uuid>,
    Option<DateTime<Utc>>,
);

const CODE_SELECT: &str = "SELECT id, branch_id, code, label, kind, created_at, expires_at, \
     used_at, used_by_device, revoked_at FROM device_activation_codes";

fn to_code(r: CodeRow, now: DateTime<Utc>) -> ActivationCode {
    let state = if r.9.is_some() {
        ActivationCodeState::Revoked
    } else if r.7.is_some() {
        ActivationCodeState::Used
    } else if r.6 <= now {
        ActivationCodeState::Expired
    } else {
        ActivationCodeState::Free
    };
    ActivationCode {
        id: r.0,
        branch_id: r.1,
        code: r.2,
        label: r.3,
        kind: r.4,
        state,
        created_at: r.5,
        expires_at: r.6,
        used_at: r.7,
        used_by_device: r.8,
        revoked_at: r.9,
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateActivationCodeRequest {
    pub branch_id: Uuid,
    /// A name for the tablet it is meant for ("Front counter").
    #[serde(default)]
    pub label: Option<String>,
    /// `pos` (default) | `kds` | `waiter`
    #[serde(default)]
    #[schema(value_type = Option<DeviceKind>)]
    pub kind: Option<String>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListActivationCodesQuery {
    pub branch_id: Uuid,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ActivateDeviceRequest {
    /// The 8-digit code from the dashboard.
    #[schema(example = "40721958")]
    pub code: String,
    /// The install's own id (the core's `lan_device_id`).
    pub device_id: Uuid,
    /// The device's short code on receipts (`T1`); a default is derived when
    /// absent or invalid.
    #[serde(default)]
    pub device_code: Option<String>,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub app_version: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ActivateDeviceResponse {
    pub org_id: Uuid,
    pub org_name: String,
    pub branch_id: Uuid,
    pub branch_name: String,
    pub device: Device,
    /// The device's own credential. Returned ONCE; store it in the device
    /// vault. Sent later as `X-Madar-Device-Token`.
    pub device_token: String,
}

/// Header a device sends its credential in.
pub const DEVICE_TOKEN_HEADER: &str = "X-Madar-Device-Token";

fn hash_token(token: &str) -> String {
    hex_lower(&Sha256::digest(token.as_bytes()))
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 32 random bytes from the OS RNG (two v4 uuids — no new dependency, the same
/// way PIN salts are minted), hex.
fn new_token() -> String {
    let mut b = Vec::with_capacity(32);
    b.extend_from_slice(Uuid::new_v4().as_bytes());
    b.extend_from_slice(Uuid::new_v4().as_bytes());
    hex_lower(&b)
}

/// 8 random digits.
fn new_code() -> String {
    let n = u64::from_le_bytes(Uuid::new_v4().as_bytes()[..8].try_into().expect("8 bytes"));
    format!("{:08}", n % 100_000_000)
}

/// Does `token` belong to the live (not retired) device `device_id`?
pub async fn verify_credential(
    pool: &PgPool,
    device_id: Uuid,
    token: &str,
) -> Result<bool, AppError> {
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT credential_hash FROM devices WHERE id = $1 AND retired_at IS NULL",
    )
    .bind(device_id)
    .fetch_optional(pool)
    .await?
    .flatten();
    let Some(stored) = stored else {
        return Ok(false);
    };
    let given = hash_token(token.trim());
    // Constant-time over equal-length hex strings.
    Ok(stored.len() == given.len()
        && stored
            .bytes()
            .zip(given.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0)
}

async fn gate(
    pool: &PgPool,
    req: &HttpRequest,
    branch: Uuid,
) -> Result<crate::auth::jwt::Claims, AppError> {
    let claims = extract_claims(req)?;
    crate::authz::scope::require_branch_access(pool, &claims, branch).await?;
    crate::authz::require::require(pool, &claims, Cap::BranchesEdit, Some(branch)).await?;
    Ok(claims)
}

#[utoipa::path(post, path = "/devices/activation-codes", tag = "devices",
    request_body = CreateActivationCodeRequest,
    responses((status = 201, description = "A new code, free for 24 hours", body = ActivationCode), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn create_code(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CreateActivationCodeRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = gate(pool.get_ref(), &req, body.branch_id).await?;
    let kind = body.kind.clone().unwrap_or_else(|| "pos".into());
    if !matches!(kind.as_str(), "pos" | "kds" | "waiter") {
        return Err(AppError::BadRequest(
            "kind must be pos, kds or waiter".into(),
        ));
    }
    let label = body
        .label
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(120).collect::<String>());
    let org: Uuid = sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1")
        .bind(body.branch_id)
        .fetch_one(pool.get_ref())
        .await?;
    // A handful of tries: a collision with another live code is a unique
    // violation, and the space is a hundred million.
    for _ in 0..8 {
        let inserted: Result<CodeRow, sqlx::Error> = sqlx::query_as(&format!(
            "INSERT INTO device_activation_codes
                 (org_id, branch_id, code, label, kind, created_by, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(hours => $7))
             RETURNING id, branch_id, code, label, kind, created_at, expires_at,
                       used_at, used_by_device, revoked_at"
        ))
        .bind(org)
        .bind(body.branch_id)
        .bind(new_code())
        .bind(&label)
        .bind(&kind)
        .bind(claims.user_id())
        .bind(CODE_TTL_HOURS as i32)
        .fetch_one(pool.get_ref())
        .await;
        match inserted {
            Ok(r) => return Ok(HttpResponse::Created().json(to_code(r, Utc::now()))),
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23505") => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(AppError::Internal)
}

#[utoipa::path(get, path = "/devices/activation-codes", tag = "devices",
    params(ListActivationCodesQuery),
    responses((status = 200, description = "A branch's codes, newest first", body = Vec<ActivationCode>), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn list_codes(
    req: HttpRequest,
    pool: crate::db::Db,
    q: web::Query<ListActivationCodesQuery>,
) -> Result<HttpResponse, AppError> {
    gate(pool.get_ref(), &req, q.branch_id).await?;
    let now = Utc::now();
    let rows: Vec<CodeRow> = sqlx::query_as(&format!(
        "{CODE_SELECT} WHERE branch_id = $1 ORDER BY created_at DESC LIMIT 200"
    ))
    .bind(q.branch_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(
        rows.into_iter()
            .map(|r| to_code(r, now))
            .collect::<Vec<_>>(),
    ))
}

#[utoipa::path(post, path = "/devices/activation-codes/{id}/revoke", tag = "devices",
    params(("id" = Uuid, Path, description = "Activation code id")),
    responses((status = 200, description = "Revoked", body = ActivationCode), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn revoke_code(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    // Refuse a caller who may edit no branch at all before looking the code up
    // (route guard); `gate` below then decides at the code's own branch.
    let claims = extract_claims(&req)?;
    crate::authz::require::require(pool.get_ref(), &claims, Cap::BranchesEdit, None).await?;
    let branch: Uuid =
        sqlx::query_scalar("SELECT branch_id FROM device_activation_codes WHERE id = $1")
            .bind(*id)
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::NotFound("No such activation code".into()))?;
    gate(pool.get_ref(), &req, branch).await?;
    // A used code has done its work; revoking it would say nothing true.
    let row: Option<CodeRow> = sqlx::query_as(
        "UPDATE device_activation_codes SET revoked_at = COALESCE(revoked_at, now())
          WHERE id = $1 AND used_at IS NULL
          RETURNING id, branch_id, code, label, kind, created_at, expires_at,
                    used_at, used_by_device, revoked_at",
    )
    .bind(*id)
    .fetch_optional(pool.get_ref())
    .await?;
    let row = row.ok_or_else(|| AppError::Refused {
        code: "ACTIVATION_CODE_USED",
        reason: "This code was already used; deactivate the device instead".into(),
    })?;
    Ok(HttpResponse::Ok().json(to_code(row, Utc::now())))
}

#[utoipa::path(post, path = "/auth/activate-device", tag = "auth",
    request_body = ActivateDeviceRequest,
    responses(
        (status = 200, description = "Device bound to the code's org and branch", body = ActivateDeviceResponse),
        (status = 404, description = "ACTIVATION_CODE_INVALID: unknown, used, expired or revoked"),
        AppErrorResponse),
    )]
pub async fn activate(
    pool: web::Data<PgPool>,
    body: web::Json<ActivateDeviceRequest>,
) -> Result<HttpResponse, AppError> {
    let invalid = || AppError::Coded {
        status: 404,
        code: "ACTIVATION_CODE_INVALID",
        reason: "That code is not valid. Ask for a new one.".into(),
    };
    let code = body.code.trim();
    if code.len() != 8 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    let mut tx = pool.begin().await?;
    // Claim the code in one statement, so two tablets typing it at once cannot
    // both win.
    let claimed: Option<(Uuid, Uuid, Uuid, String, Option<String>)> = sqlx::query_as(
        "UPDATE device_activation_codes SET used_at = now(), used_by_device = NULL
          WHERE code = $1 AND used_at IS NULL AND revoked_at IS NULL AND expires_at > now()
          RETURNING id, org_id, branch_id, kind, label",
    )
    .bind(code)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((code_id, org_id, branch_id, kind, label)) = claimed else {
        return Err(invalid());
    };
    let (org_name, branch_name): (String, String) = sqlx::query_as(
        "SELECT o.name, b.name FROM branches b JOIN organizations o ON o.id = b.org_id
          WHERE b.id = $1 AND b.deleted_at IS NULL AND b.is_active",
    )
    .bind(branch_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(invalid)?;

    let device_code = body
        .device_code
        .as_deref()
        .map(|c| c.trim().to_ascii_uppercase())
        .filter(|c| super::valid_code(c))
        .unwrap_or_else(|| super::fallback_code(body.device_id));
    let token = new_token();
    // Upsert by the install id. A device of ANOTHER org is never re-homed by a
    // code (S6): the WHERE leaves it untouched and the check below refuses.
    sqlx::query(
        "INSERT INTO devices (id, org_id, branch_id, code, label, kind, platform, app_version, credential_hash)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT (id) DO UPDATE SET branch_id = EXCLUDED.branch_id,
             kind = EXCLUDED.kind,
             label = COALESCE(EXCLUDED.label, devices.label),
             platform = COALESCE(EXCLUDED.platform, devices.platform),
             app_version = COALESCE(EXCLUDED.app_version, devices.app_version),
             credential_hash = EXCLUDED.credential_hash,
             retired_at = NULL,
             last_seen_at = now()
         WHERE devices.org_id = EXCLUDED.org_id",
    )
    .bind(body.device_id)
    .bind(org_id)
    .bind(branch_id)
    .bind(&device_code)
    .bind(&label)
    .bind(&kind)
    .bind(&body.platform)
    .bind(&body.app_version)
    .bind(hash_token(&token))
    .execute(&mut *tx)
    .await?;
    let owner: Uuid = sqlx::query_scalar("SELECT org_id FROM devices WHERE id = $1")
        .bind(body.device_id)
        .fetch_one(&mut *tx)
        .await?;
    if owner != org_id {
        // Rolls back the claim too: the code stays free for the right tablet.
        return Err(AppError::Forbidden(
            "This device belongs to another organization".into(),
        ));
    }
    sqlx::query("UPDATE device_activation_codes SET used_by_device = $2 WHERE id = $1")
        .bind(code_id)
        .bind(body.device_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    let device = sqlx::query_as::<_, Device>(&format!(
        "{} WHERE d.id = $1",
        super::handlers::DEVICE_SELECT
    ))
    .bind(body.device_id)
    .fetch_one(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(ActivateDeviceResponse {
        org_id,
        org_name,
        branch_id,
        branch_name,
        device,
        device_token: token,
    }))
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn codes_are_eight_digits_and_tokens_hash_stably() {
        for _ in 0..200 {
            let c = new_code();
            assert_eq!(c.len(), 8);
            assert!(c.bytes().all(|b| b.is_ascii_digit()));
        }
        let t = new_token();
        assert_eq!(t.len(), 64);
        assert_eq!(hash_token(&t), hash_token(&t));
        assert_ne!(hash_token(&t), hash_token(&new_token()));
    }
}
