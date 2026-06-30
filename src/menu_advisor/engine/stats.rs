//! Total math helpers — every division, median, and money conversion in the
//! engine goes through these so no NaN/∞ or panic can originate elsewhere.

use crate::menu_advisor::dto::PriceRoundingRule;

#[derive(Debug, Clone, Copy)]
pub(crate) struct WilsonInterval {
    pub lo: f64,
    pub hi: f64,
}

/// `weight = exp(-ln(2) * age_days / half_life_days)`
/// weight(0) = 1.0,  weight(half_life) = 0.5
pub(crate) fn recency_weight(age_days: f64, half_life_days: f64) -> f64 {
    (-(std::f64::consts::LN_2) * age_days / half_life_days.max(1e-9)).exp()
}

/// Wilson 95% score interval for a proportion `p` observed over `n` trials.
/// `n <= 0` yields the maximally uninformative `[0, 1]`.
pub(crate) fn wilson_95_ci(p: f64, n: f64) -> WilsonInterval {
    let z = 1.96_f64;
    if n <= 0.0 || !p.is_finite() {
        return WilsonInterval { lo: 0.0, hi: 1.0 };
    }
    let p = p.clamp(0.0, 1.0);
    let denom = 1.0 + z * z / n;
    let center = (p + z * z / (2.0 * n)) / denom;
    let spread = z * (p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt() / denom;
    WilsonInterval {
        lo: (center - spread).max(0.0),
        hi: (center + spread).min(1.0),
    }
}

/// Median of the finite values in `vals`. `None` when no finite values exist.
pub(crate) fn median(vals: &[f64]) -> Option<f64> {
    let mut finite: Vec<f64> = vals.iter().copied().filter(|v| v.is_finite()).collect();
    if finite.is_empty() {
        return None;
    }
    finite.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = finite.len() / 2;
    if finite.len().is_multiple_of(2) {
        Some((finite.get(mid - 1)? + finite.get(mid)?) / 2.0)
    } else {
        finite.get(mid).copied()
    }
}

/// Geometric mean of the finite values in `vals` (each clamped to ≥ 1e-12 so
/// zeros don't blow up the log). `None` on empty input.
pub(crate) fn geometric_mean(vals: &[f64]) -> Option<f64> {
    let finite: Vec<f64> = vals.iter().copied().filter(|v| v.is_finite()).collect();
    if finite.is_empty() {
        return None;
    }
    let sum_ln: f64 = finite.iter().map(|v| v.max(1e-12).ln()).sum();
    Some((sum_ln / finite.len() as f64).exp())
}

/// `num / den`, or `None` when the denominator is ~0 or the result is not finite.
pub(crate) fn ratio(num: f64, den: f64) -> Option<f64> {
    if den.abs() < 1e-9 {
        return None;
    }
    let r = num / den;
    r.is_finite().then_some(r)
}

/// `num / den`, falling back to `default` when the division is undefined.
pub(crate) fn ratio_or(num: f64, den: f64, default: f64) -> f64 {
    ratio(num, den).unwrap_or(default)
}

/// Egyptian cafe price grid: 2.5 EGP steps below 25 EGP, 5 EGP steps above.
/// Input and output in piastres.
pub(crate) fn snap_egyptian(price: f64) -> i64 {
    let step: f64 = if price < 2500.0 { 250.0 } else { 500.0 };
    (price / step).round() as i64 * step as i64
}

/// Grid step size (piastres) the Egyptian rule uses at `price`.
pub(crate) fn egyptian_step(price: f64) -> i64 {
    if price < 2500.0 { 250 } else { 500 }
}

pub(crate) fn apply_rounding(price: f64, rule: &PriceRoundingRule) -> i64 {
    if !price.is_finite() {
        return 0;
    }
    match rule {
        PriceRoundingRule::NearestUnit => price.round() as i64,
        PriceRoundingRule::EgyptianCafe => snap_egyptian(price),
    }
}

pub(crate) fn rounding_step(price: f64, rule: &PriceRoundingRule) -> i64 {
    match rule {
        PriceRoundingRule::NearestUnit => 1,
        PriceRoundingRule::EgyptianCafe => egyptian_step(price),
    }
}

/// Moves under 1.5% are noise — not worth a menu reprint.
pub(crate) fn below_no_change_threshold(current: f64, suggested: f64) -> bool {
    if current <= 0.0 {
        return true;
    }
    (suggested - current).abs() / current < 0.015
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod tests {
    use super::*;

    #[test]
    fn recency_weight_today_is_one() {
        assert!((recency_weight(0.0, 14.0) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn recency_weight_at_halflife_is_half() {
        assert!((recency_weight(14.0, 14.0) - 0.5).abs() < 1e-12);
    }

    #[test]
    fn recency_weight_decreases_monotonically() {
        let w1 = recency_weight(1.0, 14.0);
        let w7 = recency_weight(7.0, 14.0);
        let w30 = recency_weight(30.0, 14.0);
        assert!(w1 > w7 && w7 > w30);
    }

    #[test]
    fn wilson_bounds_valid() {
        let ci = wilson_95_ci(0.3, 50.0);
        assert!(ci.lo >= 0.0 && ci.hi <= 1.0 && ci.lo < 0.3 && ci.hi > 0.3);
    }

    #[test]
    fn wilson_empty_sample_is_unit_interval() {
        let ci = wilson_95_ci(0.5, 0.0);
        assert_eq!(ci.lo, 0.0);
        assert_eq!(ci.hi, 1.0);
    }

    #[test]
    fn median_empty_is_none() {
        assert!(median(&[]).is_none());
        assert!(median(&[f64::NAN]).is_none());
    }

    #[test]
    fn median_even_odd() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
    }

    #[test]
    fn geometric_mean_empty_is_none() {
        assert!(geometric_mean(&[]).is_none());
        let gm = geometric_mean(&[2.0, 8.0]).unwrap();
        assert!((gm - 4.0).abs() < 1e-9);
    }

    #[test]
    fn ratio_zero_denominator_is_none() {
        assert!(ratio(1.0, 0.0).is_none());
        assert_eq!(ratio_or(1.0, 0.0, 7.0), 7.0);
        assert_eq!(ratio(6.0, 2.0), Some(3.0));
    }

    #[test]
    fn snap_egyptian_rules() {
        // < 25 EGP → 2.5 EGP grid
        assert_eq!(snap_egyptian(1100.0), 1000);
        assert_eq!(snap_egyptian(1130.0), 1250);
        assert_eq!(snap_egyptian(2374.0), 2250);
        // >= 25 EGP → 5 EGP grid
        assert_eq!(snap_egyptian(2600.0), 2500);
        assert_eq!(snap_egyptian(2750.0), 3000);
        assert_eq!(snap_egyptian(9800.0), 10000);
    }
}
