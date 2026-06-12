//! User-facing explanation strings — the one place advisor English lives.
//!
//! Template shape: `[State]. [Evidence]. [Action]. [Caveat?]` — at most two
//! sentences plus an optional caveat. Money is formatted in EGP (piastres /
//! 100), percentages with one decimal.

/// Piastres → "EGP 45" (or "EGP 45.50" when not a whole pound).
pub(crate) fn fmt_egp(piastres: f64) -> String {
    if !piastres.is_finite() {
        return "EGP ?".into();
    }
    let egp = piastres / 100.0;
    if (egp - egp.round()).abs() < 0.005 {
        format!("EGP {:.0}", egp.round())
    } else {
        format!("EGP {egp:.2}")
    }
}

/// 0.123 → "12.3%".
pub(crate) fn fmt_pct(fraction: f64) -> String {
    if !fraction.is_finite() {
        return "?%".into();
    }
    format!("{:.1}%", fraction * 100.0)
}

// ── Price suggestions ────────────────────────────────────────────────

pub(crate) fn star_hold(cur: f64) -> String {
    format!("Star: popular and profitable. Hold at {}.", fmt_egp(cur))
}

pub(crate) fn star_hold_no_benchmark() -> String {
    "Star: popular and profitable, with no same-category Star benchmark to justify a raise. \
     Hold current price."
        .into()
}

pub(crate) fn star_raise(cur: f64, peer: f64) -> String {
    format!(
        "Star priced below category peers ({} vs median {}) with below-Star margin. \
         Raise toward peer pricing.",
        fmt_egp(cur),
        fmt_egp(peer)
    )
}

pub(crate) fn plowhorse_raise(margin_pct: f64) -> String {
    format!(
        "Plowhorse: popular but margin {} is below the menu average. \
         A moderate raise would lift margin by roughly 4 points.",
        fmt_pct(margin_pct)
    )
}

pub(crate) fn puzzle_lower(premium_pct: f64) -> String {
    format!(
        "Puzzle: profitable but slow, priced {} above category peers. \
         A small trim may improve trial.",
        fmt_pct(premium_pct)
    )
}

pub(crate) fn puzzle_bundle() -> String {
    "Puzzle: profitable but slow. Bundle with a popular partner rather than repricing.".into()
}

pub(crate) fn dog_remove() -> String {
    "Dog: low sales and low margin. Candidate for removal — see the removal scenario.".into()
}

pub(crate) fn dog_reformulate(food_cost_pct: f64) -> String {
    format!(
        "Dog with food cost {}. Reformulate the recipe before considering removal.",
        fmt_pct(food_cost_pct)
    )
}

pub(crate) fn hero_raise(below_peer_pct: f64) -> String {
    format!(
        "Hero: a top seller priced {} below category peers. Small raise suggested. \
         Cost data missing — add ingredient costs for margin-aware advice.",
        fmt_pct(below_peer_pct)
    )
}

pub(crate) fn hero_hold() -> String {
    "Hero: popular and high-priced. Hold. Cost data missing — margin analysis \
     unavailable until ingredient costs are added."
        .into()
}

pub(crate) fn steady_raise(cap_pct: f64) -> String {
    format!(
        "Steady: high popularity at a low average price. Small {} raise suggested. \
         Cost data missing — add ingredient costs for margin-aware advice.",
        fmt_pct(cap_pct)
    )
}

pub(crate) fn slow_bundle() -> String {
    "Slow: priced high but unpopular. Bundle to drive trial. Removal not assessed — \
     cost data missing."
        .into()
}

pub(crate) fn quiet_monitor() -> String {
    "Quiet: low popularity and low price. Not enough signal for a price move \
     without cost data."
        .into()
}

pub(crate) fn insufficient(raw_units: f64, window_days: f64, min_units: f64) -> String {
    format!(
        "Only {raw_units:.0} units in {window_days:.0} days — below the {min_units:.0} \
         needed for a recommendation."
    )
}

pub(crate) fn bundle_only_sku() -> String {
    "Sells only inside bundles — no standalone price signal in this window.".into()
}

pub(crate) fn inactive_with_sales() -> String {
    "Inactive item with sales inside the window — review availability.".into()
}

pub(crate) fn suppressed_suffix() -> &'static str {
    " Price changed recently — letting the new price accumulate data before another move."
}

pub(crate) fn grid_too_coarse_suffix() -> &'static str {
    " The price grid has no point inside the per-cycle change cap, so holding for now."
}

// ── Bundles ──────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub(crate) fn bundle(
    names: &[&str],
    pair_count: u64,
    window_days: f64,
    lift: f64,
    suggested: f64,
    discount_pct: f64,
    list: f64,
    velocity_lo: f64,
    velocity_hi: f64,
    incremental_cm_mid: Option<f64>,
) -> String {
    let joined = names.join(" + ");
    let cm_clause = match incremental_cm_mid {
        Some(cm) => format!(
            "about {} contribution over {window_days:.0} days",
            fmt_egp(cm)
        ),
        None => "contribution unknown — cost data missing for a component".into(),
    };
    format!(
        "{joined}: bought together {pair_count} times in {window_days:.0} days (lift {lift:.2}). \
         Suggested {} ({} off {}). Expected {velocity_lo:.1}–{velocity_hi:.1} bundles/day; {cm_clause}.",
        fmt_egp(suggested),
        fmt_pct(discount_pct),
        fmt_egp(list),
    )
}

// ── Removal ──────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub(crate) fn removal(
    name: &str,
    baseline_cm: f64,
    window_days: f64,
    recovered: f64,
    lost: f64,
    net: f64,
    net_lo: f64,
    net_hi: f64,
) -> String {
    format!(
        "Removing {name} frees {} of contribution per {window_days:.0} days. Category peers \
         should absorb part of its demand (recovering {}); attached sales would lose {}. \
         Net effect {} (range {} to {}).",
        fmt_egp(baseline_cm),
        fmt_egp(recovered),
        fmt_egp(lost),
        fmt_egp(net),
        fmt_egp(net_lo),
        fmt_egp(net_hi),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn egp_formatting() {
        assert_eq!(fmt_egp(4500.0), "EGP 45");
        assert_eq!(fmt_egp(4550.0), "EGP 45.50");
        assert_eq!(fmt_egp(f64::NAN), "EGP ?");
    }

    #[test]
    fn pct_formatting() {
        assert_eq!(fmt_pct(0.123), "12.3%");
        assert_eq!(fmt_pct(f64::INFINITY), "?%");
    }

    #[test]
    fn templates_interpolate() {
        assert!(star_raise(2000.0, 2500.0).contains("EGP 20"));
        assert!(plowhorse_raise(0.42).contains("42.0%"));
        assert!(insufficient(5.0, 30.0, 20.0).contains("5 units"));
    }
}
