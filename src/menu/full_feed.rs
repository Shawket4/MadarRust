//! The per-item parts of `GET /menu-items?full=true`, loaded for every item of
//! the list in a fixed number of queries.
//!
//! The list used to run eight queries PER ITEM (sizes, all sizes, two branch
//! size-override reads, add-on slots, optional fields, recipes, the add-on
//! allowlist): ~555 statements for a 69-item menu on the prod copy, most of the
//! build's database time and a good share of its app CPU. Each part here is
//! one query, and each keeps the exact rows and the exact per-item order the
//! per-item query returned, because old tills parse this body and the default
//! shape must stay byte-identical:
//!
//! * sizes, all sizes, branch size prices and optional fields have a full
//!   ORDER BY in the old per-item query; the batch orders by item first, then
//!   by the same keys.
//! * recipes had NO ORDER BY (their order is the plan's: index order on the
//!   prod catalogue, insertion order on a small one), and add-on slots and the
//!   allowlist sort on `created_at`, which the compat views define as `now()`
//!   (ties everywhere, so their order is whatever the plan feeds the sort).
//!   These run the SAME per-item query, once per item inside one statement (a
//!   LATERAL / ARRAY subplan), so each item's rows come out of the per-item
//!   plan they always did.
//!
//! `tests/menu_full_feed_tests.rs` pins the batch against the per-item queries.
use std::collections::HashMap;

use bigdecimal::ToPrimitive;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::errors::AppError;
use crate::menu::handlers::{AddonSlot, ItemSize, MenuItemRecipe, OptionalField};

/// Every per-item part of a `full=true` row, keyed by menu item id.
#[derive(Default)]
pub struct FullParts {
    pub sizes: HashMap<Uuid, Vec<ItemSize>>,
    pub all_sizes: HashMap<Uuid, Vec<ItemSize>>,
    pub addon_slots: HashMap<Uuid, Vec<AddonSlot>>,
    pub optional_fields: HashMap<Uuid, Vec<OptionalField>>,
    pub recipes: HashMap<Uuid, Vec<MenuItemRecipe>>,
    pub allowed_addon_ids: HashMap<Uuid, Vec<Uuid>>,
}

impl FullParts {
    /// Load every part for `ids`, with the branch's size prices overlaid when
    /// `branch` is set (what `apply_branch_size_overrides` did per item).
    pub async fn load(
        conn: &mut PgConnection,
        ids: &[Uuid],
        branch: Option<Uuid>,
    ) -> Result<Self, AppError> {
        let mut out = Self::default();
        if ids.is_empty() {
            return Ok(out);
        }

        for r in sqlx::query_as!(
            ItemSize,
            r#"SELECT id AS "id!", menu_item_id AS "menu_item_id!", label::text AS "label!",
                      price_override AS "price_override!", is_active AS "is_active!"
                 FROM compat.item_sizes
                WHERE menu_item_id = ANY($1)
                ORDER BY menu_item_id, label ASC"#,
            ids
        )
        .fetch_all(&mut *conn)
        .await?
        {
            out.sizes.entry(r.menu_item_id).or_default().push(r);
        }

        for r in sqlx::query_as!(
            ItemSize,
            r#"SELECT id, menu_item_id, label::text AS "label!", price AS price_override, is_active
                 FROM menu_item_sizes
                WHERE menu_item_id = ANY($1)
                ORDER BY menu_item_id, sort ASC, label ASC"#,
            ids
        )
        .fetch_all(&mut *conn)
        .await?
        {
            out.all_sizes.entry(r.menu_item_id).or_default().push(r);
        }

        if let Some(branch) = branch {
            // Per item: label → branch price, the last row of a label winning
            // (the per-item map was collected in size_label order too).
            let mut prices: HashMap<Uuid, HashMap<String, i32>> = HashMap::new();
            for r in sqlx::query!(
                r#"SELECT menu_item_id AS "menu_item_id!", size_label::text AS "size_label!",
                          price_override AS "price_override!"
                     FROM compat.branch_menu_size_overrides
                    WHERE branch_id = $1 AND menu_item_id = ANY($2)
                    ORDER BY menu_item_id, size_label"#,
                branch,
                ids
            )
            .fetch_all(&mut *conn)
            .await?
            {
                prices
                    .entry(r.menu_item_id)
                    .or_default()
                    .insert(r.size_label, r.price_override);
            }
            for map in [&mut out.sizes, &mut out.all_sizes] {
                for (item, sizes) in map.iter_mut() {
                    if let Some(p) = prices.get(item) {
                        for s in sizes.iter_mut() {
                            if let Some(price) = p.get(&s.label) {
                                s.price_override = *price;
                            }
                        }
                    }
                }
            }
        }

        // The per-item slot query, once per item in one statement (see the
        // module doc: its sort key is `now()`).
        for r in sqlx::query!(
            r#"SELECT s.id AS "id!", s.menu_item_id AS "menu_item_id!",
                      s.addon_type AS "addon_type!", s.label,
                      s.label_translations AS "label_translations!",
                      s.is_required AS "is_required!", s.min_selections AS "min_selections!",
                      s.max_selections, s.created_at AS "created_at!"
                 FROM unnest($1::uuid[]) AS m(id)
                 CROSS JOIN LATERAL (
                      SELECT id, menu_item_id, addon_type, label, label_translations, is_required,
                             min_selections, max_selections, created_at
                        FROM compat.menu_item_addon_slots
                       WHERE menu_item_id = m.id
                       ORDER BY created_at ASC) s"#,
            ids
        )
        .fetch_all(&mut *conn)
        .await?
        {
            out.addon_slots
                .entry(r.menu_item_id)
                .or_default()
                .push(AddonSlot {
                    id: r.id,
                    menu_item_id: r.menu_item_id,
                    addon_type: r.addon_type,
                    label: r.label,
                    label_translations: r.label_translations,
                    is_required: r.is_required,
                    min_selections: r.min_selections,
                    max_selections: r.max_selections,
                    created_at: r.created_at,
                });
        }

        for r in sqlx::query!(
            r#"SELECT id AS "id!", menu_item_id AS "menu_item_id!", name AS "name!",
                      name_translations AS "name_translations!", price AS "price!",
                      org_ingredient_id, ingredient_name, ingredient_unit,
                      quantity_used AS "quantity_used: sqlx::types::BigDecimal",
                      size_label::text AS size_label,
                      is_active AS "is_active!", created_at AS "created_at!",
                      updated_at AS "updated_at!"
                 FROM compat.menu_item_optional_fields
                WHERE menu_item_id = ANY($1) AND is_active = true
                ORDER BY menu_item_id, name ASC"#,
            ids
        )
        .fetch_all(&mut *conn)
        .await?
        {
            out.optional_fields
                .entry(r.menu_item_id)
                .or_default()
                .push(OptionalField {
                    id: r.id,
                    menu_item_id: r.menu_item_id,
                    name: r.name,
                    name_translations: r.name_translations,
                    price: r.price,
                    org_ingredient_id: r.org_ingredient_id,
                    ingredient_name: r.ingredient_name,
                    ingredient_unit: r.ingredient_unit,
                    quantity_used: r.quantity_used,
                    size_label: r.size_label,
                    is_active: r.is_active,
                    created_at: r.created_at,
                    updated_at: r.updated_at,
                });
        }

        // The per-item recipe query, verbatim, once per item in one statement:
        // it has no ORDER BY, so its order is its plan's (the size and recipe
        // indexes on a large catalogue, insertion order on a small one).
        // `OFFSET 0` keeps the subquery from being flattened into the outer
        // join, so each item's rows come out of the same per-item plan.
        for r in sqlx::query!(
            r#"SELECT m.id AS "menu_item_id!", r.org_ingredient_id,
                      r.quantity_used AS "quantity_used!: sqlx::types::BigDecimal",
                      r.ingredient_name AS "ingredient_name!", r.ingredient_unit AS "ingredient_unit!",
                      r.category AS "category!", r.size_label AS "size_label!", r.yield_pct
                 FROM unnest($1::uuid[]) AS m(id)
                 CROSS JOIN LATERAL (
                      SELECT r.org_ingredient_id, r.quantity_used,
                             r.ingredient_name, r.ingredient_unit,
                             (SELECT ic.slug FROM ingredient_categories ic WHERE ic.id = i.category_id) as category,
                             r.size_label::text AS size_label,
                             i.yield_pct::float8 AS yield_pct
                        FROM compat.menu_item_recipes r
                        LEFT JOIN org_ingredients i ON i.id = r.org_ingredient_id
                       WHERE r.menu_item_id = m.id
                      OFFSET 0) r"#,
            ids
        )
        .fetch_all(&mut *conn)
        .await?
        {
            let stored = r.quantity_used.to_f64().unwrap_or(0.0);
            out.recipes
                .entry(r.menu_item_id)
                .or_default()
                .push(MenuItemRecipe {
                    org_ingredient_id: r.org_ingredient_id,
                    quantity_used: r.quantity_used,
                    ingredient_name: r.ingredient_name,
                    ingredient_unit: r.ingredient_unit,
                    category: r.category,
                    size_label: r.size_label,
                    usable_quantity: madar_units::usable_qty(stored, r.yield_pct),
                    yield_pct: r.yield_pct,
                });
        }

        // The per-item allowlist query as a subplan per item (module doc).
        for r in sqlx::query!(
            r#"SELECT m.id AS "id!",
                      ARRAY(SELECT addon_item_id FROM compat.menu_item_allowed_addons
                             WHERE menu_item_id = m.id
                             ORDER BY sort_order ASC, created_at ASC) AS "ids!: Vec<Uuid>"
                 FROM unnest($1::uuid[]) AS m(id)"#,
            ids
        )
        .fetch_all(&mut *conn)
        .await?
        {
            if !r.ids.is_empty() {
                out.allowed_addon_ids.insert(r.id, r.ids);
            }
        }
        Ok(out)
    }
}

/// Every item's `default_milk_addon_id` in one query: the per-row subquery of
/// the list query (which re-walked every milk add-on's recipe lines for every
/// item), batched. First milk_type add-on carrying one of the item's recipe
/// ingredients, in the group's display order, then name, then id.
pub async fn default_milk_ids(
    conn: &mut PgConnection,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, String>, AppError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(sqlx::query!(
        r#"SELECT DISTINCT ON (r.menu_item_id) r.menu_item_id AS "menu_item_id!", a.id::text AS "addon_id!"
             FROM compat.menu_item_recipes r
             JOIN compat.addon_item_ingredients ai ON ai.org_ingredient_id = r.org_ingredient_id
             JOIN compat.addon_items a ON a.id = ai.addon_item_id
            WHERE r.menu_item_id = ANY($1) AND a.type = 'milk_type'
            ORDER BY r.menu_item_id,
                     (SELECT mo.sort FROM modifier_options mo WHERE mo.id = a.id) NULLS LAST,
                     a.name, a.id"#,
        ids
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|r| (r.menu_item_id, r.addon_id))
    .collect())
}
