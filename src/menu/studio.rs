//! Menu Studio — additive Wave-2 API over the NEW unified menu tables.
//!
//! This module is ADDITIVE: it reads/writes the unified catalog tables created by
//! `migrations/20260703100000_menu_unification_expand.sql` (menu_item_sizes,
//! modifier_groups, modifier_options, menu_item_modifier_groups, recipe_lines,
//! menu_price_overrides, catalog_revision) and never touches the legacy menu/recipe
//! handlers, their tables, or their tests. The legacy handlers remain the live source
//! of truth until the Wave-2 FLIP; these endpoints are what the new Menu Studio screen
//! drives.
//!
//! Conventions honoured (see CONTRACT.md):
//! * Money is integer **piastres** end to end.
//! * Unknown cost is **NULL, never 0**. A `recipe_lines.quantity = 0` is a swap marker
//!   (records the swapped-in ingredient), NOT unknown cost — it contributes 0 to the
//!   rollup but does not make the rollup incomplete.
//! * Cost rollups are partial-tolerant: priced ingredients are summed even when others
//!   are unlinked/uncosted, and `cost_incomplete`/`cost_missing` flags the partial figure.
//!   `COALESCE(..., 0)` is never used to hide an unknown cost.
//! * Override resolution (price + availability, resolved independently) follows the
//!   documented precedence: branch_channel > branch > channel > catalog default.
//! * Every mutation bumps `catalog_revision` for the item's org so the offline POS resyncs.
//!
//! Costing note: the canonical `costing::service` engine still reads the LEGACY
//! `item_sizes`/`menu_item_recipes` tables (it is not repointed until the FLIP), so it
//! cannot see recipes written into `recipe_lines`. To make the studio's per-size and
//! per-option cost reflect the NEW tables, the rollup here reads `recipe_lines` directly
//! using the exact same NULL-aware piastres math (`round_piastres`, partial-tolerant)
//! that `costing::service` uses. We still call `costing::sku_costs_for_items` for the
//! item so the aggregate stays wired to the canonical engine for legacy-sourced sizes.

use actix_web::HttpMessage;
use actix_web::{HttpRequest, HttpResponse, web};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    auth::{guards::require_same_org, jwt::Claims},
    costing::service::round_piastres,
    errors::{AppError, AppErrorResponse},
    permissions::checker::check_permission,
};
use utoipa::ToSchema;

// ── Aggregate response shapes (CONTRACT §5.1) ─────────────────────────

/// One recipe line, hydrated with the ingredient name and a per-line cost.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RecipeLineOut {
    pub id: Uuid,
    pub ingredient_id: Uuid,
    pub ingredient_name: String,
    /// Base-unit, yield-normalized quantity, serialized as a string (numeric fidelity).
    pub quantity: String,
    pub unit: String,
    /// Cost of this line in piastres. `null` = UNKNOWN (ingredient unlinked/uncosted),
    /// never shown as 0. A priced line with `quantity = 0` (swap marker) costs 0.
    pub line_cost_piastres: Option<i64>,
}

/// A size (menu_item_sizes row) with its recipe and live cost.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SizeOut {
    pub id: Uuid,
    pub label: String,
    pub price: i32,
    pub sort: i32,
    pub is_active: bool,
    pub recipe: Vec<RecipeLineOut>,
    /// Recipe cost rollup in piastres over the priced ingredients. `null` when there is
    /// no recipe or nothing is priced; a partial rollup returns the sum-so-far with
    /// `cost_incomplete = true`.
    pub cost_piastres: Option<i64>,
    /// `true` when at least one recipe line is unlinked/uncosted (so `cost_piastres`, if
    /// present, is a partial figure rather than the full COGS).
    pub cost_incomplete: bool,
}

/// A modifier option inside an attached group.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ModifierOptionOut {
    pub id: Uuid,
    pub name: String,
    pub price: i32,
    pub is_default: bool,
    pub is_active: bool,
    /// `false` = the group offers this option but it is not enabled on this item
    /// (item's `included_option_ids` allowlist excludes it).
    pub included: bool,
    pub replaces_ingredient_id: Option<Uuid>,
    pub recipe: Vec<RecipeLineOut>,
    /// Option recipe cost in piastres (swap markers cost 0). `null` = unknown.
    pub cost_piastres: Option<i64>,
    pub cost_incomplete: bool,
}

/// A reusable modifier group attached to this item, with min/max/required resolved
/// from the attachment overrides (falling back to the group defaults).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ModifierGroupOut {
    pub attachment_id: Uuid,
    pub group_id: Uuid,
    pub name: String,
    pub name_translations: serde_json::Value,
    pub selection_type: String,
    pub legacy_addon_type: Option<String>,
    pub min: i32,
    pub max: Option<i32>,
    pub is_required: bool,
    pub sort: i32,
    pub options: Vec<ModifierOptionOut>,
}

/// A priced optional — a member of the item-private `Options` group
/// (a modifier_group with `legacy_addon_type IS NULL` owned by this item).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ItemOptionOut {
    pub id: Uuid,
    pub name: String,
    pub price: i32,
    pub is_active: bool,
    pub recipe: Vec<RecipeLineOut>,
    pub cost_piastres: Option<i64>,
    pub cost_incomplete: bool,
}

/// Per-branch/channel availability & price overrides for a single size.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SizeOverrideOut {
    pub size_id: Uuid,
    /// Override price in piastres; `null` = inherit the catalog default.
    pub price: Option<i32>,
    /// Override availability; `null` = inherit (defaults to available).
    pub is_available: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ChannelOverrideOut {
    pub channel: String,
    pub sizes: Vec<SizeOverrideOut>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BranchAvailabilityOut {
    pub branch_id: Uuid,
    pub sizes: Vec<SizeOverrideOut>,
    pub channels: Vec<ChannelOverrideOut>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AvailabilityOut {
    pub org_active: bool,
    pub branches: Vec<BranchAvailabilityOut>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UsedInBundleOut {
    pub bundle_id: Uuid,
    pub name: String,
}

/// The full item aggregate the one-page Menu Studio editor renders.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct StudioAggregate {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub name_translations: serde_json::Value,
    pub description: Option<String>,
    pub image_url: Option<String>,
    pub category_id: Option<Uuid>,
    pub is_active: bool,
    pub catalog_revision: i64,
    pub sizes: Vec<SizeOut>,
    pub modifier_groups: Vec<ModifierGroupOut>,
    pub options: Vec<ItemOptionOut>,
    pub availability: AvailabilityOut,
    pub used_in_bundles: Vec<UsedInBundleOut>,
}

// ── Request payloads ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SizeInput {
    pub label: String,
    pub price: i32,
    #[serde(default)]
    pub sort: i32,
    #[serde(default = "default_true")]
    pub is_active: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PutSizesRequest {
    pub sizes: Vec<SizeInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RecipeLineInput {
    pub ingredient_id: Uuid,
    /// Submitted quantity (in `unit`); server normalizes to the ingredient base unit.
    pub quantity: f64,
    pub unit: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PutRecipeRequest {
    pub lines: Vec<RecipeLineInput>,
}

/// Result of a recipe replace: the recomputed size cost.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RecipeCostResult {
    pub size_id: Uuid,
    pub recipe: Vec<RecipeLineOut>,
    pub cost_piastres: Option<i64>,
    pub cost_incomplete: bool,
    pub catalog_revision: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GroupAttachInput {
    pub group_id: Uuid,
    #[serde(default)]
    pub sort: i32,
    pub min_override: Option<i32>,
    pub max_override: Option<i32>,
    pub is_required_override: Option<bool>,
    /// `null` = offer all of the group's options; else the allowlisted subset.
    pub included_option_ids: Option<Vec<Uuid>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PutModifierGroupsRequest {
    pub groups: Vec<GroupAttachInput>,
}

// ── Helpers ──────────────────────────────────────────────────────────

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

/// Minimal item basics fetched up front for auth + the aggregate header.
struct ItemBasics {
    id: Uuid,
    org_id: Uuid,
    name: String,
    name_translations: serde_json::Value,
    description: Option<String>,
    image_url: Option<String>,
    category_id: Option<Uuid>,
    is_active: bool,
}

/// Load an item's basics (not soft-deleted). `None` = not found.
#[allow(clippy::type_complexity)]
async fn fetch_item_basics(pool: &PgPool, id: Uuid) -> Result<Option<ItemBasics>, AppError> {
    let row: Option<(
        Uuid,
        Uuid,
        String,
        serde_json::Value,
        Option<String>,
        Option<String>,
        Option<Uuid>,
        bool,
    )> = sqlx::query_as(
        "SELECT id, org_id, name, name_translations, description, image_url, category_id, is_active \
         FROM menu_items WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(
        |(id, org_id, name, name_translations, description, image_url, category_id, is_active)| {
            ItemBasics {
                id,
                org_id,
                name,
                name_translations,
                description,
                image_url,
                category_id,
                is_active,
            }
        },
    ))
}

/// Bump the org's catalog revision (monotonic; seeds at 1 on first write).
pub(crate) async fn bump_catalog_revision(
    conn: &mut sqlx::PgConnection,
    org_id: Uuid,
) -> Result<i64, AppError> {
    let rev: i64 = sqlx::query_scalar(
        "INSERT INTO catalog_revision (org_id, revision) VALUES ($1, 1) \
         ON CONFLICT (org_id) DO UPDATE \
             SET revision = catalog_revision.revision + 1, updated_at = now() \
         RETURNING revision",
    )
    .bind(org_id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(rev)
}

/// Current catalog revision for an org (0 if never seeded — an unwritten org is
/// revision 0 from the POS's perspective).
async fn current_catalog_revision(pool: &PgPool, org_id: Uuid) -> Result<i64, AppError> {
    let rev: Option<i64> =
        sqlx::query_scalar("SELECT revision FROM catalog_revision WHERE org_id = $1")
            .bind(org_id)
            .fetch_optional(pool)
            .await?;
    Ok(rev.unwrap_or(0))
}

/// A raw recipe_lines row joined to its ingredient (name + cost + org base unit).
pub(crate) struct RawRecipeLine {
    id: Uuid,
    ingredient_id: Uuid,
    ingredient_name: String,
    quantity: Decimal,
    unit: String,
    /// The org-default per-unit cost in piastres, or `None` when uncosted/unlinked.
    cost_per_unit: Option<Decimal>,
}

/// Load all recipe_lines for a set of owners of one `owner_type`, joined to
/// org_ingredients for the name + org-default cost. Keyed by `owner_id`.
///
/// Uses the org-default `org_ingredients.cost_per_unit` (the studio is an org-wide
/// authoring view with no branch context, mirroring `sku_costs_for_items(.., None)`).
#[allow(clippy::type_complexity)]
pub(crate) async fn load_recipe_lines(
    pool: &PgPool,
    owner_type: &str,
    owner_ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, Vec<RawRecipeLine>>, AppError> {
    let mut map: std::collections::HashMap<Uuid, Vec<RawRecipeLine>> =
        std::collections::HashMap::new();
    if owner_ids.is_empty() {
        return Ok(map);
    }

    let rows: Vec<(Uuid, Uuid, Uuid, String, Decimal, String, Option<Decimal>)> = sqlx::query_as(
        "SELECT rl.owner_id, rl.id, rl.ingredient_id, oi.name, rl.quantity, rl.unit, oi.cost_per_unit \
         FROM recipe_lines rl \
         JOIN org_ingredients oi ON oi.id = rl.ingredient_id \
         WHERE rl.owner_type = $1 AND rl.owner_id = ANY($2) \
         ORDER BY oi.name",
    )
    .bind(owner_type)
    .bind(owner_ids)
    .fetch_all(pool)
    .await?;

    for (owner_id, id, ingredient_id, ingredient_name, quantity, unit, cost_per_unit) in rows {
        map.entry(owner_id).or_default().push(RawRecipeLine {
            id,
            ingredient_id,
            ingredient_name,
            quantity,
            unit,
            cost_per_unit,
        });
    }
    Ok(map)
}

/// Roll up a set of recipe lines into (hydrated lines, total_cost, cost_incomplete),
/// using the exact NULL-aware piastres math the canonical engine uses:
///   * an uncosted/unlinked line contributes nothing to the sum AND sets incomplete;
///   * a costed line contributes `round(quantity * cost_per_unit)` (swap markers, qty=0,
///     contribute 0 without setting incomplete);
///   * total is `None` only when there is no line or NO line is costed.
pub(crate) fn rollup_recipe(lines: &[RawRecipeLine]) -> (Vec<RecipeLineOut>, Option<i64>, bool) {
    let mut out = Vec::with_capacity(lines.len());
    let mut sum = Decimal::ZERO;
    let mut any_priced = false;
    let mut incomplete = false;

    for l in lines {
        let line_cost = match l.cost_per_unit {
            Some(cpu) => {
                let c = round_piastres(l.quantity * cpu);
                sum += l.quantity * cpu;
                any_priced = true;
                Some(c)
            }
            None => {
                // Unlinked/uncosted ingredient → unknown line cost; flags the rollup.
                incomplete = true;
                None
            }
        };
        out.push(RecipeLineOut {
            id: l.id,
            ingredient_id: l.ingredient_id,
            ingredient_name: l.ingredient_name.clone(),
            quantity: l.quantity.normalize().to_string(),
            unit: l.unit.clone(),
            line_cost_piastres: line_cost,
        });
    }

    // `None` (unknown) unless at least one ingredient is priced — never fabricate 0.
    let total = if any_priced {
        Some(round_piastres(sum))
    } else {
        None
    };
    (out, total, incomplete)
}

// ── Aggregate builder ────────────────────────────────────────────────

/// Build the full `/studio` aggregate for one item. Assumes auth already checked.
#[allow(clippy::type_complexity)]
async fn build_studio_aggregate(
    pool: &PgPool,
    basics: &ItemBasics,
) -> Result<StudioAggregate, AppError> {
    let item_id = basics.id;
    let org_id = basics.org_id;

    // ── Sizes (menu_item_sizes) + their recipe_lines cost. ──
    // We ALSO consult the canonical engine (sku_costs_for_items) so legacy-sourced
    // sizes still resolve through it; when a size has recipe_lines those take
    // precedence for the studio's live number (the engine can't see the new table
    // until the FLIP). Keyed by size label.
    let size_rows: Vec<(Uuid, String, i32, i32, bool)> = sqlx::query_as(
        "SELECT id, label, price, sort, is_active FROM menu_item_sizes \
         WHERE menu_item_id = $1 ORDER BY sort, label",
    )
    .bind(item_id)
    .fetch_all(pool)
    .await?;

    let size_ids: Vec<Uuid> = size_rows.iter().map(|r| r.0).collect();
    let size_recipes = load_recipe_lines(pool, "item_size", &size_ids).await?;

    // Canonical per-size costs (legacy tables); used as a fallback for sizes with no
    // recipe_lines row so the aggregate stays consistent with the rest of the app.
    let sku_costs = crate::costing::sku_costs_for_items(pool, org_id, &[item_id], None).await?;
    let legacy_cost_by_label: std::collections::HashMap<String, (Option<i64>, bool)> = sku_costs
        .iter()
        .map(|s| (s.size_label.clone(), (s.cost, s.cost_missing)))
        .collect();

    let mut sizes = Vec::with_capacity(size_rows.len());
    for (id, label, price, sort, is_active) in size_rows {
        let lines = size_recipes.get(&id).map(|v| v.as_slice()).unwrap_or(&[]);
        let (recipe, cost, incomplete) = if lines.is_empty() {
            // No recipe_lines for this size → defer to the canonical engine's figure
            // for the matching label (legacy menu_item_recipes), else unknown.
            let (c, inc) = legacy_cost_by_label
                .get(&label)
                .copied()
                .unwrap_or((None, false));
            (Vec::new(), c, inc)
        } else {
            rollup_recipe(lines)
        };
        sizes.push(SizeOut {
            id,
            label,
            price,
            sort,
            is_active,
            recipe,
            cost_piastres: cost,
            cost_incomplete: incomplete,
        });
    }

    // ── Attached modifier groups (menu_item_modifier_groups → modifier_groups). ──
    // The item's own `Options` group (legacy_addon_type IS NULL) is surfaced under
    // `options`, NOT here; `modifier_groups` shows the reusable typed groups.
    let attach_rows: Vec<(
        Uuid,
        Uuid,
        String,
        serde_json::Value,
        String,
        Option<String>,
        i32,
        Option<i32>,
        bool,
        i32,
        Option<i32>,
        Option<i32>,
        Option<bool>,
        Option<Vec<Uuid>>,
    )> = sqlx::query_as(
        "SELECT mimg.id, mg.id, mg.name, mg.name_translations, mg.selection_type, \
                mg.legacy_addon_type, mg.min_selections, mg.max_selections, mg.is_required, \
                mimg.sort, mimg.min_override, mimg.max_override, mimg.is_required_override, \
                mimg.included_option_ids \
         FROM menu_item_modifier_groups mimg \
         JOIN modifier_groups mg ON mg.id = mimg.group_id \
         WHERE mimg.menu_item_id = $1 AND mg.legacy_addon_type IS NOT NULL \
         ORDER BY mimg.sort, mg.name",
    )
    .bind(item_id)
    .fetch_all(pool)
    .await?;

    // Collect the group ids so we can load all options + their recipes in bulk.
    let group_ids: Vec<Uuid> = attach_rows.iter().map(|r| r.1).collect();
    let (options_by_group, opt_recipes) = load_group_options(pool, &group_ids).await?;

    let mut modifier_groups = Vec::with_capacity(attach_rows.len());
    for r in attach_rows {
        let (
            attachment_id,
            group_id,
            name,
            name_translations,
            selection_type,
            legacy_addon_type,
            g_min,
            g_max,
            g_req,
            sort,
            min_override,
            max_override,
            is_required_override,
            included_option_ids,
        ) = r;

        // Resolve min/max/required from the attachment overrides (fall back to group).
        let min = min_override.unwrap_or(g_min);
        let max = match max_override {
            Some(m) => Some(m),
            None => g_max,
        };
        let is_required = is_required_override.unwrap_or(g_req);

        let opts = options_by_group.get(&group_id).cloned().unwrap_or_default();
        let mut options = Vec::with_capacity(opts.len());
        for o in opts {
            // included = allowlist NULL (all) OR id present in the allowlist.
            let included = match &included_option_ids {
                None => true,
                Some(ids) => ids.contains(&o.id),
            };
            let lines = opt_recipes.get(&o.id).map(|v| v.as_slice()).unwrap_or(&[]);
            let (recipe, cost, incomplete) = rollup_recipe(lines);
            options.push(ModifierOptionOut {
                id: o.id,
                name: o.name,
                price: o.price,
                is_default: o.is_default,
                is_active: o.is_active,
                included,
                replaces_ingredient_id: o.replaces_ingredient_id,
                recipe,
                cost_piastres: cost,
                cost_incomplete: incomplete,
            });
        }

        modifier_groups.push(ModifierGroupOut {
            attachment_id,
            group_id,
            name,
            name_translations,
            selection_type,
            legacy_addon_type,
            min,
            max,
            is_required,
            sort,
            options,
        });
    }

    // ── Priced optionals: the item-private `Options` group (legacy_addon_type NULL). ──
    let options = fetch_item_options(pool, item_id).await?;

    // ── Availability: org_active + per-branch/channel from menu_price_overrides. ──
    let availability = fetch_availability(pool, org_id, &size_ids, basics.is_active).await?;

    // ── Bundles this item is a component of. ──
    let bundle_rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT DISTINCT b.id, b.name FROM bundle_components bc \
         JOIN bundles b ON b.id = bc.bundle_id \
         WHERE bc.item_id = $1 ORDER BY b.name",
    )
    .bind(item_id)
    .fetch_all(pool)
    .await?;
    let used_in_bundles = bundle_rows
        .into_iter()
        .map(|(bundle_id, name)| UsedInBundleOut { bundle_id, name })
        .collect();

    let catalog_revision = current_catalog_revision(pool, org_id).await?;

    Ok(StudioAggregate {
        id: basics.id,
        org_id,
        name: basics.name.clone(),
        name_translations: basics.name_translations.clone(),
        description: basics.description.clone(),
        image_url: basics.image_url.clone(),
        category_id: basics.category_id,
        is_active: basics.is_active,
        catalog_revision,
        sizes,
        modifier_groups,
        options,
        availability,
        used_in_bundles,
    })
}

/// Raw modifier_option (without cost) used while assembling groups.
#[derive(Clone)]
struct RawOption {
    id: Uuid,
    name: String,
    price: i32,
    is_default: bool,
    is_active: bool,
    replaces_ingredient_id: Option<Uuid>,
}

/// Load every option of a set of groups + all those options' recipe_lines, in bulk.
#[allow(clippy::type_complexity)]
async fn load_group_options(
    pool: &PgPool,
    group_ids: &[Uuid],
) -> Result<
    (
        std::collections::HashMap<Uuid, Vec<RawOption>>,
        std::collections::HashMap<Uuid, Vec<RawRecipeLine>>,
    ),
    AppError,
> {
    let mut by_group: std::collections::HashMap<Uuid, Vec<RawOption>> =
        std::collections::HashMap::new();
    if group_ids.is_empty() {
        return Ok((by_group, std::collections::HashMap::new()));
    }

    let opt_rows: Vec<(Uuid, Uuid, String, i32, bool, bool, Option<Uuid>)> = sqlx::query_as(
        "SELECT group_id, id, name, price, is_default, is_active, replaces_ingredient_id \
         FROM modifier_options WHERE group_id = ANY($1) ORDER BY sort, name",
    )
    .bind(group_ids)
    .fetch_all(pool)
    .await?;

    let mut option_ids = Vec::new();
    for (group_id, id, name, price, is_default, is_active, replaces_ingredient_id) in opt_rows {
        option_ids.push(id);
        by_group.entry(group_id).or_default().push(RawOption {
            id,
            name,
            price,
            is_default,
            is_active,
            replaces_ingredient_id,
        });
    }

    let opt_recipes = load_recipe_lines(pool, "modifier_option", &option_ids).await?;
    Ok((by_group, opt_recipes))
}

/// Fetch the item-private priced optionals (its `Options` group: a modifier_group
/// with `legacy_addon_type IS NULL` attached to exactly this item), hydrated with cost.
pub(crate) async fn fetch_item_options(
    pool: &PgPool,
    item_id: Uuid,
) -> Result<Vec<ItemOptionOut>, AppError> {
    // The per-item Options group is the attached group whose legacy_addon_type IS NULL.
    let group_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT mg.id FROM menu_item_modifier_groups mimg \
         JOIN modifier_groups mg ON mg.id = mimg.group_id \
         WHERE mimg.menu_item_id = $1 AND mg.legacy_addon_type IS NULL \
         ORDER BY mimg.sort LIMIT 1",
    )
    .bind(item_id)
    .fetch_optional(pool)
    .await?;

    let Some(group_id) = group_id else {
        return Ok(Vec::new());
    };

    let (by_group, opt_recipes) = load_group_options(pool, &[group_id]).await?;
    let opts = by_group.get(&group_id).cloned().unwrap_or_default();

    let mut out = Vec::with_capacity(opts.len());
    for o in opts {
        let lines = opt_recipes.get(&o.id).map(|v| v.as_slice()).unwrap_or(&[]);
        let (recipe, cost, incomplete) = rollup_recipe(lines);
        out.push(ItemOptionOut {
            id: o.id,
            name: o.name,
            price: o.price,
            is_active: o.is_active,
            recipe,
            cost_piastres: cost,
            cost_incomplete: incomplete,
        });
    }
    Ok(out)
}

/// Assemble the availability block from menu_price_overrides targeting this item's
/// sizes (target_type='menu_item_size'). Branch-scoped and branch_channel-scoped rows
/// are grouped by branch. Channel-only (org-wide) overrides are intentionally NOT
/// surfaced here: they have no branch, so they apply org-wide per §3 resolution and are
/// consumed by the POS sync path, not by this per-branch editor view.
#[allow(clippy::type_complexity)]
async fn fetch_availability(
    pool: &PgPool,
    _org_id: Uuid,
    size_ids: &[Uuid],
    org_active: bool,
) -> Result<AvailabilityOut, AppError> {
    if size_ids.is_empty() {
        return Ok(AvailabilityOut {
            org_active,
            branches: Vec::new(),
        });
    }

    // Pull every branch / branch_channel override for these sizes.
    let rows: Vec<(
        String,
        Option<Uuid>,
        Option<String>,
        Uuid,
        Option<i32>,
        Option<bool>,
    )> = sqlx::query_as(
        "SELECT scope, branch_id, channel::text, target_id, price, is_available \
             FROM menu_price_overrides \
             WHERE target_type = 'menu_item_size' AND target_id = ANY($1) \
               AND scope IN ('branch','branch_channel') \
             ORDER BY branch_id, channel, target_id",
    )
    .bind(size_ids)
    .fetch_all(pool)
    .await?;

    // branch_id -> (branch-level size overrides, channel -> size overrides)
    #[allow(clippy::type_complexity)]
    let mut by_branch: std::collections::HashMap<
        Uuid,
        (
            Vec<SizeOverrideOut>,
            std::collections::HashMap<String, Vec<SizeOverrideOut>>,
        ),
    > = std::collections::HashMap::new();

    for (scope, branch_id, channel, target_id, price, is_available) in rows {
        let Some(branch_id) = branch_id else { continue };
        let entry = by_branch.entry(branch_id).or_default();
        let sv = SizeOverrideOut {
            size_id: target_id,
            price,
            is_available,
        };
        if scope == "branch" {
            entry.0.push(sv);
        } else if let Some(ch) = channel {
            entry.1.entry(ch).or_default().push(sv);
        }
    }

    let mut branches: Vec<BranchAvailabilityOut> = by_branch
        .into_iter()
        .map(|(branch_id, (sizes, chan_map))| {
            let mut channels: Vec<ChannelOverrideOut> = chan_map
                .into_iter()
                .map(|(channel, sizes)| ChannelOverrideOut { channel, sizes })
                .collect();
            channels.sort_by(|a, b| a.channel.cmp(&b.channel));
            BranchAvailabilityOut {
                branch_id,
                sizes,
                channels,
            }
        })
        .collect();
    branches.sort_by(|a, b| a.branch_id.cmp(&b.branch_id));

    Ok(AvailabilityOut {
        org_active,
        branches,
    })
}

// ── Endpoint 1: GET /menu-items/{id}/studio ──────────────────────────

#[utoipa::path(
    get,
    path = "/menu-items/{id}/studio",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Menu item ID")),
    responses((status = 200, description = "Full Menu Studio item aggregate", body = StudioAggregate), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_studio(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;

    let basics = fetch_item_basics(pool.get_ref(), *id)
        .await?
        .ok_or_else(|| AppError::NotFound("Menu item not found".into()))?;
    require_same_org(&claims, Some(basics.org_id))?;

    let agg = build_studio_aggregate(pool.get_ref(), &basics).await?;
    Ok(HttpResponse::Ok().json(agg))
}

// ── Endpoint 2: PUT /menu-items/{id}/sizes ───────────────────────────

#[utoipa::path(
    put,
    path = "/menu-items/{id}/sizes",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Menu item ID")),
    request_body = PutSizesRequest,
    responses((status = 200, description = "Sizes replaced", body = StudioAggregate), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_sizes(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
    body: web::Json<PutSizesRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let basics = fetch_item_basics(pool.get_ref(), *id)
        .await?
        .ok_or_else(|| AppError::NotFound("Menu item not found".into()))?;
    require_same_org(&claims, Some(basics.org_id))?;

    let item_id = basics.id;
    let incoming = body.into_inner().sizes;

    // Reject duplicate labels in the payload (UNIQUE(menu_item_id,label) would 500).
    let mut seen = std::collections::HashSet::new();
    for s in &incoming {
        if !seen.insert(s.label.clone()) {
            return Err(AppError::BadRequest(format!(
                "Duplicate size label '{}' in request",
                s.label
            )));
        }
    }

    let mut tx = pool.begin().await?;

    // Existing sizes for this item (id + label).
    let existing: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, label FROM menu_item_sizes WHERE menu_item_id = $1")
            .bind(item_id)
            .fetch_all(&mut *tx)
            .await?;
    let incoming_labels: std::collections::HashSet<&str> =
        incoming.iter().map(|s| s.label.as_str()).collect();

    // Upsert each incoming size by (menu_item_id, label).
    for s in &incoming {
        sqlx::query(
            "INSERT INTO menu_item_sizes (menu_item_id, label, price, sort, is_active) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (menu_item_id, label) DO UPDATE \
                 SET price = EXCLUDED.price, sort = EXCLUDED.sort, is_active = EXCLUDED.is_active",
        )
        .bind(item_id)
        .bind(&s.label)
        .bind(s.price)
        .bind(s.sort)
        .bind(s.is_active)
        .execute(&mut *tx)
        .await?;
    }

    // Sizes present before but dropped from the set: soft-deactivate if they have order
    // history (order_items.size_label references the label), else hard-delete.
    for (sid, label) in &existing {
        if incoming_labels.contains(label.as_str()) {
            continue;
        }
        let history: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM order_items \
             WHERE menu_item_id = $1 AND size_label = $2",
        )
        .bind(item_id)
        .bind(label)
        .fetch_one(&mut *tx)
        .await?;

        if history > 0 {
            // Immutable order history keeps referencing this label → keep the row,
            // just deactivate it (never delete a size with sales).
            sqlx::query("UPDATE menu_item_sizes SET is_active = false WHERE id = $1")
                .bind(sid)
                .execute(&mut *tx)
                .await?;
        } else {
            // No history → safe to remove (recipe_lines for this size go too).
            sqlx::query(
                "DELETE FROM recipe_lines WHERE owner_type = 'item_size' AND owner_id = $1",
            )
            .bind(sid)
            .execute(&mut *tx)
            .await?;
            sqlx::query("DELETE FROM menu_item_sizes WHERE id = $1")
                .bind(sid)
                .execute(&mut *tx)
                .await?;
        }
    }

    bump_catalog_revision(&mut tx, basics.org_id).await?;
    tx.commit().await?;

    let agg = build_studio_aggregate(pool.get_ref(), &basics).await?;
    Ok(HttpResponse::Ok().json(agg))
}

// ── Endpoint 3: PUT /menu-item-sizes/{size_id}/recipe ────────────────

#[utoipa::path(
    put,
    path = "/menu-item-sizes/{size_id}/recipe",
    tag = "menu",
    params(("size_id" = Uuid, Path, description = "menu_item_sizes ID")),
    request_body = PutRecipeRequest,
    responses((status = 200, description = "Size recipe replaced; recomputed cost", body = RecipeCostResult), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_size_recipe(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    size_id: web::Path<Uuid>,
    body: web::Json<PutRecipeRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    // Resolve the size → its item → org (for auth + normalization scope).
    let owner: Option<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT mi.id, mi.org_id FROM menu_item_sizes s \
         JOIN menu_items mi ON mi.id = s.menu_item_id \
         WHERE s.id = $1 AND mi.deleted_at IS NULL",
    )
    .bind(*size_id)
    .fetch_optional(pool.get_ref())
    .await?;
    let (item_id, org_id) = owner.ok_or_else(|| AppError::NotFound("Size not found".into()))?;
    require_same_org(&claims, Some(org_id))?;

    let lines = body.into_inner().lines;

    // Reject duplicate ingredient ids (UNIQUE(owner_type,owner_id,ingredient_id)).
    let mut seen = std::collections::HashSet::new();
    for l in &lines {
        if !seen.insert(l.ingredient_id) {
            return Err(AppError::BadRequest(
                "Duplicate ingredient in recipe".into(),
            ));
        }
    }

    // Normalize each line to the ingredient base unit BEFORE opening the tx (the helper
    // takes a pool). Rejects an ingredient from another org / a too-small quantity.
    let mut normalized: Vec<(Uuid, f64, String)> = Vec::with_capacity(lines.len());
    for l in &lines {
        let (base_unit, qty) = crate::recipes::handlers::normalize_recipe_unit(
            pool.get_ref(),
            org_id,
            Some(l.ingredient_id),
            &l.unit,
            l.quantity,
        )
        .await?;
        normalized.push((l.ingredient_id, qty, base_unit));
    }

    let mut tx = pool.begin().await?;

    // Replace the size's recipe lines.
    sqlx::query("DELETE FROM recipe_lines WHERE owner_type = 'item_size' AND owner_id = $1")
        .bind(*size_id)
        .execute(&mut *tx)
        .await?;

    for (ingredient_id, qty, base_unit) in &normalized {
        sqlx::query(
            "INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit) \
             VALUES ('item_size', $1, $2, $3, $4)",
        )
        .bind(*size_id)
        .bind(ingredient_id)
        .bind(Decimal::try_from(*qty).unwrap_or(Decimal::ZERO))
        .bind(base_unit)
        .execute(&mut *tx)
        .await?;
    }

    let revision = bump_catalog_revision(&mut tx, org_id).await?;
    tx.commit().await?;

    // Recompute the size cost from the freshly written recipe_lines.
    let recipes = load_recipe_lines(pool.get_ref(), "item_size", &[*size_id]).await?;
    let lines = recipes.get(&*size_id).map(|v| v.as_slice()).unwrap_or(&[]);
    let (recipe, cost, incomplete) = rollup_recipe(lines);
    let _ = item_id;

    Ok(HttpResponse::Ok().json(RecipeCostResult {
        size_id: *size_id,
        recipe,
        cost_piastres: cost,
        cost_incomplete: incomplete,
        catalog_revision: revision,
    }))
}

// ── Endpoint 4: PUT /menu-items/{id}/modifier-groups ─────────────────

#[utoipa::path(
    put,
    path = "/menu-items/{id}/modifier-groups",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Menu item ID")),
    request_body = PutModifierGroupsRequest,
    responses((status = 200, description = "Group attachments replaced", body = StudioAggregate), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_modifier_groups(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
    body: web::Json<PutModifierGroupsRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let basics = fetch_item_basics(pool.get_ref(), *id)
        .await?
        .ok_or_else(|| AppError::NotFound("Menu item not found".into()))?;
    require_same_org(&claims, Some(basics.org_id))?;

    let item_id = basics.id;
    let attaches = body.into_inner().groups;

    // Reject duplicate group ids (UNIQUE(menu_item_id,group_id)).
    let mut seen = std::collections::HashSet::new();
    for a in &attaches {
        if !seen.insert(a.group_id) {
            return Err(AppError::BadRequest("Duplicate group in attach-set".into()));
        }
    }

    // All referenced groups must belong to this item's org (a reusable group is
    // org-scoped). This also 400s an unknown group id early rather than 500-ing on FK.
    if !attaches.is_empty() {
        let group_ids: Vec<Uuid> = attaches.iter().map(|a| a.group_id).collect();
        let valid: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM modifier_groups WHERE id = ANY($1) AND org_id = $2",
        )
        .bind(&group_ids)
        .bind(basics.org_id)
        .fetch_one(pool.get_ref())
        .await?;
        if valid != group_ids.len() as i64 {
            return Err(AppError::BadRequest(
                "One or more modifier groups do not exist in this organization".into(),
            ));
        }
    }

    let mut tx = pool.begin().await?;

    // Delete-then-insert the reusable (typed) attachments for this item. The item's own
    // `Options` group attachment (legacy_addon_type NULL) is owned by the options
    // endpoint, so it is NOT touched here — only typed groups are replaced.
    sqlx::query(
        "DELETE FROM menu_item_modifier_groups mimg \
         USING modifier_groups mg \
         WHERE mimg.group_id = mg.id \
           AND mimg.menu_item_id = $1 \
           AND mg.legacy_addon_type IS NOT NULL",
    )
    .bind(item_id)
    .execute(&mut *tx)
    .await?;

    for a in &attaches {
        // legacy_origin stays NULL for new attaches (only the backfill sets provenance).
        sqlx::query(
            "INSERT INTO menu_item_modifier_groups \
                 (menu_item_id, group_id, sort, min_override, max_override, \
                  is_required_override, included_option_ids) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(item_id)
        .bind(a.group_id)
        .bind(a.sort)
        .bind(a.min_override)
        .bind(a.max_override)
        .bind(a.is_required_override)
        .bind(a.included_option_ids.as_deref())
        .execute(&mut *tx)
        .await?;
    }

    bump_catalog_revision(&mut tx, basics.org_id).await?;
    tx.commit().await?;

    let agg = build_studio_aggregate(pool.get_ref(), &basics).await?;
    Ok(HttpResponse::Ok().json(agg))
}

// ── Endpoint 5: POST /menu-items/{id}/duplicate ──────────────────────

#[utoipa::path(
    post,
    path = "/menu-items/{id}/duplicate",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Menu item ID to duplicate")),
    responses((status = 201, description = "Deep-copied item; returns the new item's studio aggregate", body = StudioAggregate), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
#[allow(clippy::type_complexity)]
pub async fn duplicate_item(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let basics = fetch_item_basics(pool.get_ref(), *id)
        .await?
        .ok_or_else(|| AppError::NotFound("Menu item not found".into()))?;
    require_same_org(&claims, Some(basics.org_id))?;

    let src_item = basics.id;
    let org_id = basics.org_id;

    let mut tx = pool.begin().await?;

    // 1. New menu_item copying the basics (name suffixed to avoid confusion; starts
    //    from base_price of the source; a fresh row, no order history).
    let new_item: Uuid = sqlx::query_scalar(
        "INSERT INTO menu_items \
             (org_id, category_id, name, description, image_url, base_price, is_active, \
              name_translations, description_translations) \
         SELECT org_id, category_id, name || ' (Copy)', description, image_url, base_price, \
                is_active, name_translations, description_translations \
         FROM menu_items WHERE id = $1 \
         RETURNING id",
    )
    .bind(src_item)
    .fetch_one(&mut *tx)
    .await?;

    // 2. Copy sizes with NEW ids, remembering old→new so we can clone recipes + overrides.
    let src_sizes: Vec<(Uuid, String, i32, i32, bool)> = sqlx::query_as(
        "SELECT id, label, price, sort, is_active FROM menu_item_sizes WHERE menu_item_id = $1",
    )
    .bind(src_item)
    .fetch_all(&mut *tx)
    .await?;

    let mut size_map: std::collections::HashMap<Uuid, Uuid> = std::collections::HashMap::new();
    for (old_id, label, price, sort, is_active) in &src_sizes {
        let new_size: Uuid = sqlx::query_scalar(
            "INSERT INTO menu_item_sizes (menu_item_id, label, price, sort, is_active) \
             VALUES ($1, $2, $3, $4, $5) RETURNING id",
        )
        .bind(new_item)
        .bind(label)
        .bind(price)
        .bind(sort)
        .bind(is_active)
        .fetch_one(&mut *tx)
        .await?;
        size_map.insert(*old_id, new_size);

        // Copy this size's recipe_lines onto the new size id.
        sqlx::query(
            "INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit) \
             SELECT 'item_size', $1, ingredient_id, quantity, unit \
             FROM recipe_lines WHERE owner_type = 'item_size' AND owner_id = $2",
        )
        .bind(new_size)
        .bind(old_id)
        .execute(&mut *tx)
        .await?;
    }

    // 3. Copy the item-private `Options` group (legacy_addon_type NULL) with a NEW group
    //    + NEW option ids (a duplicate is a new item with no order history → fresh uuids,
    //    NOT stable), and clone each option's recipe_lines. Track old→new option ids so
    //    overrides that target options can be re-pointed.
    let mut option_map: std::collections::HashMap<Uuid, Uuid> = std::collections::HashMap::new();

    let src_options_group: Option<(Uuid, i32, Option<i32>, Option<i32>, Option<bool>)> =
        sqlx::query_as(
            "SELECT mg.id, mimg.sort, mimg.min_override, mimg.max_override, mimg.is_required_override \
             FROM menu_item_modifier_groups mimg \
             JOIN modifier_groups mg ON mg.id = mimg.group_id \
             WHERE mimg.menu_item_id = $1 AND mg.legacy_addon_type IS NULL \
             ORDER BY mimg.sort LIMIT 1",
        )
        .bind(src_item)
        .fetch_optional(&mut *tx)
        .await?;

    if let Some((src_group, sort, min_o, max_o, req_o)) = src_options_group {
        // New Options group (copy name/translations/selection config).
        let new_group: Uuid = sqlx::query_scalar(
            "INSERT INTO modifier_groups \
                 (org_id, name, name_translations, selection_type, min_selections, \
                  max_selections, is_required, sort, is_active, legacy_addon_type) \
             SELECT org_id, name, name_translations, selection_type, min_selections, \
                    max_selections, is_required, sort, is_active, NULL \
             FROM modifier_groups WHERE id = $1 RETURNING id",
        )
        .bind(src_group)
        .fetch_one(&mut *tx)
        .await?;

        // Copy each option with a fresh id, recording old→new.
        let src_opts: Vec<(
            Uuid,
            String,
            serde_json::Value,
            i32,
            i32,
            bool,
            bool,
            Option<Uuid>,
            String,
        )> = sqlx::query_as(
            "SELECT id, name, name_translations, price, sort, is_default, is_active, \
                    replaces_ingredient_id, legacy_source \
             FROM modifier_options WHERE group_id = $1 ORDER BY sort, name",
        )
        .bind(src_group)
        .fetch_all(&mut *tx)
        .await?;

        for (old_opt, name, name_tr, price, sort, is_default, is_active, replaces, legacy_source) in
            &src_opts
        {
            let new_opt: Uuid = sqlx::query_scalar(
                "INSERT INTO modifier_options \
                     (group_id, name, name_translations, price, sort, is_default, is_active, \
                      replaces_ingredient_id, legacy_source) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING id",
            )
            .bind(new_group)
            .bind(name)
            .bind(name_tr)
            .bind(price)
            .bind(sort)
            .bind(is_default)
            .bind(is_active)
            .bind(*replaces)
            .bind(legacy_source)
            .fetch_one(&mut *tx)
            .await?;
            option_map.insert(*old_opt, new_opt);

            // Clone this option's recipe_lines onto the new option id.
            sqlx::query(
                "INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit) \
                 SELECT 'modifier_option', $1, ingredient_id, quantity, unit \
                 FROM recipe_lines WHERE owner_type = 'modifier_option' AND owner_id = $2",
            )
            .bind(new_opt)
            .bind(old_opt)
            .execute(&mut *tx)
            .await?;
        }

        // Attach the new Options group to the new item (legacy_origin NULL; included NULL
        // = all options, matching a per-item option set).
        sqlx::query(
            "INSERT INTO menu_item_modifier_groups \
                 (menu_item_id, group_id, sort, min_override, max_override, is_required_override) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(new_item)
        .bind(new_group)
        .bind(sort)
        .bind(min_o)
        .bind(max_o)
        .bind(req_o)
        .execute(&mut *tx)
        .await?;
    }

    // 4. Copy the reusable (typed) group attachments. These reference SHARED reusable
    //    groups (org-scoped, not per-item), so we keep the same group_id but must
    //    re-map any included_option_ids that happen to point at copied option ids
    //    (typed groups reference shared options → their ids are unchanged, so the
    //    allowlist copies verbatim). legacy_origin stays NULL for the copy.
    sqlx::query(
        "INSERT INTO menu_item_modifier_groups \
             (menu_item_id, group_id, sort, min_override, max_override, \
              is_required_override, included_option_ids) \
         SELECT $1, mimg.group_id, mimg.sort, mimg.min_override, mimg.max_override, \
                mimg.is_required_override, mimg.included_option_ids \
         FROM menu_item_modifier_groups mimg \
         JOIN modifier_groups mg ON mg.id = mimg.group_id \
         WHERE mimg.menu_item_id = $2 AND mg.legacy_addon_type IS NOT NULL",
    )
    .bind(new_item)
    .bind(src_item)
    .execute(&mut *tx)
    .await?;

    // 5. Copy menu_price_overrides that target the copied sizes/options, re-pointed to
    //    the new target ids. Iterate the maps so each new target inherits the source's
    //    branch/channel override rows.
    for (old_id, new_id) in size_map.iter() {
        clone_overrides(&mut tx, "menu_item_size", *old_id, *new_id).await?;
    }
    for (old_id, new_id) in option_map.iter() {
        clone_overrides(&mut tx, "modifier_option", *old_id, *new_id).await?;
    }

    bump_catalog_revision(&mut tx, org_id).await?;
    tx.commit().await?;

    // Return the new item's full studio aggregate.
    let new_basics = fetch_item_basics(pool.get_ref(), new_item)
        .await?
        .ok_or(AppError::Internal)?;
    let agg = build_studio_aggregate(pool.get_ref(), &new_basics).await?;
    Ok(HttpResponse::Created().json(agg))
}

/// Copy every menu_price_overrides row targeting `old_target` onto `new_target`,
/// preserving scope/branch/channel/price/is_available.
async fn clone_overrides(
    tx: &mut sqlx::PgConnection,
    target_type: &str,
    old_target: Uuid,
    new_target: Uuid,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO menu_price_overrides \
             (scope, branch_id, channel, target_type, target_id, price, is_available) \
         SELECT scope, branch_id, channel, target_type, $1, price, is_available \
         FROM menu_price_overrides \
         WHERE target_type = $2 AND target_id = $3",
    )
    .bind(new_target)
    .bind(target_type)
    .bind(old_target)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests;
