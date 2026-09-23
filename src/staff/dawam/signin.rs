//! Staff sign-in by WhatsApp code (RO-1..RO-5, SA-3).
//!
//! The number is the one a manager entered on the person's account; there is
//! no self-registration. The code is the delivery OTP's mechanism with six
//! digits: plain text, 300 s, five tries, deleted on use. Verifying it binds
//! ONE phone: a new phone revokes the old one, the manager is told, and no
//! approval is needed.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use super::{hash_token, notify_managers};
use crate::auth::jwt::JwtSecret;
use crate::authz::Cap;
use crate::errors::{AppError, AppErrorResponse};
use crate::models::UserRole;
use crate::staff::principal::{self, DEVICE_HEADER};

const OTP_TTL_SECONDS: i64 = 300;
const OTP_MAX_ATTEMPTS: i32 = 5;

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
    /// The staff token: `Authorization: Bearer` on `/staff/*` only. It lives
    /// an hour; refresh it with `POST /auth/staff/refresh`.
    pub token: Option<String>,
    pub token_expires_at: Option<DateTime<Utc>>,
    /// Kept in the phone's secure storage and sent as `X-Staff-Device` on
    /// every call (RO-3). It is what refreshes the session.
    pub device_token: Option<String>,
    /// Who signed in: the employee.
    pub employee_id: Option<Uuid>,
    /// Their Madar account when they have one (a manager, a cashier). Manager
    /// acts in the app go through it.
    pub user_id: Option<Uuid>,
    pub name: Option<String>,
    /// The linked account's role; null for an employee with no account.
    pub role: Option<UserRole>,
    pub org_id: Option<Uuid>,
    /// True when this sign-in moved the account from another phone.
    pub new_phone: bool,
}

#[derive(sqlx::FromRow)]
struct Account {
    employee_id: Uuid,
    user_id: Option<Uuid>,
    name: String,
    role: Option<UserRole>,
    org_id: Uuid,
    org_name: String,
    org_active: bool,
}

/// Every employee, in any business, who signs in to the app with this phone
/// (`phone` is canonical, see `crate::phone`).
async fn accounts_for(pool: &PgPool, phone: &str) -> Result<Vec<Account>, AppError> {
    Ok(sqlx::query_as(
        "SELECT e.id AS employee_id, u.id AS user_id, e.name, u.role, e.org_id, \
                o.name AS org_name, (o.is_active AND o.deleted_at IS NULL) AS org_active \
           FROM employees e \
           JOIN organizations o ON o.id = e.org_id \
           LEFT JOIN users u ON u.id = e.user_id AND u.is_active AND u.deleted_at IS NULL \
          WHERE e.phone_key = $1 AND e.app_access AND e.employment_status = 'active' \
            AND 'dawam' = ANY(o.modules) \
          ORDER BY o.name, e.id",
    )
    .bind(phone)
    .fetch_all(pool)
    .await?)
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
                token_expires_at: None,
                device_token: None,
                employee_id: None,
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

    // One live phone (RO-4): this sign-in moves the employee. Serialised per
    // employee, so two verifies at once can't both bind a phone (B-11).
    let device_token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('staff_device:' || $1::text))")
        .bind(account.employee_id)
        .execute(&mut *tx)
        .await?;
    let had_phone: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM staff_devices WHERE employee_id = $1 AND revoked_at IS NULL)",
    )
    .bind(account.employee_id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE staff_devices SET revoked_at = now() \
          WHERE employee_id = $1 AND revoked_at IS NULL",
    )
    .bind(account.employee_id)
    .execute(&mut *tx)
    .await?;
    let device_id: Uuid = sqlx::query_scalar(
        "INSERT INTO staff_devices (org_id, employee_id, token_hash, platform, model) \
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(account.org_id)
    .bind(account.employee_id)
    .bind(hash_token(&device_token))
    .bind(body.platform.as_deref().unwrap_or(""))
    .bind(body.model.as_deref().unwrap_or(""))
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    // The old phone's pushes go with it.
    crate::push::revoke_all(
        pool,
        crate::push::Recipient::Employee(account.employee_id),
        super::PUSH_APP,
    )
    .await?;
    if had_phone {
        let branch = super::branches_of(pool, account.employee_id)
            .await?
            .first()
            .copied();
        sqlx::query(
            "INSERT INTO attendance_flags (org_id, employee_id, branch_id, kind) \
             VALUES ($1, $2, $3, 'new_phone')",
        )
        .bind(account.org_id)
        .bind(account.employee_id)
        .bind(branch)
        .execute(pool)
        .await?;
        notify_managers(
            pool,
            account.org_id,
            branch,
            Cap::HrStaffEdit,
            Some(account.employee_id),
            "staff.n_new_phone",
            serde_json::json!({ "name": account.name, "device": body.model.clone().unwrap_or_default() }),
        )
        .await;
    }

    let (token, expires) = principal::mint(
        &secret,
        account.employee_id,
        account.org_id,
        account.user_id,
        device_id,
    )?;
    if let Some(user) = account.user_id {
        sqlx::query("UPDATE users SET last_login_at = now() WHERE id = $1")
            .bind(user)
            .execute(pool)
            .await?;
    }
    Ok(HttpResponse::Ok().json(StaffSession {
        needs_org: false,
        orgs,
        token: Some(token),
        token_expires_at: Some(expires),
        device_token: Some(device_token),
        employee_id: Some(account.employee_id),
        user_id: account.user_id,
        name: Some(account.name.clone()),
        role: account.role.clone(),
        org_id: Some(account.org_id),
        new_phone: had_phone,
    }))
}

#[derive(Serialize, ToSchema)]
pub struct StaffTokenRefresh {
    /// A fresh staff token for `/staff/*`.
    pub token: String,
    pub expires_at: DateTime<Utc>,
    pub employee_id: Uuid,
    pub org_id: Uuid,
}

/// A fresh staff token for the phone that sends its device token in
/// `X-Staff-Device` (RO-3). The device is the refresh credential: once it is
/// revoked (a new phone, a new number, the employee deactivated) this answers
/// 401 `DEVICE_REVOKED` and the app signs out. The same checks as every
/// `/staff/*` call: the employee is active with app access, the business is
/// active and has Dawam on.
#[utoipa::path(
    operation_id = "staff_token_refresh",
    post, path = "/auth/staff/refresh", tag = "staff-auth",
    params(("X-Staff-Device" = String, Header, description = "The device token from sign-in")),
    responses((status = 200, body = StaffTokenRefresh), AppErrorResponse)
)]
pub async fn refresh(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    secret: web::Data<JwtSecret>,
) -> Result<HttpResponse, AppError> {
    let pool = pool.get_ref();
    let sent = req
        .headers()
        .get(DEVICE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .ok_or_else(principal::device_revoked)?;
    let row: Option<(Uuid, Uuid, Uuid)> = sqlx::query_as(
        "SELECT id, employee_id, org_id FROM staff_devices \
          WHERE token_hash = $1 AND revoked_at IS NULL",
    )
    .bind(hash_token(&sent))
    .fetch_optional(pool)
    .await?;
    let Some((device_id, employee_id, org_id)) = row else {
        return Err(principal::device_revoked());
    };
    let (who, _) =
        principal::check_session(pool, employee_id, org_id, device_id, Some(&sent)).await?;
    let (token, expires_at) =
        principal::mint(&secret, employee_id, org_id, who.user_id, device_id)?;
    Ok(HttpResponse::Ok().json(StaffTokenRefresh {
        token,
        expires_at,
        employee_id,
        org_id,
    }))
}
