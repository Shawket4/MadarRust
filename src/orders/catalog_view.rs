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
    BaseCandidate, BaseCandidates, CatalogView, IngredientLine, IngredientRef, ItemView,
    OptionView, OptionalView, RecipeLine, SizeView, SizedLine,
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
    pub fn new(branch: Option<Uuid>) -> Self {
        Self {
            branch,
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
        let rows: Vec<(Uuid, String, serde_json::Value, Option<i32>, bool)> = sqlx::query_as(
            "SELECT mi.id, mi.name, mi.name_translations, bmo.price_override,
                    COALESCE(bmo.is_available, true) = false
               FROM menu_items mi
               LEFT JOIN branch_menu_overrides bmo
                      ON bmo.menu_item_id = mi.id AND bmo.branch_id = $2
              WHERE mi.id = ANY($1) AND mi.deleted_at IS NULL",
        )
        .bind(ids)
        .bind(branch)
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
                "SELECT z.menu_item_id, z.label, z.price, z.is_active, bso.price_override, z.sort
               FROM menu_item_sizes z
               LEFT JOIN branch_menu_size_overrides bso
                      ON bso.branch_id = $2 AND bso.menu_item_id = z.menu_item_id
                     AND bso.size_label = z.label
              WHERE z.menu_item_id = ANY($1)
             UNION ALL
             SELECT bso.menu_item_id, bso.size_label, NULL, false, bso.price_override, NULL
               FROM branch_menu_size_overrides bso
              WHERE bso.branch_id = $2 AND bso.menu_item_id = ANY($1)
                AND NOT EXISTS (SELECT 1 FROM menu_item_sizes z
                                 WHERE z.menu_item_id = bso.menu_item_id
                                   AND z.label = bso.size_label)
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
            "SELECT m.id,
                    (SELECT rr.size_label FROM menu_item_recipes rr
                       LEFT JOIN menu_item_sizes sz
                              ON sz.menu_item_id = rr.menu_item_id AND sz.label = rr.size_label
                      WHERE rr.menu_item_id = m.id
                      ORDER BY sz.is_active IS NOT TRUE, sz.sort NULLS LAST, rr.size_label
                      LIMIT 1)
               FROM unnest($1::uuid[]) AS m(id)",
        )
        .bind(ids)
        .fetch_all(&mut *conn)
        .await?;

        let recipes = load_recipes(conn, ids).await?;

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
            "SELECT menu_item_id, id, name, price, org_ingredient_id, ingredient_name,
                    ingredient_unit, quantity_used::float8, size_label::text, name_translations
               FROM menu_item_optional_fields
              WHERE menu_item_id = ANY($1) AND is_active = true
              ORDER BY menu_item_id, name, id",
        )
        .bind(ids)
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
        let candidates: Vec<(Uuid, Uuid, String, String, i32, Option<Uuid>, Option<Uuid>)> =
            if base_ids.is_empty() {
                Vec::new()
            } else {
                sqlx::query_as(
                    "WITH c AS (
                         SELECT i.org_ingredient_id AS ing, i.addon_item_id AS id
                           FROM addon_item_ingredients i WHERE i.org_ingredient_id = ANY($1)
                         UNION
                         SELECT mo.replaces_ingredient_id, mo.id
                           FROM modifier_options mo WHERE mo.replaces_ingredient_id = ANY($1)
                     )
                     SELECT c.ing, a.id, a.name, a.type,
                            COALESCE(bao.price_override, a.default_price),
                            mo.group_id, mg.swap_category_id
                       FROM c
                       JOIN addon_items a ON a.id = c.id
                       LEFT JOIN modifier_options mo ON mo.id = a.id
                       LEFT JOIN modifier_groups mg ON mg.id = mo.group_id
                       LEFT JOIN branch_addon_overrides bao
                              ON bao.addon_item_id = a.id AND bao.branch_id = $2
                      ORDER BY c.ing, a.is_active DESC, mo.sort NULLS LAST, a.name, a.id",
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
        let rows: Vec<OptRow> = sqlx::query_as(
            "SELECT a.id, a.name, a.name_translations,
                    COALESCE(bao.price_override, a.default_price), a.type,
                    mo.group_id, g.effect, g.swap_category_id, c.slug,
                    ri.id, ri.name, ri.unit::text
               FROM addon_items a
               LEFT JOIN branch_addon_overrides bao
                      ON bao.addon_item_id = a.id AND bao.branch_id = $2
               LEFT JOIN modifier_options mo ON mo.id = a.id
               LEFT JOIN modifier_groups g ON g.id = mo.group_id
               LEFT JOIN ingredient_categories c ON c.id = g.swap_category_id
               LEFT JOIN org_ingredients ri ON ri.id = mo.replaces_ingredient_id
              WHERE a.id = ANY($1)",
        )
        .bind(ids)
        .bind(self.branch)
        .fetch_all(&mut *conn)
        .await?;

        // Ordered like the `/addon-items` payload: a swap option with several
        // lines swaps in its first.
        let lines: Vec<(Uuid, Option<Uuid>, f64, String, String, Option<String>)> = sqlx::query_as(
            "SELECT aii.addon_item_id, aii.org_ingredient_id, aii.quantity_used::float8,
                    aii.ingredient_name, aii.ingredient_unit, c.slug
               FROM addon_item_ingredients aii
               LEFT JOIN org_ingredients i ON i.id = aii.org_ingredient_id
               LEFT JOIN ingredient_categories c ON c.id = i.category_id
              WHERE aii.addon_item_id = ANY($1)
              ORDER BY aii.addon_item_id, aii.ingredient_name, aii.org_ingredient_id",
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
        "SELECT r.menu_item_id, r.size_label, r.org_ingredient_id, r.quantity_used::float8,
                r.ingredient_name, r.ingredient_unit, ic.slug
           FROM menu_item_recipes r
           LEFT JOIN org_ingredients i ON i.id = r.org_ingredient_id
           LEFT JOIN ingredient_categories ic ON ic.id = i.category_id
          WHERE r.menu_item_id = ANY($1)
          ORDER BY r.menu_item_id, r.size_label, r.ingredient_name",
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
