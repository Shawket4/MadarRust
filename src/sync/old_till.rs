//! A refusal 409 on `POST /sync/replay`, answered as a 422 to a till that
//! would otherwise lose the op.
//!
//! Every POS build before 0.15.0 reads a 409 on an op it calls idempotent
//! (refund, cash movement, close, ticket void and line void, the floor and
//! booking ops) and on a sale void as "the server already has this": the op is
//! acked and its outbox row deleted. When the 409 is a REFUSAL (the op did not
//! apply) the work vanishes from the till and nobody is told. 0.15.0 reads the
//! code instead (`conflict_acks`), so a refusal stays on its stuck list.
//!
//! Those builds stay in the field (tills are never forced to update), so for
//! them this server answers the refusals listed in [`REFUSALS`] with 422 and
//! the very same body (`error` text and `code`). Every build since 0.1.1 maps
//! a 422 to `CoreError::Validation` and classifies it `SendOutcome::Dead`
//! whatever the op: the row is kept, shown as stuck with the server's text,
//! never acked and never retried in a loop. 403 is not used: before 0.7.9 it
//! is also Dead, but from 0.7.9 on it is `Refused` (the "needs a manager"
//! list, re-sent with an approval), which is the wrong story for these.
//!
//! The 409s that DO mean "this exact op is already here" (`REPLAY_ALREADY_APPLIED`,
//! `LINE_ALREADY_VOIDED`, `TICKET_VOIDED`: the 0.15.0 `conflict_acks`) are
//! not in the list and stay 409, because acking them is right. Uncoded 409s
//! (a plain unique violation, which old tills ack as a duplicate) stay too.
//!
//! Who is old ([`acks_every_conflict`]):
//! * `X-Madar-Client: pos/<semver> (…)` below 0.15.0 (every build since
//!   0.7.0 sends it);
//! * no `X-Madar-Client` and `User-Agent: madar-core/…`: the POS core before
//!   0.7.0, which sends only that.
//!
//! Anything else (0.15.0 and later, a header that cannot be read, the kitchen
//! screen, the dashboard, no header at all) gets exactly today's answer. A
//! 0.15.0+ build mistaken for old loses nothing: it words a refusal by its
//! code whatever the status and dead-letters a 409 refusal and a 422 alike.
//!
//! Only the single-op `/sync/replay` route does this; `/sync/replay/batch`
//! (protocol v2, 0.15.0+) and the live routes are unchanged.

use actix_web::http::header::{HeaderMap, USER_AGENT};

use crate::devices::{CLIENT_HEADER, ClientHeader};
use crate::errors::AppError;

/// The status a refusal is answered with for an old till.
pub const OLD_TILL_REFUSAL_STATUS: u16 = 422;

/// The first POS build that reads a 409's code before acking it.
pub const FIRST_CODE_READING_POS: (u64, u64, u64) = (0, 15, 0);

/// Refusal codes a queued op can meet on `/sync/replay` (MadarRust v1.14.0,
/// `tests/replay_409_codes.rs`): the op did not apply.
pub const REFUSALS: &[&str] = &[
    "ORDER_REFUNDED",
    "ORDER_FULLY_REFUNDED",
    "CASH_ALREADY_CORRECTED",
    "TICKET_SETTLED",
    "BOOKING_NOT_ACTIVE",
    "BOOKING_OTHER_BRANCH",
    "BOOKING_SEATED_ELSEWHERE",
    "BOOKING_SEATED",
];

/// Does the caller ack every 409 on an idempotent op (a POS before 0.15.0)?
/// `true` only when the headers say so; unknown callers are not old.
pub fn acks_every_conflict(headers: &HeaderMap) -> bool {
    let header = |name| headers.get(name).and_then(|v| v.to_str().ok());
    match header(CLIENT_HEADER) {
        Some(raw) => {
            let client = ClientHeader::parse(Some(raw));
            client.app.as_deref() == Some("pos")
                && client.version.is_some_and(|v| v < FIRST_CODE_READING_POS)
        }
        None => {
            header(USER_AGENT.as_str()).is_some_and(|ua| ua.trim_start().starts_with("madar-core/"))
        }
    }
}

/// The error an old till is answered with: a listed refusal becomes a 422
/// with the same `error` and `code`; anything else is returned unchanged.
pub fn for_old_till(e: AppError) -> AppError {
    match e {
        AppError::Refused { code, reason } if REFUSALS.contains(&code) => AppError::Coded {
            status: OLD_TILL_REFUSAL_STATUS,
            code,
            reason,
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::http::header::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(HeaderName::from_static(k), HeaderValue::from_static(v));
        }
        h
    }

    #[test]
    fn old_tills_are_the_pos_builds_before_0_15_0() {
        for old in [
            "pos/0.14.1 (android)",
            "pos/0.7.0 (macos)",
            "pos/0.13.10 (ios)",
        ] {
            assert!(
                acks_every_conflict(&headers(&[("x-madar-client", old)])),
                "{old}"
            );
        }
        // The pre-0.7 core: no X-Madar-Client, its own User-Agent.
        assert!(acks_every_conflict(&headers(&[(
            "user-agent",
            "madar-core/0.1.0"
        )])));
        for new in [
            "pos/0.15.0 (ios)",
            "pos/0.16.2 (android)",
            "pos/1.0.0",
            "pos/0.15.0-rc1 (ios)",
            "pos/kitchen/0.1.0 (ios)",
            "kds/0.1.0 (ios)",
            "dashboard",
            "pos/",
            "",
        ] {
            assert!(
                !acks_every_conflict(&headers(&[("x-madar-client", new)])),
                "{new}"
            );
        }
        // A header beats the User-Agent; nothing at all is not old.
        assert!(!acks_every_conflict(&headers(&[
            ("x-madar-client", "pos/0.15.0 (ios)"),
            ("user-agent", "madar-core/0.1.0")
        ])));
        assert!(!acks_every_conflict(&headers(&[])));
        assert!(!acks_every_conflict(&headers(&[(
            "user-agent",
            "Dart/3.13 (dart:io)"
        )])));
    }

    #[test]
    fn only_listed_refusals_change_and_keep_their_text_and_code() {
        for code in REFUSALS {
            match for_old_till(AppError::Refused {
                code,
                reason: "Conflict: x".into(),
            }) {
                AppError::Coded {
                    status,
                    code: c,
                    reason,
                } => {
                    assert_eq!((status, c, reason.as_str()), (422, *code, "Conflict: x"));
                }
                other => panic!("{code}: {other:?}"),
            }
        }
        for ack in [
            "REPLAY_ALREADY_APPLIED",
            "LINE_ALREADY_VOIDED",
            "TICKET_VOIDED",
            "TRANSFER_EXISTS",
        ] {
            assert!(matches!(
                for_old_till(AppError::Refused { code: ack, reason: "r".into() }),
                AppError::Refused { code, .. } if code == ack
            ));
        }
        assert!(matches!(
            for_old_till(AppError::Conflict("c".into())),
            AppError::Conflict(_)
        ));
    }
}
