//! Staff sign-in by WhatsApp code (RO-1..RO-5, SA-3).
//!
//! The number is the one a manager entered on the person's account; there is
//! no self-registration. The code is the delivery OTP's mechanism with six
//! digits: plain text, 300 s, five tries, deleted on use. Verifying it binds
//! ONE phone: a new phone revokes the old one, the manager is told, and no
//! approval is needed.

use actix_web::{HttpResponse, web};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use super::{hash_token, notify_managers, revoke_devices};
use crate::auth::jwt::{JwtSecret, create_token};
use crate::errors::{AppError, AppErrorResponse};
use crate::models::UserRole;

const OTP_TTL_SECONDS: i64 = 300;
const OTP_MAX_ATTEMPTS: i32 = 5;
/// A staff session lasts a month; the phone binding, not the token, is what
/// ends it early (RO-4).
const SESSION_HOURS: i64 = 24 * 30;

#[derive(Deserialize, ToSchema)]
pub struct StaffOtpRequest {
    pub phone: String,
}

#[derive(Serialize, ToSchema)]
pub struct StaffOtpSent {
    pub sent: bool,
    /// The code itself — only from a debug build with `MADAR_DEV_OTP_ECHO=1`,
    /// so a developer can sign in without WhatsApp. Never in release.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dev_code: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct StaffOtpVerify {
    pub phone: String,
    pub code: String,
    /// The business to sign in to, when the number works at more than one (RO-5).
    #[serde(default)]
    pub org_id: Option<Uuid>,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

/// A business the number works at.
#[derive(Serialize, ToSchema, sqlx::FromRow, Clone)]
pub struct StaffOrgChoice {
    pub org_id: Uuid,
    pub org_name: String,
    /// False when the business is suspended: sign-in is stopped (SA-3).
    pub active: bool,
}

#[derive(Serialize, ToSchema)]
pub struct StaffSession {
    /// Set when the number works at more than one business and none was
    /// picked: ask, then verify again with `org_id`. The code stays valid.
    pub needs_org: bool,
    pub orgs: Vec<StaffOrgChoice>,
    /// `Authorization: Bearer` for every other call.
    pub token: Option<String>,
    /// Kept in the phone's secure storage and sent as `X-Staff-Device` on every
    /// punch and ping (RO-3).
    pub device_token: Option<String>,
    pub user_id: Option<Uuid>,
    pub name: Option<String>,
    pub role: Option<UserRole>,
    pub org_id: Option<Uuid>,
    /// True when this sign-in moved the account from another phone.
    pub new_phone: bool,
}

#[derive(sqlx::FromRow)]
struct Account {
    user_id: Uuid,
    name: String,
    role: UserRole,
    phone: Option<String>,
    org_id: Uuid,
    org_name: String,
    org_active: bool,
}

/// Every staff account whose number is this phone, in any business.
async fn accounts_for(pool: &PgPool, phone: &str) -> Result<Vec<Account>, AppError> {
    // Stored numbers are as a manager typed them: canonicalise both sides.
    let tail: String = phone
        .chars()
        .rev()
        .take(9)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    let rows: Vec<Account> = sqlx::query_as(
        "SELECT u.id AS user_id, u.name, u.role, u.phone, u.org_id, o.name AS org_name, \
                (o.is_active AND o.deleted_at IS NULL) AS org_active \
           FROM users u \
           JOIN staff_profiles p ON p.user_id = u.id AND p.employment_status = 'active' \
           JOIN organizations o ON o.id = u.org_id \
          WHERE u.deleted_at IS NULL AND u.is_active AND 'dawam' = ANY(o.modules) \
            AND regexp_replace(COALESCE(u.phone, ''), '\\D', '', 'g') LIKE '%' || $1",
    )
    .bind(&tail)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter(|a| {
            a.phone
                .as_deref()
                .and_then(|p| crate::phone::normalize_phone(p).ok())
                .is_some_and(|p| p == phone)
        })
        .collect())
}

fn six_digits() -> String {
    let b = *Uuid::new_v4().as_bytes();
    let n = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) % 1_000_000;
    format!("{n:06}")
}

#[utoipa::path(
    operation_id = "staff_otp_request",
    post, path = "/auth/staff/otp/request", tag = "staff-auth", request_body = StaffOtpRequest,
    responses((status = 200, body = StaffOtpSent), AppErrorResponse)
)]
pub async fn otp_request(
    pool: web::Data<PgPool>,
    body: web::Json<StaffOtpRequest>,
) -> Result<HttpResponse, AppError> {
    let phone = crate::phone::normalize_phone(&body.phone)?;
    if accounts_for(pool.get_ref(), &phone).await?.is_empty() {
        // RO-1: no self-registration.
        return Err(AppError::NotFound(
            "This number isn't registered with any business. Ask your manager to add you.".into(),
        ));
    }
    let recent: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM staff_otp WHERE phone = $1 \
                        AND created_at > now() - interval '60 seconds')",
    )
    .bind(&phone)
    .fetch_one(pool.get_ref())
    .await?;
    if recent {
        return Err(AppError::Conflict(
            "A code was just sent. Please wait a minute.".into(),
        ));
    }
    let code = six_digits();
    sqlx::query("DELETE FROM staff_otp WHERE phone = $1")
        .bind(&phone)
        .execute(pool.get_ref())
        .await?;
    sqlx::query(
        "INSERT INTO staff_otp (phone, code, expires_at) \
         VALUES ($1, $2, now() + ($3 || ' seconds')::interval)",
    )
    .bind(&phone)
    .bind(&code)
    .bind(OTP_TTL_SECONDS.to_string())
    .execute(pool.get_ref())
    .await?;
    crate::delivery::whatsapp::send_message(
        pool.get_ref().clone(),
        phone,
        crate::delivery::whatsapp::build_otp_message(&code),
    );
    let echo =
        cfg!(debug_assertions) && std::env::var("MADAR_DEV_OTP_ECHO").is_ok_and(|v| v == "1");
    Ok(HttpResponse::Ok().json(StaffOtpSent {
        sent: true,
        dev_code: echo.then_some(code),
    }))
}

#[utoipa::path(
    operation_id = "staff_otp_verify",
    post, path = "/auth/staff/otp/verify", tag = "staff-auth", request_body = StaffOtpVerify,
    responses((status = 200, body = StaffSession), AppErrorResponse)
)]
pub async fn otp_verify(
    pool: web::Data<PgPool>,
    secret: web::Data<JwtSecret>,
    body: web::Json<StaffOtpVerify>,
) -> Result<HttpResponse, AppError> {
    let pool = pool.get_ref();
    let phone = crate::phone::normalize_phone(&body.phone)?;
    if body.code.len() != 6 || !body.code.chars().all(|c| c.is_ascii_digit()) {
        return Err(AppError::BadRequest("Incorrect code.".into()));
    }
    // Claim the attempt and read the code in one statement: the database is
    // the arbiter of the five tries (see delivery::public::otp_verify).
    let claimed: Option<(Uuid, String, i32)> = sqlx::query_as(
        "UPDATE staff_otp SET attempts = attempts + 1 \
          WHERE id = (SELECT id FROM staff_otp WHERE phone = $1 AND expires_at > now() \
                       ORDER BY created_at DESC LIMIT 1) \
            AND attempts < $2 \
          RETURNING id, code, attempts",
    )
    .bind(&phone)
    .bind(OTP_MAX_ATTEMPTS)
    .fetch_optional(pool)
    .await?;
    let Some((otp_id, expected, used)) = claimed else {
        return Err(AppError::BadRequest(
            "No active code — request a new one.".into(),
        ));
    };
    if !crate::secrets::constant_time_eq(body.code.as_bytes(), expected.as_bytes()) {
        let left = OTP_MAX_ATTEMPTS - used;
        return Err(AppError::BadRequest(if left > 0 {
            format!("Incorrect code. {left} tries left.")
        } else {
            "Too many tries — request a new code.".into()
        }));
    }

    let accounts = accounts_for(pool, &phone).await?;
    let orgs: Vec<StaffOrgChoice> = accounts
        .iter()
        .map(|a| StaffOrgChoice {
            org_id: a.org_id,
            org_name: a.org_name.clone(),
            active: a.org_active,
        })
        .collect();
    let account = match body.org_id {
        Some(org) => accounts.iter().find(|a| a.org_id == org),
        None if accounts.len() == 1 => accounts.first(),
        None => {
            // Two businesses: ask which (RO-5). The code stays valid for the
            // second call; this try counted.
            return Ok(HttpResponse::Ok().json(StaffSession {
                needs_org: true,
                orgs,
                token: None,
                device_token: None,
                user_id: None,
                name: None,
                role: None,
                org_id: None,
                new_phone: false,
            }));
        }
    }
    .ok_or_else(|| AppError::NotFound("You don't work at that business.".into()))?;
    if !account.org_active {
        return Err(AppError::Forbidden(format!(
            "{} is suspended. Sign-in is stopped until it's reactivated.",
            account.org_name
        )));
    }

    // Deleted on use (RO-2).
    sqlx::query("DELETE FROM staff_otp WHERE id = $1")
        .bind(otp_id)
        .execute(pool)
        .await?;

    // One live phone (RO-4): this sign-in moves the account.
    let had_phone: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM staff_devices WHERE user_id = $1 AND org_id = $2 \
                        AND revoked_at IS NULL)",
    )
    .bind(account.user_id)
    .bind(account.org_id)
    .fetch_one(pool)
    .await?;
    revoke_devices(pool, account.user_id).await?;
    let device_token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO staff_devices (org_id, user_id, token_hash, platform, model) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(account.org_id)
    .bind(account.user_id)
    .bind(hash_token(&device_token))
    .bind(body.platform.as_deref().unwrap_or(""))
    .bind(body.model.as_deref().unwrap_or(""))
    .execute(pool)
    .await?;
    if had_phone {
        let branch = super::branches_of(pool, account.user_id)
            .await?
            .first()
            .copied();
        sqlx::query(
            "INSERT INTO attendance_flags (org_id, user_id, branch_id, kind) \
             VALUES ($1, $2, $3, 'new_phone')",
        )
        .bind(account.org_id)
        .bind(account.user_id)
        .bind(branch)
        .execute(pool)
        .await?;
        notify_managers(
            pool,
            account.org_id,
            branch,
            Some(account.user_id),
            "staff.n_new_phone",
            serde_json::json!({ "name": account.name, "device": body.model.clone().unwrap_or_default() }),
        )
        .await;
    }

    let token = create_token(
        &secret,
        account.user_id,
        Some(account.org_id),
        account.role.clone(),
        None,
        SESSION_HOURS,
    )
    .map_err(|_| AppError::Internal)?;
    sqlx::query("UPDATE users SET last_login_at = now() WHERE id = $1")
        .bind(account.user_id)
        .execute(pool)
        .await?;
    Ok(HttpResponse::Ok().json(StaffSession {
        needs_org: false,
        orgs,
        token: Some(token),
        device_token: Some(device_token),
        user_id: Some(account.user_id),
        name: Some(account.name.clone()),
        role: Some(account.role.clone()),
        org_id: Some(account.org_id),
        new_phone: had_phone,
    }))
}
