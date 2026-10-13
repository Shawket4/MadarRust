//! The WhatsApp OTP's code check, shared by everything that proves a phone:
//! the delivery/ordering/booking/loyalty flow (`/public/otp/verify`, purpose
//! `delivery`) and shop signup (`/public/signup`, purpose `signup`).
//!
//! A code belongs to a PURPOSE (`delivery_otp.purpose`): a code sent to sign a
//! shop up cannot be spent as a customer's device proof, nor the other way.

use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;

/// Wrong guesses one code allows.
pub(crate) const OTP_MAX_ATTEMPTS: i32 = 5;

/// Count one attempt against the phone's newest live code of `purpose`, and
/// say whether `code` is right. Returns the code's row id on a match; the
/// caller decides when to [`consume`] it (a signup that fails on a duplicate
/// email keeps its code, within its attempts).
pub(crate) async fn check_code(
    pool: &PgPool,
    phone: &str,
    code: &str,
    purpose: &str,
) -> Result<Uuid, AppError> {
    // CLAIM the attempt and read the code in ONE statement.
    //
    // This used to SELECT the code and the attempt count, compare them in
    // Rust, and only then UPDATE the counter on a miss. Three statements, and
    // the window between the first and the last is where the cap lived. Guesses
    // arriving together all read `attempts` before any of them wrote it, so all
    // of them passed a check that only one should have — the five-attempt cap
    // was really "five attempts per round trip", and the only thing bounding a
    // parallel attacker was the per-IP limiter (three per thirty seconds).
    //
    // That matters more than it looks, because the code is FOUR digits. Ten
    // thousand possibilities, a five-minute life: an attacker spread across
    // enough addresses to dodge the per-IP limiter could cover the whole space
    // inside one code's lifetime.
    //
    // Incrementing INSIDE the predicate makes the database the arbiter. Exactly
    // five updates can ever succeed for a row, whoever sends them, from however
    // many addresses, however simultaneously — the sixth matches no row because
    // `attempts < $2` is false by then, and returns nothing.
    //
    // The successful attempt is counted too. It costs one of the five and is
    // immediately irrelevant, because a correct code consumes the row.
    let claimed: Option<(Uuid, String)> = sqlx::query_as(
        "UPDATE delivery_otp SET attempts = attempts + 1 \
         WHERE id = ( \
             SELECT id FROM delivery_otp \
              WHERE phone = $1 AND purpose = $3 AND consumed_at IS NULL AND expires_at > now() \
              ORDER BY created_at DESC LIMIT 1 \
         ) \
           AND attempts < $2 \
         RETURNING id, code",
    )
    .bind(phone)
    .bind(OTP_MAX_ATTEMPTS)
    .bind(purpose)
    .fetch_optional(pool)
    .await?;

    let Some((id, expected)) = claimed else {
        // Nothing was claimed, for one of two reasons, and they are worth
        // telling apart: "request a new one" is the right instruction for an
        // expired code and the wrong one for a code being ground down. This
        // read races nothing — the cap has already been enforced above.
        let spent: bool = sqlx::query_scalar(
            "SELECT EXISTS( \
                 SELECT 1 FROM delivery_otp \
                  WHERE phone = $1 AND purpose = $3 AND consumed_at IS NULL AND expires_at > now() \
                    AND attempts >= $2 \
             )",
        )
        .bind(phone)
        .bind(OTP_MAX_ATTEMPTS)
        .bind(purpose)
        .fetch_one(pool)
        .await?;
        return Err(AppError::BadRequest(if spent {
            "Too many attempts — request a new code.".into()
        } else {
            "No active code — request a new one.".to_string()
        }));
    };

    // Constant-time, so a wrong code cannot be narrowed a digit at a time by
    // timing the reply. Cheap enough not to think about — the reason this is
    // not bcrypt is in the migration that made the column plain.
    if !crate::secrets::constant_time_eq(code.as_bytes(), expected.as_bytes()) {
        return Err(AppError::BadRequest("Incorrect code.".into()));
    }
    Ok(id)
}

/// Spend a code: it matches nothing from now on.
pub(crate) async fn consume(pool: &PgPool, id: Uuid) -> Result<(), AppError> {
    sqlx::query("UPDATE delivery_otp SET consumed_at = now() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
