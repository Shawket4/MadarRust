//! `GET /devices/me/authz-snapshot` (PERMISSIONS_ARCHITECTURE §4.4.2): what a
//! till needs to authorize offline, for its own branch, signed by the server.
//!
//! Device-authenticated, never user-authenticated: the device names itself
//! (`X-Madar-Device`) and proves it with the credential it got at activation
//! (`X-Madar-Device-Token`). A device bound the old way (manager email login)
//! has no credential and gets 401 — it keeps using the feed's teller rows.
//!
//! No expiry (locked owner decision, 2026-09-15): `expires_at` is `i64::MAX`.
//! Freshness is the org epoch, which the device compares with the feed.

use actix_web::{HttpRequest, HttpResponse, web};
use sqlx::PgPool;
use uuid::Uuid;

use madar_authz::Cap;
use madar_authz::snapshot::{SnapshotBody, SnapshotUser};

use crate::devices::DeviceHeader;
use crate::devices::activation::{DEVICE_TOKEN_HEADER, verify_credential};
use crate::errors::{AppError, AppErrorResponse};

#[utoipa::path(get, path = "/auth/authz-keys", tag = "authz",
    responses((status = 200, description = "Public keys that sign permission snapshots, current first", body = Vec<crate::authz::keys::AuthzPublicKey>)))]
pub async fn authz_keys() -> HttpResponse {
    HttpResponse::Ok().json(super::keys::public_keys())
}

#[utoipa::path(get, path = "/devices/me/authz-snapshot", tag = "devices",
    params(
        ("X-Madar-Device" = String, Header, description = "The device id"),
        ("X-Madar-Device-Token" = String, Header, description = "The credential issued at activation"),
    ),
    responses(
        (status = 200, description = "The signed snapshot for the device's branch (madar_authz::snapshot::SignedSnapshot)", body = Object),
        (status = 401, description = "Unknown, retired or unauthenticated device"),
        (status = 503, description = "The server has no signing key configured"),
        AppErrorResponse))]
pub async fn device_snapshot(
    req: HttpRequest,
    pool: web::Data<PgPool>,
) -> Result<HttpResponse, AppError> {
    let unauth = || AppError::Unauthorized("This device is not activated".into());
    let device = DeviceHeader::from_request_headers(&req).ok_or_else(unauth)?;
    let token = req
        .headers()
        .get(DEVICE_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(unauth)?;
    if !verify_credential(pool.get_ref(), device, token).await? {
        return Err(unauth());
    }
    let (org, branch): (Uuid, Option<Uuid>) =
        sqlx::query_as("SELECT org_id, branch_id FROM devices WHERE id = $1")
            .bind(device)
            .fetch_one(pool.get_ref())
            .await?;
    let branch = branch.ok_or_else(unauth)?;
    let body = build(pool.get_ref(), org, branch, device).await?;
    let signed = super::keys::sign(body).ok_or_else(|| {
        AppError::ServiceUnavailable("Permission snapshots are not configured".into())
    })?;
    Ok(HttpResponse::Ok().json(signed))
}

/// Everyone with a PIN who may sign in at `branch`, fully resolved there.
pub async fn build(
    pool: &PgPool,
    org: Uuid,
    branch: Uuid,
    device: Uuid,
) -> Result<SnapshotBody, AppError> {
    let epoch: i64 =
        sqlx::query_scalar("SELECT COALESCE((SELECT epoch FROM authz_epoch WHERE org_id = $1), 0)")
            .bind(org)
            .fetch_one(pool)
            .await?;
    let people: Vec<(Uuid, String, String, bool)> = sqlx::query_as(
        "SELECT id, name, role::text, COALESCE(is_owner, false) FROM users
          WHERE org_id = $1 AND pin_hash IS NOT NULL AND is_active AND deleted_at IS NULL
            AND role <> 'super_admin' AND NOT is_guest_principal
          ORDER BY name, id",
    )
    .bind(org)
    .fetch_all(pool)
    .await?;
    let mut users = Vec::new();
    let mut policy = madar_authz::OrgPolicy::default();
    for (id, name, role, owner) in people {
        let mut conn = pool.acquire().await?;
        if let Some(loaded) = super::load::load(&mut conn, id).await? {
            policy = loaded.policy.clone();
        }
        let eff = super::require::effective_on(&mut conn, id, Some(branch)).await?;
        if !eff.can(Cap::PosSignIn) {
            continue;
        }
        users.push(SnapshotUser {
            user_id: id.to_string(),
            name,
            legacy_role: role,
            is_owner: owner,
            active: true,
            eff,
        });
    }
    Ok(SnapshotBody {
        v: SnapshotBody::VERSION,
        spec_version: madar_authz::SPEC_VERSION,
        org_id: org.to_string(),
        branch_id: branch.to_string(),
        device_id: device.to_string(),
        org_epoch: epoch,
        issued_at: chrono::Utc::now().timestamp(),
        expires_at: i64::MAX,
        policy,
        users,
    })
}
