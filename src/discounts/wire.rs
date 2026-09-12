//! The two spellings a discount has on the wire, and why both must ship.
//!
//! A percentage discount moved from `14` to `0.14` — a fraction, like every
//! other rate in this schema. That was the right model and the wrong rollout:
//! the field also changed JSON type, from `integer` to `double`, and every
//! till in the field was generated against `integer`. A client does not read
//! `0.14` as "zero point one four percent"; it fails to deserialise the whole
//! object and shows nothing at all. That is why discounts "stopped parsing"
//! rather than coming out wrong.
//!
//! So the wire carries BOTH. `value` keeps the legacy spelling — an integer,
//! 0-100 for a percentage — which is what every shipped client expects, and
//! `value_rate` carries the fraction for clients that know to ask. Storage is
//! unchanged: the column is still the fraction, and the legacy integer is
//! derived on the way out.
//!
//! Nothing about this is negotiated per client. Negotiation needs a version
//! signal the fleet does not send (every build reports `madar-core/0.1.0`),
//! and a response whose SHAPE depends on a request header is a thing every
//! cache, test and new endpoint then has to reason about. Two fields is
//! boring and local. For a money field, boring wins.
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::Serializer;

/// The legacy integer for a stored rate: `0.14` → `14`, `5000` → `5000`.
///
/// Told apart by magnitude, not by `dtype`, so this stays a single-field
/// transform: a percentage is validated to be at most 1, and a fixed
/// discount is in minor units, where anything at or below 1 is a single
/// piastre off a bill — not a thing a shop offers. It is the exact inverse
/// of the leniency already applied when READING an old till's value.
pub fn legacy_value(v: Decimal) -> i64 {
    let scaled = if v > Decimal::ZERO && v <= Decimal::ONE {
        v * Decimal::ONE_HUNDRED
    } else {
        v
    };
    // Half AWAY from zero, not Decimal's default banker's rounding: 12.5%
    // becomes 13 rather than 12, which is what a person reading a rounded
    // percentage expects. Either answer is a lie by half a percent — an old
    // till cannot express 12.5% at all, which is the whole reason the
    // fraction exists — so it may as well be the unsurprising one.
    scaled
        .round_dp_with_strategy(0, rust_decimal::RoundingStrategy::MidpointAwayFromZero)
        .to_i64()
        .unwrap_or(0)
}

/// Serialises a stored rate in the legacy spelling. See [`legacy_value`].
pub fn ser_legacy<S: Serializer>(v: &Decimal, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_i64(legacy_value(*v))
}

/// The same, for a field that may be absent.
pub fn ser_legacy_opt<S: Serializer>(v: &Option<Decimal>, s: S) -> Result<S::Ok, S::Error> {
    match v {
        Some(d) => s.serialize_i64(legacy_value(*d)),
        None => s.serialize_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn a_rate_goes_back_to_the_spelling_every_shipped_till_expects() {
        assert_eq!(legacy_value(dec!(0.14)), 14);
        assert_eq!(legacy_value(dec!(1)), 100, "100% is still a percentage");
        assert_eq!(
            legacy_value(dec!(0.125)),
            13,
            "12.5% rounds; old tills
             never could express a half percent, which is why the fraction
             exists at all"
        );
        assert_eq!(legacy_value(dec!(0)), 0);
    }

    #[test]
    fn a_fixed_discount_in_minor_units_is_left_alone() {
        assert_eq!(legacy_value(dec!(5000)), 5000);
        assert_eq!(legacy_value(dec!(250)), 250);
    }
    /// The incident, as a test: the JSON a shipped till receives must carry
    /// `value` as an INTEGER. A double there does not read as a small
    /// percentage — it fails to deserialise the whole object, which is why
    /// discounts vanished rather than coming out wrong.
    #[test]
    fn the_wire_still_speaks_the_spelling_shipped_tills_were_built_for() {
        #[derive(serde::Serialize)]
        struct Wire {
            #[serde(serialize_with = "ser_legacy")]
            value: Decimal,
            value_rate: Decimal,
        }
        let json = serde_json::to_string(&Wire {
            value: dec!(0.14),
            value_rate: dec!(0.14),
        })
        .unwrap();
        assert_eq!(
            json, r#"{"value":14,"value_rate":0.14}"#,
            "an integer for the fleet, the fraction beside it for everyone else"
        );
        assert!(
            !json.contains("14.0"),
            "not 14.0 either — a shipped client parses `value` as an int, and \
             serde writes an f64 zero as 0.0, which is equally unparseable"
        );
    }
}
