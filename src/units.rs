//! Unit-family conversion for recipe quantities → an ingredient's base stock
//! unit. Base units are the `inventory_unit` enum: g, kg, ml, l, pcs.
//!
//! The system invariant (enforced by recipe-save handlers + the
//! `backfill-recipe-units` binary) is that every stored recipe `quantity_used`
//! is expressed in the linked ingredient's base unit. That keeps the sale
//! deduction and every cost rollup correct with no runtime conversion.

use crate::errors::AppError;

/// (family, factor-to-canonical). Canonical per family: grams for mass,
/// millilitres for volume, pcs for count.
fn unit_spec(unit: &str) -> Option<(&'static str, f64)> {
    match unit.trim().to_ascii_lowercase().as_str() {
        "g"   => Some(("mass",   1.0)),
        "kg"  => Some(("mass",   1000.0)),
        "ml"  => Some(("volume", 1.0)),
        "l"   => Some(("volume", 1000.0)),
        "pcs" => Some(("count",  1.0)),
        _ => None,
    }
}

/// True iff `unit` is a recognized inventory unit.
pub fn is_valid_unit(unit: &str) -> bool {
    unit_spec(unit).is_some()
}

/// Convert `qty` from `from_unit` into `to_unit`. Cross-family conversions
/// (e.g. g → pcs) are a `BadRequest`. Result is rounded to 3 decimals to match
/// `numeric(12,3)` storage.
pub fn convert(qty: f64, from_unit: &str, to_unit: &str) -> Result<f64, AppError> {
    let (ff, fk) = unit_spec(from_unit)
        .ok_or_else(|| AppError::BadRequest(format!("Unknown unit '{from_unit}'")))?;
    let (tf, tk) = unit_spec(to_unit)
        .ok_or_else(|| AppError::BadRequest(format!("Unknown unit '{to_unit}'")))?;
    if ff != tf {
        return Err(AppError::BadRequest(format!(
            "Cannot convert '{from_unit}' to '{to_unit}': incompatible unit families ({ff} vs {tf})"
        )));
    }
    let converted = qty * fk / tk;
    Ok((converted * 1000.0).round() / 1000.0)
}

/// Validate + normalize a recipe entry to the ingredient's base unit.
/// Returns `(base_unit, normalized_qty)`.
pub fn normalize_to_base(
    qty: f64,
    recipe_unit: &str,
    base_unit: &str,
) -> Result<(String, f64), AppError> {
    let q = convert(qty, recipe_unit, base_unit)?;
    Ok((base_unit.to_string(), q))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions() {
        assert_eq!(convert(1.0, "kg", "g").unwrap(), 1000.0);
        assert_eq!(convert(250.0, "g", "kg").unwrap(), 0.25);
        assert_eq!(convert(2.0, "l", "ml").unwrap(), 2000.0);
        assert_eq!(convert(5.0, "g", "g").unwrap(), 5.0);
        assert_eq!(convert(3.0, "pcs", "pcs").unwrap(), 3.0);
    }

    #[test]
    fn cross_family_is_rejected() {
        assert!(convert(5.0, "g", "pcs").is_err());
        assert!(convert(5.0, "ml", "g").is_err());
        assert!(convert(5.0, "bogus", "g").is_err());
    }

    #[test]
    fn rounds_to_three_dp() {
        // 0.0001 g → kg is 0.0000001, rounds to 0.000.
        assert_eq!(convert(0.0001, "g", "kg").unwrap(), 0.0);
    }

    #[test]
    fn validity() {
        assert!(is_valid_unit("kg"));
        assert!(is_valid_unit("PCS"));
        assert!(!is_valid_unit("cups"));
    }
}
