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
}

/// A drink has ONE milk and ONE coffee: a swap-family addon (`milk_type` /
/// `coffee_type`) REPLACES the recipe's ingredient, so two of one family on a
/// line cannot be made, costed or deducted — the second swap silently
/// overwrote the first while both were charged. Tills already in the field sent
/// such lines (and replay them from their outbox), so the line is not refused —
/// the LAST choice of each family wins, at quantity 1, which is what the till
/// shows the customer after picking a second milk.
pub(crate) fn collapse_swap_families(types: &[Option<String>]) -> Vec<bool> {
    let mut keep = vec![true; types.len()];
    for family in ["milk_type", "coffee_type"] {
        let hits: Vec<usize> = (0..types.len())
            .filter(|&i| types[i].as_deref() == Some(family))
            .collect();
        if let Some((_, earlier)) = hits.split_last() {
            for &i in earlier {
                keep[i] = false;
            }
        }
    }
    keep
}

async fn one_choice_per_swap_family(
    pool: &PgPool,
    addons: &[AddonInput],
) -> Result<Vec<AddonInput>, AppError> {
    if addons.len() < 2 {
        return Ok(addons.to_vec());
    }
    let ids: Vec<Uuid> = addons.iter().map(|a| a.addon_item_id).collect();
    let rows: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, type FROM addon_items WHERE id = ANY($1)")
            .bind(&ids)
            .fetch_all(pool)
            .await?;
    let types: Vec<Option<String>> = ids
        .iter()
        .map(|id| rows.iter().find(|(r, _)| r == id).map(|(_, t)| t.clone()))
        .collect();
    let keep = collapse_swap_families(&types);
    if keep.iter().any(|k| !k) {
        tracing::warn!("order line carried more than one milk/coffee swap; kept the last");
    }
    Ok(addons
        .iter()
        .zip(&types)
        .zip(keep)
        .filter(|(_, k)| *k)
        .map(|((a, t), _)| {
            let mut a = a.clone();
            if matches!(t.as_deref(), Some("milk_type" | "coffee_type")) {
                a.quantity = 1;
            }
            a
        })
        .collect())
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
                         (SELECT size_label FROM menu_item_recipes WHERE menu_item_id = $1 ORDER BY size_label LIMIT 1),
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
        });

        let addon_rows: Vec<(Option<Uuid>, f64, String, String)> = sqlx::query_as(
            "SELECT org_ingredient_id, quantity_used::float8, ingredient_name, ingredient_unit
             FROM addon_item_ingredients WHERE addon_item_id = $1",
        )
        .bind(addon_input.addon_item_id)
        .fetch_all(pool)
        .await?;

        let target_category = match addon_type.as_str() {
            "milk_type" => Some("milk"),
            "coffee_type" => Some("coffee_bean"),
            _ => None,
        };

        if let Some(cat) = target_category {
            let base_ing_id = deductions
                .iter()
                .find(|d| d.source == "drink_recipe" && d.category == cat)
                .and_then(|d| d.org_ingredient_id);

            let addon_ing_id = addon_rows.first().and_then(|(id, _, _, _)| *id);

            let is_base =
                base_ing_id.is_some() && addon_ing_id.is_some() && base_ing_id == addon_ing_id;

            if is_base {
                if let Some(last) = resolved_addons.last_mut() {
                    last.unit_price = 0;
                    last.is_swap = true;
                }
            } else if let Some((repl_id, _, repl_name, repl_unit)) = addon_rows.first() {
                let base_addon_price: i32 = if let Some(base_id) = base_ing_id {
                    sqlx::query_scalar(
                        "SELECT COALESCE(MAX(COALESCE(bao.price_override, a.default_price)), 0)
                         FROM addon_items a
                         JOIN addon_item_ingredients i ON i.addon_item_id = a.id
                         LEFT JOIN branch_addon_overrides bao
                                ON bao.addon_item_id = a.id AND bao.branch_id = $3
                         WHERE i.org_ingredient_id = $1 AND a.type = $2",
                    )
                    .bind(base_id)
                    .bind(addon_type.as_str())
                    .bind(branch_id)
                    .fetch_optional(pool)
                    .await?
                    .flatten()
                    .unwrap_or(0)
                } else {
                    0
                };

                let new_price = (default_price - base_addon_price).max(0);
                if let Some(last) = resolved_addons.last_mut() {
                    last.unit_price = new_price;
                    last.is_swap = true;
                }

                let mut swapped = false;
                for ded in deductions.iter_mut() {
                    if ded.source == "drink_recipe" && ded.category == cat {
                        // Convert the recipe quantity into the replacement ingredient's
                        // base unit (g↔kg / ml↔l) BEFORE swapping the unit — otherwise
                        // the raw quantity is mis-deducted by up to 1000× and COGS is
                        // inflated. Mirrors the direct-item path in handlers.rs (V19).
                        match crate::units::convert(ded.quantity, &ded.unit, repl_unit) {
                            Ok(q) => {
                                ded.quantity = q;
                                ded.org_ingredient_id = *repl_id;
                            }
                            Err(_) => {
                                tracing::warn!(
                                    from_unit = %ded.unit, to_unit = %repl_unit, addon = %addon_name,
                                    "addon swap across incompatible unit families; inventory not deducted"
                                );
                                ded.org_ingredient_id = None;
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
                }
            }
            continue;
        }

        // Additive addon: it carries its own cost iff it has ingredient rows.
        if let Some(last) = resolved_addons.last_mut() {
            last.has_ingredients = !addon_rows.is_empty();
        }
        for (ing_id, qty, name, unit) in addon_rows {
            deductions.push(InventoryDeduction {
                org_ingredient_id: ing_id,
                ingredient_name: name,
                unit,
                quantity: qty * line_quantity as f64 * addon_qty,
                source: "addon".into(),
                category: "general".into(),
                addon_item_id: Some(addon_input.addon_item_id),
                optional_field_id: None,
            });
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
            continue;
        };

        if let Some(fs) = &field_size
            && size_label.as_deref() != Some(fs.as_str())
        {
            tracing::warn!(field_id = %field_id, "Optional field size mismatch — skipping");
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
