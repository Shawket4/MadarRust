# Smart POS — Menu Pricing & Bundle Suggestion Engine

> Non‑opinionated, schema‑agnostic spec for a **suggestion** engine. The engine never auto‑applies changes. It produces ranked, explainable proposals: price tweaks, bundle candidates, and demand‑impact scenarios.

This document is the source of truth for the engine's math, logic, and outputs. It does **not** name database tables, columns, or API routes — those are owned by the existing codebase. When Cursor asks "what is this field called?", the answer is always: take it as a typed parameter; do not assume.

---

## 0. Design Principles

1. **Suggest, never act.** Every output is a proposal with an explanation, a confidence level, and a comparison anchor. The owner approves or ignores.
2. **Explain everything.** Each suggestion ships with: the numbers, the peer comparison, the rule that fired, and the uncertainty.
3. **Be honest about data sufficiency.** If a suggestion would be based on too little data, the engine says so and abstains rather than producing a noisy recommendation.
4. **Schema‑agnostic.** All inputs are passed as plain typed values or iterators. No coupling to ORM, table names, or column names.
5. **Deterministic and pure.** Given the same inputs, the engine returns the same outputs. Time is an explicit parameter, not `now()`.
6. **Safety guards are non‑negotiable.** Hard caps on price change magnitude, minimum margin floors, minimum sample sizes.

---

## 1. Inputs (abstract, not schema)

The engine consumes three abstract streams. The host system maps its own schema into these shapes.

### 1.1 Item snapshot

For each menu item, at the moment of analysis:

- `item_id` — opaque identifier (string or uuid).
- `category_id` — opaque grouping (e.g., "coffee", "pastries"). Optional but recommended.
- `current_price` — money, in minor units (e.g., piastres for EGP).
- `cost_per_serving` — fully loaded ingredient cost for one unit at the current recipe, in the same currency. Should already include yield loss / portioning.
- `is_active` — whether the item is currently sellable (excludes from suggestions but kept for history).
- *(Optional)* `variant_of` — id of the parent item if this is a size/variant. The engine treats variants as independent items unless told otherwise.

### 1.2 Sales events

A flat stream of unit‑level sales over a configurable analysis window:

- `transaction_id` — opaque ticket id (groups co‑purchases).
- `item_id`
- `quantity_sold` — usually 1 but may be higher.
- `unit_price_paid` — what the customer actually paid per unit (may differ from `current_price` if discounted at sale).
- `unit_cost_at_sale` — cost per unit at the moment the sale happened (so historical CM is computed at the cost that actually applied, not today's cost).
- `sold_at` — timestamp.
- *(Optional)* `daypart` — derived bucket (e.g., morning/afternoon/evening). The engine can compute this from `sold_at` if not provided.

### 1.3 Configuration

All thresholds are parameters, not constants:

- `analysis_window_days` (default 30)
- `recency_half_life_days` (default 14) — for exponential decay weighting
- `target_food_cost_pct` (default 0.30) — used as a goalpost, not a hard rule
- `min_gross_margin_pct` (default 0.55) — floor; engine never suggests below this
- `max_price_change_pct_per_cycle` (default 0.15) — cap on a single suggestion
- `min_units_for_classification` (default 20) — below this, item is "insufficient data"
- `min_cooccurrences_for_bundle` (default 8) — minimum co‑purchase count
- `min_lift_for_bundle` (default 1.20) — association strength threshold
- `bundle_discount_pct_range` — `(0.10, 0.25)` default
- `price_rounding_rule` — culturally appropriate; for EGP coffee shops, default is "round to nearest 5 EGP, with `.5` allowed for items under 25 EGP".
- `category_substitution_map` — optional; if absent, the engine derives substitutes empirically (see §7.3).

---

## 2. Data Preparation

Before any KPI is computed, the engine performs three preparation steps. Cursor should implement these as pure functions over the input streams.

### 2.1 Window filter

Keep only sales events with `sold_at` inside `[now - analysis_window_days, now]`.

### 2.2 Recency weight

Assign each sale a weight using exponential decay:

```
weight(sale) = exp(- ln(2) * age_days(sale) / recency_half_life_days)
```

So a sale today has weight 1.0; a sale at the half‑life has weight 0.5. All "units sold" sums in §3 are **weighted** sums unless explicitly stated. Two parallel computations are useful: weighted (for ranking and suggestions) and unweighted raw counts (for displaying "real" sales numbers to the user).

### 2.3 Data‑sufficiency gate

For each item, compute `raw_units_sold` over the window. If `raw_units_sold < min_units_for_classification`, the item is flagged `insufficient_data` and is **excluded from quadrant classification and price suggestions**. It is still eligible to appear in co‑purchase analysis as a *partner* (because it can still show up in baskets), but never as the *focus* of a price suggestion.

This step alone eliminates most of the noise that ruins naïve menu‑engineering tools.

---

## 3. Per‑Item KPIs

For each item that passes the sufficiency gate, compute the following over the weighted window.

Let `w(s)` denote the recency weight of sale `s`, and `S(i)` the set of sales of item `i`.

| KPI | Formula |
|---|---|
| `weighted_units_sold` | `Σ_{s ∈ S(i)} w(s) * quantity_sold(s)` |
| `weighted_revenue` | `Σ_{s ∈ S(i)} w(s) * quantity_sold(s) * unit_price_paid(s)` |
| `weighted_cost` | `Σ_{s ∈ S(i)} w(s) * quantity_sold(s) * unit_cost_at_sale(s)` |
| `contribution_margin` | `weighted_revenue - weighted_cost` |
| `cm_per_unit` | `contribution_margin / weighted_units_sold` |
| `effective_price` | `weighted_revenue / weighted_units_sold` (may differ from `current_price` if discounts were applied) |
| `effective_cost` | `weighted_cost / weighted_units_sold` |
| `margin_pct` | `cm_per_unit / effective_price` |
| `food_cost_pct` | `effective_cost / effective_price` |
| `popularity_share` | `weighted_units_sold(i) / Σ_j weighted_units_sold(j)` over all active items |

**Why `effective_price` instead of `current_price`?** Because if an item has been sold mostly at promo prices, treating its list price as reality misleads the suggestion engine. Both numbers are kept and shown.

### 3.1 Uncertainty

For each rate‑like metric (`popularity_share`, anything proportional), also compute a **Wilson score 95% confidence interval**. This is cheap and gives Cursor a principled way to say "this item's popularity is `0.08 ± 0.03`" rather than pretending point estimates are truth.

```
Wilson(p, n, z=1.96):
  denom = 1 + z²/n
  center = (p + z²/(2n)) / denom
  spread = z * sqrt(p(1-p)/n + z²/(4n²)) / denom
  return (center - spread, center + spread)
```

---

## 4. Menu‑Engineering Classification

The engine uses the **Kasavana–Smith framework** (the original menu‑engineering matrix), which is more defensible than naïve tercile splits.

### 4.1 Thresholds

Let `N` = number of items passing the sufficiency gate.

- **Popularity threshold**: an item is "high popularity" if `popularity_share ≥ 0.70 / N`. (The 70% rule: if every item sold equally, each would have share `1/N`; high‑popularity items capture at least 70% of fair share.)
- **Profitability threshold**: an item is "high profitability" if `cm_per_unit ≥ weighted_average_cm_per_unit`, where the average is weighted by `weighted_units_sold`.

### 4.2 Quadrants

| | High profitability | Low profitability |
|---|---|---|
| **High popularity** | **Star** | **Plowhorse** |
| **Low popularity** | **Puzzle** | **Dog** |

The classification is reported with the actual distance from each threshold, so Cursor can show "barely a Star" vs "deep Star" — useful for the UI.

### 4.3 Stability over time

Quadrants can flap when an item sits near a threshold. To prevent UI noise, the engine optionally takes the previous cycle's classification as input and applies hysteresis: an item that was a Star last cycle stays Star unless it falls below the threshold by more than 5%. This is a UX feature; the underlying math is unchanged.

---

## 5. Price Suggestion Engine

### 5.1 Fixing the original formula

A clarification first, because the original spec had a real bug:

> `suggested_price_fc = cost / target_food_cost_pct`
> `suggested_price_margin = cost / (1 - target_margin_pct)`

These are the **same formula** whenever `target_margin_pct = 1 - target_food_cost_pct`, which is the normal case. They are not two candidates; they are one. The engine treats it as a single anchor and calls it `cost_plus_anchor`:

```
cost_plus_anchor = cost_per_serving / target_food_cost_pct
```

### 5.2 Three anchors, not one

A robust suggestion uses three reference points and picks intelligently, rather than blindly applying a single rule:

1. **`cost_plus_anchor`** — what the price *should* be to hit `target_food_cost_pct`.
2. **`peer_anchor`** — the median `effective_price` of items in the same `category_id` whose `cm_per_unit` is at or above the category's weighted‑average `cm_per_unit`. This is "what well‑performing peers charge".
3. **`status_quo`** — the item's own `current_price`.

For each item, compute all three. The suggested price is a quadrant‑dependent blend, clipped by safety guards.

### 5.3 Quadrant‑specific rules

Let `Δ%` be the suggested change as a fraction of `current_price`. All `Δ%` values below are *targets*; the final number is clipped (see §5.4).

#### Star

A Star is popular **and** profitable. The default action is **hold**. Two exceptions:

- If `current_price < peer_anchor × 0.95` *and* `margin_pct` is below the median Star margin in the same category, suggest a **small increase** toward `peer_anchor`, capped at `+8%`.
- If `current_price > peer_anchor × 1.10`, do **nothing** — the Star is already pricing premium and the data supports it.

#### Plowhorse

Popular but underprofitable. Two parallel suggestions are useful:

- **Price action:** raise toward `cost_plus_anchor`, but only by enough to lift `margin_pct` by 3–5 percentage points. Bigger jumps risk losing the volume that makes it a Plowhorse. Target `Δ% ∈ [+3%, +10%]`.
- **Cost action:** flag for recipe review. Output a "what‑if": `if cost_per_serving fell by 10%, new margin_pct would be X` — this lets the owner decide whether to renegotiate suppliers or raise price.

#### Puzzle

Profitable but unpopular. **Price changes rarely fix Puzzles** — the problem is awareness or positioning, not price. Default actions:

- Suggest a **bundle** (see §6) using this item as the *focus*.
- Optionally suggest a **small decrease** (`Δ% ∈ [-5%, 0%]`) only if `current_price > peer_anchor × 1.15` (the item is priced above peers despite low popularity, which may be the cause).

#### Dog

Unpopular and unprofitable. Price tweaks are last resort. Default actions, in order:

1. Suggest **removal** with a quantified removal scenario (§7.3).
2. Suggest **reformulation** if `food_cost_pct > 0.45` — the recipe is the problem.
3. Suggest **bundle inclusion** as a Hail Mary if there's strong co‑purchase signal with a Star.

The engine never auto‑suggests a price *increase* on a Dog.

### 5.4 Safety guards (applied to every suggestion)

After the quadrant rule produces a candidate `suggested_price`:

1. **Margin floor**: clip so that `(suggested_price - cost_per_serving) / suggested_price ≥ min_gross_margin_pct`.
2. **Change cap**: clip so that `|suggested_price - current_price| / current_price ≤ max_price_change_pct_per_cycle`.
3. **Cultural rounding**: round `suggested_price` to the nearest value allowed by `price_rounding_rule`. For Egyptian coffee‑shop pricing, that means snap to multiples of 5 EGP (or 2.5 EGP for items under 25 EGP). Psychological pricing (`X.99`) is **not** the norm in Egyptian café culture and is **not** applied by default.
4. **No‑change threshold**: if `|Δ%| < 1.5%`, emit `hold` instead of a numeric suggestion. Tiny changes aren't worth re‑pricing a menu.

### 5.5 Confidence level

Every price suggestion carries a `confidence` ∈ `{low, medium, high}`:

- `high` — passes sufficiency gate by 3× (i.e., `raw_units_sold ≥ 3 × min_units_for_classification`) **and** quadrant placement is more than 10% past the threshold.
- `medium` — passes sufficiency gate, quadrant placement is past threshold.
- `low` — passes sufficiency gate but is within 10% of a threshold (likely to flap).

The UI should show all three but visually distinguish them.

---

## 6. Bundle Suggestion Engine

The bundle engine answers: *given an underperforming item X, what is the most profitable bundle containing X that customers are likely to buy?*

### 6.1 Association mining (proper market‑basket math)

The original spec used only `co_purchase_rate = co_purchase_count / X_total_count`, which is just *confidence*. That's not enough to distinguish "Y is bought with X because Y is bought with everything" from "Y is genuinely associated with X". Use the full triplet:

For any pair of items `(X, Y)`:

```
T = total number of transactions in the window
support(X)    = transactions_containing(X) / T
support(Y)    = transactions_containing(Y) / T
support(X∩Y)  = transactions_containing_both(X, Y) / T
confidence(X→Y) = support(X∩Y) / support(X)        # = P(Y | X)
lift(X, Y)      = support(X∩Y) / (support(X) * support(Y))
```

- `lift > 1` means Y is bought with X more than chance would predict.
- `lift = 1` means independent.
- `lift < 1` means *negatively* associated (substitutes, possibly).

Filter candidate partners to: `support(X∩Y) ≥ min_cooccurrences_for_bundle / T` AND `lift(X, Y) ≥ min_lift_for_bundle`.

### 6.2 Partner ranking

Rank surviving partners `Y` of focus item `X` by a composite score:

```
partner_score(Y | X) = (lift(X, Y) - 1) * sqrt(support(X∩Y)) * cm_per_unit(Y)
```

Intuition:
- `(lift - 1)` rewards genuine association strength.
- `sqrt(support)` rewards real volume but with diminishing returns (so a giant‑volume Y doesn't dominate every list).
- `cm_per_unit(Y)` rewards using high‑margin items as bundle anchors (so they subsidize the underperformer).

Take the top‑K partners (default K=5).

### 6.3 Bundle composition

Generate candidate bundles of size 2–3 (configurable, default max size = 3):

- Every bundle includes the focus item `X`.
- Choose 1 or 2 partners from the top‑K list.
- **Deduplicate** by normalized sorted item‑id tuple. The engine should not present `(X, Y, Z)` and `(X, Z, Y)` as two suggestions.

For each candidate bundle `B = {X, Y, ...}`:

```
bundle_cost          = Σ cost_per_serving(i)   for i in B
bundle_list_price    = Σ current_price(i)      for i in B
```

### 6.4 Bundle pricing (three strategies, pick the best)

Compute three candidate prices, then pick the highest that still meets all guards:

1. **Discount‑anchored**: `price_a = bundle_list_price * (1 - d)` for `d` swept across `bundle_discount_pct_range` in 5% steps. Pick the smallest discount (largest `price_a`) that the customer is likely to perceive as a real deal — empirically `d ≥ 0.10`.

2. **Cost‑anchored**: `price_b = bundle_cost / target_food_cost_pct`.

3. **Anchor‑and‑round**: take `max(price_a, price_b)` and snap to the cultural rounding rule. Call this `bundle_suggested_price`.

Guards on the final bundle price:
- Must show a perceivable discount: `bundle_suggested_price ≤ bundle_list_price * 0.95`.
- Must meet bundle margin floor: `(bundle_suggested_price - bundle_cost) / bundle_suggested_price ≥ min_gross_margin_pct - 0.05` (allow a 5‑point slack for bundles, since they trade margin for volume — but no more).
- If both can't be satisfied, **reject the bundle**.

### 6.5 Bundle scoring

For ranking bundles per focus item:

```
bundle_score(B) = bundle_cm * expected_bundle_velocity(B) * association_strength(B)
```

where:

- `bundle_cm = bundle_suggested_price - bundle_cost`
- `expected_bundle_velocity(B)` — see §7.1.
- `association_strength(B) = geometric_mean over partner_score(Y | X) for partners in B` — penalizes bundles whose partners are weakly associated.

Present the top 1–3 bundles per focus item.

### 6.6 Avoiding cannibalization in scoring

A bundle that simply repackages what customers were already buying *together at full price* generates no incremental margin — it just gives away the discount.

For each bundle, compute a **cannibalization adjustment**:

```
cannibalized_baskets = expected_bundle_velocity * P(customer would have bought all of B anyway)
P(would_have_bought_all_anyway) ≈ Π over partners Y of confidence(X → Y)
                                   # crude but conservative
incremental_velocity = expected_bundle_velocity * (1 - P(would_have_bought_all_anyway))
incremental_cm       = incremental_velocity * bundle_cm
                       - cannibalized_baskets * (bundle_list_price - bundle_suggested_price)
                                                # margin lost on baskets that would
                                                # have been bought anyway
```

The engine displays **both** `bundle_cm` (gross) and `incremental_cm` (true) and ranks by `incremental_cm` when available.

---

## 7. Demand Impact Estimation

This is where the engine gets honest about uncertainty. It runs three scenarios per focus item.

### 7.1 Expected bundle velocity

Estimating how many bundles will sell is a forecast. There are three regimes:

- **No prior bundles** in this menu: use a **prior** based on focus item's standalone velocity, scaled by association strength.
  ```
  expected_bundle_velocity_per_day ≈ weighted_units_sold(X) / window_days
                                    × partner_score_normalized
                                    × promotion_lift_prior
  ```
  Default `promotion_lift_prior = 1.25` (modest — bundles don't 10× things).

- **Prior bundles exist**: fit a simple geometric‑mean of historical bundle adoption rates from the existing bundle catalog.

- **Insufficient data either way**: report velocity as a range `[low, mid, high]` derived from prior ± 50%.

The expected velocity always ships as a triplet, never a single number, so the UI can show "best case / expected / worst case".

### 7.2 Halo effect (post‑bundle standalone uplift)

Some customers who try X in a bundle will return to buy X alone. Model this with a **trial‑and‑repeat** structure rather than a single "persistence factor":

```
trial_units      = expected_bundle_velocity * window_days   # one analysis window
repeat_rate      = configurable; default 0.15
                   # fraction of unique triers who return for X alone
                   # at least once in the next window
unique_trier_rate = 0.85
                   # crude: assume 85% of bundle baskets are distinct customers
unique_triers     = trial_units * unique_trier_rate
halo_units        = unique_triers * repeat_rate
```

Output:

- `inside_bundle_units(X)` over the next window
- `halo_units(X)` over the next window
- `total_units_uplift(X) = inside_bundle_units + halo_units`
- Compare against `weighted_units_sold(X)` over the *current* window — the user sees uplift in concrete terms.

`repeat_rate` is **always shown** as the assumption it is, with a slider or input so the owner can pressure‑test the forecast.

### 7.3 Removal scenario

For Dogs and weak Puzzles, the engine offers a counterfactual: *what happens to total CM if X is removed?*

For each candidate destination item `Z` (a substitute for `X`), estimate a **shift coefficient** `s(X → Z)` representing the fraction of X's lost demand that flows to Z. Two methods:

1. **Explicit map** (if `category_substitution_map` is provided): use the configured weights.
2. **Empirical fallback**: use *negative co‑purchase* signal within the same category. Items in the same category as X with `lift(X, Z) < 1` (i.e., substitutes — bought *instead of* X, not with it) are candidates. Weight by `support(Z)`.

```
absorbed_units(Z) = weighted_units_sold(X) * s(X → Z)
absorbed_cm(Z)    = absorbed_units(Z) * cm_per_unit(Z)
total_lost_cm     = contribution_margin(X)
total_recovered_cm = Σ_Z absorbed_cm(Z)
net_cm_change     = total_recovered_cm - total_lost_cm
```

Also model **complementary loss**: if X had strong positive lift with item Y, removing X may suppress Y's sales.

```
for each Y with lift(X, Y) > 1.2:
  y_loss_units = absorbed_units_total * (lift(X, Y) - 1) * support_share(X among Y's baskets)
  y_loss_cm    = y_loss_units * cm_per_unit(Y)
  net_cm_change -= y_loss_cm
```

The output is `net_cm_change` with a confidence interval derived from varying `s(X → Z)` and `repeat_rate` over their ranges.

---

## 8. Outputs

The engine produces three kinds of records. The host system decides how to render them.

### 8.1 Price suggestion record

```
PriceSuggestion {
  item_id
  quadrant: enum { Star, Plowhorse, Puzzle, Dog, InsufficientData }
  current:  { price, cm_per_unit, margin_pct, food_cost_pct, popularity_share, units_sold_raw }
  anchors:  { cost_plus, peer_median, status_quo }
  suggested_price
  suggested_delta_abs
  suggested_delta_pct
  action: enum { Hold, RaisePrice, LowerPrice, Bundle, Remove, Reformulate, Monitor }
  confidence: enum { Low, Medium, High }
  explanation: string  // human-readable, references the rule that fired
  guard_clips: [string]  // which safety guards modified the raw candidate
  peer_comparison: {
    same_category_count
    median_margin_pct_peers
    median_cm_per_unit_peers
    your_position: enum { AboveMedian, AtMedian, BelowMedian }
  }
}
```

### 8.2 Bundle suggestion record

```
BundleSuggestion {
  focus_item_id
  bundle_items: [item_id]
  bundle_list_price
  bundle_suggested_price
  bundle_discount_pct
  bundle_cost
  bundle_cm
  bundle_margin_pct
  association: {
    pair_lifts: [(item_pair, lift, support, confidence)]
    composite_score
  }
  forecast: {
    expected_bundle_velocity_low
    expected_bundle_velocity_mid
    expected_bundle_velocity_high
    inside_bundle_units_X
    halo_units_X
    total_units_uplift_X
    incremental_cm_low
    incremental_cm_mid
    incremental_cm_high
  }
  guard_clips: [string]
  explanation: string
}
```

### 8.3 Removal scenario record

```
RemovalScenario {
  item_id
  baseline_cm: contribution_margin(X)
  absorbed_by: [(item_id, absorbed_units, absorbed_cm)]
  complementary_losses: [(item_id, lost_units, lost_cm)]
  net_cm_change
  net_cm_change_low
  net_cm_change_high
  recommendation: enum { Remove, KeepAndBundle, KeepAndReformulate, NoStrongSignal }
  explanation: string
}
```

---

## 9. A/B Tracking (close the loop)

A suggestion engine that doesn't learn from outcomes degrades into a confident liar over time. The host system should persist:

- Every suggestion made, with timestamp and full numeric context.
- Whether the owner accepted, modified, or rejected it.
- For accepted suggestions: a snapshot of the item's KPIs at `T+7`, `T+14`, `T+30` days post‑change.

The engine itself does not need to update its weights automatically. But it should produce a periodic **calibration report**:

- For accepted price changes, the realized `Δ margin_pct` vs. predicted.
- For accepted bundles, realized velocity vs. predicted `[low, mid, high]`.
- A simple "calibration score" — what fraction of realized outcomes fell inside the predicted range.

When calibration drifts, the spec's default constants (e.g., `repeat_rate`, `promotion_lift_prior`) should be reviewed by the owner.

---

## 10. Edge Cases & Required Behavior

| Case | Required behavior |
|---|---|
| Item is brand new (< analysis window) | `InsufficientData`. No suggestion. No quadrant. |
| Item has `cost_per_serving = 0` (e.g., complimentary water) | Skip from all suggestions. Excluded from popularity denominator. |
| Item is sold only as part of a bundle (never standalone) | Track separately. Do not include in standalone popularity_share. |
| Cost surged > 25% in the window | Use a **time‑weighted** cost in CM calculations, and flag the item with `cost_volatility=high` in its explanation. |
| All items in a category are below profitability threshold | Don't classify the whole category as Plowhorses. Instead, recompute the threshold against the **menu‑wide** average, not the category average. The category may genuinely be a weak category. |
| Bundle would include items that are variants of each other (e.g., small latte + large latte) | Reject. Variants of the same parent never co‑bundle. |
| Same item appears twice in a transaction (quantity > 1) | Count as one co‑occurrence event for association, but full quantity for revenue/units. Co‑purchase is about basket composition, not basket size. |
| Item is currently marked inactive but had sales in the window | Compute historical KPIs for reporting, but emit `action=Monitor` and no price suggestion. |
| Two items have identical names with different `item_id` | Treat as independent. Do not auto‑merge. |

---

## 11. Implementation Prompts for Cursor

Use these as the actual prompts when generating Rust code. They are deliberately framed to avoid schema assumptions.

### 11.1 KPI computation

> Generate a pure Rust function that takes an iterator over typed `SaleEvent { item_id, quantity, unit_price, unit_cost, sold_at }` plus an `AnalysisConfig`, and returns a `HashMap<ItemId, ItemKpi>` where `ItemKpi` contains weighted_units_sold, weighted_revenue, weighted_cost, contribution_margin, cm_per_unit, effective_price, margin_pct, food_cost_pct, popularity_share, and a Wilson 95% CI for popularity_share. Recency weights use exponential decay with the configured half‑life. The function must be deterministic given the same `now` value and must not call any clock function.

### 11.2 Menu‑engineering classifier

> Generate a Rust function `classify_items(kpis: &HashMap<ItemId, ItemKpi>, config: &AnalysisConfig, previous: Option<&HashMap<ItemId, Quadrant>>) -> HashMap<ItemId, Quadrant>`. Use the Kasavana–Smith thresholds: popularity high if `popularity_share ≥ 0.70 / N`, profitability high if `cm_per_unit ≥ weighted_average_cm_per_unit`. Apply hysteresis from `previous` to avoid flapping (±5% deadband around thresholds).

### 11.3 Price suggestion

> Generate a Rust function `suggest_price(item: &ItemKpi, quadrant: Quadrant, peers: &[&ItemKpi], config: &PriceConfig) -> PriceSuggestion`. Implement the three anchors (cost_plus, peer_median, status_quo) and the quadrant‑specific blending rules from §5.3. Apply all safety guards from §5.4 in order: margin_floor, change_cap, cultural_rounding, no_change_threshold. Each guard that fires must be recorded in `guard_clips`. The explanation field must be a templated human‑readable string, not a debug dump.

### 11.4 Association mining

> Generate a Rust function `compute_pairwise_associations(transactions: &[Vec<ItemId>]) -> HashMap<(ItemId, ItemId), Association>` where `Association { support, confidence_xy, confidence_yx, lift }`. Use canonical ordering of the pair (smaller item_id first) for hashmap keys, but return both directional confidences. Optimize for sparsity — most pairs will not co‑occur; use a HashMap of counts, not a dense matrix.

### 11.5 Bundle generator

> Generate a Rust function that, for each focus item X with quadrant in {Puzzle, Dog}, produces up to 3 bundle suggestions per the rules in §6. Enforce: max bundle size = 3, deduplication by sorted item‑id tuple, all safety guards on bundle price, rejection of bundles containing variant pairs (use the `variant_of` field if present). Return `Vec<BundleSuggestion>` with `incremental_cm` triplet (low/mid/high).

### 11.6 Demand impact

> Generate a Rust function `estimate_halo(focus: &ItemKpi, bundle: &BundleSuggestion, config: &ForecastConfig) -> HaloEstimate` implementing the trial‑and‑repeat model from §7.2. The repeat_rate must be a config parameter, not a constant. Return low/mid/high triplets by sweeping repeat_rate over ±50% of its configured value.

### 11.7 Removal scenario

> Generate a Rust function `simulate_removal(target: ItemId, kpis: &HashMap<ItemId, ItemKpi>, associations: &HashMap<(ItemId, ItemId), Association>, config: &RemovalConfig) -> RemovalScenario`. Implement substitution via the negative‑lift fallback if no explicit substitution map is provided. Always compute the complementary‑loss term (§7.3) for partners with `lift > 1.2`. Return net_cm_change as a triplet.

### 11.8 Output rendering

> Generate Rust serde‑serializable structs for `PriceSuggestion`, `BundleSuggestion`, and `RemovalScenario` matching the schemas in §8. Provide `Display` impls that render the `explanation` field as plain text without ANSI codes (so the same string can be sent to a web UI or a printed report).

---

## 12. Out of Scope (explicitly)

These are deliberately not part of this engine. If they show up in code, push back:

- **Auto‑applying** any suggestion. The engine emits records; a separate, audited workflow applies them.
- **User‑level personalization** (recommending different prices to different customers). Pricing is per‑item, period.
- **Real‑time price changes** (surge pricing). Suggestions are computed on a configurable cadence (daily/weekly), not per‑transaction.
- **Inventory‑driven pricing** (raising price when stock is low). Out of scope for the suggestion engine; belongs in inventory.
- **Cross‑outlet pricing** in a multi‑location chain. Each outlet runs its own engine on its own data.
- **Anything that requires customer identity.** The engine works on baskets and items only.

---

## 13. Quick Mental Checklist Before Shipping

- [ ] No literal table or column names appear in the engine code.
- [ ] Every threshold is read from config; no magic numbers in function bodies.
- [ ] Every output carries `confidence` and a templated explanation.
- [ ] Every safety guard is unit‑tested by a case that triggers it.
- [ ] `insufficient_data` flows cleanly through every layer (no `unwrap()` on a missing KPI).
- [ ] Pair associations use lift, not just confidence.
- [ ] Bundle scoring uses `incremental_cm`, not gross `cm`, where data allows.
- [ ] Halo and removal estimates return triplets, never single point values.
- [ ] Cultural rounding is applied to every final monetary value.
- [ ] The A/B logging hook exists, even if no calibration is computed yet.
