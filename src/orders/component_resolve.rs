//! Shared menu-item configuration resolution (sizes, addons, optionals, inventory).
//! Used by standalone order lines and bundle component lines.

use crate::errors::AppError;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Deserialize, Serialize, Clone, Default, ToSchema)]
pub struct AddonInput {
    pub addon_item_id: Uuid,
    #[serde(default = "default_qty")]
    pub quantity: i32,
    /// Charged unit price (piastres) the POS applied for this addon. When present
    /// it is RECORDED as the addon's unit_price; absent → the server's expected
    /// (catalog) price is used. Bundle-component addons ignore this (server-priced).
    #[serde(default)]
    pub unit_price: Option<i32>,
}

pub fn default_qty() -> i32 {
    1
}

#[derive(Deserialize, Serialize, Clone, ToSchema)]
pub struct BundleComponentInput {
    pub item_id: Uuid,
    pub quantity: i32,
    #[serde(default)]
    pub size_label: Option<String>,
    #[serde(default)]
    pub addons: Vec<AddonInput>,
    #[serde(default)]
    pub optional_field_ids: Vec<Uuid>,
}

#[derive(Clone)]
pub struct InventoryDeduction {
    pub org_ingredient_id: Option<Uuid>,
    pub ingredient_name: String,
    pub unit: String,
    pub quantity: f64,
    pub source: String,
    pub category: String,
    /// Set for additive-addon deductions — attribution for per-addon costing.
    pub addon_item_id: Option<Uuid>,
    /// Set for optional-field deductions.
    pub optional_field_id: Option<Uuid>,
    /// Why this line differs from what was authored ("swapped from X",
    /// "follows the chosen X"). Read by the dry-run preview; orders ignore it.
    pub note: Option<String>,
    /// The resolver could not deduct this line (e.g. incompatible units): its
    /// `org_ingredient_id` is `None`, so inventory skips it.
    pub undeducted: bool,
}

/// A resolution problem that the order path only logs; the preview returns it.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct ResolveWarning {
    /// `unit_conversion` | `swap_failed` | `optional_not_found` | `optional_size_mismatch`,
    /// or a lint rule id (`F4`…`F10`) when added by the preview.
    pub rule: String,
    pub message: String,
}

#[derive(Clone)]
pub struct ResolvedAddon {
    pub addon_item_id: Uuid,
    pub addon_name: String,
    pub name_translations: serde_json::Value,
    pub unit_price: i32,
    pub quantity: i32,
    /// A milk/coffee swap (replaces the base recipe ingredient) — its cost lives
    /// inside the item's recipe rollup, so it isn't costed as an additive addon.
    pub is_swap: bool,
    /// An additive addon that has its own ingredient rows (so it carries cost).
    pub has_ingredients: bool,
    /// For a charged swap: the name of the default option the price is the
    /// difference over.
    pub swap_over: Option<String>,
}

#[derive(Clone)]
pub struct ResolvedOptional {
    pub optional_field_id: Uuid,
    pub field_name: String,
    pub name_translations: serde_json::Value,
    pub price: i32,
    pub org_ingredient_id: Option<Uuid>,
    pub ingredient_name: Option<String>,
    pub ingredient_unit: Option<String>,
    pub quantity_used: Option<f64>,
}

pub struct MenuItemResolution {
    pub deductions: Vec<InventoryDeduction>,
    pub addons: Vec<ResolvedAddon>,
    pub optionals: Vec<ResolvedOptional>,
    pub addon_line: i32,
    pub optional_line: i32,
    pub warnings: Vec<ResolveWarning>,
}

/// How one addon choice relates to the drink's recipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SwapTarget {
    /// Ingredient category slug of the recipe line the choice replaces.
    pub slug: String,
    /// The group's explicit swap category (B2); `None` = inferred from the type.
    pub category_id: Option<Uuid>,
    /// Family key: two choices with the same key cannot share a line.
    pub family: String,
}

/// Explicit (`effect = 'swaps'` + `swap_category_id`) wins; otherwise today's
/// inference from the legacy type (`milk_type` → `milk`, `coffee_type` →
/// `coffee_bean`). The magic families keep their type as the key so an explicit
/// milk group and an inferred milk group still collapse together.
pub(crate) fn swap_target(
    addon_type: Option<&str>,
    effect: Option<&str>,
    swap_category_id: Option<Uuid>,
    swap_category_slug: Option<&str>,
) -> Option<SwapTarget> {
    let family_of = |slug: &str, fallback: String| match slug {
        "milk" => "milk_type".to_string(),
        "coffee_bean" => "coffee_type".to_string(),
        _ => fallback,
    };
    if effect == Some("swaps")
        && let (Some(cid), Some(slug)) = (swap_category_id, swap_category_slug)
    {
        return Some(SwapTarget {
            slug: slug.to_string(),
            category_id: Some(cid),
            family: family_of(slug, format!("category:{cid}")),
        });
    }
    let slug = match addon_type {
        Some("milk_type") => "milk",
        Some("coffee_type") => "coffee_bean",
        _ => return None,
    };
    Some(SwapTarget {
        slug: slug.to_string(),
        category_id: None,
        family: family_of(slug, String::new()),
    })
}

/// A drink has ONE milk and ONE coffee: a swap-family addon (`milk_type` /
/// `coffee_type`, or any explicit `swaps` group) REPLACES the recipe's ingredient,
/// so two of one family on a line cannot be made, costed or deducted — the second
/// swap silently overwrote the first while both were charged. Tills already in the
/// field sent such lines (and replay them from their outbox), so the line is not
/// refused — the LAST choice of each family wins, at quantity 1, which is what the
/// till shows the customer after picking a second milk.
pub(crate) fn collapse_families(families: &[Option<String>]) -> Vec<bool> {
    let mut keep = vec![true; families.len()];
    for i in 0..families.len() {
        if let Some(f) = &families[i]
            && families[i + 1..].iter().any(|g| g.as_ref() == Some(f))
        {
            keep[i] = false;
        }
    }
    keep
}

#[cfg(test)]
pub(crate) fn collapse_swap_families(types: &[Option<String>]) -> Vec<bool> {
    let families: Vec<Option<String>> = types
        .iter()
        .map(|t| swap_target(t.as_deref(), None, None, None).map(|s| s.family))
        .collect();
    collapse_families(&families)
}

/// Per addon id: its legacy type plus its group's explicit swap settings.
#[allow(clippy::type_complexity)]
async fn load_swap_rows(
    pool: &PgPool,
    ids: &[Uuid],
) -> Result<Vec<(Uuid, String, Option<String>, Option<Uuid>, Option<String>)>, AppError> {
    Ok(sqlx::query_as(
        "SELECT a.id, a.type, g.effect, g.swap_category_id, c.slug
           FROM addon_items a
           LEFT JOIN modifier_options mo ON mo.id = a.id
           LEFT JOIN modifier_groups g ON g.id = mo.group_id
           LEFT JOIN ingredient_categories c ON c.id = g.swap_category_id
          WHERE a.id = ANY($1)",
    )
    .bind(ids)
    .fetch_all(pool)
    .await?)
}

async fn one_choice_per_swap_family(
    pool: &PgPool,
    addons: &[AddonInput],
) -> Result<Vec<AddonInput>, AppError> {
    if addons.len() < 2 {
        return Ok(addons.to_vec());
    }
    let ids: Vec<Uuid> = addons.iter().map(|a| a.addon_item_id).collect();
    let rows = load_swap_rows(pool, &ids).await?;
    let families: Vec<Option<String>> = ids
        .iter()
        .map(|id| {
            rows.iter()
                .find(|r| r.0 == *id)
                .and_then(|(_, t, e, cid, slug)| {
                    swap_target(Some(t), e.as_deref(), *cid, slug.as_deref())
                })
                .map(|s| s.family)
        })
        .collect();
    let keep = collapse_families(&families);
    if keep.iter().any(|k| !k) {
        tracing::warn!("order line carried more than one choice of a swap family; kept the last");
    }
    Ok(addons
        .iter()
        .zip(&families)
        .zip(keep)
        .filter(|(_, k)| *k)
        .map(|((a, f), _)| {
            let mut a = a.clone();
            if f.is_some() {
                a.quantity = 1;
            }
            a
        })
        .collect())
}

/// An option's explicit replacement ingredient (`replaces_ingredient_id`) with its
/// name and stock unit, when set.
async fn explicit_replacement(
    pool: &PgPool,
    option_id: Uuid,
) -> Result<Option<(Uuid, String, String)>, AppError> {
    Ok(sqlx::query_as(
        "SELECT oi.id, oi.name, oi.unit::text
           FROM modifier_options mo
           JOIN org_ingredients oi ON oi.id = mo.replaces_ingredient_id
          WHERE mo.id = $1",
    )
    .bind(option_id)
    .fetch_optional(pool)
    .await?)
}

/// Per-size option amounts (menu modeling B9): an option line whose `size_label`
/// equals the ordered size REPLACES the generic (NULL-size) line for the same
/// ingredient, and a sized line for an ingredient with no generic line is added.
/// The generic lines come from `addon_item_ingredients` (which only ever shows NULL-
/// size rows), so with no sized lines — every old catalog — nothing changes.
pub fn merge_sized_option_lines(
    generic: Vec<(Option<Uuid>, f64, String, String)>,
    sized: Vec<(Option<Uuid>, f64, String, String)>,
) -> Vec<(Option<Uuid>, f64, String, String)> {
    if sized.is_empty() {
        return generic;
    }
    let mut out: Vec<_> = generic
        .into_iter()
        .map(
            |g| match sized.iter().find(|s| s.0.is_some() && s.0 == g.0) {
                Some(s) => s.clone(),
                None => g,
            },
        )
        .collect();
    for s in sized {
        if !out.iter().any(|o| o.0.is_some() && o.0 == s.0) {
            out.push(s);
        }
    }
    // Keep the resolver's order (ingredient name, then id) so a multi-line swap
    // option's `.first()` stays deterministic once sized lines are merged in.
    out.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.0.cmp(&b.0)));
    out
}

async fn prefer_sized_option_lines(
    pool: &PgPool,
    option_id: Uuid,
    size_label: Option<&str>,
    generic: Vec<(Option<Uuid>, f64, String, String)>,
) -> Result<Vec<(Option<Uuid>, f64, String, String)>, AppError> {
    let Some(label) = size_label else {
        return Ok(generic);
    };
    let sized: Vec<(Option<Uuid>, f64, String, String)> = sqlx::query_as(
        "SELECT rl.ingredient_id, rl.quantity::float8, oi.name, rl.unit
         FROM recipe_lines rl JOIN org_ingredients oi ON oi.id = rl.ingredient_id
         WHERE rl.owner_type = 'modifier_option' AND rl.owner_id = $1 AND rl.size_label = $2
         ORDER BY oi.name, rl.ingredient_id",
    )
    .bind(option_id)
    .bind(label)
    .fetch_all(pool)
    .await?;
    Ok(merge_sized_option_lines(generic, sized))
}

/// Resolve a menu item configuration (same rules as a standalone POS line).
/// [line_quantity] is the total multiplier for inventory (e.g. bundle line qty × component qty per bundle).
pub async fn resolve_menu_item_configuration(
    pool: &PgPool,
    menu_item_id: Uuid,
    size_label: Option<String>,
    line_quantity: i32,
    addons: &[AddonInput],
    optional_field_ids: &[Uuid],
    // Branch the line is sold at — addon prices are resolved branch-effective so a
    // bundle's component-addon surcharge matches what the branch POS charged.
    branch_id: Uuid,
) -> Result<MenuItemResolution, AppError> {
    if line_quantity <= 0 {
        return Err(AppError::BadRequest("Quantity must be > 0".into()));
    }

    let addons = &one_choice_per_swap_family(pool, addons).await?[..];

    let mut deductions: Vec<InventoryDeduction> = Vec::new();
    let mut resolved_addons: Vec<ResolvedAddon> = Vec::new();
    let mut resolved_optionals: Vec<ResolvedOptional> = Vec::new();
    // Ingredient categories swapped by an explicit choice on this line.
    let mut swap_slugs: Vec<String> = Vec::new();
    let mut warnings: Vec<ResolveWarning> = Vec::new();

    // Base drink recipe
    let recipe_rows: Vec<(Option<Uuid>, f64, String, String, String)> = if let Some(ref size) =
        size_label
    {
        sqlx::query_as(
            r#"SELECT r.org_ingredient_id, r.quantity_used::float8,
                          r.ingredient_name, r.ingredient_unit,
                          (SELECT ic.slug FROM ingredient_categories ic WHERE ic.id = i.category_id) as category
                   FROM   menu_item_recipes r
                   LEFT JOIN org_ingredients i ON i.id = r.org_ingredient_id
                   WHERE  r.menu_item_id = $1 AND r.size_label = $2"#,
        )
        .bind(menu_item_id)
        .bind(size)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as(
                r#"SELECT r.org_ingredient_id, r.quantity_used::float8,
                          r.ingredient_name, r.ingredient_unit,
                          (SELECT ic.slug FROM ingredient_categories ic WHERE ic.id = i.category_id) as category
                   FROM   menu_item_recipes r
                   LEFT JOIN org_ingredients i ON i.id = r.org_ingredient_id
                   WHERE  r.menu_item_id = $1
                     AND  r.size_label = COALESCE(
                         (SELECT rr.size_label FROM menu_item_recipes rr
                          LEFT JOIN menu_item_sizes sz ON sz.menu_item_id = rr.menu_item_id AND sz.label = rr.size_label
                          WHERE rr.menu_item_id = $1
                          -- The item's FIRST size as listed (sort, then label), as the POS
                          -- shows it — not the alphabetical first (Can < Cup).
                          ORDER BY sz.is_active IS NOT TRUE, sz.sort NULLS LAST, rr.size_label
                          LIMIT 1),
                         'one_size'
                     )"#,
            )
            .bind(menu_item_id)
            .fetch_all(pool)
            .await?
    };

    for (ing_id, qty, name, unit, category) in recipe_rows {
        deductions.push(InventoryDeduction {
            org_ingredient_id: ing_id,
            ingredient_name: name,
            unit,
            quantity: qty * line_quantity as f64,
            source: "drink_recipe".into(),
            category,
            addon_item_id: None,
            optional_field_id: None,
            note: None,
            undeducted: false,
        });
    }

    // Addons
    for addon_input in addons {
        let addon_qty = addon_input.quantity.max(1) as f64;

        let (addon_name, name_translations, default_price, addon_type): (
            String,
            serde_json::Value,
            i32,
            String,
        ) = sqlx::query_as(
            "SELECT a.name, a.name_translations,
                    COALESCE(bao.price_override, a.default_price) AS default_price,
                    a.type
             FROM addon_items a
             LEFT JOIN branch_addon_overrides bao
                    ON bao.addon_item_id = a.id AND bao.branch_id = $2
             WHERE a.id = $1",
        )
        .bind(addon_input.addon_item_id)
        .bind(branch_id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| {
            AppError::NotFound(format!("Addon {} not found", addon_input.addon_item_id))
        })?;

        resolved_addons.push(ResolvedAddon {
            addon_item_id: addon_input.addon_item_id,
            addon_name: addon_name.clone(),
            name_translations: name_translations.clone(),
            unit_price: default_price,
            quantity: addon_input.quantity.max(1),
            is_swap: false,
            has_ingredients: false,
            swap_over: None,
        });

        let addon_rows: Vec<(Option<Uuid>, f64, String, String)> = sqlx::query_as(
            // Ordered like the `/addon-items` payload the POS picks `.first()` from, so a
            // swap option with several lines (lint F7) resolves the same on both sides.
            "SELECT org_ingredient_id, quantity_used::float8, ingredient_name, ingredient_unit
             FROM addon_item_ingredients WHERE addon_item_id = $1
             ORDER BY ingredient_name, org_ingredient_id",
        )
        .bind(addon_input.addon_item_id)
        .fetch_all(pool)
        .await?;
        let addon_rows = prefer_sized_option_lines(
            pool,
            addon_input.addon_item_id,
            size_label.as_deref(),
            addon_rows,
        )
        .await?;

        let swap_row = load_swap_rows(pool, &[addon_input.addon_item_id]).await?;
        let target = swap_row.first().and_then(|(_, t, eff, cid, slug)| {
            swap_target(Some(t), eff.as_deref(), *cid, slug.as_deref())
        });

        if let Some(target) = target {
            let cat = target.slug.as_str();
            if !swap_slugs.iter().any(|s| s == cat) {
                swap_slugs.push(cat.to_string());
            }
            let base_ing_id = deductions
                .iter()
                .find(|d| d.source == "drink_recipe" && d.category == cat)
                .and_then(|d| d.org_ingredient_id);

            // The replacement: an explicit swap group names it on the option
            // (`replaces_ingredient_id`); otherwise the option's first recipe line.
            let replacement: Option<(Option<Uuid>, String, String)> = match (
                target.category_id,
                explicit_replacement(pool, addon_input.addon_item_id).await?,
            ) {
                (Some(_), Some(ing)) => Some(
                    addon_rows
                        .iter()
                        .find(|(id, _, _, _)| *id == Some(ing.0))
                        .map(|(id, _, n, u)| (*id, n.clone(), u.clone()))
                        .unwrap_or((Some(ing.0), ing.1, ing.2)),
                ),
                _ => addon_rows
                    .first()
                    .map(|(id, _, n, u)| (*id, n.clone(), u.clone())),
            };
            let addon_ing_id = replacement.as_ref().and_then(|r| r.0);

            let is_base =
                base_ing_id.is_some() && addon_ing_id.is_some() && base_ing_id == addon_ing_id;

            if is_base {
                if let Some(last) = resolved_addons.last_mut() {
                    last.unit_price = 0;
                    last.is_swap = true;
                }
            } else if let Some((repl_id, repl_name, repl_unit)) = replacement {
                let base_addon: Option<(Option<i32>, String)> = if let Some(base_id) = base_ing_id {
                    sqlx::query_as(
                        // The swap is charged above the DEFAULT option: the one carrying
                        // the recipe's ingredient, preferring the chosen option's own
                        // group, then in the group's display order (sort, name, id) —
                        // the POS's rule. Not MAX over every candidate, which disagreed
                        // with the till whenever two options shared the base ingredient.
                        // Candidates share the family: the explicit swap category when
                        // the chosen group has one, else the legacy type.
                        "SELECT COALESCE(bao.price_override, a.default_price), a.name
                         FROM addon_items a
                         LEFT JOIN modifier_options mo ON mo.id = a.id
                         LEFT JOIN modifier_groups mg ON mg.id = mo.group_id
                         LEFT JOIN modifier_options chosen ON chosen.id = $4
                         LEFT JOIN branch_addon_overrides bao
                                ON bao.addon_item_id = a.id AND bao.branch_id = $3
                         WHERE (EXISTS (SELECT 1 FROM addon_item_ingredients i
                                         WHERE i.addon_item_id = a.id AND i.org_ingredient_id = $1)
                                OR mo.replaces_ingredient_id = $1)
                           AND CASE WHEN $5::uuid IS NULL THEN a.type = $2
                                    ELSE mg.swap_category_id = $5 END
                         ORDER BY (mo.group_id IS NOT NULL AND mo.group_id = chosen.group_id) DESC,
                                  a.is_active DESC, mo.sort NULLS LAST, a.name, a.id
                         LIMIT 1",
                    )
                    .bind(base_id)
                    .bind(addon_type.as_str())
                    .bind(branch_id)
                    .bind(addon_input.addon_item_id)
                    .bind(target.category_id)
                    .fetch_optional(pool)
                    .await?
                } else {
                    None
                };
                let base_addon_price = base_addon.as_ref().and_then(|b| b.0).unwrap_or(0);

                let new_price = (default_price - base_addon_price).max(0);
                if let Some(last) = resolved_addons.last_mut() {
                    last.unit_price = new_price;
                    last.is_swap = true;
                    last.swap_over = base_addon.map(|b| b.1);
                }

                let mut swapped = false;
                for ded in deductions.iter_mut() {
                    if ded.source == "drink_recipe" && ded.category == cat {
                        ded.note = Some(format!("swapped from {}", ded.ingredient_name));
                        // Convert the recipe quantity into the replacement ingredient's
                        // base unit (g↔kg / ml↔l) BEFORE swapping the unit — otherwise
                        // the raw quantity is mis-deducted by up to 1000× and COGS is
                        // inflated. Mirrors the direct-item path in handlers.rs (V19).
                        match crate::units::convert(ded.quantity, &ded.unit, &repl_unit) {
                            Ok(q) => {
                                ded.quantity = q;
                                ded.org_ingredient_id = repl_id;
                            }
                            Err(_) => {
                                tracing::warn!(
                                    from_unit = %ded.unit, to_unit = %repl_unit, addon = %addon_name,
                                    "addon swap across incompatible unit families; inventory not deducted"
                                );
                                ded.org_ingredient_id = None;
                                ded.undeducted = true;
                                warnings.push(ResolveWarning {
                                    rule: "unit_conversion".into(),
                                    message: format!(
                                        "\"{addon_name}\" swaps {} ({}) for {repl_name} ({repl_unit}): incompatible units, nothing is deducted",
                                        ded.ingredient_name, ded.unit
                                    ),
                                });
                            }
                        }
                        ded.ingredient_name = repl_name.clone();
                        ded.unit = repl_unit.clone();
                        ded.source = format!("addon_swap:{}", addon_name);
                        swapped = true;
                    }
                }
                if !swapped {
                    tracing::warn!(addon_name = %addon_name, cat = %cat, "Addon swap failed");
                    warnings.push(ResolveWarning {
                        rule: "swap_failed".into(),
                        message: format!(
                            "\"{addon_name}\" swaps the drink's {cat} but the recipe has no {cat} line: nothing is swapped"
                        ),
                    });
                }
            }
            continue;
        }

        // Additive addon: it carries its own cost iff it has ingredient rows.
        if let Some(last) = resolved_addons.last_mut() {
            last.has_ingredients = !addon_rows.is_empty();
        }
        for (ing_id, qty, name, unit) in addon_rows {
            // The addon's OWN ingredient category, not "general": an extra shot
            // is a coffee_bean and an extra milk is a milk, and the pass below
            // needs to know that to make them follow the drink's choice.
            let category: String = match ing_id {
                Some(id) => sqlx::query_scalar(
                    "SELECT c.slug FROM org_ingredients i \
                     JOIN ingredient_categories c ON c.id = i.category_id WHERE i.id = $1",
                )
                .bind(id)
                .fetch_optional(pool)
                .await?
                .unwrap_or_else(|| "general".to_string()),
                None => "general".to_string(),
            };
            deductions.push(InventoryDeduction {
                org_ingredient_id: ing_id,
                ingredient_name: name,
                unit,
                quantity: qty * line_quantity as f64 * addon_qty,
                source: "addon".into(),
                category,
                addon_item_id: Some(addon_input.addon_item_id),
                optional_field_id: None,
                note: None,
                undeducted: false,
            });
        }
    }

    // An ADDITIVE addon in a swap family follows the drink's own choice: an
    // extra shot on a decaf latte is a decaf shot, and extra milk on an oat
    // latte is oat. Without this the addon keeps whatever bean the catalog
    // happened to name, so the sale charges for one thing and deducts another.
    // Milk and coffee always follow (as before); an explicit custom swap family
    // follows only on lines where one of its choices was made.
    //
    // A second pass, because the swaps above are applied as the addons are
    // walked — the line's final choice is only known once that loop is done.
    let mut follow_slugs: Vec<String> = vec!["milk".into(), "coffee_bean".into()];
    for s in swap_slugs {
        if !follow_slugs.contains(&s) {
            follow_slugs.push(s);
        }
    }
    for cat in follow_slugs.iter().map(String::as_str) {
        let chosen = deductions
            .iter()
            .find(|d| d.category == cat && d.source != "addon")
            .map(|d| {
                (
                    d.org_ingredient_id,
                    d.ingredient_name.clone(),
                    d.unit.clone(),
                )
            });
        let Some((id, name, unit)) = chosen else {
            continue;
        };
        for d in deductions.iter_mut() {
            if d.source == "addon" && d.category == cat && d.org_ingredient_id != id {
                // Convert first: the addon's quantity is in ITS unit, and the
                // chosen ingredient may be stocked in another (g vs ml).
                match crate::units::convert(d.quantity, &d.unit, &unit) {
                    Ok(q) => {
                        d.quantity = q;
                        d.org_ingredient_id = id;
                        d.ingredient_name = name.clone();
                        d.unit = unit.clone();
                        d.note = Some(format!("follows the chosen {name}"));
                    }
                    Err(_) => {
                        tracing::warn!(
                            from_unit = %d.unit, to_unit = %unit, addon = %d.ingredient_name,
                            "addon follow-the-drink across incompatible units; left as authored"
                        );
                        warnings.push(ResolveWarning {
                            rule: "unit_conversion".into(),
                            message: format!(
                                "{} ({}) should follow {name} ({unit}): incompatible units, deducted as authored",
                                d.ingredient_name, d.unit
                            ),
                        });
                    }
                }
            }
        }
    }

    // Optionals
    for &field_id in optional_field_ids {
        let row_result = sqlx::query_as::<
            _,
            (
                String,
                i32,
                Option<Uuid>,
                Option<String>,
                Option<String>,
                Option<f64>,
                Option<String>,
                serde_json::Value,
            ),
        >(
            r#"SELECT name, price, org_ingredient_id, ingredient_name, ingredient_unit,
                      quantity_used::float8, size_label::text, name_translations
               FROM menu_item_optional_fields
               WHERE id = $1 AND menu_item_id = $2 AND is_active = true"#,
        )
        .bind(field_id)
        .bind(menu_item_id)
        .fetch_optional(pool)
        .await?;

        let Some((
            fname,
            fprice,
            ing_id,
            ing_name,
            ing_unit,
            qty_used,
            field_size,
            name_translations,
        )) = row_result
        else {
            tracing::warn!(field_id = %field_id, "Optional field not found — skipping");
            warnings.push(ResolveWarning {
                rule: "optional_not_found".into(),
                message: format!("Optional field {field_id} is not an active option of this item: not charged nor deducted"),
            });
            continue;
        };

        if let Some(fs) = &field_size
            && size_label.as_deref() != Some(fs.as_str())
        {
            tracing::warn!(field_id = %field_id, "Optional field size mismatch — skipping");
            warnings.push(ResolveWarning {
                rule: "optional_size_mismatch".into(),
                message: format!("\"{fname}\" is only offered on size {fs}: skipped"),
            });
            continue;
        }

        if let (Some(ref name), Some(ref unit), Some(qty)) =
            (ing_name.clone(), ing_unit.clone(), qty_used)
        {
            deductions.push(InventoryDeduction {
                org_ingredient_id: ing_id,
                ingredient_name: name.clone(),
                unit: unit.clone(),
                quantity: qty * line_quantity as f64,
                source: "optional".into(),
                category: "general".into(),
                addon_item_id: None,
                optional_field_id: Some(field_id),
                note: None,
                undeducted: false,
            });
        }

        resolved_optionals.push(ResolvedOptional {
            optional_field_id: field_id,
            field_name: fname,
            name_translations,
            price: fprice,
            org_ingredient_id: ing_id,
            ingredient_name: ing_name,
            ingredient_unit: ing_unit,
            quantity_used: qty_used,
        });
    }

    let addon_line: i32 = resolved_addons
        .iter()
        .map(|a| a.unit_price * a.quantity)
        .sum();
    let optional_line: i32 = resolved_optionals.iter().map(|o| o.price).sum();

    Ok(MenuItemResolution {
        deductions,
        addons: resolved_addons,
        optionals: resolved_optionals,
        addon_line,
        optional_line,
        warnings,
    })
}

#[cfg(test)]
mod swap_family_tests {
    use super::collapse_swap_families;

    fn v(xs: &[&str]) -> Vec<Option<String>> {
        xs.iter().map(|s| Some(s.to_string())).collect()
    }

    #[test]
    fn one_of_each_family_keeps_everything() {
        assert_eq!(
            collapse_swap_families(&v(&["milk_type", "coffee_type", "extra", "extra"])),
            vec![true; 4]
        );
    }

    #[test]
    fn the_last_milk_and_the_last_coffee_win() {
        assert_eq!(
            collapse_swap_families(&v(&["milk_type", "extra", "milk_type"])),
            vec![false, true, true]
        );
        assert_eq!(
            collapse_swap_families(&v(&["coffee_type", "coffee_type"])),
            vec![false, true]
        );
    }
}
