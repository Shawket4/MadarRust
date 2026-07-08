//! POS catalog sync — additive Wave-2 read endpoint over the NEW unified menu
//! tables (CONTRACT.md §5.2).
//!
//! This module is ADDITIVE and READ-ONLY: it reads the unified catalog tables
//! created by `migrations/20260703100000_menu_unification_expand.sql`
//! (menu_item_sizes, modifier_groups, modifier_options,
//! menu_item_modifier_groups, recipe_lines, menu_price_overrides,
//! catalog_revision) and never touches the legacy menu/recipe handlers, their
//! tables, or their tests. It matches the conventions of the sibling
//! `menu::studio` module exactly (runtime `sqlx::query`/`query_as` with `.bind()`,
//! ToSchema response structs, utoipa annotations, `extract_claims` + permission +
//! `require_same_org` auth, `current_catalog_revision` gating).
//!
//! ONE endpoint:
//!   `GET /catalog/sync?branch_id={uuid}&channel={channel}&since={revision}`
//! returns the unified catalog snapshot a POS device caches, with per-size and
//! per-option **price and availability already resolved** for that
//! `(branch, channel)` per the documented override precedence (CONTRACT §3):
//!
//!   effective_price(target) = COALESCE(
//!       branch_channel.price, branch.price, channel.price, catalog default)
//!   effective_avail(target) = COALESCE(
//!       branch_channel.is_available, branch.is_available, channel.is_available, TRUE)
//!
//! Price and availability resolve INDEPENDENTLY (a row may set one and inherit the
//! other); price always falls back to the catalog default (never NULL); avail
//! defaults to TRUE. Overrides live in `menu_price_overrides`
//! (target_type 'menu_item_size' | 'modifier_option', target_id = the size/option id).
//!
//! DOCUMENTED CHOICES:
//! * **Unavailable SKUs are INCLUDED with `is_available: false`** (not filtered
//!   out). This is simpler than omitting them and lets the POS grey out a
//!   temporarily-unavailable size/option rather than have it vanish (and lets an
//!   offline device keep a stable id set across resyncs). Only *inactive* rows
//!   (`is_active = false`, soft-deleted items, unattached groups, allowlist-excluded
//!   options) are omitted — availability is an override flag, activeness is catalog state.
//! * **`ingredients[]` = the active `org_ingredients` referenced by any returned
//!   recipe line** (id, name, unit). We scope it to referenced ingredients (not all
//!   org ingredients) so the POS payload stays lean — a device only needs the
//!   ingredients its recipes deduct.
//! * **`changed` gating:** when `since` is provided and equals the org's current
//!   `catalog_revision`, the response is `{catalog_revision, changed:false}` with
//!   empty `items`/`ingredients` (a cheap poll). Otherwise `changed:true` + the full
//!   payload. `since` omitted ⇒ always a full payload.
//!
//! Money is integer **piastres** end to end. Cost is intentionally OUT OF SCOPE
//! here — this is the customer-facing price/availability catalog, not the cost engine.

use actix_web::HttpMessage;
use actix_web::{HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    auth::{guards::require_same_org, jwt::Claims},
    errors::{AppError, AppErrorResponse},
    permissions::checker::check_permission,
};
use utoipa::ToSchema;

// ── Response shapes (CONTRACT §5.2) ──────────────────────────────────

/// One recipe line of a modifier option: which ingredient the option deducts
/// (or swaps in, when `quantity = 0`). Base-unit, yield-normalized values.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SyncRecipeLine {
    pub ingredient_id: Uuid,
    /// Base-unit quantity, serialized as a string for numeric fidelity.
    pub quantity: String,
    pub unit: String,
}

/// A size (menu_item_sizes row) with its price/availability resolved for the
/// requested `(branch, channel)`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SyncSize {
    pub id: Uuid,
    pub label: String,
    /// Effective price in piastres (branch_channel → branch → channel → catalog default).
    pub price: i32,
    /// Effective availability (branch_channel → branch → channel → TRUE).
    pub is_available: bool,
}

/// A modifier option, with price/availability resolved for `(branch, channel)`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SyncOption {
    pub id: Uuid,
    pub name: String,
    /// Effective price in piastres (branch_channel → branch → channel → catalog default).
    pub price: i32,
    /// Effective availability (branch_channel → branch → channel → TRUE).
    pub is_available: bool,
    /// The org_ingredient this option swaps out, if it is a swap-style option.
    pub replaces_ingredient_id: Option<Uuid>,
    pub recipe: Vec<SyncRecipeLine>,
}

/// A modifier group attached to an item, with min/max/required resolved from the
/// attachment overrides (falling back to the group defaults).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SyncModifierGroup {
    pub group_id: Uuid,
    /// The group's authored display name (custom groups have no legacy type —
    /// this is what the POS renders as the section title).
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub selection_type: String,
    pub min: i32,
    pub max: Option<i32>,
    pub is_required: bool,
    pub legacy_addon_type: Option<String>,
    pub options: Vec<SyncOption>,
}

/// One menu item with its sizes and attached modifier groups.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SyncItem {
    pub id: Uuid,
    pub name: String,
    pub name_translations: serde_json::Value,
    pub category_id: Option<Uuid>,
    pub sizes: Vec<SyncSize>,
    pub modifier_groups: Vec<SyncModifierGroup>,
}

/// An org ingredient referenced by a returned option recipe.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SyncIngredient {
    pub id: Uuid,
    pub name: String,
    pub unit: String,
}

/// The full catalog snapshot for a POS device.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CatalogSyncResponse {
    pub catalog_revision: i64,
    /// `false` when `since` equals the current revision (client is up to date;
    /// `items`/`ingredients` are then empty). `true` ⇒ the full payload follows.
    pub changed: bool,
    #[serde(default)]
    pub items: Vec<SyncItem>,
    #[serde(default)]
    pub ingredients: Vec<SyncIngredient>,
}

/// Query string for `GET /catalog/sync`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CatalogSyncQuery {
    /// The branch whose resolved prices/availability the POS wants.
    pub branch_id: Uuid,
    /// The `delivery_channel` ('in_mall' | 'outside' | 'umbrella' | 'pickup').
    /// OMIT for branch-only resolution (the in-store POS: branch → catalog
    /// default, no channel scope applied).
    pub channel: Option<String>,
    /// The device's cached revision. If it equals the current revision the
    /// server answers `changed:false` with no payload (a cheap poll).
    pub since: Option<i64>,
}

// ── Helpers ──────────────────────────────────────────────────────────

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

/// The `delivery_channel` enum values, mirrored so we can 400 a bad `channel`
/// before it hits the DB (a `22P02` would 400 anyway, but this gives a clear
/// message and keeps the query out of the error path).
const VALID_CHANNELS: [&str; 4] = ["in_mall", "outside", "umbrella", "pickup"];

/// Current catalog revision for an org (0 if never seeded — an unwritten org is
/// revision 0 from the POS's perspective). Mirrors `menu::studio`.
async fn current_catalog_revision(pool: &PgPool, org_id: Uuid) -> Result<i64, AppError> {
    let rev: Option<i64> =
        sqlx::query_scalar("SELECT revision FROM catalog_revision WHERE org_id = $1")
            .bind(org_id)
            .fetch_optional(pool)
            .await?;
    Ok(rev.unwrap_or(0))
}

// ── Endpoint: GET /catalog/sync ──────────────────────────────────────

#[utoipa::path(
    get,
    path = "/catalog/sync",
    tag = "menu",
    params(
        ("branch_id" = Uuid, Query, description = "Branch whose resolved prices/availability to return"),
        ("channel" = Option<String>, Query, description = "delivery_channel: in_mall | outside | umbrella | pickup — omit for branch-only resolution (in-store POS)"),
        ("since" = Option<i64>, Query, description = "Device's cached catalog_revision; == current ⇒ changed:false, no payload")
    ),
    responses(
        (status = 200, description = "Unified catalog snapshot with prices/availability resolved for (branch, channel)", body = CatalogSyncResponse),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn catalog_sync(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<CatalogSyncQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;

    let q = query.into_inner();

    // Validate the channel up front (clear 400 rather than a raw enum cast error).
    // Omitted channel = branch-only resolution (the in-store POS has no channel).
    if let Some(ch) = q.channel.as_deref()
        && !VALID_CHANNELS.contains(&ch)
    {
        return Err(AppError::BadRequest(format!(
            "Invalid channel '{ch}' (expected one of: in_mall, outside, umbrella, pickup)"
        )));
    }

    // Resolve org from the branch, and gate access: the caller's org must own it.
    let org_id: Option<Uuid> = sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1")
        .bind(q.branch_id)
        .fetch_optional(pool.get_ref())
        .await?;
    let org_id = org_id.ok_or_else(|| AppError::NotFound("Branch not found".into()))?;
    require_same_org(&claims, Some(org_id))?;

    let catalog_revision = current_catalog_revision(pool.get_ref(), org_id).await?;

    // Cheap poll: if the device is already current, return no payload.
    if q.since == Some(catalog_revision) {
        return Ok(HttpResponse::Ok().json(CatalogSyncResponse {
            catalog_revision,
            changed: false,
            items: Vec::new(),
            ingredients: Vec::new(),
        }));
    }

    let snapshot = build_catalog_snapshot(
        pool.get_ref(),
        org_id,
        q.branch_id,
        q.channel.as_deref(),
        catalog_revision,
    )
    .await?;
    Ok(HttpResponse::Ok().json(snapshot))
}

// ── Snapshot builder ─────────────────────────────────────────────────

/// Build the full resolved catalog for `(branch, channel)`. `channel: None` =
/// branch-only resolution (a NULL channel bind makes every channel-scoped
/// override join match nothing, so COALESCE falls through branch → catalog).
async fn build_catalog_snapshot(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    channel: Option<&str>,
    catalog_revision: i64,
) -> Result<CatalogSyncResponse, AppError> {
    // ── Active items for the org. ──
    let item_rows: Vec<(Uuid, String, serde_json::Value, Option<Uuid>)> = sqlx::query_as(
        "SELECT id, name, name_translations, category_id \
         FROM menu_items \
         WHERE org_id = $1 AND is_active = true AND deleted_at IS NULL \
         ORDER BY name, id",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await?;
    let item_ids: Vec<Uuid> = item_rows.iter().map(|r| r.0).collect();

    // ── Sizes (active) with price/availability resolved per §3. ──
    // LEFT JOIN the three scope rows and COALESCE most-specific-first; price and
    // availability resolve independently (a scope row may set only one). The
    // catalog default price is menu_item_sizes.price; avail defaults to TRUE.
    let sizes_by_item = load_sizes(pool, &item_ids, branch_id, channel).await?;

    // ── Attached modifier groups → options (resolved) + option recipes. ──
    let (groups_by_item, referenced_ingredient_ids) =
        load_modifier_groups(pool, &item_ids, branch_id, channel).await?;

    // ── Assemble items. ──
    let mut items = Vec::with_capacity(item_rows.len());
    for (id, name, name_translations, category_id) in item_rows {
        items.push(SyncItem {
            id,
            name,
            name_translations,
            category_id,
            sizes: sizes_by_item.get(&id).cloned().unwrap_or_default(),
            modifier_groups: groups_by_item.get(&id).cloned().unwrap_or_default(),
        });
    }

    // ── Ingredients referenced by the returned option recipes (active only). ──
    let ingredients = load_referenced_ingredients(pool, &referenced_ingredient_ids).await?;

    Ok(CatalogSyncResponse {
        catalog_revision,
        changed: true,
        items,
        ingredients,
    })
}

/// Load active sizes for the items, keyed by menu_item_id, with price/availability
/// resolved for `(branch, channel)` via the documented COALESCE precedence.
async fn load_sizes(
    pool: &PgPool,
    item_ids: &[Uuid],
    branch_id: Uuid,
    channel: Option<&str>,
) -> Result<std::collections::HashMap<Uuid, Vec<SyncSize>>, AppError> {
    let mut map: std::collections::HashMap<Uuid, Vec<SyncSize>> = std::collections::HashMap::new();
    if item_ids.is_empty() {
        return Ok(map);
    }

    // effective_price = COALESCE(bc.price, b.price, c.price, s.price)
    // effective_avail = COALESCE(bc.is_available, b.is_available, c.is_available, TRUE)
    let rows: Vec<(Uuid, Uuid, String, i32, bool)> = sqlx::query_as(
        "SELECT s.menu_item_id, s.id, s.label, \
                COALESCE(bc.price, b.price, c.price, s.price) AS price, \
                COALESCE(bc.is_available, b.is_available, c.is_available, TRUE) AS is_available \
         FROM menu_item_sizes s \
         LEFT JOIN menu_price_overrides bc \
                ON bc.target_type = 'menu_item_size' AND bc.target_id = s.id \
               AND bc.scope = 'branch_channel' AND bc.branch_id = $2 \
               AND bc.channel = $3::delivery_channel \
         LEFT JOIN menu_price_overrides b \
                ON b.target_type = 'menu_item_size' AND b.target_id = s.id \
               AND b.scope = 'branch' AND b.branch_id = $2 \
         LEFT JOIN menu_price_overrides c \
                ON c.target_type = 'menu_item_size' AND c.target_id = s.id \
               AND c.scope = 'channel' AND c.channel = $3::delivery_channel \
         WHERE s.menu_item_id = ANY($1) AND s.is_active = true \
         ORDER BY s.menu_item_id, s.sort, s.label",
    )
    .bind(item_ids)
    .bind(branch_id)
    .bind(channel)
    .fetch_all(pool)
    .await?;

    for (menu_item_id, id, label, price, is_available) in rows {
        map.entry(menu_item_id).or_default().push(SyncSize {
            id,
            label,
            price,
            is_available,
        });
    }
    Ok(map)
}

/// Raw attachment (item ↔ group) with resolved min/max/required.
struct RawGroupAttachment {
    menu_item_id: Uuid,
    group_id: Uuid,
    name: String,
    name_translations: serde_json::Value,
    selection_type: String,
    min: i32,
    max: Option<i32>,
    is_required: bool,
    legacy_addon_type: Option<String>,
    included_option_ids: Option<Vec<Uuid>>,
}

/// Load attached modifier groups + their resolved options for the items, keyed by
/// menu_item_id. Also returns the set of ingredient ids referenced by any returned
/// option recipe (so the caller can hydrate `ingredients[]`).
///
/// Resolution:
/// * only attached groups whose group `is_active`;
/// * min/max/required = COALESCE(attachment override, group default);
/// * only options that are `is_active` AND allowed by `included_option_ids`
///   (NULL = all options);
/// * option price/availability resolved for `(branch, channel)` per §3;
/// * option recipe = recipe_lines WHERE owner_type='modifier_option' AND owner_id=option.id.
async fn load_modifier_groups(
    pool: &PgPool,
    item_ids: &[Uuid],
    branch_id: Uuid,
    channel: Option<&str>,
) -> Result<
    (
        std::collections::HashMap<Uuid, Vec<SyncModifierGroup>>,
        Vec<Uuid>,
    ),
    AppError,
> {
    let mut map: std::collections::HashMap<Uuid, Vec<SyncModifierGroup>> =
        std::collections::HashMap::new();
    if item_ids.is_empty() {
        return Ok((map, Vec::new()));
    }

    // Attachments joined to active groups, with min/max/required resolved.
    #[allow(clippy::type_complexity)]
    let attach_rows: Vec<(
        Uuid,
        Uuid,
        String,
        serde_json::Value,
        String,
        i32,
        Option<i32>,
        bool,
        Option<String>,
        Option<Vec<Uuid>>,
        i32,
    )> = sqlx::query_as(
        "SELECT mimg.menu_item_id, mg.id, mg.name, mg.name_translations, mg.selection_type, \
                COALESCE(mimg.min_override, mg.min_selections) AS min, \
                COALESCE(mimg.max_override, mg.max_selections) AS max, \
                COALESCE(mimg.is_required_override, mg.is_required) AS is_required, \
                mg.legacy_addon_type, mimg.included_option_ids, mimg.sort \
         FROM menu_item_modifier_groups mimg \
         JOIN modifier_groups mg ON mg.id = mimg.group_id \
         WHERE mimg.menu_item_id = ANY($1) AND mg.is_active = true \
         ORDER BY mimg.menu_item_id, mimg.sort, mg.name",
    )
    .bind(item_ids)
    .fetch_all(pool)
    .await?;

    let attachments: Vec<RawGroupAttachment> = attach_rows
        .into_iter()
        .map(
            |(
                menu_item_id,
                group_id,
                name,
                name_translations,
                selection_type,
                min,
                max,
                is_required,
                legacy_addon_type,
                included_option_ids,
                _sort,
            )| RawGroupAttachment {
                menu_item_id,
                group_id,
                name,
                name_translations,
                selection_type,
                min,
                max,
                is_required,
                legacy_addon_type,
                included_option_ids,
            },
        )
        .collect();

    // Bulk-load every active option (with resolved price/avail) for the referenced
    // groups, plus every option's recipe. Keyed by group_id / option_id.
    let group_ids: Vec<Uuid> = {
        let mut g: Vec<Uuid> = attachments.iter().map(|a| a.group_id).collect();
        g.sort();
        g.dedup();
        g
    };
    let (options_by_group, recipes_by_option, referenced_ingredient_ids) =
        load_group_options(pool, &group_ids, branch_id, channel).await?;

    for a in &attachments {
        let all_opts = options_by_group.get(&a.group_id);
        let mut options = Vec::new();
        if let Some(all_opts) = all_opts {
            for o in all_opts {
                // Allowlist filter: NULL = all; else the option must be listed.
                let included = match &a.included_option_ids {
                    None => true,
                    Some(ids) => ids.contains(&o.id),
                };
                if !included {
                    continue;
                }
                let recipe = recipes_by_option.get(&o.id).cloned().unwrap_or_default();
                options.push(SyncOption {
                    id: o.id,
                    name: o.name.clone(),
                    price: o.price,
                    is_available: o.is_available,
                    replaces_ingredient_id: o.replaces_ingredient_id,
                    recipe,
                });
            }
        }

        map.entry(a.menu_item_id)
            .or_default()
            .push(SyncModifierGroup {
                group_id: a.group_id,
                name: a.name.clone(),
                name_translations: a.name_translations.clone(),
                selection_type: a.selection_type.clone(),
                min: a.min,
                max: a.max,
                is_required: a.is_required,
                legacy_addon_type: a.legacy_addon_type.clone(),
                options,
            });
    }

    Ok((map, referenced_ingredient_ids))
}

/// A resolved option (price/availability already computed for the request scope),
/// pre-allowlist-filtering.
struct RawOption {
    id: Uuid,
    name: String,
    price: i32,
    is_available: bool,
    replaces_ingredient_id: Option<Uuid>,
}

/// Load every active option of the given groups, resolving each option's
/// price/availability for `(branch, channel)` per §3 (target_type='modifier_option',
/// catalog default = modifier_options.price). Also load each option's recipe lines,
/// and collect the ingredient ids they reference.
///
/// Returns `(options_by_group, recipes_by_option, referenced_ingredient_ids)`.
#[allow(clippy::type_complexity)]
async fn load_group_options(
    pool: &PgPool,
    group_ids: &[Uuid],
    branch_id: Uuid,
    channel: Option<&str>,
) -> Result<
    (
        std::collections::HashMap<Uuid, Vec<RawOption>>,
        std::collections::HashMap<Uuid, Vec<SyncRecipeLine>>,
        Vec<Uuid>,
    ),
    AppError,
> {
    let mut by_group: std::collections::HashMap<Uuid, Vec<RawOption>> =
        std::collections::HashMap::new();
    if group_ids.is_empty() {
        return Ok((by_group, std::collections::HashMap::new(), Vec::new()));
    }

    // Options with price/availability resolved (same COALESCE shape as sizes, but
    // target_type='modifier_option' and default price = mo.price).
    let opt_rows: Vec<(Uuid, Uuid, String, i32, bool, Option<Uuid>)> = sqlx::query_as(
        "SELECT mo.group_id, mo.id, mo.name, \
                COALESCE(bc.price, b.price, c.price, mo.price) AS price, \
                COALESCE(bc.is_available, b.is_available, c.is_available, TRUE) AS is_available, \
                mo.replaces_ingredient_id \
         FROM modifier_options mo \
         LEFT JOIN menu_price_overrides bc \
                ON bc.target_type = 'modifier_option' AND bc.target_id = mo.id \
               AND bc.scope = 'branch_channel' AND bc.branch_id = $2 \
               AND bc.channel = $3::delivery_channel \
         LEFT JOIN menu_price_overrides b \
                ON b.target_type = 'modifier_option' AND b.target_id = mo.id \
               AND b.scope = 'branch' AND b.branch_id = $2 \
         LEFT JOIN menu_price_overrides c \
                ON c.target_type = 'modifier_option' AND c.target_id = mo.id \
               AND c.scope = 'channel' AND c.channel = $3::delivery_channel \
         WHERE mo.group_id = ANY($1) AND mo.is_active = true \
         ORDER BY mo.group_id, mo.sort, mo.name",
    )
    .bind(group_ids)
    .bind(branch_id)
    .bind(channel)
    .fetch_all(pool)
    .await?;

    let mut option_ids = Vec::with_capacity(opt_rows.len());
    for (group_id, id, name, price, is_available, replaces_ingredient_id) in opt_rows {
        option_ids.push(id);
        by_group.entry(group_id).or_default().push(RawOption {
            id,
            name,
            price,
            is_available,
            replaces_ingredient_id,
        });
    }

    // Recipe lines for every returned option, keyed by option id.
    let mut recipes_by_option: std::collections::HashMap<Uuid, Vec<SyncRecipeLine>> =
        std::collections::HashMap::new();
    let mut referenced: Vec<Uuid> = Vec::new();
    if !option_ids.is_empty() {
        let recipe_rows: Vec<(Uuid, Uuid, rust_decimal::Decimal, String)> = sqlx::query_as(
            "SELECT owner_id, ingredient_id, quantity, unit \
             FROM recipe_lines \
             WHERE owner_type = 'modifier_option' AND owner_id = ANY($1) \
             ORDER BY owner_id, ingredient_id",
        )
        .bind(&option_ids)
        .fetch_all(pool)
        .await?;

        for (owner_id, ingredient_id, quantity, unit) in recipe_rows {
            referenced.push(ingredient_id);
            recipes_by_option
                .entry(owner_id)
                .or_default()
                .push(SyncRecipeLine {
                    ingredient_id,
                    quantity: quantity.normalize().to_string(),
                    unit,
                });
        }
    }

    referenced.sort();
    referenced.dedup();
    Ok((by_group, recipes_by_option, referenced))
}

/// Hydrate the active org_ingredients referenced by returned recipes (id, name, unit).
/// An ingredient soft-deleted/inactive is silently dropped from the list (the recipe
/// line still references its id; the POS treats an absent ingredient as unknown).
async fn load_referenced_ingredients(
    pool: &PgPool,
    ingredient_ids: &[Uuid],
) -> Result<Vec<SyncIngredient>, AppError> {
    if ingredient_ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<(Uuid, String, String)> = sqlx::query_as(
        "SELECT id, name, unit::text FROM org_ingredients \
         WHERE id = ANY($1) AND is_active = true AND deleted_at IS NULL \
         ORDER BY name",
    )
    .bind(ingredient_ids)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, name, unit)| SyncIngredient { id, name, unit })
        .collect())
}

#[cfg(test)]
mod tests;
