//! The OPT-IN compact shape of `GET /menu-items?full=true`
//! (`&shape=compact`). A client that does not ask gets the default array,
//! byte for byte as before: old tills parse it, and this module never runs for
//! them.
//!
//! What the default body repeats (prod copy, 69 items, 412 KB decoded):
//! * `pricing.groups[].options` — every option of every attached choice group,
//!   copied into every item that attaches the group: 178 KB, 43 % of the body,
//!   for two distinct groups. The options depend on (option, branch) only, so
//!   here they go out ONCE, in `group_options`, keyed by group id; each item's
//!   group keeps only what is per attachment (`min`, `is_required`,
//!   `included`) and per group (`effect`, `legacy_type`).
//! * fields no client reads (`org_id`, the `image` asset refs — 49 KB of signed
//!   URLs —, `created_at`, `updated_at`): left out.
//!
//! Everything else is the default row's field, unchanged, so a reader of the
//! default row reads a compact row the same way once it puts each group's
//! options back: `group.options = group_options[group.id]`.
use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::menu::handlers::{AddonSlot, ItemSize, MenuItemFull, MenuItemRecipe, OptionalField};

/// The `shape` value that opts in.
pub const SHAPE: &str = "compact";

/// The compact body: an object, so a client can never mistake it for the
/// default array.
#[derive(Serialize)]
pub struct CompactMenu<'a> {
    /// Always `"compact-1"`; a later compact revision gets a new marker.
    pub shape: &'static str,
    pub items: Vec<CompactItem<'a>>,
    /// Every attached choice group's options, once, keyed by group id, in the
    /// order an item's `pricing.groups[].options` lists them.
    pub group_options: BTreeMap<&'a str, &'a [madar_catalog::GroupOption]>,
}

/// A default row minus `org_id`, `image`, `created_at` and `updated_at`, with
/// its pricing groups' options moved to [`CompactMenu::group_options`].
#[derive(Serialize)]
pub struct CompactItem<'a> {
    id: Uuid,
    category_id: Option<Uuid>,
    name: &'a str,
    name_translations: &'a serde_json::Value,
    description: &'a Option<String>,
    description_translations: &'a serde_json::Value,
    #[serde(serialize_with = "url_ref")]
    image_url: &'a Option<String>,
    base_price: i32,
    is_active: bool,
    deleted_at: Option<DateTime<Utc>>,
    default_milk_addon_id: &'a Option<String>,
    kind: &'a str,
    sizes: &'a [ItemSize],
    all_sizes: &'a [ItemSize],
    addon_slots: &'a [AddonSlot],
    optional_fields: &'a [OptionalField],
    recipes: &'a [MenuItemRecipe],
    recipe_steps: &'a [crate::recipes::steps::RecipeStep],
    allowed_addon_ids: &'a [Uuid],
    #[serde(skip_serializing_if = "Option::is_none")]
    pricing: Option<CompactPricing<'a>>,
    meal: &'a Option<crate::combos::types::MealLink>,
    combo: &'a Option<crate::combos::types::ComboFeed>,
}

fn url_ref<S: serde::Serializer>(v: &&Option<String>, s: S) -> Result<S::Ok, S::Error> {
    crate::uploads::handlers::serialize_opt_url(v, s)
}

/// madar-catalog's `ItemView`, field for field, with [`CompactGroup`]s.
#[derive(Serialize)]
struct CompactPricing<'a> {
    id: &'a str,
    branch_price: Option<i64>,
    sizes: &'a [madar_catalog::SizeView],
    default_recipe_size: &'a Option<String>,
    recipe: &'a [madar_catalog::RecipeLine],
    bases: &'a [madar_catalog::BaseCandidates],
    optionals: &'a [madar_catalog::OptionalView],
    #[serde(skip_serializing_if = "Vec::is_empty")]
    groups: Vec<CompactGroup<'a>>,
}

/// madar-catalog's `GroupView` without `options`.
#[derive(Serialize)]
struct CompactGroup<'a> {
    id: &'a str,
    min: i64,
    is_required: bool,
    effect: &'a str,
    legacy_type: &'a Option<String>,
    included: &'a Option<Vec<String>>,
}

/// The compact body over the rows the default body is built from.
pub fn build(rows: &[MenuItemFull]) -> CompactMenu<'_> {
    let mut group_options = BTreeMap::new();
    let items = rows
        .iter()
        .map(|r| {
            let pricing = r.pricing.as_ref().map(|p| CompactPricing {
                id: &p.id,
                branch_price: p.branch_price,
                sizes: &p.sizes,
                default_recipe_size: &p.default_recipe_size,
                recipe: &p.recipe,
                bases: &p.bases,
                optionals: &p.optionals,
                groups: p
                    .groups
                    .iter()
                    .map(|g| {
                        // The same group's options are the same on every item
                        // (they depend on the option and the branch only).
                        group_options
                            .entry(g.id.as_str())
                            .or_insert(g.options.as_slice());
                        CompactGroup {
                            id: &g.id,
                            min: g.min,
                            is_required: g.is_required,
                            effect: &g.effect,
                            legacy_type: &g.legacy_type,
                            included: &g.included,
                        }
                    })
                    .collect(),
            });
            CompactItem {
                id: r.item.id,
                category_id: r.item.category_id,
                name: &r.item.name,
                name_translations: &r.item.name_translations,
                description: &r.item.description,
                description_translations: &r.item.description_translations,
                image_url: &r.item.image_url,
                base_price: r.item.base_price,
                is_active: r.item.is_active,
                deleted_at: r.item.deleted_at,
                default_milk_addon_id: &r.item.default_milk_addon_id,
                kind: &r.item.kind,
                sizes: &r.sizes,
                all_sizes: &r.all_sizes,
                addon_slots: &r.addon_slots,
                optional_fields: &r.optional_fields,
                recipes: &r.recipes,
                recipe_steps: &r.recipe_steps,
                allowed_addon_ids: &r.allowed_addon_ids,
                pricing,
                meal: &r.meal,
                combo: &r.combo,
            }
        })
        .collect();
    CompactMenu {
        shape: "compact-1",
        items,
        group_options,
    }
}
