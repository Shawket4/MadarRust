//! The one canonical phone form: E.164 digits without the `+`
//! (`201001234567`). Customers, loyalty, delivery and bookings all key on it.
//!
//! The same rule lives in SQL (`phone_canonical`), in the POS core and in the
//! dashboard (`src/lib/phone.ts`). All four run `tests/phone_vectors.json`, so
//! they cannot drift. The rule, in order:
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

/// Longest raw input considered at all.
pub const MAX_PHONE_RAW_LEN: usize = 32;

fn ascii_digit(c: char) -> Option<char> {
    match c {
        '0'..='9' => Some(c),
        '\u{0660}'..='\u{0669}' => char::from_digit(c as u32 - 0x0660, 10),
        '\u{06F0}'..='\u{06F9}' => char::from_digit(c as u32 - 0x06F0, 10),
        _ => None,
    }
}

/// ASCII digits of `raw`, with Arabic-Indic digits mapped. No validation.
pub fn digits(raw: &str) -> String {
    raw.chars().filter_map(ascii_digit).collect()
}

/// The canonical form, or `None` when `raw` is not a phone number.
pub fn canonical(raw: &str) -> Option<String> {
    if raw.chars().count() > MAX_PHONE_RAW_LEN {
        return None;
    }
    let d = digits(raw);
    let n = if let Some(rest) = d.strip_prefix("00") {
        rest.to_string()
    } else if d.starts_with("20") {
        d
    } else if let Some(rest) = d.strip_prefix('0') {
        format!("20{rest}")
    } else if d.len() == 10 && d.starts_with('1') {
        format!("20{d}")
    } else {
        d
    };
    if n.len() < 10 || n.len() > 15 {
        return None;
    }
    let mobile = ["2010", "2011", "2012", "2015"]
        .iter()
        .any(|p| n.starts_with(p));
    if mobile && n.len() != 12 {
        return None;
    }
    Some(n)
}

/// [`canonical`] for a request field: an invalid phone is the caller's 400.
pub fn normalize_phone(raw: &str) -> Result<String, AppError> {
    canonical(raw).ok_or_else(|| AppError::BadRequest("phone number looks invalid".into()))
}

/// What a search box's digits should be matched against canonical keys with:
/// a partial number cannot be canonicalised, but its local prefix can be
/// dropped (`0100…` is stored as `20100…`, so `100…` is what both contain).
pub fn search_digits(raw: &str) -> Option<String> {
    let d = digits(raw);
    let d = d
        .strip_prefix("00")
        .or_else(|| d.strip_prefix('0'))
        .unwrap_or(&d);
    (d.len() >= 3).then(|| d.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn vectors() -> (Vec<(String, String)>, Vec<String>) {
        let v: serde_json::Value =
            serde_json::from_str(include_str!("../tests/phone_vectors.json")).unwrap();
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

    #[test]
    fn the_shared_vectors_hold_in_rust() {
        let (valid, invalid) = vectors();
        assert!(valid.len() >= 20 && invalid.len() >= 8);
        for (raw, want) in valid {
            assert_eq!(canonical(&raw).as_deref(), Some(want.as_str()), "{raw:?}");
            // Canonical is a fixed point.
            assert_eq!(canonical(&want).as_deref(), Some(want.as_str()), "{want:?}");
        }
        for raw in invalid {
            assert_eq!(canonical(&raw), None, "{raw:?}");
        }
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

    #[test]
    fn a_search_drops_the_local_prefix() {
        assert_eq!(search_digits("0100 123").as_deref(), Some("100123"));
        assert_eq!(search_digits("+20100").as_deref(), Some("20100"));
        assert_eq!(search_digits("٠١٠٠").as_deref(), Some("100"));
        assert_eq!(search_digits("01"), None);
        assert_eq!(search_digits("Ali"), None);
    }
}
