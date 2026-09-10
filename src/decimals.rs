//! Every `numeric` on the wire.
//!
//! A `numeric` column is read as a `BigDecimal`, and bigdecimal's serde writes
//! that as a **JSON string** — `"0.1400"`, not `0.14`.
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
//! This had been found once before and fixed in ONE place —
//! `orders::handlers::serialize_bigdecimal_opt_as_number`, for
//! `quantity_deducted` alone — while twelve other fields kept shipping strings:
//! every recipe's `quantity_used`, every stock `quantity` and `balance_after`,
//! both branch tax overrides, both org rates. A fix that does not generalise is
//! a fix that will be needed again, so this is the one place, and
//! `no_numeric_reaches_a_client_as_a_string` fails the build if a new field
//! forgets it.
//!
//! Nothing is lost on the way out: these columns are `numeric(5,4)` for rates
//! and `numeric(12,3)` for quantities, and f64 carries both exactly.

use bigdecimal::ToPrimitive;
use serde::Serializer;
use sqlx::types::BigDecimal;

/// Write a `numeric` as a JSON number.
pub fn serialize<S: Serializer>(value: &BigDecimal, s: S) -> Result<S::Ok, S::Error> {
    // A value that will not fit an f64 cannot have come out of these columns,
    // so this is unreachable in practice — and 0 is the answer the client was
    // already computing from the string it could not parse.
    s.serialize_f64(value.to_f64().unwrap_or(0.0))
}

/// Write an optional `numeric` as a JSON number or `null`. A branch's tax
/// override is NULL when it inherits, which is not the same as zero.
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

    /// NO `numeric` REACHES A CLIENT AS A STRING.
    ///
    /// This is a source scan rather than a round-trip, because the failure mode
    /// is not a wrong value — it is a field somebody adds next year without the
    /// annotation, whose own tests all pass because Rust round-trips a string
    /// happily. Only the client notices, and it notices by quietly showing
    /// zero.
    ///
    /// That already happened: the fix existed in `orders` for
    /// `quantity_deducted` and twelve other fields shipped strings for months.
    #[test]
    fn no_numeric_reaches_a_client_as_a_string() {
        let mut missing = Vec::new();
        let mut checked = 0usize;
        let mut stack = vec![std::path::PathBuf::from("src")];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read src").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                let name = path.file_name().unwrap().to_string_lossy().to_string();
                // Test modules build their own fixtures; nothing there is on a
                // wire. `decimals.rs` is the fix itself.
                if name.contains("tests") || name == "decimals.rs" {
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("read file");
                let lines: Vec<&str> = text.lines().collect();
                for (i, line) in lines.iter().enumerate() {
                    let t = line.trim();
                    if !(t.starts_with("pub ") && t.contains("sqlx::types::BigDecimal")) {
                        continue;
                    }
                    checked += 1;
                    let annotated = lines[i.saturating_sub(4)..i]
                        .iter()
                        .any(|l| l.contains("crate::decimals::serialize"));
                    if !annotated {
                        missing.push(format!("{}:{} {}", path.display(), i + 1, t));
                    }
                }
            }
        }
        assert!(
            checked >= 12,
            "the scan found only {checked} fields — did the \
             declaration shape change?"
        );
        assert!(
            missing.is_empty(),
            "these `numeric` fields will reach a client as JSON STRINGS, and a \
             dashboard reading them with Number.isFinite will show zero. Add \
             `#[serde(serialize_with = \"crate::decimals::serialize\")]` (or \
             `serialize_opt` for an Option):\n  {}",
            missing.join("\n  ")
        );
    }

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
