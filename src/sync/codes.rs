//! Codes on the 409s an offline till's queued op can meet through
//! `/sync/replay`.
//!
//! The POS used to read every 409 on an op it called idempotent as "the server
//! already has this", and deleted the op. Most of those 409s are refusals: the
//! work never applied, and nobody at the till was told. A replayed duplicate is
//! answered 200 almost everywhere, so a 409 is a refusal unless its code says
//! otherwise, and the codes here are how a till tells the two apart.
//!
//! Adding a code never changes a status or a message: an old till that reads
//! no codes behaves exactly as it did.

use crate::errors::AppError;

/// The server already holds this exact op: a second copy of it (the till's
/// own retry racing its LAN peer's backup of the same op) lost the race on the
/// op's own key. The till may treat it as applied.
pub const REPLAY_ALREADY_APPLIED: &str = "REPLAY_ALREADY_APPLIED";

/// A unique violation the caller has proved was this op's own key, landed by
/// a concurrent copy of the op. The status (409) and the text are exactly what
/// the plain database error rendered; only the code is added.
pub fn already_applied(e: sqlx::Error) -> AppError {
    AppError::Refused {
        code: REPLAY_ALREADY_APPLIED,
        reason: AppError::Db(e).to_string(),
    }
}

/// A 409 with a code and the `Conflict: ` prefix the plain variant rendered,
/// so the `error` text is byte-identical to the uncoded refusal it replaces.
pub fn conflict(code: &'static str, reason: impl AsRef<str>) -> AppError {
    AppError::Refused {
        code,
        reason: format!("Conflict: {}", reason.as_ref()),
    }
}

/// Is this a unique violation (SQLSTATE 23505)?
pub fn is_unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23505")
}
