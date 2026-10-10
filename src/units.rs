//! Unit-family conversion for recipe quantities → an ingredient's base stock
//! unit. Base units are the `inventory_unit` enum: g, kg, ml, l, pcs.
//!
//! The system invariant (enforced by recipe-save handlers + the
//! `backfill-recipe-units` binary) is that every stored recipe `quantity_used`
//! is expressed in the linked ingredient's base unit. That keeps the sale
//! deduction and every cost rollup correct with no runtime conversion.

use crate::errors::AppError;

// The unit rules are madar-shared's (`madar_units`), the one copy the POS core
// converts a waste with too. The messages are this server's, word for word.
pub use madar_units::{is_valid_unit, unit_spec};

/// Convert `qty` from `from_unit` into `to_unit`. Cross-family conversions
/// (e.g. g → pcs) are a `BadRequest`. Result is rounded to 3 decimals to match
/// `numeric(12,3)` storage.
pub fn convert(qty: f64, from_unit: &str, to_unit: &str) -> Result<f64, AppError> {
    madar_units::convert(qty, from_unit, to_unit).map_err(|e| AppError::BadRequest(e.to_string()))
}

/// Like [`convert`], but a mass↔volume conversion is allowed when a `density`
/// (grams per millilitre) is supplied. `count` (pcs) never bridges families.
pub fn convert_with_density(
    qty: f64,
    from_unit: &str,
    to_unit: &str,
    density_g_per_ml: Option<f64>,
) -> Result<f64, AppError> {
    madar_units::convert_with_density(qty, from_unit, to_unit, density_g_per_ml)
        .map_err(|e| AppError::BadRequest(e.to_string()))
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

    /// `recipes::handlers::normalize_recipe_unit` stored `convert_with_density`
    /// ÷ the yield factor, rounded to 3 dp, inline. It now calls
    /// `madar_units::recipe_base_qty`; stored quantities (and the POS's waste
    /// values over them) must not move by a single bit for any input.
    #[test]
    fn recipe_base_qty_is_the_old_inline_normalization() {
        fn old(
            qty: f64,
            unit: &str,
            base: &str,
            density: Option<f64>,
            yield_pct: Option<f64>,
        ) -> Result<f64, String> {
            let base_q =
                convert_with_density(qty, unit, base, density).map_err(|e| e.to_string())?;
            let yf = yield_pct
                .map(|y| y / 100.0)
                .filter(|y| *y > 0.0)
                .unwrap_or(1.0);
            Ok(((base_q / yf) * 1000.0).round() / 1000.0)
        }
        let units = ["g", "kg", "ml", "l", "pcs", "G", " Kg ", "cups"];
        let densities = [None, Some(0.92), Some(1.03), Some(0.0)];
        let mut yields = vec![None, Some(0.0), Some(-5.0), Some(0.001), Some(33.333)];
        yields.extend((1..=150).map(|y| Some(f64::from(y))));
        yields.extend([Some(72.5), Some(99.999), Some(100.001), Some(300.0)]);
        // Typed amounts: a dense grid near the thousandth plus pseudo-random ones.
        let mut qtys: Vec<f64> = (0..=400).map(|m| f64::from(m) / 1000.0).collect();
        qtys.extend([
            0.0005, 0.0015, 0.0625, 1.0005, 2.675, 1e-7, 1e9, -1.0, 12_345.678,
        ]);
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        for _ in 0..200 {
            x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            qtys.push((x >> 11) as f64 / (1u64 << 53) as f64 * 5_000.0);
        }
        let mut n = 0u64;
        for &q in &qtys {
            for (i, from) in units.iter().enumerate() {
                // Every unit pair over a sample of the yields, to keep it fast.
                for to in &units[..5] {
                    for d in densities {
                        for y in yields.iter().skip(i % 4).step_by(4) {
                            // As the handler maps it: the same 400, word for word.
                            let new = madar_units::recipe_base_qty(q, from, to, d, *y)
                                .map_err(|e| AppError::BadRequest(e.to_string()).to_string());
                            let was = old(q, from, to, d, *y);
                            assert_eq!(
                                new.as_ref().map(|v| v.to_bits()),
                                was.as_ref().map(|v| v.to_bits()),
                                "{q} {from}->{to} density {d:?} yield {y:?}: {new:?} vs {was:?}"
                            );
                            n += 1;
                        }
                    }
                }
            }
        }
        assert!(n > 1_000_000, "{n} cases");
    }
}
