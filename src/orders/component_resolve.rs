//! Shared menu-item configuration resolution (sizes, addons, optionals, inventory).
//! Used by standalone order lines and bundle component lines.

use crate::errors::AppError;
use crate::orders::catalog_view::Catalog;
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

/// Per-size option amounts (menu modeling B9): an option line whose `size_label`
/// equals the ordered size REPLACES the generic (NULL-size) line for the same
/// ingredient, and a sized line for an ingredient with no generic line is added.
/// With no sized lines — every old catalog — nothing changes. The rule is
/// madar-catalog's (`merge_sized_lines`); this is its shape over
/// `(ingredient, quantity, name, unit)` rows.
pub fn merge_sized_option_lines(
    generic: Vec<(Option<Uuid>, f64, String, String)>,
    sized: Vec<(Option<Uuid>, f64, String, String)>,
) -> Vec<(Option<Uuid>, f64, String, String)> {
    madar_catalog::merge_sized_lines(generic, sized, |l| l.0, |l| l.2.as_str())
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
    let mut catalog = Catalog::new(Some(branch_id));
    let ids: Vec<Uuid> = addons.iter().map(|a| a.addon_item_id).collect();
    catalog.ensure_on(pool, &[menu_item_id], &ids).await?;
    resolve_loaded(
        &catalog,
        menu_item_id,
        size_label,
        line_quantity,
        addons,
        optional_field_ids,
    )
}

/// The selection a line states, as madar-catalog reads it.
pub fn selection_of(
    size_label: Option<&str>,
    addons: &[AddonInput],
    optional_field_ids: &[Uuid],
) -> madar_catalog::Selection {
    madar_catalog::Selection {
        size_label: size_label.map(str::to_string),
        options: addons
            .iter()
            .map(|a| madar_catalog::Pick {
                id: a.addon_item_id.to_string(),
                quantity: i64::from(a.quantity),
            })
            .collect(),
        optionals: optional_field_ids.iter().map(Uuid::to_string).collect(),
    }
}

/// [`resolve_menu_item_configuration`] over an already loaded catalogue: the
/// line priced by madar-catalog, then its stock deductions from the same rows
/// and the rule's own decisions (which option swapped what, into which
/// ingredient) — a price and a deduction cannot disagree.
pub fn resolve_loaded(
    catalog: &Catalog,
    menu_item_id: Uuid,
    size_label: Option<String>,
    line_quantity: i32,
    addons: &[AddonInput],
    optional_field_ids: &[Uuid],
) -> Result<MenuItemResolution, AppError> {
    if line_quantity <= 0 {
        return Err(AppError::BadRequest("Quantity must be > 0".into()));
    }
    let loaded = catalog.item(menu_item_id);
    let view = catalog.view(menu_item_id);
    let size = size_label.as_deref();

    let mut deductions: Vec<InventoryDeduction> = Vec::new();
    let mut resolved_addons: Vec<ResolvedAddon> = Vec::new();
    let mut resolved_optionals: Vec<ResolvedOptional> = Vec::new();
    // Ingredient categories swapped by an explicit choice on this line.
    let mut swap_slugs: Vec<String> = Vec::new();
    let mut warnings: Vec<ResolveWarning> = Vec::new();

    // Base drink recipe: the line's size, else the item's first size.
    let recipe_size = madar_catalog::recipe_size(&view.item, size);
    for r in loaded
        .map(|i| i.recipe.as_slice())
        .unwrap_or_default()
        .iter()
        .filter(|r| r.size_label == recipe_size)
    {
        // A recipe line with no ingredient has no category; the order path has
        // always refused such a line (it could not read the category).
        let Some(category) = r.category.clone() else {
            return Err(AppError::Db(sqlx::Error::ColumnDecode {
                index: "category".into(),
                source: "unexpected null; try decoding as an `Option`".into(),
            }));
        };
        deductions.push(InventoryDeduction {
            org_ingredient_id: r.ingredient_id,
            ingredient_name: r.name.clone(),
            unit: r.unit.clone(),
            quantity: r.quantity * line_quantity as f64,
            source: "drink_recipe".into(),
            category,
            addon_item_id: None,
            optional_field_id: None,
            note: None,
            undeducted: false,
        });
    }

    let priced =
        madar_catalog::price_options(&view, &selection_of(size, addons, optional_field_ids))
            .map_err(|e| match e {
                madar_catalog::PriceError::UnknownOption { id } => {
                    AppError::NotFound(format!("Addon {id} not found"))
                }
                madar_catalog::PriceError::NoPricedSize => {
                    AppError::BadRequest(format!("Menu item {menu_item_id} has no priced size"))
                }
            })?;
    if priced
        .notes
        .iter()
        .any(|n| matches!(n, madar_catalog::Note::CollapsedFamily))
    {
        tracing::warn!("order line carried more than one choice of a swap family; kept the last");
    }

    for p in &priced.options {
        let addon_id: Uuid = p.id.parse().map_err(|_| AppError::Internal)?;
        let Some(option) = catalog.option(addon_id) else {
            return Err(AppError::NotFound(format!("Addon {addon_id} not found")));
        };
        let addon_name = option.name.clone();
        let addon_qty = p.quantity as f64;
        resolved_addons.push(ResolvedAddon {
            addon_item_id: addon_id,
            addon_name: addon_name.clone(),
            name_translations: option.name_translations.clone(),
            unit_price: p.unit_price as i32,
            quantity: p.quantity as i32,
            is_swap: p.is_swap,
            has_ingredients: p.has_ingredients,
            swap_over: p.over.as_ref().map(|o| o.name.clone()),
        });

        if let Some(target) = &p.target {
            let cat = target.slug.as_str();
            if !swap_slugs.iter().any(|s| s == cat) {
                swap_slugs.push(cat.to_string());
            }
            // The recipe's own choice changes nothing; an option with nothing
            // to swap in is charged and changes nothing either.
            let Some(repl) = p.replacement.as_ref().filter(|_| !p.is_base) else {
                continue;
            };
            let repl_id: Option<Uuid> = repl.id.as_deref().and_then(|i| i.parse().ok());
            let (repl_name, repl_unit) = (repl.name.clone(), repl.unit.clone());
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
            continue;
        }

        // Additive addon: it carries its own cost iff it has ingredient rows.
        // Each line keeps the addon's OWN ingredient category, not "general":
        // an extra shot is a coffee_bean and an extra milk is a milk, and the
        // pass below needs to know that to make them follow the drink's choice.
        for line in option.lines_for(size) {
            deductions.push(InventoryDeduction {
                org_ingredient_id: line.ingredient_id,
                ingredient_name: line.name,
                unit: line.unit,
                quantity: line.quantity * line_quantity as f64 * addon_qty,
                source: "addon".into(),
                category: line.category,
                addon_item_id: Some(addon_id),
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

    // Optionals, as madar-catalog kept them (an inactive or foreign field is
    // skipped, one offered on another size only too).
    let optional_row =
        |id: &str| loaded.and_then(|i| i.optionals.iter().find(|o| o.id.to_string() == id));
    for kept in &priced.optionals {
        let Some(row) = optional_row(&kept.id) else {
            continue;
        };
        if let (Some(name), Some(unit), Some(qty)) = (
            row.ingredient_name.clone(),
            row.ingredient_unit.clone(),
            row.quantity_used,
        ) {
            deductions.push(InventoryDeduction {
                org_ingredient_id: row.ingredient_id,
                ingredient_name: name,
                unit,
                quantity: qty * line_quantity as f64,
                source: "optional".into(),
                category: "general".into(),
                addon_item_id: None,
                optional_field_id: Some(row.id),
                note: None,
                undeducted: false,
            });
        }
        resolved_optionals.push(ResolvedOptional {
            optional_field_id: row.id,
            field_name: row.name.clone(),
            name_translations: row.name_translations.clone(),
            price: row.price,
            org_ingredient_id: row.ingredient_id,
            ingredient_name: row.ingredient_name.clone(),
            ingredient_unit: row.ingredient_unit.clone(),
            quantity_used: row.quantity_used,
        });
    }
    for note in &priced.notes {
        match note {
            madar_catalog::Note::OptionalNotFound { id } => {
                tracing::warn!(field_id = %id, "Optional field not found — skipping");
                warnings.push(ResolveWarning {
                    rule: "optional_not_found".into(),
                    message: format!("Optional field {id} is not an active option of this item: not charged nor deducted"),
                });
            }
            madar_catalog::Note::OptionalSizeMismatch { id, size_label } => {
                tracing::warn!(field_id = %id, "Optional field size mismatch — skipping");
                let fname = optional_row(id).map(|o| o.name.clone()).unwrap_or_default();
                warnings.push(ResolveWarning {
                    rule: "optional_size_mismatch".into(),
                    message: format!("\"{fname}\" is only offered on size {size_label}: skipped"),
                });
            }
            madar_catalog::Note::CollapsedFamily => {}
        }
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
