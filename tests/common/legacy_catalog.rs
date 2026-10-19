//! Seed what the LEGACY catalog fixtures used to insert, into the unified
//! tables, so the compat views read it back exactly as the old rows meant.
//!
//! Since DB Phase 2.4 (migration `20261018200000_p2_converge_legacy_catalog`)
//! every database has prod's shape: `addon_items`, `addon_item_ingredients`,
//! `menu_item_recipes`, `menu_item_optional_fields`, `menu_item_addon_slots`,
//! `menu_item_allowed_addons` and the five `branch_*_overrides` are read-only
//! views over `modifier_groups` / `modifier_options` / `recipe_lines` /
//! `menu_item_sizes` / `menu_item_modifier_groups` / `menu_price_overrides`.
//! A fixture cannot insert into them any more (nor can the app: prod never
//! could), so it writes the unified rows the menu-unification backfill
//! (`src/menu_unification/backfill.rs`) would have made from the old ones:
//!
//! * an add-on's id IS its `modifier_options.id` (the stable-id invariant), in
//!   the org's group for its legacy type, `md5(org || ':addon:' || type)`;
//! * an optional field is an option of the item's own `Options` group,
//!   `md5(item || ':options')`, attached with `legacy_origin = 'options'`;
//! * an item recipe line hangs off the item's size with that label;
//! * every override is a `menu_price_overrides` row.
//!
//! The views themselves are left exactly as prod has them: no triggers, no
//! writable shim. `legacy_catalog_tests` reads each helper back through its
//! view.

#![allow(dead_code)]

use sqlx::PgPool;
use uuid::Uuid;

/// The org's group for a legacy add-on type (created on first use), exactly
/// as the backfill named it: a swap family (milk_type / coffee_type) is
/// single-choice, max 1; everything else multi.
pub async fn addon_group(pool: &PgPool, org: Uuid, addon_type: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO modifier_groups
             (id, org_id, name, selection_type, min_selections, max_selections, legacy_addon_type)
         VALUES ((md5($1::text || ':addon:' || $2))::uuid, $1, $2,
                 CASE WHEN $2 IN ('milk_type', 'coffee_type') THEN 'single' ELSE 'multi' END,
                 0, CASE WHEN $2 IN ('milk_type', 'coffee_type') THEN 1 END, $2)
         ON CONFLICT (id) DO UPDATE SET legacy_addon_type = EXCLUDED.legacy_addon_type
         RETURNING id",
    )
    .bind(org)
    .bind(addon_type)
    .fetch_one(pool)
    .await
    .expect("seed add-on group")
}

/// The fields of an old `addon_items` row.
#[derive(Clone, Debug)]
pub struct Addon<'a> {
    pub id: Option<Uuid>,
    pub org: Uuid,
    pub name: &'a str,
    pub addon_type: &'a str,
    pub price: i32,
    pub is_active: bool,
    pub name_translations: serde_json::Value,
}

impl<'a> Addon<'a> {
    pub fn new(org: Uuid, name: &'a str, addon_type: &'a str, price: i32) -> Self {
        Addon {
            id: None,
            org,
            name,
            addon_type,
            price,
            is_active: true,
            name_translations: serde_json::json!({}),
        }
    }
    pub fn id(mut self, id: Uuid) -> Self {
        self.id = Some(id);
        self
    }
    pub fn active(mut self, a: bool) -> Self {
        self.is_active = a;
        self
    }
    pub fn translations(mut self, t: serde_json::Value) -> Self {
        self.name_translations = t;
        self
    }
}

/// `INSERT INTO addon_items …` → the option in its type's group. Returns the
/// add-on id (= option id). Re-seeding the same id updates it in place.
pub async fn addon(pool: &PgPool, a: Addon<'_>) -> Uuid {
    let group = addon_group(pool, a.org, a.addon_type).await;
    sqlx::query_scalar(
        "INSERT INTO modifier_options
             (id, group_id, name, name_translations, price, is_active, legacy_source)
         VALUES (COALESCE($1, gen_random_uuid()), $2, $3, $4, GREATEST($5, 0), $6, 'addon')
         ON CONFLICT (id) DO UPDATE SET group_id = EXCLUDED.group_id, name = EXCLUDED.name,
             name_translations = EXCLUDED.name_translations, price = EXCLUDED.price,
             is_active = EXCLUDED.is_active, legacy_source = 'addon'
         RETURNING id",
    )
    .bind(a.id)
    .bind(group)
    .bind(a.name)
    .bind(&a.name_translations)
    .bind(a.price)
    .bind(a.is_active)
    .fetch_one(pool)
    .await
    .expect("seed add-on")
}

/// Shorthand for the common `(id?, org, name, type, default_price)` row.
pub async fn addon_item(
    pool: &PgPool,
    id: Option<Uuid>,
    org: Uuid,
    name: &str,
    addon_type: &str,
    price: i32,
) -> Uuid {
    let mut a = Addon::new(org, name, addon_type, price);
    a.id = id;
    addon(pool, a).await
}

/// `INSERT INTO addon_item_ingredients …` → the option's generic (size-less)
/// recipe line. The view's `ingredient_name` is the ingredient's own name.
pub async fn addon_ingredient(
    pool: &PgPool,
    addon_id: Uuid,
    ingredient: Uuid,
    quantity: impl std::fmt::Display,
    unit: &str,
) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit)
         VALUES ('modifier_option', $1, $2, $3::numeric, $4)
         ON CONFLICT (owner_type, owner_id, ingredient_id, size_label)
         DO UPDATE SET quantity = EXCLUDED.quantity, unit = EXCLUDED.unit
         RETURNING id",
    )
    .bind(addon_id)
    .bind(ingredient)
    .bind(quantity.to_string())
    .bind(unit)
    .fetch_one(pool)
    .await
    .expect("seed add-on ingredient")
}

/// The item's size with this label, if it has one (the `one_size` row the
/// menu_items trigger made, or a size the fixture created). Never invents one:
/// the backfill dropped a legacy row that named a size the item lacked.
pub async fn find_size(pool: &PgPool, menu_item: Uuid, label: &str) -> Option<Uuid> {
    sqlx::query_scalar("SELECT id FROM menu_item_sizes WHERE menu_item_id = $1 AND label = $2")
        .bind(menu_item)
        .bind(label)
        .fetch_optional(pool)
        .await
        .expect("read size")
}

/// [`find_size`], for a row that is meaningless without it.
pub async fn size_for(pool: &PgPool, menu_item: Uuid, label: &str) -> Uuid {
    find_size(pool, menu_item, label)
        .await
        .unwrap_or_else(|| panic!("menu item {menu_item} has no size {label:?}: create it first"))
}

/// `INSERT INTO menu_item_recipes …` → a recipe line on the item's size.
pub async fn item_recipe(
    pool: &PgPool,
    menu_item: Uuid,
    size_label: &str,
    ingredient: Uuid,
    quantity: impl std::fmt::Display,
    unit: &str,
) -> Uuid {
    let size = size_for(pool, menu_item, size_label).await;
    sqlx::query_scalar(
        "INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit)
         VALUES ('item_size', $1, $2, $3::numeric, $4)
         ON CONFLICT (owner_type, owner_id, ingredient_id, size_label)
         DO UPDATE SET quantity = EXCLUDED.quantity, unit = EXCLUDED.unit
         RETURNING id",
    )
    .bind(size)
    .bind(ingredient)
    .bind(quantity.to_string())
    .bind(unit)
    .fetch_one(pool)
    .await
    .expect("seed item recipe")
}

/// The item's own `Options` group, created and attached on first use.
pub async fn options_group(pool: &PgPool, menu_item: Uuid) -> Uuid {
    let group: Uuid = sqlx::query_scalar(
        "INSERT INTO modifier_groups (id, org_id, name, selection_type, min_selections, sort)
         SELECT (md5(m.id::text || ':options'))::uuid, m.org_id, 'Options', 'multi', 0, 100
           FROM menu_items m WHERE m.id = $1
         ON CONFLICT (id) DO UPDATE SET name = modifier_groups.name
         RETURNING id",
    )
    .bind(menu_item)
    .fetch_one(pool)
    .await
    .expect("seed options group");
    sqlx::query(
        "INSERT INTO menu_item_modifier_groups (menu_item_id, group_id, sort, legacy_origin)
         VALUES ($1, $2, 100, 'options') ON CONFLICT (menu_item_id, group_id) DO NOTHING",
    )
    .bind(menu_item)
    .bind(group)
    .execute(pool)
    .await
    .expect("attach options group");
    group
}

/// The fields of an old `menu_item_optional_fields` row.
#[derive(Clone, Debug)]
pub struct Optional<'a> {
    pub id: Option<Uuid>,
    pub menu_item: Uuid,
    pub name: &'a str,
    pub price: i32,
    pub is_active: bool,
    /// The inline recipe: (ingredient, quantity, unit).
    pub recipe: Option<(Uuid, String, &'a str)>,
    pub name_translations: serde_json::Value,
}

impl<'a> Optional<'a> {
    pub fn new(menu_item: Uuid, name: &'a str, price: i32) -> Self {
        Optional {
            id: None,
            menu_item,
            name,
            price,
            is_active: true,
            recipe: None,
            name_translations: serde_json::json!({}),
        }
    }
    pub fn id(mut self, id: Uuid) -> Self {
        self.id = Some(id);
        self
    }
    pub fn recipe(
        mut self,
        ingredient: Uuid,
        quantity: impl std::fmt::Display,
        unit: &'a str,
    ) -> Self {
        self.recipe = Some((ingredient, quantity.to_string(), unit));
        self
    }
    pub fn active(mut self, a: bool) -> Self {
        self.is_active = a;
        self
    }
}

/// `INSERT INTO menu_item_optional_fields …` → an option of the item's
/// `Options` group (+ its single generic recipe line). Returns the field id.
pub async fn optional_field(pool: &PgPool, f: Optional<'_>) -> Uuid {
    let group = options_group(pool, f.menu_item).await;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO modifier_options
             (id, group_id, name, name_translations, price, is_active, legacy_source)
         VALUES (COALESCE($1, gen_random_uuid()), $2, $3, $4, GREATEST($5, 0), $6, 'optional')
         ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, price = EXCLUDED.price,
             is_active = EXCLUDED.is_active, name_translations = EXCLUDED.name_translations
         RETURNING id",
    )
    .bind(f.id)
    .bind(group)
    .bind(f.name)
    .bind(&f.name_translations)
    .bind(f.price)
    .bind(f.is_active)
    .fetch_one(pool)
    .await
    .expect("seed optional field");
    if let Some((ing, qty, unit)) = f.recipe {
        addon_ingredient(pool, id, ing, qty, unit).await;
    }
    id
}

/// `INSERT INTO menu_item_addon_slots …` → the type's group attached to the
/// item as a slot, with the slot's overrides. Returns the attachment id (the
/// view's slot id).
pub async fn addon_slot(
    pool: &PgPool,
    org: Uuid,
    menu_item: Uuid,
    addon_type: &str,
    is_required: bool,
    min_selections: i32,
    max_selections: Option<i32>,
) -> Uuid {
    let group = addon_group(pool, org, addon_type).await;
    sqlx::query_scalar(
        "INSERT INTO menu_item_modifier_groups
             (menu_item_id, group_id, sort, min_override, max_override, is_required_override, legacy_origin)
         VALUES ($1, $2, 0, $3, $4, $5, 'slot')
         ON CONFLICT (menu_item_id, group_id) DO UPDATE SET
             min_override = EXCLUDED.min_override, max_override = EXCLUDED.max_override,
             is_required_override = EXCLUDED.is_required_override, legacy_origin = 'slot'
         RETURNING id",
    )
    .bind(menu_item)
    .bind(group)
    .bind(min_selections)
    .bind(max_selections)
    .bind(is_required)
    .fetch_one(pool)
    .await
    .expect("seed add-on slot")
}

/// `INSERT INTO menu_item_allowed_addons …` → each type's subset as the
/// attachment's `included_option_ids`, in the order given (attached with the
/// group defaults when the item has no slot for that type).
pub async fn allowed_addons(pool: &PgPool, menu_item: Uuid, addon_ids: &[Uuid]) {
    sqlx::query(
        "WITH a AS (
             SELECT o.group_id, array_agg(o.id ORDER BY u.ord) AS ids
               FROM unnest($2::uuid[]) WITH ORDINALITY AS u(id, ord)
               JOIN modifier_options o ON o.id = u.id
              GROUP BY o.group_id)
         INSERT INTO menu_item_modifier_groups (menu_item_id, group_id, sort, included_option_ids, legacy_origin)
         SELECT $1, a.group_id, 0, a.ids, 'allowlist' FROM a
         ON CONFLICT (menu_item_id, group_id) DO UPDATE SET included_option_ids = EXCLUDED.included_option_ids",
    )
    .bind(menu_item)
    .bind(addon_ids)
    .execute(pool)
    .await
    .expect("seed allowed add-ons");
}

async fn upsert_override(
    pool: &PgPool,
    branch: Uuid,
    channel: Option<&str>,
    target_type: &str,
    target: Uuid,
    price: Option<i32>,
    is_available: Option<bool>,
) {
    if price.is_none() && is_available.is_none() {
        return;
    }
    let (scope, conflict) = match channel {
        None => (
            "branch",
            "(target_type, target_id, branch_id) WHERE scope = 'branch'",
        ),
        Some(_) => (
            "branch_channel",
            "(target_type, target_id, branch_id, channel) WHERE scope = 'branch_channel'",
        ),
    };
    sqlx::query(&format!(
        "INSERT INTO menu_price_overrides (scope, branch_id, channel, target_type, target_id, price, is_available)
         VALUES ($1, $2, $3::delivery_channel, $4, $5, $6, $7)
         ON CONFLICT {conflict} DO UPDATE SET
             price = COALESCE(EXCLUDED.price, menu_price_overrides.price),
             is_available = COALESCE(EXCLUDED.is_available, menu_price_overrides.is_available),
             updated_at = now()"
    ))
    .bind(scope)
    .bind(branch)
    .bind(channel)
    .bind(target_type)
    .bind(target)
    .bind(price)
    .bind(is_available)
    .execute(pool)
    .await
    .expect("seed price override");
}

/// `INSERT INTO branch_menu_overrides (branch, item, price_override, is_available)`:
/// the price lands on the item's `one_size` row (size-less items only, as in
/// the backfill), unavailability on every size.
pub async fn branch_menu_override(
    pool: &PgPool,
    branch: Uuid,
    menu_item: Uuid,
    price: Option<i32>,
    is_available: bool,
) {
    // A sized item has no `one_size` row: its menu-level price was never applied.
    if let (Some(_), Some(one)) = (price, find_size(pool, menu_item, "one_size").await) {
        upsert_override(pool, branch, None, "menu_item_size", one, price, None).await;
    }
    if !is_available {
        for size in sizes_of(pool, menu_item).await {
            upsert_override(
                pool,
                branch,
                None,
                "menu_item_size",
                size,
                None,
                Some(false),
            )
            .await;
        }
    }
}

/// `DELETE FROM branch_menu_overrides / branch_menu_size_overrides` for one
/// (branch, item): every branch-scoped override of its sizes.
pub async fn clear_branch_menu_override(pool: &PgPool, branch: Uuid, menu_item: Uuid) {
    sqlx::query(
        "DELETE FROM menu_price_overrides p USING menu_item_sizes s
          WHERE p.scope = 'branch' AND p.branch_id = $1 AND p.target_type = 'menu_item_size'
            AND p.target_id = s.id AND s.menu_item_id = $2",
    )
    .bind(branch)
    .bind(menu_item)
    .execute(pool)
    .await
    .expect("clear branch menu override");
}

/// `INSERT INTO branch_menu_size_overrides (branch, item, size_label, price_override)`.
pub async fn branch_size_override(
    pool: &PgPool,
    branch: Uuid,
    menu_item: Uuid,
    size_label: &str,
    price: i32,
) {
    // A size the item does not have: the legacy row priced nothing.
    if let Some(size) = find_size(pool, menu_item, size_label).await {
        upsert_override(
            pool,
            branch,
            None,
            "menu_item_size",
            size,
            Some(price),
            None,
        )
        .await;
    }
}

/// `INSERT INTO branch_addon_overrides (branch, addon, price_override, is_available)`.
/// A row that says nothing (no price, available) is no override at all.
pub async fn branch_addon_override(
    pool: &PgPool,
    branch: Uuid,
    addon_id: Uuid,
    price: Option<i32>,
    is_available: bool,
) {
    sqlx::query(
        "DELETE FROM menu_price_overrides
          WHERE scope = 'branch' AND branch_id = $1 AND target_type = 'modifier_option' AND target_id = $2",
    )
    .bind(branch)
    .bind(addon_id)
    .execute(pool)
    .await
    .expect("reset add-on override");
    upsert_override(
        pool,
        branch,
        None,
        "modifier_option",
        addon_id,
        price,
        (!is_available).then_some(false),
    )
    .await;
}

/// `INSERT INTO branch_channel_menu_overrides (branch, item, channel, price, is_available)`:
/// price on `one_size`, the tri-state availability on every size.
pub async fn channel_menu_override(
    pool: &PgPool,
    branch: Uuid,
    menu_item: Uuid,
    channel: &str,
    price: Option<i32>,
    is_available: Option<bool>,
) {
    if let (Some(_), Some(one)) = (price, find_size(pool, menu_item, "one_size").await) {
        upsert_override(
            pool,
            branch,
            Some(channel),
            "menu_item_size",
            one,
            price,
            None,
        )
        .await;
    }
    if is_available.is_some() {
        for size in sizes_of(pool, menu_item).await {
            upsert_override(
                pool,
                branch,
                Some(channel),
                "menu_item_size",
                size,
                None,
                is_available,
            )
            .await;
        }
    }
}

/// `INSERT INTO branch_channel_addon_overrides (branch, addon, channel, price, is_available)`.
pub async fn channel_addon_override(
    pool: &PgPool,
    branch: Uuid,
    addon_id: Uuid,
    channel: &str,
    price: Option<i32>,
    is_available: Option<bool>,
) {
    sqlx::query(
        "DELETE FROM menu_price_overrides WHERE scope = 'branch_channel' AND branch_id = $1
            AND channel = $2::delivery_channel AND target_type = 'modifier_option' AND target_id = $3",
    )
    .bind(branch)
    .bind(channel)
    .bind(addon_id)
    .execute(pool)
    .await
    .expect("reset channel add-on override");
    upsert_override(
        pool,
        branch,
        Some(channel),
        "modifier_option",
        addon_id,
        price,
        is_available,
    )
    .await;
}

async fn sizes_of(pool: &PgPool, menu_item: Uuid) -> Vec<Uuid> {
    sqlx::query_scalar(
        "SELECT id FROM menu_item_sizes WHERE menu_item_id = $1 ORDER BY sort, label",
    )
    .bind(menu_item)
    .fetch_all(pool)
    .await
    .expect("read sizes")
}

/// `DELETE FROM branch_menu_overrides [WHERE branch_id = …]`: every
/// branch-scoped size override (of one branch, or of all).
pub async fn clear_all_branch_menu_overrides(pool: &PgPool, branch: Option<Uuid>) {
    sqlx::query(
        "DELETE FROM menu_price_overrides
          WHERE scope = 'branch' AND target_type = 'menu_item_size'
            AND ($1::uuid IS NULL OR branch_id = $1)",
    )
    .bind(branch)
    .execute(pool)
    .await
    .expect("clear branch menu overrides");
}

/// What the retired `PUT /branch-menu-overrides` meant, written to the unified
/// rows: the item's override is REPLACED (its price on `one_size`, its
/// availability on every size); `sizes: None` keeps the item's size prices,
/// `Some(list)` replaces them with exactly that list (sizes it lacks priced
/// nothing).
pub async fn put_branch_menu_override(
    pool: &PgPool,
    branch: Uuid,
    menu_item: Uuid,
    price: Option<i32>,
    is_available: bool,
    sizes: Option<&[(String, i32)]>,
) {
    let kept: Vec<(String, i32)> = match sizes {
        Some(list) => list.to_vec(),
        None => sqlx::query_as(
            "SELECT s.label, p.price FROM menu_price_overrides p JOIN menu_item_sizes s ON s.id = p.target_id
              WHERE p.scope = 'branch' AND p.branch_id = $1 AND p.target_type = 'menu_item_size'
                AND s.menu_item_id = $2 AND s.label <> 'one_size' AND p.price IS NOT NULL",
        )
        .bind(branch)
        .bind(menu_item)
        .fetch_all(pool)
        .await
        .expect("read size overrides"),
    };
    clear_branch_menu_override(pool, branch, menu_item).await;
    branch_menu_override(pool, branch, menu_item, price, is_available).await;
    for (label, p) in &kept {
        branch_size_override(pool, branch, menu_item, label, *p).await;
    }
}
