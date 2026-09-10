//! Rates on the wire.
//!
//! A rate is stored as `numeric` and read as a `BigDecimal`, and bigdecimal's
//! serde writes that as a **JSON string** — `"0.1400"`, not `0.14`.
//!
//! That is what "I set the tax to 14, it says saved, and nothing changes" looks
//! like from the dashboard. The write lands exactly as asked; the form that
//! reads it back runs it through `Number.isFinite("0.1400")`, gets false, and
//! falls back to zero. The setting appears to do nothing while the database has
//! precisely the value that was typed. Nothing failed loudly enough to look at,
//! because nothing failed.
//!
//! The OpenAPI schema has always said `f64` for these fields, so the generated
//! TypeScript said `number` too — the contract and the bytes disagreed, and only
//! the bytes were true.
//!
//! So a rate is serialized as a number. Four decimal places is what the column
//! stores (`numeric(5,4)`) and a fraction between 0 and 1 has no magnitude
//! problem in an f64, so nothing is lost on the way out.

use bigdecimal::ToPrimitive;
use serde::Serializer;
use sqlx::types::BigDecimal;

/// Write a rate as a JSON number.
pub fn serialize<S: Serializer>(value: &BigDecimal, s: S) -> Result<S::Ok, S::Error> {
    // A rate that will not fit an f64 cannot have come out of `numeric(5,4)`,
    // so this is unreachable in practice — but 0 is the safe answer, and it is
    // the same answer the dashboard was already showing.
    s.serialize_f64(value.to_f64().unwrap_or(0.0))
}

/// Write an optional rate as a JSON number or `null`. A branch's override is
/// NULL when it inherits, which is not the same as zero.
pub fn serialize_opt<S: Serializer>(value: &Option<BigDecimal>, s: S) -> Result<S::Ok, S::Error> {
    match value {
        Some(v) => s.serialize_f64(v.to_f64().unwrap_or(0.0)),
        None => s.serialize_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[derive(serde::Serialize)]
    struct Row {
        #[serde(serialize_with = "serialize")]
        rate: BigDecimal,
        #[serde(serialize_with = "serialize_opt")]
        override_rate: Option<BigDecimal>,
    }

    #[test]
    fn a_rate_is_a_number_not_a_string() {
        let row = Row {
            rate: BigDecimal::from_str("0.1400").unwrap(),
            override_rate: None,
        };
        let v = serde_json::to_value(&row).unwrap();
        assert!(v["rate"].is_number(), "got {:?}", v["rate"]);
        assert_eq!(v["rate"].as_f64().unwrap(), 0.14);
        assert!(v["override_rate"].is_null(), "inherit is not zero");
    }

    #[test]
    fn an_override_survives_as_a_number() {
        let row = Row {
            rate: BigDecimal::from_str("0").unwrap(),
            override_rate: Some(BigDecimal::from_str("0.1250").unwrap()),
        };
        let v = serde_json::to_value(&row).unwrap();
        assert_eq!(v["rate"].as_f64().unwrap(), 0.0);
        assert_eq!(v["override_rate"].as_f64().unwrap(), 0.125);
    }
}
