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

    #[error("Service unavailable: {0}")]
    ServiceUnavailable(String),

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

impl AppError {
    /// Classify a database error by its Postgres SQLSTATE so failures caused by
    /// bad *client* input surface as 4xx instead of a blanket 500. Genuine
    /// backend failures (connection loss, deadlock, etc.) still map to 500.
    ///
    /// Found via API fuzzing: previously a negative `page`, an out-of-range
    /// number, an invalid enum/UUID, a NUL byte, or a unique/FK violation all
    /// returned 500 because every sqlx error mapped to InternalServerError.
    fn db_status(e: &sqlx::Error) -> actix_web::http::StatusCode {
        status_for_sqlstate(e.as_database_error().and_then(|d| d.code()).as_deref())
    }
}

/// Map a Postgres SQLSTATE to an HTTP status. Pure so it can be unit-tested.
/// Class 22 (data exception) and the check/not-null integrity codes are
/// client-input faults → 4xx; unique/FK and other integrity violations → 409;
/// anything else (connection, deadlock, internal) stays 500.
fn status_for_sqlstate(code: Option<&str>) -> actix_web::http::StatusCode {
    use actix_web::http::StatusCode;
    match code {
        Some("23505") | Some("23503") => StatusCode::CONFLICT,    // unique / foreign-key violation
        Some("23514") | Some("23502") => StatusCode::BAD_REQUEST, // check / not-null violation
        Some(c) if c.starts_with("22") => StatusCode::BAD_REQUEST, // data exception (overflow, bad enum/uuid/encoding, offset range)
        Some(c) if c.starts_with("23") => StatusCode::CONFLICT,    // other integrity violations
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::status_for_sqlstate;
    use actix_web::http::StatusCode;

    #[test]
    fn classifies_sqlstates() {
        assert_eq!(status_for_sqlstate(Some("23505")), StatusCode::CONFLICT); // unique
        assert_eq!(status_for_sqlstate(Some("23503")), StatusCode::CONFLICT); // foreign key
        assert_eq!(status_for_sqlstate(Some("23514")), StatusCode::BAD_REQUEST); // check
        assert_eq!(status_for_sqlstate(Some("23502")), StatusCode::BAD_REQUEST); // not null
        assert_eq!(status_for_sqlstate(Some("22003")), StatusCode::BAD_REQUEST); // numeric overflow
        assert_eq!(status_for_sqlstate(Some("22P02")), StatusCode::BAD_REQUEST); // invalid text/enum
        assert_eq!(status_for_sqlstate(Some("22021")), StatusCode::BAD_REQUEST); // bad encoding / NUL
        assert_eq!(status_for_sqlstate(Some("2201X")), StatusCode::BAD_REQUEST); // offset out of range
        assert_eq!(status_for_sqlstate(Some("40P01")), StatusCode::INTERNAL_SERVER_ERROR); // deadlock
        assert_eq!(status_for_sqlstate(Some("08006")), StatusCode::INTERNAL_SERVER_ERROR); // connection failure
        assert_eq!(status_for_sqlstate(None), StatusCode::INTERNAL_SERVER_ERROR);
    }
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
            AppError::Db(e)           => HttpResponse::build(Self::db_status(e)).json(body),
            AppError::ServiceUnavailable(_) => HttpResponse::ServiceUnavailable().json(body),
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