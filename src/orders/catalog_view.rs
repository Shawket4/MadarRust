//! The catalogue as the pricing rule reads it — madar-catalog's
//! [`CatalogView`] — loaded in a fixed number of queries for every item and
//! option an order (or a feed page) names, never one query per add-on.
//!
//! Three readers share it, so a price and a stock deduction cannot disagree:
//! - the order path (`catalog_unit_price`, `component_resolve`): price the
//!   line with `madar_catalog`, then deduct stock from the same rows and the
//!   rule's own decisions (which option swapped what, into which ingredient);
//! - the till's feed: every `/menu-items?full=true` row carries its
//!   [`ItemView`] as `pricing`, every add-on row its [`OptionView`], so the
//!   POS core builds the same view from its menu mirror;
//! - `tests/catalog_pricing_tests.rs`, which checks both against each other.
//!
//! Every order below is the one the per-query code it replaced used, with one
//! exception: a recipe's lines are read in ingredient-name order (the old
//! query had none; see `load_recipes`).

// Each batched query reads its rows as a tuple.
#![allow(clippy::type_complexity)]

use std::collections::{BTreeMap, HashMap, HashSet};

use madar_catalog::{
    BaseCandidate, BaseCandidates, CatalogView, GroupOption, GroupView, IngredientLine,
    IngredientRef, ItemView, OptionView, OptionalView, RecipeLine, SizeView, SizedLine,
};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::errors::AppError;

/// One recipe line with what a stock deduction needs.
#[derive(Clone, Debug)]
pub struct RecipeRow {
    pub size_label: String,
    pub ingredient_id: Option<Uuid>,
    pub quantity: f64,
    pub name: String,
    pub unit: String,
    /// The ingredient's category slug; `None` only for a line with no
    /// ingredient.
    pub category: Option<String>,
}

/// One active optional field of an item, as the order path records it.
#[derive(Clone, Debug)]
pub struct OptionalRow {
    pub id: Uuid,
    pub name: String,
    pub name_translations: serde_json::Value,
    pub price: i32,
    pub ingredient_id: Option<Uuid>,
    pub ingredient_name: Option<String>,
    pub ingredient_unit: Option<String>,
    pub quantity_used: Option<f64>,
    pub size_label: Option<String>,
}

/// One ingredient line of an option (generic or per size), with its
/// quantity and its ingredient's category (an extra shot is a coffee bean).
#[derive(Clone, Debug)]
pub struct OptionLine {
    pub ingredient_id: Option<Uuid>,
    pub quantity: f64,
    pub name: String,
    pub unit: String,
    /// The ingredient's category slug, `general` when it has none.
    pub category: String,
}

/// A menu item as loaded.
#[derive(Clone, Debug)]
pub struct LoadedItem {
    /// `None` when the item does not exist or is soft-deleted.
    pub row: Option<ItemRow>,
    pub view: ItemView,
    pub recipe: Vec<RecipeRow>,
    pub optionals: Vec<OptionalRow>,
}

#[derive(Clone, Debug)]
pub struct ItemRow {
    pub name: String,
    pub name_translations: serde_json::Value,
    /// The branch has the item switched off (flagged, never refused).
    pub branch_disabled: bool,
}

/// An option (add-on item) as loaded.
#[derive(Clone, Debug)]
pub struct LoadedOption {
    pub name: String,
    pub name_translations: serde_json::Value,
    pub view: OptionView,
    pub lines: Vec<OptionLine>,
    /// `(size label, line)`, per size.
    pub sized: Vec<(String, OptionLine)>,
}

impl LoadedOption {
    /// The option's lines for a line of `size_label`: madar-catalog's merge,
    /// over the rows that carry quantities.
    pub fn lines_for(&self, size_label: Option<&str>) -> Vec<OptionLine> {
        let Some(label) = size_label else {
            return self.lines.clone();
        };
        let sized: Vec<OptionLine> = self
            .sized
            .iter()
            .filter(|(l, _)| l == label)
            .map(|(_, line)| line.clone())
            .collect();
        madar_catalog::merge_sized_lines(
            self.lines.clone(),
            sized,
            |l| l.ingredient_id,
            |l| l.name.as_str(),
        )
    }
}

/// Items and options loaded for one branch.
pub struct Catalog {
    branch: Option<Uuid>,
    /// Also load what has been deleted or switched off since (see
    /// [`Self::including_retired`]).
    retired: bool,
    items: BTreeMap<Uuid, LoadedItem>,
    options: BTreeMap<Uuid, LoadedOption>,
    /// Option ids asked for that do not exist.
    missing: HashSet<Uuid>,
}

fn s(id: Uuid) -> String {
    id.to_string()
}

impl Catalog {
    /// Prices and overrides as `branch` sells them (`None`: the catalogue's).
    /// A queued sale's catalogue: also the items deleted and the optional
    /// extras switched off since. The sale was rung at the till before the
    /// change reached it; refusing to price it cannot un-take the money, it
    /// only loses the sale (the till dead-letters a rejected op). Only a
    /// REPLAY asks for this — a live sale naming a retired item is refused.
    pub fn including_retired(mut self, yes: bool) -> Self {
        self.retired = yes;
        self
    }

    pub fn new(branch: Option<Uuid>) -> Self {
        Self {
            branch,
            retired: false,
            items: BTreeMap::new(),
            options: BTreeMap::new(),
            missing: HashSet::new(),
        }
    }

    pub fn item(&self, id: Uuid) -> Option<&LoadedItem> {
        self.items.get(&id)
    }

    pub fn option(&self, id: Uuid) -> Option<&LoadedOption> {
        self.options.get(&id)
    }

    /// The rule's view of `item` with every loaded option.
    pub fn view(&self, item: Uuid) -> CatalogView {
        CatalogView {
            item: self
                .items
                .get(&item)
                .map(|i| i.view.clone())
                .unwrap_or_else(|| ItemView {
                    id: s(item),
                    ..Default::default()
                }),
            options: self.options.values().map(|o| o.view.clone()).collect(),
        }
    }

    /// The ids of `items` / `options` not loaded yet (deduplicated).
    fn wanted(&self, items: &[Uuid], options: &[Uuid]) -> (Vec<Uuid>, Vec<Uuid>) {
        let mut want_items: Vec<Uuid> = items
            .iter()
            .copied()
            .filter(|i| !self.items.contains_key(i))
            .collect();
        want_items.sort();
        want_items.dedup();
        let mut want_options: Vec<Uuid> = options
            .iter()
            .copied()
            .filter(|o| !self.options.contains_key(o) && !self.missing.contains(o))
            .collect();
        want_options.sort();
        want_options.dedup();
        (want_items, want_options)
    }

    /// Load every item and option not loaded yet.
    pub async fn ensure(
        &mut self,
        conn: &mut PgConnection,
        items: &[Uuid],
        options: &[Uuid],
    ) -> Result<(), AppError> {
        let (want_items, want_options) = self.wanted(items, options);
        if !want_items.is_empty() {
            self.load_items(conn, &want_items).await?;
        }
        if !want_options.is_empty() {
            self.load_options(conn, &want_options).await?;
        }
        Ok(())
    }

    /// [`Self::ensure`] on a connection from `pool`, taken only when something
    /// is left to load.
    pub async fn ensure_on(
        &mut self,
        pool: &sqlx::PgPool,
        items: &[Uuid],
        options: &[Uuid],
    ) -> Result<(), AppError> {
        let (want_items, want_options) = self.wanted(items, options);
        if want_items.is_empty() && want_options.is_empty() {
            return Ok(());
        }
        let mut conn = pool.acquire().await?;
        self.ensure(&mut conn, &want_items, &want_options).await
    }

    async fn load_items(&mut self, conn: &mut PgConnection, ids: &[Uuid]) -> Result<(), AppError> {
        let branch = self.branch;
        // On the unified tables (DB Phase 3.1), exactly what the old
        // `branch_menu_overrides` rollup gave: the price is the item's
        // `one_size` branch price, and the item is off at the branch when any
        // of its sizes is explicitly off there.
        let rows: Vec<(Uuid, String, serde_json::Value, Option<i32>, bool)> = sqlx::query_as(
            "SELECT mi.id, mi.name, mi.name_translations,
                    (SELECT max(p.price) FROM menu_price_overrides p
                       JOIN menu_item_sizes ms ON ms.id = p.target_id
                      WHERE p.scope = 'branch' AND p.target_type = 'menu_item_size'
                        AND p.branch_id = $2 AND ms.menu_item_id = mi.id
                        AND ms.label = 'one_size'),
                    EXISTS (SELECT 1 FROM menu_price_overrides p
                              JOIN menu_item_sizes ms ON ms.id = p.target_id
                             WHERE p.scope = 'branch' AND p.target_type = 'menu_item_size'
                               AND p.branch_id = $2 AND ms.menu_item_id = mi.id
                               AND p.is_available = false)
               FROM menu_items mi
              WHERE mi.id = ANY($1) AND ($3 OR mi.deleted_at IS NULL)",
        )
        .bind(ids)
        .bind(branch)
        .bind(self.retired)
        .fetch_all(&mut *conn)
        .await?;
        let mut row_of: HashMap<Uuid, (ItemRow, Option<i32>)> = rows
            .into_iter()
            .map(|(id, name, tr, bp, off)| {
                (
                    id,
                    (
                        ItemRow {
                            name,
                            name_translations: tr,
                            branch_disabled: off,
                        },
                        bp,
                    ),
                )
            })
            .collect();

        // Every size, and every label only the branch prices.
        let sizes: Vec<(Uuid, String, Option<i32>, bool, Option<i32>, Option<i32>)> =
            sqlx::query_as(
                // The size's own branch price (what `branch_menu_size_overrides`
                // listed: a priced, non-`one_size` size override). A branch
                // price is always on an existing size row now, so the old
                // second branch (a label only the branch priced) is empty.
                "SELECT z.menu_item_id, z.label, z.price, z.is_active,
                        (SELECT max(p.price) FROM menu_price_overrides p
                          WHERE p.scope = 'branch' AND p.target_type = 'menu_item_size'
                            AND p.branch_id = $2 AND p.target_id = z.id
                            AND p.price IS NOT NULL AND z.label <> 'one_size'),
                        z.sort
               FROM menu_item_sizes z
              WHERE z.menu_item_id = ANY($1)
              ORDER BY 1, 6 NULLS LAST, 2",
            )
            .bind(ids)
            .bind(branch)
            .fetch_all(&mut *conn)
            .await?;

        // The recipe size a sizeless line is made from: the item's first size
        // as listed (active first, then sort, then label) among the recipe's
        // own labels — the order path's subquery, verbatim.
        let default_sizes: Vec<(Uuid, Option<String>)> = sqlx::query_as(
            // A recipe line of a size is recipe_lines (owner_type item_size) on
            // that menu_item_sizes row (the old `menu_item_recipes` view).
            "SELECT m.id,
                    (SELECT sz.label FROM recipe_lines rl
                       JOIN menu_item_sizes sz ON sz.id = rl.owner_id
                      WHERE rl.owner_type = 'item_size' AND sz.menu_item_id = m.id
                      ORDER BY sz.is_active IS NOT TRUE, sz.sort NULLS LAST, sz.label
                      LIMIT 1)
               FROM unnest($1::uuid[]) AS m(id)",
        )
        .bind(ids)
        .fetch_all(&mut *conn)
        .await?;

        let recipes = load_recipes(conn, ids).await?;

        // The choice groups attached to each item (active groups only), every
        // option with its catalogue price, the branch's price and whether it is
        // on — what madar-catalog's staff comp input reads (`staff`). Ordered
        // as the staff pool always read them: attachment sort, group name, id,
        // then option sort, name, id.
        let group_rows: Vec<(
            Uuid,
            Uuid,
            i32,
            bool,
            Option<Vec<Uuid>>,
            String,
            Option<String>,
            Uuid,
            i32,
            Option<i32>,
            bool,
            bool,
        )> = sqlx::query_as(
            "SELECT a.menu_item_id, g.id, COALESCE(a.min_override, g.min_selections),
                    COALESCE(a.is_required_override, g.is_required), mimg_included_options(a.id, a.option_allowlist),
                    g.effect, g.legacy_addon_type,
                    mo.id, mo.price, bao.price, mo.is_default,
                    (mo.is_active AND COALESCE(bao.is_available, true))
               FROM menu_item_modifier_groups a
               JOIN modifier_groups g ON g.id = a.group_id AND g.is_active
               JOIN modifier_options mo ON mo.group_id = g.id AND mo.legacy_source = 'addon'
               LEFT JOIN menu_price_overrides bao
                      ON bao.scope = 'branch' AND bao.target_type = 'modifier_option'
                     AND bao.target_id = mo.id AND bao.branch_id = $2
              WHERE a.menu_item_id = ANY($1)
              ORDER BY a.menu_item_id, a.sort, g.name, g.id, mo.sort, mo.name, mo.id",
        )
        .bind(ids)
        .bind(branch)
        .fetch_all(&mut *conn)
        .await?;
        let mut groups_of: HashMap<Uuid, Vec<GroupView>> = HashMap::new();
        for (item, gid, min, required, included, effect, legacy, oid, price, bp, default, on) in
            group_rows
        {
            let groups = groups_of.entry(item).or_default();
            if groups.last().is_none_or(|g| g.id != s(gid)) {
                groups.push(GroupView {
                    id: s(gid),
                    min: i64::from(min),
                    is_required: required,
                    effect,
                    legacy_type: legacy,
                    included: included.map(|ids| ids.into_iter().map(s).collect()),
                    options: Vec::new(),
                });
            }
            if let Some(g) = groups.last_mut() {
                g.options.push(GroupOption {
                    id: s(oid),
                    price: i64::from(price),
                    branch_price: bp.map(i64::from),
                    is_default: default,
                    is_active: on,
                });
            }
        }

        let optionals: Vec<(
            Uuid,
            Uuid,
            String,
            i32,
            Option<Uuid>,
            Option<String>,
            Option<String>,
            Option<f64>,
            Option<String>,
            serde_json::Value,
        )> = sqlx::query_as(
            // An optional field is a modifier option of legacy_source
            // 'optional' in the item's per-item Options attachment, with its
            // generic (sizeless) recipe line (the old `menu_item_optional_fields`).
            "SELECT mimg.menu_item_id, mo.id, mo.name, mo.price, rl.ingredient_id, oi.name,
                    rl.unit, rl.quantity::float8, NULL::text, mo.name_translations
               FROM modifier_options mo
               JOIN menu_item_modifier_groups mimg
                    ON mimg.group_id = mo.group_id AND mimg.legacy_origin = 'options'
               LEFT JOIN recipe_lines rl ON rl.owner_type = 'modifier_option'
                    AND rl.owner_id = mo.id AND rl.size_label IS NULL
               LEFT JOIN org_ingredients oi ON oi.id = rl.ingredient_id
              WHERE mo.legacy_source = 'optional'
                AND mimg.menu_item_id = ANY($1) AND ($2 OR mo.is_active = true)
              ORDER BY mimg.menu_item_id, mo.name, mo.id",
        )
        .bind(ids)
        .bind(self.retired)
        .fetch_all(&mut *conn)
        .await?;

        // A swap is charged over the first option carrying the recipe's
        // ingredient (as a line, or as the ingredient it replaces) in the
        // chosen option's family: active first, then sort (missing last),
        // then name, then id. madar-catalog puts the chosen option's own
        // group first.
        let mut base_ids: Vec<Uuid> = recipes
            .values()
            .flatten()
            .filter_map(|r| r.ingredient_id)
            .collect();
        base_ids.sort();
        base_ids.dedup();
        // A custom group's option (no legacy type: `a.type` NULL, see
        // `menu::handlers::AddonItem`) is a candidate like any other, with no
        // add-on type, so it never matches an inferred milk / coffee family.
        let candidates: Vec<(Uuid, Uuid, String, String, i32, Option<Uuid>, Option<Uuid>)> =
            if base_ids.is_empty() {
                Vec::new()
            } else {
                sqlx::query_as(
                    // Unified tables (DB Phase 3.1): an add-on is a modifier
                    // option of legacy_source 'addon', its type the group's
                    // legacy_addon_type; its generic recipe line names the
                    // ingredient it carries.
                    "WITH c AS (
                         SELECT rl.ingredient_id AS ing, rl.owner_id AS id
                           FROM recipe_lines rl
                          WHERE rl.owner_type = 'modifier_option' AND rl.size_label IS NULL
                            AND rl.ingredient_id = ANY($1)
                         UNION
                         SELECT mo.replaces_ingredient_id, mo.id
                           FROM modifier_options mo WHERE mo.replaces_ingredient_id = ANY($1)
                     )
                     SELECT c.ing, mo.id, mo.name, COALESCE(mg.legacy_addon_type, ''),
                            COALESCE(bao.price, mo.price),
                            mo.group_id, mg.swap_category_id
                       FROM c
                       JOIN modifier_options mo ON mo.id = c.id AND mo.legacy_source = 'addon'
                       JOIN modifier_groups mg ON mg.id = mo.group_id
                       LEFT JOIN menu_price_overrides bao
                              ON bao.scope = 'branch' AND bao.target_type = 'modifier_option'
                             AND bao.target_id = mo.id AND bao.branch_id = $2
                      ORDER BY c.ing, mo.is_active DESC, mo.sort NULLS LAST, mo.name, mo.id",
                )
                .bind(&base_ids)
                .bind(branch)
                .fetch_all(&mut *conn)
                .await?
            };
        let mut bases: HashMap<Uuid, Vec<BaseCandidate>> = HashMap::new();
        for (ing, id, name, kind, price, group, swap_cat) in candidates {
            bases.entry(ing).or_default().push(BaseCandidate {
                option_id: s(id),
                name,
                kind,
                price: i64::from(price),
                group_id: group.map(s),
                swap_category_id: swap_cat.map(s),
            });
        }

        let mut recipes = recipes;
        for &id in ids {
            let recipe = recipes.remove(&id).unwrap_or_default();
            let mut ing_order: Vec<Uuid> = Vec::new();
            for r in &recipe {
                if let Some(i) = r.ingredient_id
                    && !ing_order.contains(&i)
                {
                    ing_order.push(i);
                }
            }
            let (row, branch_price) = match row_of.remove(&id) {
                Some((r, bp)) => (Some(r), bp),
                None => (None, None),
            };
            let item_optionals: Vec<OptionalRow> = optionals
                .iter()
                .filter(|o| o.0 == id)
                .map(|o| OptionalRow {
                    id: o.1,
                    name: o.2.clone(),
                    name_translations: o.9.clone(),
                    price: o.3,
                    ingredient_id: o.4,
                    ingredient_name: o.5.clone(),
                    ingredient_unit: o.6.clone(),
                    quantity_used: o.7,
                    size_label: o.8.clone(),
                })
                .collect();
            let view = ItemView {
                id: s(id),
                branch_price: branch_price.map(i64::from),
                sizes: sizes
                    .iter()
                    .filter(|z| z.0 == id)
                    .map(|z| SizeView {
                        label: z.1.clone(),
                        price: z.2.map(i64::from),
                        is_active: z.3,
                        branch_price: z.4.map(i64::from),
                    })
                    .collect(),
                default_recipe_size: default_sizes
                    .iter()
                    .find(|d| d.0 == id)
                    .and_then(|d| d.1.clone()),
                recipe: recipe
                    .iter()
                    .map(|r| RecipeLine {
                        size_label: r.size_label.clone(),
                        category: r.category.clone(),
                        ingredient_id: r.ingredient_id.map(s),
                    })
                    .collect(),
                bases: ing_order
                    .iter()
                    .filter_map(|i| {
                        bases.get(i).map(|c| BaseCandidates {
                            ingredient_id: s(*i),
                            candidates: c.clone(),
                        })
                    })
                    .collect(),
                optionals: item_optionals
                    .iter()
                    .map(|o| OptionalView {
                        id: s(o.id),
                        price: i64::from(o.price),
                        size_label: o.size_label.clone(),
                    })
                    .collect(),
                groups: groups_of.remove(&id).unwrap_or_default(),
            };
            self.items.insert(
                id,
                LoadedItem {
                    row,
                    view,
                    recipe,
                    optionals: item_optionals,
                },
            );
        }
        Ok(())
    }

    async fn load_options(
        &mut self,
        conn: &mut PgConnection,
        ids: &[Uuid],
    ) -> Result<(), AppError> {
        type OptRow = (
            Uuid,
            String,
            serde_json::Value,
            i32,
            String,
            Option<Uuid>,
            Option<String>,
            Option<Uuid>,
            Option<String>,
            Option<Uuid>,
            Option<String>,
            Option<String>,
        );
        // A custom group's option is sold like any other, with no add-on type.
        let rows: Vec<OptRow> = sqlx::query_as(
            // Unified tables (DB Phase 3.1): an add-on is a modifier option of
            // legacy_source 'addon' (the old `addon_items`), its branch price a
            // branch-scope menu_price_overrides row (`branch_addon_overrides`).
            "SELECT mo.id, mo.name, mo.name_translations,
                    COALESCE(bao.price, mo.price), COALESCE(g.legacy_addon_type, ''),
                    mo.group_id, g.effect, g.swap_category_id, c.slug,
                    ri.id, ri.name, ri.unit::text
               FROM modifier_options mo
               JOIN modifier_groups g ON g.id = mo.group_id
               LEFT JOIN menu_price_overrides bao
                      ON bao.scope = 'branch' AND bao.target_type = 'modifier_option'
                     AND bao.target_id = mo.id AND bao.branch_id = $2
               LEFT JOIN ingredient_categories c ON c.id = g.swap_category_id
               LEFT JOIN org_ingredients ri ON ri.id = mo.replaces_ingredient_id
              WHERE mo.id = ANY($1) AND mo.legacy_source = 'addon'",
        )
        .bind(ids)
        .bind(self.branch)
        .fetch_all(&mut *conn)
        .await?;

        // Ordered like the `/addon-items` payload: a swap option with several
        // lines swaps in its first.
        let lines: Vec<(Uuid, Option<Uuid>, f64, String, String, Option<String>)> = sqlx::query_as(
            // An add-on's generic (sizeless) recipe lines (`addon_item_ingredients`).
            "SELECT rl.owner_id, rl.ingredient_id, rl.quantity::float8,
                    oi.name, rl.unit, c.slug
               FROM recipe_lines rl
               JOIN modifier_options mo ON mo.id = rl.owner_id AND mo.legacy_source = 'addon'
               JOIN org_ingredients oi ON oi.id = rl.ingredient_id
               LEFT JOIN ingredient_categories c ON c.id = oi.category_id
              WHERE rl.owner_type = 'modifier_option' AND rl.size_label IS NULL
                AND rl.owner_id = ANY($1)
              ORDER BY rl.owner_id, oi.name, rl.ingredient_id",
        )
        .bind(ids)
        .fetch_all(&mut *conn)
        .await?;

        // Per-size option amounts (menu modeling B9).
        let sized: Vec<(Uuid, String, Uuid, f64, String, String, Option<String>)> = sqlx::query_as(
            "SELECT rl.owner_id, rl.size_label, rl.ingredient_id, rl.quantity::float8,
                    oi.name, rl.unit, c.slug
               FROM recipe_lines rl
               JOIN org_ingredients oi ON oi.id = rl.ingredient_id
               LEFT JOIN ingredient_categories c ON c.id = oi.category_id
              WHERE rl.owner_type = 'modifier_option' AND rl.owner_id = ANY($1)
                AND rl.size_label IS NOT NULL
              ORDER BY rl.owner_id, rl.size_label, oi.name, rl.ingredient_id",
        )
        .bind(ids)
        .fetch_all(&mut *conn)
        .await?;

        let found: HashSet<Uuid> = rows.iter().map(|r| r.0).collect();
        for &id in ids {
            if !found.contains(&id) {
                self.missing.insert(id);
            }
        }
        for (id, name, tr, price, kind, group, effect, swap_cat, slug, r_id, r_name, r_unit) in rows
        {
            let own: Vec<OptionLine> = lines
                .iter()
                .filter(|l| l.0 == id)
                .map(|l| OptionLine {
                    ingredient_id: l.1,
                    quantity: l.2,
                    name: l.3.clone(),
                    unit: l.4.clone(),
                    category: l.5.clone().unwrap_or_else(|| "general".into()),
                })
                .collect();
            let own_sized: Vec<(String, OptionLine)> = sized
                .iter()
                .filter(|l| l.0 == id)
                .map(|l| {
                    (
                        l.1.clone(),
                        OptionLine {
                            ingredient_id: Some(l.2),
                            quantity: l.3,
                            name: l.4.clone(),
                            unit: l.5.clone(),
                            category: l.6.clone().unwrap_or_else(|| "general".into()),
                        },
                    )
                })
                .collect();
            let view = OptionView {
                id: s(id),
                name: name.clone(),
                kind,
                price: i64::from(price),
                group_id: group.map(s),
                effect,
                swap_category_id: swap_cat.map(s),
                swap_category_slug: slug,
                replaces: match (r_id, r_name, r_unit) {
                    (Some(i), Some(n), Some(u)) => Some(IngredientRef {
                        id: s(i),
                        name: n,
                        unit: u,
                    }),
                    _ => None,
                },
                ingredients: own
                    .iter()
                    .map(|l| IngredientLine {
                        id: l.ingredient_id.map(s),
                        name: l.name.clone(),
                        unit: l.unit.clone(),
                    })
                    .collect(),
                sized: own_sized
                    .iter()
                    .map(|(label, l)| SizedLine {
                        size_label: label.clone(),
                        id: l.ingredient_id.map(s).unwrap_or_default(),
                        name: l.name.clone(),
                        unit: l.unit.clone(),
                    })
                    .collect(),
            };
            self.options.insert(
                id,
                LoadedOption {
                    name,
                    name_translations: tr,
                    view,
                    lines: own,
                    sized: own_sized,
                },
            );
        }
        Ok(())
    }
}

/// Every recipe line of the items, per item, in (size label, ingredient name)
/// order.
///
/// The per-line query this replaced had no ORDER BY, so a recipe was read in
/// whatever order Postgres returned — the unique index's (size label,
/// ingredient name) when it used the index, insertion order on a small table.
/// The order matters in two places only: which line is the recipe's own when a
/// size has TWO lines of one swap category (the first), and the order of the
/// lines in a deduction snapshot. It is fixed here as the index's order.
async fn load_recipes(
    conn: &mut PgConnection,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<RecipeRow>>, AppError> {
    let rows: Vec<(
        Uuid,
        String,
        Option<Uuid>,
        f64,
        String,
        String,
        Option<String>,
    )> = sqlx::query_as(
        // A size's recipe: recipe_lines (owner_type item_size) on its
        // menu_item_sizes row (the old `menu_item_recipes`).
        "SELECT ms.menu_item_id, ms.label, rl.ingredient_id, rl.quantity::float8,
                oi.name, rl.unit, ic.slug
           FROM recipe_lines rl
           JOIN menu_item_sizes ms ON ms.id = rl.owner_id
           JOIN org_ingredients oi ON oi.id = rl.ingredient_id
           LEFT JOIN ingredient_categories ic ON ic.id = oi.category_id
          WHERE rl.owner_type = 'item_size' AND ms.menu_item_id = ANY($1)
          ORDER BY ms.menu_item_id, ms.label, oi.name",
    )
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?;
    let mut out: HashMap<Uuid, Vec<RecipeRow>> = HashMap::new();
    for (item, size_label, ingredient_id, quantity, name, unit, category) in rows {
        out.entry(item).or_default().push(RecipeRow {
            size_label,
            ingredient_id,
            quantity,
            name,
            unit,
            category,
        });
    }
    Ok(out)
}
