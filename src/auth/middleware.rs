use actix_web::{
    Error, HttpMessage, HttpRequest, ResponseError,
    body::{BoxBody, EitherBody},
    dev::{Service, ServiceRequest, ServiceResponse, Transform, forward_ready},
    web,
};
use futures::future::{LocalBoxFuture, Ready, ready};
use std::rc::Rc;
use uuid::Uuid;

use sqlx::PgPool;

use crate::{
    auth::{
        jwt::{JwtSecret, verify_token},
        org_status::OrgStatusCache,
    },
    errors::AppError,
};

/// The org the dashboard pinned via the `X-Org-Id` request header, if present
/// and a valid UUID. Pair with [`crate::auth::jwt::Claims::scope_org`]: the
/// header is only ever honoured for super admins; every other role's own token
/// org takes precedence, so reading it here is always safe.
pub fn header_org_id(req: &HttpRequest) -> Option<Uuid> {
    req.headers()
        .get("X-Org-Id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| Uuid::parse_str(s).ok())
}

// ── JwtMiddleware factory ─────────────────────────────────────

pub struct JwtMiddleware;

impl<S, B> Transform<S, ServiceRequest> for JwtMiddleware
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: 'static,
{
    type Response = ServiceResponse<EitherBody<B, BoxBody>>;
    type Error = Error;
    type Transform = JwtMiddlewareService<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ready(Ok(JwtMiddlewareService {
            service: Rc::new(service),
        }))
    }
}

pub struct JwtMiddlewareService<S> {
    service: Rc<S>,
}

impl<S, B> Service<ServiceRequest> for JwtMiddlewareService<S>
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
            // Extract Bearer token from Authorization header
            let token = req
                .headers()
                .get("Authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(|s| s.to_string());

            let token = match token {
                Some(t) => t,
                None => {
                    let resp = AppError::Unauthorized("Missing Authorization header".into())
                        .error_response()
                        .map_into_boxed_body();
                    return Ok(req.into_response(resp).map_into_right_body());
                }
            };

            // Verify token using JwtSecret from app data
            let secret = req
                .app_data::<web::Data<JwtSecret>>()
                .expect("JwtSecret not registered");

            let claims = match verify_token(secret, &token) {
                Ok(c) => c,
                Err(_) => {
                    let resp = AppError::Unauthorized("Invalid or expired token".into())
                        .error_response()
                        .map_into_boxed_body();
                    return Ok(req.into_response(resp).map_into_right_body());
                }
            };

            // Org-suspension kill-switch: reject every authenticated request
            // scoped to a suspended / soft-deleted org. Super admins carry no
            // `org_id` (None) and so bypass this — which also keeps the
            // reactivation path (super-admin-only) working against a down org.
            //
            // Enforcement is gated on `OrgStatusCache` being registered: prod
            // wires it in `main.rs`, so the check is always live there; the many
            // test apps that don't register it simply skip the check (no behaviour
            // change, no DB hit). The pool is read the same way so a stray test
            // without one degrades gracefully rather than panicking.
            if let (Some(org_id), Some(cache), Some(pool)) = (
                claims.org_id(),
                req.app_data::<web::Data<OrgStatusCache>>().cloned(),
                req.app_data::<web::Data<PgPool>>().cloned(),
            ) {
                match cache.is_allowed(pool.get_ref(), org_id).await {
                    Ok(true) => {}
                    Ok(false) => {
                        let resp = AppError::OrgSuspended
                            .error_response()
                            .map_into_boxed_body();
                        return Ok(req.into_response(resp).map_into_right_body());
                    }
                    // DB unreachable while resolving org status — fail closed.
                    // The handler would fail on its own queries anyway; a 503
                    // here is the honest signal.
                    Err(_) => {
                        let resp = AppError::ServiceUnavailable(
                            "Could not verify organization status".into(),
                        )
                        .error_response()
                        .map_into_boxed_body();
                        return Ok(req.into_response(resp).map_into_right_body());
                    }
                }
            }

            // Attach claims to request extensions so handlers can read them
            req.extensions_mut().insert(claims);

            svc.call(req).await.map(|r| r.map_into_left_body())
        })
    }
}
