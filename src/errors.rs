use std::collections::BTreeMap;

use actix_web::HttpResponse;
use serde::Serialize;
use thiserror::Error;
use utoipa::openapi::{ContentBuilder, Ref, RefOr, Response, ResponseBuilder};
use utoipa::{IntoResponses, ToSchema};

#[derive(Debug, Error)]
pub enum AppError {
    #[error("Unauthorized: {0}")]
    Unauthorized(String),

    #[error("Forbidden: {0}")]
    Forbidden(String),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Bad request: {0}")]
    BadRequest(String),

    #[error("Conflict: {0}")]
    Conflict(String),

    #[error("Database error: {0}")]
    Db(#[from] sqlx::Error),

    #[error("Internal error")]
    Internal,
}

/// Wire shape of every error JSON. Keep in lockstep with
/// `AppError::error_response` below.
#[derive(Serialize, ToSchema)]
pub struct ErrorBody {
    /// Human-readable error message.
    #[schema(example = "Something went wrong")]
    pub error: String,
}

impl actix_web::ResponseError for AppError {
    fn error_response(&self) -> HttpResponse {
        let body = ErrorBody { error: self.to_string() };
        match self {
            AppError::Unauthorized(_) => HttpResponse::Unauthorized().json(body),
            AppError::Forbidden(_)    => HttpResponse::Forbidden().json(body),
            AppError::NotFound(_)     => HttpResponse::NotFound().json(body),
            AppError::BadRequest(_)   => HttpResponse::BadRequest().json(body),
            AppError::Conflict(_)     => HttpResponse::Conflict().json(body),
            AppError::Db(_)           => HttpResponse::InternalServerError().json(body),
            AppError::Internal        => HttpResponse::InternalServerError().json(body),
        }
    }
}

/// Marker type used in `#[utoipa::path(responses(..., AppErrorResponse))]`
/// to attach the shared error-response set to a handler in one token.
///
/// The `IntoResponses` impl below is manual rather than derived because
/// utoipa's derive macro either inlines the body schema at every error
/// site (with `#[to_schema]`) or requires a `ToResponse` wrapper plus
/// `components(responses(...))` registration. The hand-rolled impl gives
/// us exactly what we want: each status emits a `$ref` to the registered
/// `ErrorBody` schema, so the spec stays compact and generated TS/Dart
/// clients get one shared `ErrorBody` type instead of one per error site.
pub struct AppErrorResponse;

impl IntoResponses for AppErrorResponse {
    fn responses() -> BTreeMap<String, RefOr<Response>> {
        // Helper: build a JSON response with `$ref` to ErrorBody.
        fn err(description: &str) -> RefOr<Response> {
            let content = ContentBuilder::new()
                .schema(Some(Ref::from_schema_name("ErrorBody")))
                .build();
            RefOr::T(
                ResponseBuilder::new()
                    .description(description)
                    .content("application/json", content)
                    .build(),
            )
        }

        BTreeMap::from([
            ("400".to_string(), err("Bad request — validation failed or malformed input")),
            ("401".to_string(), err("Unauthorized — missing or invalid bearer token")),
            ("403".to_string(), err("Forbidden — insufficient permission or wrong org")),
            ("404".to_string(), err("Not found")),
            ("409".to_string(), err("Conflict — FK or domain invariant violation")),
            ("500".to_string(), err("Internal server error")),
        ])
    }
}