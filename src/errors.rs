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

    #[error("This organization is suspended. Contact support to reactivate it.")]
    OrgSuspended,

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Bad request: {0}")]
    BadRequest(String),

    #[error("Conflict: {0}")]
    Conflict(String),

    /// A 409 the client must branch on. Same status as `Conflict`, with a
    /// stable `code` in the body: the ops a till queues offline replay with
    /// nobody reading the prose, and without a code every conflict collapsed
    /// into "done" on the device (see `floor_ops::refusal`).
    #[error("{reason}")]
    Refused { code: &'static str, reason: String },

    /// A `Refused` 409 that also names the till in the way (`TILL_OPEN_*`), so
    /// the device can say where the person's till is open.
    #[error("{reason}")]
    RefusedWith {
        code: &'static str,
        reason: String,
        till: serde_json::Value,
    },

    /// A coded refusal at a status other than 409 (`RECONCILIATION_*` 400,
    /// `PAYMENT_METHOD_UNAVAILABLE` 422, `TILL_ENTITY_REMOVED` 410).
    #[error("{reason}")]
    Coded {
        status: u16,
        code: &'static str,
        reason: String,
    },

    /// A coded refusal that also carries its figures (`vars`), so a client can
    /// word it in its own language (AT-13): e.g. `OUTSIDE_FENCE`
    /// `{distance_m, radius_m}`. `reason` stays the English sentence.
    #[error("{reason}")]
    CodedVars {
        status: u16,
        code: &'static str,
        reason: String,
        vars: serde_json::Value,
    },

    #[error("Database error: {0}")]
    Db(sqlx::Error),

    #[error("Service unavailable: {0}")]
    ServiceUnavailable(String),

    /// Asked for too much, too fast. Distinct from `Conflict` because a client
    /// should retry this one and only this one.
    #[error("{0}")]
    TooManyRequests(String),

    /// Too many wrong PINs at this till. Carries the remaining wait in seconds
    /// so the POS can show a live countdown instead of guessing
    /// (POS_SIGNIN_OVERHAUL.md §3.4). A wrong PIN matches nobody, so there is
    /// no account to lock — and a shared counter tablet must never be locked.
    #[error("Too many wrong PINs. Try again in {seconds} seconds.")]
    PinThrottled { seconds: i64 },

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
    /// Stable, machine-readable code for the error classes a client must branch
    /// on programmatically (e.g. `ORG_SUSPENDED`). Omitted for the generic
    /// cases where the status code alone is enough.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "ORG_SUSPENDED")]
    pub code: Option<String>,
    /// The till a `TILL_OPEN_AT_OTHER_BRANCH` / `TILL_OPEN_ELSEWHERE` refusal
    /// is about (`TillBrief`). Omitted everywhere else.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub till: Option<serde_json::Value>,
    /// How long to wait before trying again, in seconds. Present on a
    /// `PIN_THROTTLED` refusal, absent everywhere else, so the PIN pad can run
    /// a countdown rather than inventing one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<i64>,
    /// The figures of a coded refusal (`CodedVars`), for the client's own
    /// wording. Omitted everywhere else.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub vars: Option<serde_json::Value>,
}

/// Convert sqlx errors into `AppError`. `RowNotFound` — what `fetch_one` /
/// `query_scalar` return when a client asks for a row that doesn't exist — has
/// no SQLSTATE, so it would otherwise fall through `db_status` to a blanket 500.
/// It's a client-caused "resource absent" condition, so map it to a clean 404;
/// everything else keeps its SQLSTATE-based classification via [`AppError::Db`].
/// (API fuzzing flagged `GET /orgs/{id}/offline-auth-bundle` 500-ing on an
/// unknown id — this fixes that whole `fetch_one`-on-missing-row class.)
impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        match e {
            sqlx::Error::RowNotFound => AppError::NotFound("Resource not found".into()),
            // The combos module's guard triggers raise their contract code
            // first in the message (a check violation); the API checks first,
            // so this is the backstop's wording for every other write path
            // (the studio's sizes, a recipe line, a choice group on a combo).
            other if combo_guard(&other).is_some() => {
                let (status, code, reason) = combo_guard(&other).expect("checked");
                AppError::Coded {
                    status,
                    code,
                    reason: reason.into(),
                }
            }
            // The database refuses these on its own (Dawam RQ-9, RQ-11), so two
            // requests sent at once can't both land; say why in words.
            other => match other.as_database_error().and_then(|d| d.constraint()) {
                Some("staff_requests_no_overlap") => AppError::Refused {
                    code: "OVERLAPPING_REQUEST",
                    reason: "You already have a request like this for that time.".into(),
                },
                Some("staff_swaps_one_open") => AppError::Refused {
                    code: "SWAP_EXISTS",
                    reason: "You've already asked for this swap — it's waiting.".into(),
                },
                Some("staff_requests_live_correction_unique")
                | Some("staff_requests_live_shift_correction_unique") => AppError::Refused {
                    code: "CORRECTION_WAITING",
                    reason: "This shift already has a correction waiting.".into(),
                },
                _ => AppError::Db(other),
            },
        }
    }
}

/// A combos guard trigger's refusal (`migrations/20261005100000_combos.sql`):
/// its code, HTTP status and English sentence.
fn combo_guard(e: &sqlx::Error) -> Option<(u16, &'static str, &'static str)> {
    let d = e.as_database_error()?;
    if d.code().as_deref() != Some("23514") {
        return None;
    }
    let msg = d.message();
    const GUARDS: &[(&str, u16, &str)] = &[
        ("COMBO_NESTED", 400, "A combo can't contain another combo."),
        (
            "COMBO_NO_RECIPE",
            409,
            "A combo has no recipe of its own; each item uses its own.",
        ),
        (
            "COMBO_KIND_LOCKED",
            409,
            "This item has sales; its type can't change.",
        ),
        (
            "MEAL_TARGET_INVALID",
            400,
            "That combo has no slot for this item.",
        ),
        ("COMBO_SLOT_INVALID", 400, "Check the combo's slots."),
        (
            "COMBO_CHOICE_NOT_ALLOWED",
            400,
            "That item can't be chosen here.",
        ),
        ("DEAL_INVALID", 400, "Check the deal."),
    ];
    GUARDS.iter().find_map(|(code, status, reason)| {
        msg.starts_with(&format!("{code}:"))
            .then_some((*status, *code, *reason))
    })
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

    /// Stable machine-readable code surfaced in [`ErrorBody::code`]. Only the
    /// classes a client must branch on carry one; everything else is `None`.
    fn code(&self) -> Option<String> {
        match self {
            AppError::OrgSuspended => Some("ORG_SUSPENDED".to_string()),
            // The dashboard branches on this to say "wait a moment" rather
            // than showing a raw error for something that is not a fault.
            AppError::TooManyRequests(_) => Some("EXPORT_RATE_LIMITED".to_string()),
            AppError::PinThrottled { .. } => Some("PIN_THROTTLED".to_string()),
            AppError::Refused { code, .. }
            | AppError::RefusedWith { code, .. }
            | AppError::Coded { code, .. }
            | AppError::CodedVars { code, .. } => Some((*code).to_string()),
            _ => None,
        }
    }
}

/// Map a Postgres SQLSTATE to an HTTP status. Pure so it can be unit-tested.
/// Class 22 (data exception) and the check/not-null integrity codes are
/// client-input faults → 4xx; unique/FK and other integrity violations → 409;
/// anything else (connection, deadlock, internal) stays 500.
fn status_for_sqlstate(code: Option<&str>) -> actix_web::http::StatusCode {
    use actix_web::http::StatusCode;
    match code {
        Some("23505") | Some("23503") => StatusCode::CONFLICT, // unique / foreign-key violation
        Some("23P01") => StatusCode::CONFLICT,                 // exclusion (overlapping ranges)
        Some("23514") | Some("23502") => StatusCode::BAD_REQUEST, // check / not-null violation
        // 55000 = write to a non-updatable VIEW. Post-flip (menu unification)
        // the legacy catalog tables are read-only shim views, so a straggler
        // caller of a retired legacy WRITE endpoint gets a clean 409 pointing
        // at the DB detail ("cannot insert into view …"), not a 500.
        Some("55000") => StatusCode::CONFLICT,
        Some(c) if c.starts_with("22") => StatusCode::BAD_REQUEST, // data exception (overflow, bad enum/uuid/encoding, offset range)
        Some(c) if c.starts_with("23") => StatusCode::CONFLICT,    // other integrity violations
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::{AppError, status_for_sqlstate};
    use actix_web::ResponseError;
    use actix_web::http::StatusCode;

    #[test]
    fn row_not_found_maps_to_404() {
        // `fetch_one` on a missing row must surface as 404, not 500.
        let resp = AppError::from(sqlx::Error::RowNotFound).error_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn classifies_sqlstates() {
        assert_eq!(status_for_sqlstate(Some("23505")), StatusCode::CONFLICT); // unique
        assert_eq!(status_for_sqlstate(Some("23503")), StatusCode::CONFLICT); // foreign key
        assert_eq!(status_for_sqlstate(Some("23P01")), StatusCode::CONFLICT); // exclusion
        assert_eq!(status_for_sqlstate(Some("23514")), StatusCode::BAD_REQUEST); // check
        assert_eq!(status_for_sqlstate(Some("23502")), StatusCode::BAD_REQUEST); // not null
        assert_eq!(status_for_sqlstate(Some("22003")), StatusCode::BAD_REQUEST); // numeric overflow
        assert_eq!(status_for_sqlstate(Some("22P02")), StatusCode::BAD_REQUEST); // invalid text/enum
        assert_eq!(status_for_sqlstate(Some("22021")), StatusCode::BAD_REQUEST); // bad encoding / NUL
        assert_eq!(status_for_sqlstate(Some("2201X")), StatusCode::BAD_REQUEST); // offset out of range
        assert_eq!(
            status_for_sqlstate(Some("40P01")),
            StatusCode::INTERNAL_SERVER_ERROR
        ); // deadlock
        assert_eq!(
            status_for_sqlstate(Some("08006")),
            StatusCode::INTERNAL_SERVER_ERROR
        ); // connection failure
        assert_eq!(status_for_sqlstate(None), StatusCode::INTERNAL_SERVER_ERROR);
    }
}

impl actix_web::ResponseError for AppError {
    fn error_response(&self) -> HttpResponse {
        let body = ErrorBody {
            error: self.to_string(),
            code: self.code(),
            till: match self {
                AppError::RefusedWith { till, .. } => Some(till.clone()),
                _ => None,
            },
            retry_after_seconds: match self {
                AppError::PinThrottled { seconds } => Some(*seconds),
                _ => None,
            },
            vars: match self {
                AppError::CodedVars { vars, .. } => Some(vars.clone()),
                _ => None,
            },
        };
        match self {
            AppError::Unauthorized(_) => HttpResponse::Unauthorized().json(body),
            AppError::Forbidden(_) => HttpResponse::Forbidden().json(body),
            AppError::OrgSuspended => HttpResponse::Forbidden().json(body),
            AppError::NotFound(_) => HttpResponse::NotFound().json(body),
            AppError::BadRequest(_) => HttpResponse::BadRequest().json(body),
            AppError::Conflict(_) => HttpResponse::Conflict().json(body),
            AppError::Refused { .. } => HttpResponse::Conflict().json(body),
            AppError::RefusedWith { .. } => HttpResponse::Conflict().json(body),
            AppError::Coded { status, .. } | AppError::CodedVars { status, .. } => {
                HttpResponse::build(
                    actix_web::http::StatusCode::from_u16(*status)
                        .unwrap_or(actix_web::http::StatusCode::BAD_REQUEST),
                )
                .json(body)
            }
            AppError::Db(e) => HttpResponse::build(Self::db_status(e)).json(body),
            AppError::ServiceUnavailable(_) => HttpResponse::ServiceUnavailable().json(body),
            AppError::TooManyRequests(_) => HttpResponse::TooManyRequests().json(body),
            // Retry-After as well as the body field: the header is the standard
            // any HTTP client already understands.
            AppError::PinThrottled { seconds } => HttpResponse::TooManyRequests()
                .insert_header(("Retry-After", seconds.to_string()))
                .json(body),
            AppError::Internal => HttpResponse::InternalServerError().json(body),
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
            (
                "400".to_string(),
                err("Bad request — validation failed or malformed input"),
            ),
            (
                "401".to_string(),
                err("Unauthorized — missing or invalid bearer token"),
            ),
            (
                "403".to_string(),
                err("Forbidden — insufficient permission or wrong org"),
            ),
            ("404".to_string(), err("Not found")),
            (
                "409".to_string(),
                err("Conflict — FK or domain invariant violation"),
            ),
            ("500".to_string(), err("Internal server error")),
        ])
    }
}
