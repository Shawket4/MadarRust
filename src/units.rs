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
        "g" => Some(("mass", 1.0)),
        "kg" => Some(("mass", 1000.0)),
        "ml" => Some(("volume", 1.0)),
        "l" => Some(("volume", 1000.0)),
        "pcs" => Some(("count", 1.0)),
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

/// Like [`convert`], but a mass↔volume conversion is allowed when a `density`
/// (grams per millilitre) is supplied. `count` (pcs) never bridges families.
pub fn convert_with_density(
    qty: f64,
    from_unit: &str,
    to_unit: &str,
    density_g_per_ml: Option<f64>,
) -> Result<f64, AppError> {
    let (ff, fk) = unit_spec(from_unit)
        .ok_or_else(|| AppError::BadRequest(format!("Unknown unit '{from_unit}'")))?;
    let (tf, tk) = unit_spec(to_unit)
        .ok_or_else(|| AppError::BadRequest(format!("Unknown unit '{to_unit}'")))?;
    if ff == tf {
        let converted = qty * fk / tk;
        return Ok((converted * 1000.0).round() / 1000.0);
    }
    // Cross-family: only mass↔volume, only with a positive density.
    let density = match density_g_per_ml {
        Some(d) if d > 0.0 => d,
        _ => {
            return Err(AppError::BadRequest(format!(
                "Cannot convert '{from_unit}' to '{to_unit}': set a density (g/ml) on the ingredient to convert between weight and volume."
            )));
        }
    };
    let from_canonical = qty * fk; // grams if mass, millilitres if volume
    let to_canonical = match (ff, tf) {
        ("mass", "volume") => from_canonical / density, // g → ml
        ("volume", "mass") => from_canonical * density, // ml → g
        _ => {
            return Err(AppError::BadRequest(format!(
                "Cannot convert '{from_unit}' to '{to_unit}': only weight↔volume is bridged by density."
            )));
        }
    };
    let converted = to_canonical / tk;
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

    #[test]
    fn density_bridges_mass_and_volume() {
        // water ~1 g/ml: 250 ml → 250 g.
        assert_eq!(
            convert_with_density(250.0, "ml", "g", Some(1.0)).unwrap(),
            250.0
        );
        // oil 0.92 g/ml: 1 l → 920 g; and the reverse.
        assert_eq!(
            convert_with_density(1.0, "l", "g", Some(0.92)).unwrap(),
            920.0
        );
        assert_eq!(
            convert_with_density(920.0, "g", "ml", Some(0.92)).unwrap(),
            1000.0
        );
        // same-family still works (density ignored).
        assert_eq!(convert_with_density(1.0, "kg", "g", None).unwrap(), 1000.0);
        // cross-family without a density is still rejected; pcs never bridges.
        assert!(convert_with_density(5.0, "ml", "g", None).is_err());
        assert!(convert_with_density(5.0, "pcs", "g", Some(1.0)).is_err());
    }

    #[test]
    fn density_must_be_positive() {
        // Zero/negative density can't bridge mass↔volume — must be a BadRequest,
        // never a divide-by-zero or a negative quantity. (Mutation testing flagged
        // the `d > 0.0` guard in convert_with_density as untested.)
        assert!(convert_with_density(250.0, "ml", "g", Some(0.0)).is_err());
        assert!(convert_with_density(250.0, "ml", "g", Some(-1.0)).is_err());
        assert!(convert_with_density(250.0, "g", "ml", Some(0.0)).is_err());
    }

    #[test]
    fn convert_with_density_divides_by_target_factor() {
        // Targets with a non-1 unit factor (tk≠1) exercise the `/ tk` divisions
        // that base-unit targets (g, ml) hide. Mutation testing flagged units.rs
        // :59 and :77 — `/ tk` survived because every prior case used tk=1.
        // same-family g→kg: 250 g / 1000 = 0.25 kg
        assert_eq!(convert_with_density(250.0, "g", "kg", None).unwrap(), 0.25);
        // cross-family ml→kg at water density: 1000 ml → 1000 g / 1000 = 1 kg
        assert_eq!(
            convert_with_density(1000.0, "ml", "kg", Some(1.0)).unwrap(),
            1.0
        );
    }

    #[test]
    fn normalize_to_base_returns_base_unit_and_converted_qty() {
        // Mutation testing flagged normalize_to_base as having no direct test.
        assert_eq!(
            normalize_to_base(1.0, "kg", "g").unwrap(),
            ("g".to_string(), 1000.0)
        );
        assert_eq!(
            normalize_to_base(250.0, "g", "kg").unwrap(),
            ("kg".to_string(), 0.25)
        );
        assert!(normalize_to_base(5.0, "g", "pcs").is_err());
    }
}
