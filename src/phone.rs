//! The one canonical phone form: E.164 digits without the `+`
//! (`201001234567`). Customers, loyalty, delivery and bookings all key on it.
//!
//! The rule is madar-shared's (`madar_ids::phone`, the POS core's too). The
//! same rule lives in SQL (`phone_canonical`) and in the dashboard
//! (`src/lib/phone.ts`); all run `madar_ids::vectors::PHONE`, so they cannot
//! drift. The rule, in order:
//!
//!   1. raw input longer than 32 characters is invalid;
//!   2. Arabic-Indic (U+0660–0669) and Extended Arabic-Indic (U+06F0–06F9)
//!      digits become ASCII;
//!   3. only ASCII digits are kept;
//!   4. a leading `00` is stripped; else a leading `20` is kept; else a leading
//!      `0` becomes `20`; else exactly ten digits starting with `1` get `20`
//!      prefixed; else unchanged;
//!   5. a result outside 10–15 digits is invalid;
//!   6. Egyptian mobile guard: a result starting with `2010`, `2011`, `2012`
//!      or `2015` must be exactly 12 digits, else invalid. A truncated or
//!      over-long mobile is the commonest typo; landlines such as
//!      `2013xxxxxxx` are untouched.

use crate::errors::AppError;

// The rule is madar-shared's (`madar_ids::phone`), the POS core's too, pinned
// with this database's `phone_canonical` by `madar_ids::vectors::PHONE`.
pub use madar_ids::phone::{MAX_PHONE_RAW_LEN, canonical, digits, search_digits};

/// [`canonical`] for a request field: an invalid phone is the caller's 400.
pub fn normalize_phone(raw: &str) -> Result<String, AppError> {
    canonical(raw).ok_or_else(|| AppError::BadRequest("phone number looks invalid".into()))
}

#[cfg(test)]
mod tests {
    pub(crate) fn vectors() -> (Vec<(String, String)>, Vec<String>) {
        let v: serde_json::Value = serde_json::from_str(madar_ids::vectors::PHONE).unwrap();
        let valid = v["valid"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                (
                    p[0].as_str().unwrap().to_string(),
                    p[1].as_str().unwrap().to_string(),
                )
            })
            .collect();
        let invalid = v["invalid"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_string())
            .collect();
        (valid, invalid)
    }

    #[sqlx::test]
    async fn the_shared_vectors_hold_in_sql(pool: sqlx::PgPool) {
        let (valid, invalid) = vectors();
        for (raw, want) in valid {
            let got: Option<String> = sqlx::query_scalar("SELECT phone_canonical($1)")
                .bind(&raw)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(got.as_deref(), Some(want.as_str()), "{raw:?}");
        }
        for raw in invalid {
            let got: Option<String> = sqlx::query_scalar("SELECT phone_canonical($1)")
                .bind(&raw)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(got, None, "{raw:?}");
        }
        let null: Option<String> = sqlx::query_scalar("SELECT phone_canonical(NULL)")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(null, None);
    }
}
