//! Who is calling a `/staff/*` route: the staff app's employee, or a Madar
//! user (dashboard, POS).
//!
//! THE STAFF TOKEN (Dawam Phase A, PHASE_A_DESIGN.md §4). A WhatsApp code
//! signs a phone in for one EMPLOYEE (not a user): the token's subject is the
//! employee, it names the device it was minted for and, when the employee is
//! linked to a Madar user, that user. It lives an hour; the phone refreshes it
//! with its device token (`POST /auth/staff/refresh`), so revoking the device
//! ends the session at once and for good.
//!
//! It is accepted ONLY here. The ordinary [`Claims`] verifier refuses it (it
//! carries an audience and no role), so a staff token is a 401 on every POS
//! and dashboard route, and a WhatsApp code can never become a dashboard
//! session.
//!
//! [`StaffAuth`] wraps the `/staff` scope instead of `JwtMiddleware`. On EVERY
//! request made with a staff token it checks the device (live, the employee's,
//! and the one whose secret the phone sends in `X-Staff-Device`), the employee
//! (active, app access), the org (active) and the Dawam module (on). A linked,
//! active user is then exposed as ordinary `Claims`, so the management
//! handlers and authz run exactly as they do for a dashboard session; an
//! unlinked employee has no manager powers at all.

use std::rc::Rc;

use actix_web::{
    Error, FromRequest, HttpMessage, HttpRequest, ResponseError,
    body::{BoxBody, EitherBody},
    dev::{Payload, Service, ServiceRequest, ServiceResponse, Transform, forward_ready},
    web,
};
use chrono::{DateTime, Duration, Utc};
use futures::future::{LocalBoxFuture, Ready, ready};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::{Claims, JwtSecret, verify_token};
use crate::errors::AppError;
use crate::models::UserRole;

/// The audience only `/staff/*` accepts.
pub const STAFF_AUDIENCE: &str = "dawam-staff";
const STAFF_TYP: &str = "dawam_staff";
/// A staff token's life. The device refreshes it (RO-3); revoking the device
/// is what ends a session.
pub const STAFF_TOKEN_MINUTES: i64 = 60;

/// The header the staff app sends its device token in (RO-3).
pub const DEVICE_HEADER: &str = "x-staff-device";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaffClaims {
    /// The employee.
    pub sub: String,
    pub org: String,
    /// The linked Madar user, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
    /// The `staff_devices` row this session belongs to.
    pub dev: String,
    pub typ: String,
    pub aud: String,
    pub iat: usize,
    pub exp: usize,
}

/// The staff app's caller, set by [`StaffAuth`] for a staff token.
#[derive(Debug, Clone)]
pub struct StaffPrincipal {
    pub employee_id: Uuid,
    pub org_id: Uuid,
    /// The linked user — present only when that user is active in this org.
    pub user_id: Option<Uuid>,
    pub device_id: Uuid,
}

impl StaffPrincipal {
    /// What vouches for this phone's offline stamps: its own device row (CL-11).
    pub fn verifier<'a>(&self, secret: &'a JwtSecret) -> crate::staff::dawam::clock::Verifier<'a> {
        crate::staff::dawam::clock::Verifier {
            secret,
            device: Some(self.device_id),
        }
    }
}

/// The staff principal on a request, if the staff app made it.
pub fn staff_principal(req: &HttpRequest) -> Option<StaffPrincipal> {
    req.extensions().get::<StaffPrincipal>().cloned()
}

/// Mint a staff token for `employee` on `device`. Returns the token and when
/// it expires.
pub fn mint(
    secret: &JwtSecret,
    employee: Uuid,
    org: Uuid,
    user: Option<Uuid>,
    device: Uuid,
) -> Result<(String, DateTime<Utc>), AppError> {
    let now = Utc::now();
    let exp = now + Duration::minutes(STAFF_TOKEN_MINUTES);
    let claims = StaffClaims {
        sub: employee.to_string(),
        org: org.to_string(),
        uid: user.map(|u| u.to_string()),
        dev: device.to_string(),
        typ: STAFF_TYP.into(),
        aud: STAFF_AUDIENCE.into(),
        iat: now.timestamp() as usize,
        exp: exp.timestamp() as usize,
    };
    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.0.as_bytes()),
    )
    .map_err(|_| AppError::Internal)?;
    Ok((token, exp))
}

/// Verify a staff token (signature, audience, expiry, type).
pub fn verify(secret: &JwtSecret, token: &str) -> Result<StaffClaims, jsonwebtoken::errors::Error> {
    let mut v = Validation::default();
    v.set_audience(&[STAFF_AUDIENCE]);
    v.set_required_spec_claims(&["exp", "aud", "sub"]);
    let data = decode::<StaffClaims>(token, &DecodingKey::from_secret(secret.0.as_bytes()), &v)?;
    if data.claims.typ != STAFF_TYP {
        return Err(jsonwebtoken::errors::ErrorKind::InvalidToken.into());
    }
    Ok(data.claims)
}

fn coded(status: u16, code: &'static str, reason: &str) -> AppError {
    AppError::Coded {
        status,
        code,
        reason: reason.into(),
    }
}

/// The phone was signed out: revoked, replaced, or never signed in here.
pub fn device_revoked() -> AppError {
    coded(
        401,
        "DEVICE_REVOKED",
        "This phone was signed out. Sign in again with your code.",
    )
}

pub fn dawam_off(org_name: &str) -> AppError {
    AppError::Coded {
        status: 403,
        code: "DAWAM_OFF",
        reason: format!("Dawam is switched off for {org_name}."),
    }
}

pub fn hash_device_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(token.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Everything a staff session needs to still be true, read in one query on
/// the owner pool (the token names the org; RLS has nothing to add).
#[derive(sqlx::FromRow)]
struct Live {
    employment_status: String,
    app_access: bool,
    token_hash: Option<String>,
    device_live: bool,
    org_ok: bool,
    dawam_on: bool,
    org_name: String,
    user_id: Option<Uuid>,
    user_role: Option<UserRole>,
    user_ok: bool,
}

/// Check a staff session against the database: the device, the employee,
/// the org and the module. The same checks guard the token refresh.
pub(crate) async fn check_session(
    pool: &PgPool,
    employee_id: Uuid,
    org_id: Uuid,
    device_id: Uuid,
    device_token: Option<&str>,
) -> Result<(StaffPrincipal, Option<UserRole>), AppError> {
    let row: Option<Live> = sqlx::query_as(
        "SELECT e.employment_status, e.app_access, d.token_hash, \
                COALESCE(d.revoked_at IS NULL, false) AS device_live, \
                (o.is_active AND o.deleted_at IS NULL) AS org_ok, \
                'dawam' = ANY(o.modules) AS dawam_on, o.name AS org_name, \
                u.id AS user_id, u.role AS user_role, \
                COALESCE(u.is_active AND u.deleted_at IS NULL AND u.org_id = e.org_id, false) AS user_ok \
           FROM employees e \
           JOIN organizations o ON o.id = e.org_id \
           LEFT JOIN staff_devices d ON d.id = $2 AND d.employee_id = e.id \
           LEFT JOIN users u ON u.id = e.user_id \
          WHERE e.id = $1 AND e.org_id = $3",
    )
    .bind(employee_id)
    .bind(device_id)
    .bind(org_id)
    .fetch_optional(pool)
    .await?;
    let Some(live) = row else {
        return Err(device_revoked());
    };
    let sent = device_token.map(hash_device_token);
    if !live.device_live || live.token_hash.is_none() || sent != live.token_hash {
        return Err(device_revoked());
    }
    if !live.org_ok {
        return Err(AppError::OrgSuspended);
    }
    if !live.dawam_on {
        return Err(dawam_off(&live.org_name));
    }
    if live.employment_status != "active" || !live.app_access {
        return Err(coded(
            403,
            "EMPLOYEE_INACTIVE",
            "You're no longer active at this business. Ask your manager.",
        ));
    }
    // Seen: at most a write a minute, not one per request.
    sqlx::query(
        "UPDATE staff_devices SET last_seen_at = now() \
          WHERE id = $1 AND last_seen_at < now() - INTERVAL '1 minute'",
    )
    .bind(device_id)
    .execute(pool)
    .await?;
    let user = live.user_id.filter(|_| live.user_ok);
    Ok((
        StaffPrincipal {
            employee_id,
            org_id,
            user_id: user,
            device_id,
        },
        user.and(live.user_role),
    ))
}

/// A dashboard or POS session on `/staff/*`: the org must be active and have
/// Dawam switched on (PS-7, SA-3). A super admin (no org of their own) passes.
async fn check_user_org(pool: &PgPool, claims: &Claims) -> Result<(), AppError> {
    let Some(org) = claims.org_id() else {
        return if claims.role == UserRole::SuperAdmin {
            Ok(())
        } else {
            Err(AppError::Forbidden(
                "Token carries no organization scope".into(),
            ))
        };
    };
    let row: Option<(bool, bool, String)> = sqlx::query_as(
        "SELECT (is_active AND deleted_at IS NULL), 'dawam' = ANY(modules), name \
           FROM organizations WHERE id = $1",
    )
    .bind(org)
    .fetch_optional(pool)
    .await?;
    match row {
        None | Some((false, _, _)) => Err(AppError::OrgSuspended),
        Some((true, false, name)) => Err(dawam_off(&name)),
        Some((true, true, _)) => Ok(()),
    }
}

// ── StaffAuth ──────────────────────────────────────────────────────────────

pub struct StaffAuth;

impl<S, B> Transform<S, ServiceRequest> for StaffAuth
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: 'static,
{
    type Response = ServiceResponse<EitherBody<B, BoxBody>>;
    type Error = Error;
    type Transform = StaffAuthService<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ready(Ok(StaffAuthService {
            service: Rc::new(service),
        }))
    }
}

pub struct StaffAuthService<S> {
    service: Rc<S>,
}

/// Authenticate a `/staff/*` request. For the staff app's own session it also
/// returns the signed server time to hand back to that phone (CL-11).
async fn authenticate(req: &ServiceRequest) -> Result<Option<String>, AppError> {
    let token = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string)
        .ok_or_else(|| AppError::Unauthorized("Missing Authorization header".into()))?;
    let secret = req
        .app_data::<web::Data<JwtSecret>>()
        .cloned()
        .ok_or(AppError::Internal)?;
    let pool = req
        .app_data::<web::Data<PgPool>>()
        .cloned()
        .ok_or(AppError::Internal)?;

    // A Madar user's session (dashboard, POS).
    if let Ok(claims) = verify_token(&secret, &token) {
        check_user_org(pool.get_ref(), &claims).await?;
        req.extensions_mut().insert(claims);
        return Ok(None);
    }

    // The staff app's session.
    let staff = match verify(&secret, &token) {
        Ok(c) => c,
        Err(e) if matches!(e.kind(), jsonwebtoken::errors::ErrorKind::ExpiredSignature) => {
            return Err(coded(
                401,
                "TOKEN_EXPIRED",
                "Your session expired. Refresh it with this phone.",
            ));
        }
        Err(_) => return Err(AppError::Unauthorized("Invalid or expired token".into())),
    };
    let parse = |s: &str| Uuid::parse_str(s).map_err(|_| device_revoked());
    let employee = parse(&staff.sub)?;
    let org = parse(&staff.org)?;
    let device = parse(&staff.dev)?;
    let sent = req
        .headers()
        .get(DEVICE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let (principal, role) =
        check_session(pool.get_ref(), employee, org, device, sent.as_deref()).await?;
    // The token's own link must still hold: relinking to another user, or
    // unlinking, takes the manager powers away at once.
    let token_user = staff.uid.as_deref().and_then(|u| Uuid::parse_str(u).ok());
    if let (Some(user), Some(role)) = (principal.user_id.filter(|u| Some(*u) == token_user), role) {
        req.extensions_mut().insert(Claims {
            sub: user.to_string(),
            org_id: Some(org.to_string()),
            role,
            branch_id: None,
            exp: staff.exp,
            iat: staff.iat,
        });
    }
    let anchor = crate::staff::dawam::clock::sign_anchor(&secret, principal.device_id, Utc::now());
    req.extensions_mut().insert(principal);
    Ok(Some(anchor))
}

impl<S, B> Service<ServiceRequest> for StaffAuthService<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: 'static,
{
    type Response = ServiceResponse<EitherBody<B, BoxBody>>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let svc = self.service.clone();
        Box::pin(async move {
            let anchor = match authenticate(&req).await {
                Ok(a) => a,
                Err(e) => {
                    let resp = e.error_response().map_into_boxed_body();
                    return Ok(req.into_response(resp).map_into_right_body());
                }
            };
            let mut res = svc.call(req).await?;
            // Every answer the phone gets carries the signed server time it
            // dates offline punches from (CL-11).
            if let Some(a) = anchor
                && let Ok(v) = actix_web::http::header::HeaderValue::from_str(&a)
            {
                res.headers_mut().insert(
                    actix_web::http::header::HeaderName::from_static(
                        crate::staff::dawam::clock::ANCHOR_HEADER,
                    ),
                    v,
                );
            }
            Ok(res.map_into_left_body())
        })
    }
}

// ── extractors ─────────────────────────────────────────────────────────────

/// The employee calling `/staff/me/*` from the staff app. Anything else is a
/// 403 `STAFF_APP_ONLY`: a punch comes only from the employee's live phone
/// (CL-1), never from a dashboard or till session.
#[derive(Debug, Clone)]
pub struct Me(pub StaffPrincipal);

impl std::ops::Deref for Me {
    type Target = StaffPrincipal;
    fn deref(&self) -> &StaffPrincipal {
        &self.0
    }
}

impl FromRequest for Me {
    type Error = AppError;
    type Future = Ready<Result<Me, AppError>>;

    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        ready(
            req.extensions()
                .get::<StaffPrincipal>()
                .cloned()
                .map(Me)
                .ok_or_else(|| {
                    coded(
                        403,
                        "STAFF_APP_ONLY",
                        "This is for the Dawam app, signed in on your phone.",
                    )
                }),
        )
    }
}

/// The acting Madar user on a management route: a dashboard/POS session, or
/// the staff app's employee through their linked user. An employee with no
/// linked (active) user gets a 403, never a 401 — the app must not read it as
/// "signed out".
pub fn caller(req: &HttpRequest) -> Result<Claims, AppError> {
    if let Some(c) = req.extensions().get::<Claims>() {
        return Ok(c.clone());
    }
    if req.extensions().get::<StaffPrincipal>().is_some() {
        return Err(coded(
            403,
            "MANAGER_ACCOUNT_NEEDED",
            "Managing others needs a Madar account with those rights.",
        ));
    }
    Err(AppError::Unauthorized("Missing claims".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret() -> JwtSecret {
        JwtSecret("unit".into())
    }

    #[test]
    fn a_staff_token_is_never_a_user_session_and_back() {
        let (e, o, u, d) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        let (staff, exp) = mint(&secret(), e, o, Some(u), d).unwrap();
        assert!(exp <= Utc::now() + Duration::minutes(STAFF_TOKEN_MINUTES));
        // The dashboard / POS verifier refuses it.
        assert!(verify_token(&secret(), &staff).is_err());
        // The staff verifier reads it back.
        let c = verify(&secret(), &staff).unwrap();
        assert_eq!(
            (c.sub, c.org, c.uid, c.dev),
            (
                e.to_string(),
                o.to_string(),
                Some(u.to_string()),
                d.to_string()
            )
        );
        // And refuses a user's session token.
        let user =
            crate::auth::jwt::create_token(&secret(), u, Some(o), UserRole::OrgAdmin, None, 24)
                .unwrap();
        assert!(verify(&secret(), &user).is_err());
        // Another secret: nothing.
        assert!(verify(&JwtSecret("other".into()), &staff).is_err());
    }

    #[test]
    fn the_device_token_is_stored_hashed() {
        let h = hash_device_token("abc");
        assert_eq!(h.len(), 64);
        assert_ne!(h, "abc");
        assert_eq!(h, hash_device_token("abc"));
    }
}
