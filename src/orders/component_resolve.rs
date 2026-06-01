//! Shared menu-item configuration resolution (sizes, addons, optionals, inventory).
//! Used by standalone order lines and bundle component lines.

use sqlx::PgPool;
use uuid::Uuid;
use crate::errors::AppError;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Deserialize, Serialize, Clone, ToSchema)]
pub struct AddonInput {
    pub addon_item_id: Uuid,
    #[serde(default = "default_qty")]
    pub quantity: i32,
}

pub fn default_qty() -> i32 { 1 }

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
    pub ingredient_name:   String,
    pub unit:              String,
    pub quantity:          f64,
    pub source:            String,
    pub category:          String,
}

#[derive(Clone)]
pub struct ResolvedAddon {
    pub addon_item_id: Uuid,
    pub addon_name:    String,
    pub name_translations: serde_json::Value,
    pub unit_price:    i32,
    pub quantity:      i32,
}

#[derive(Clone)]
pub struct ResolvedOptional {
    pub optional_field_id: Uuid,
    pub field_name:        String,
    pub price:             i32,
    pub org_ingredient_id: Option<Uuid>,
    pub ingredient_name:   Option<String>,
    pub ingredient_unit:   Option<String>,
    pub quantity_used:     Option<f64>,
}

pub struct MenuItemResolution {
    pub deductions:        Vec<InventoryDeduction>,
    pub addons:            Vec<ResolvedAddon>,
    pub optionals:         Vec<ResolvedOptional>,
    pub addon_line:        i32,
    pub optional_line:     i32,
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
) -> Result<MenuItemResolution, AppError> {
    if line_quantity <= 0 {
        return Err(AppError::BadRequest("Quantity must be > 0".into()));
    }

    let mut deductions: Vec<InventoryDeduction> = Vec::new();
    let mut resolved_addons: Vec<ResolvedAddon> = Vec::new();
    let mut resolved_optionals: Vec<ResolvedOptional> = Vec::new();

    // Base drink recipe
    let recipe_rows: Vec<(Option<Uuid>, f64, String, String, String)> =
        if let Some(ref size) = size_label {
            sqlx::query_as(
                r#"SELECT r.org_ingredient_id, r.quantity_used::float8,
                          r.ingredient_name, r.ingredient_unit,
                          COALESCE(i.category, 'general') as category
                   FROM   menu_item_recipes r
                   LEFT JOIN org_ingredients i ON i.id = r.org_ingredient_id
                   WHERE  r.menu_item_id = $1 AND r.size_label = $2::item_size"#,
            )
            .bind(menu_item_id)
            .bind(size)
            .fetch_all(pool)
            .await?
        } else {
            sqlx::query_as(
                r#"SELECT r.org_ingredient_id, r.quantity_used::float8,
                          r.ingredient_name, r.ingredient_unit,
                          COALESCE(i.category, 'general') as category
                   FROM   menu_item_recipes r
                   LEFT JOIN org_ingredients i ON i.id = r.org_ingredient_id
                   WHERE  r.menu_item_id = $1
                     AND  r.size_label = COALESCE(
                         (SELECT size_label FROM menu_item_recipes WHERE menu_item_id = $1 LIMIT 1),
                         'one_size'::item_size
                     )"#,
            )
            .bind(menu_item_id)
            .fetch_all(pool)
            .await?
        };

    for (ing_id, qty, name, unit, category) in recipe_rows {
        deductions.push(InventoryDeduction {
            org_ingredient_id: ing_id,
            ingredient_name:   name,
            unit,
            quantity:          qty * line_quantity as f64,
            source:            "drink_recipe".into(),
            category,
        });
    }

    // Addons
    for addon_input in addons {
        let addon_qty = addon_input.quantity.max(1) as f64;

        let (addon_name, name_translations, default_price, addon_type): (String, serde_json::Value, i32, String) = sqlx::query_as(
            "SELECT name, name_translations, default_price, type FROM addon_items WHERE id = $1",
        )
        .bind(addon_input.addon_item_id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Addon {} not found", addon_input.addon_item_id)))?;

        resolved_addons.push(ResolvedAddon {
            addon_item_id: addon_input.addon_item_id,
            addon_name:    addon_name.clone(),
            name_translations: name_translations.clone(),
            unit_price:    default_price,
            quantity:      addon_input.quantity.max(1),
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

            let is_base = base_ing_id.is_some()
                && addon_ing_id.is_some()
                && base_ing_id == addon_ing_id;

            if is_base {
                if let Some(last) = resolved_addons.last_mut() {
                    last.unit_price = 0;
                }
            } else if let Some((repl_id, _, repl_name, repl_unit)) = addon_rows.first() {
                let base_addon_price: i32 = if let Some(base_id) = base_ing_id {
                    sqlx::query_scalar(
                        "SELECT COALESCE(MAX(a.default_price), 0)
                         FROM addon_items a
                         JOIN addon_item_ingredients i ON i.addon_item_id = a.id
                         WHERE i.org_ingredient_id = $1 AND a.type = $2",
                    )
                    .bind(base_id)
                    .bind(addon_type.as_str())
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
                }

                let mut swapped = false;
                for ded in deductions.iter_mut() {
                    if ded.source == "drink_recipe" && ded.category == cat {
                        ded.org_ingredient_id = *repl_id;
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

        for (ing_id, qty, name, unit) in addon_rows {
            deductions.push(InventoryDeduction {
                org_ingredient_id: ing_id,
                ingredient_name:   name,
                unit,
                quantity:          qty * line_quantity as f64 * addon_qty,
                source:            "addon".into(),
                category:          "general".into(),
            });
        }
    }

    // Optionals
    for &field_id in optional_field_ids {
        let row_result = sqlx::query_as::<_, (
            String,
            i32,
            Option<Uuid>,
            Option<String>,
            Option<String>,
            Option<f64>,
            Option<String>,
        )>(
            r#"SELECT name, price, org_ingredient_id, ingredient_name, ingredient_unit,
                      quantity_used::float8, size_label::text
               FROM menu_item_optional_fields
               WHERE id = $1 AND menu_item_id = $2 AND is_active = true"#,
        )
        .bind(field_id)
        .bind(menu_item_id)
        .fetch_optional(pool)
        .await?;

        let Some((fname, fprice, ing_id, ing_name, ing_unit, qty_used, field_size)) = row_result
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
                ingredient_name:   name.clone(),
                unit:              unit.clone(),
                quantity:          qty * line_quantity as f64,
                source:            "optional".into(),
                category:          "general".into(),
            });
        }

        resolved_optionals.push(ResolvedOptional {
            optional_field_id: field_id,
            field_name:        fname,
            price:             fprice,
            org_ingredient_id: ing_id,
            ingredient_name:   ing_name,
            ingredient_unit:   ing_unit,
            quantity_used:     qty_used,
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
