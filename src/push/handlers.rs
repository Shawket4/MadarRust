//! `PUT`/`DELETE /push/token` — any authenticated user of any app registers
//! or clears one push device here. `src/staff/dawam/mod.rs`'s
//! `PUT /staff/me/push-token` is a thin alias of the same core for old app
//! builds.

use actix_web::{HttpRequest, HttpResponse, web};
use serde::Deserialize;
use utoipa::ToSchema;

use super::{register, unregister};
use crate::errors::{AppError, AppErrorResponse};
use crate::orgs::handlers::extract_claims;

#[derive(Deserialize, ToSchema)]
pub struct RegisterPushDevice {
    /// Which app this device belongs to, e.g. `"dawam"`.
    pub app: String,
    /// The FCM registration token.
    pub token: String,
    /// `ar` or `en`; defaults to `ar`.
    #[serde(default)]
    pub locale: Option<String>,
    /// Free-form platform hint (`"ios"`, `"android"`, ...); optional.
    #[serde(default)]
    pub platform: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct UnregisterPushDevice {
    pub app: String,
    pub token: String,
}

#[utoipa::path(
    put, path = "/push/token", tag = "push", request_body = RegisterPushDevice,
    responses((status = 204), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn set_push_token(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<RegisterPushDevice>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let user_id = claims.user_id_safe()?;
    let org_id = claims
        .org_id()
        .ok_or_else(|| AppError::Unauthorized("No organization for this account".into()))?;
    let app = body.app.trim();
    let token = body.token.trim();
    if app.is_empty() || token.is_empty() {
        return Err(AppError::BadRequest("app and token are required".into()));
    }
    // The staff app signs in as an employee and registers through
    // PUT /staff/me/push-token; a user session can't claim Dawam pushes.
    if app == crate::staff::dawam::PUSH_APP {
        return Err(AppError::BadRequest(
            "The Dawam app registers its pushes through /staff/me/push-token".into(),
        ));
    }
    let locale = match body.locale.as_deref() {
        Some("en") => "en",
        _ => "ar",
    };
    let platform = body.platform.as_deref().unwrap_or("");
    register(
        pool.get_ref(),
        org_id,
        super::Recipient::User(user_id),
        app,
        token,
        locale,
        platform,
    )
    .await?;
    Ok(HttpResponse::NoContent().finish())
}

#[utoipa::path(
    delete, path = "/push/token", tag = "push", request_body = UnregisterPushDevice,
    responses((status = 204), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_push_token(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<UnregisterPushDevice>,
) -> Result<HttpResponse, AppError> {
    let user_id = extract_claims(&req)?.user_id_safe()?;
    unregister(
        pool.get_ref(),
        super::Recipient::User(user_id),
        body.app.trim(),
        body.token.trim(),
    )
    .await?;
    Ok(HttpResponse::NoContent().finish())
}
