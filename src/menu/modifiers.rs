//! Reusable modifier groups/options + pricing & availability overrides —
//! additive Wave-2 API over the NEW unified menu tables (CONTRACT.md §2, §3, §5.1).
//!
//! This module is ADDITIVE: it reads/writes the unified catalog tables created by
//! `migrations/20260703100000_menu_unification_expand.sql` (modifier_groups,
//! modifier_options, menu_item_modifier_groups, recipe_lines, menu_price_overrides,
//! catalog_revision) and never touches the legacy menu/recipe handlers, their
//! tables, or their tests. It matches the conventions of the sibling `menu::studio`
//! module exactly (runtime `sqlx::query`/`query_as`/`query_scalar` with `.bind()`,
//! NO `query!` macro; ToSchema response structs; utoipa annotations; auth via
//! `extract_claims` + `check_permission` + `require_same_org`; the
//! `catalog_revision` bump on every write). It reuses `menu::studio`'s recipe-cost
//! rollup + item-option helpers rather than re-deriving the math.
//!
//! Conventions honoured (see CONTRACT.md):
//! * Money is integer **piastres** end to end.
//! * Unknown cost is **NULL, never 0**. A `recipe_lines.quantity = 0` is a swap
//!   marker, NOT unknown cost — the rollup keeps `menu::studio`'s NULL-tolerance.
//! * Override resolution (price + availability, resolved independently) follows the
//!   documented precedence: branch_channel > branch > channel > catalog default.
//! * Every mutation bumps `catalog_revision` for the affected org so the offline POS
//!   resyncs; new groups get `legacy_addon_type = NULL`, new options
//!   `legacy_source = 'addon'` (the FLIP shim provenance columns are only ever set by
//!   the backfill / the legacy shim, never by these new-authoring endpoints).
//!
//! Endpoints:
//!   A. Reusable modifier groups (org-scoped):
//!      GET    /modifier-groups?org_id={uuid}
//!      POST   /modifier-groups
//!      PATCH  /modifier-groups/{gid}
//!      DELETE /modifier-groups/{gid}                 (soft if attached/order-referenced)
//!      POST   /modifier-groups/{gid}/options
//!      PATCH  /modifier-options/{oid}
//!      DELETE /modifier-options/{oid}                (soft if order-referenced)
//!      PUT    /modifier-options/{oid}/recipe
//!   B. Item priced optionals:
//!      PUT    /menu-items/{id}/options                (replace-set of the item Options group)
//!   C. Pricing & availability (merged override table):
//!      PUT    /menu-price-overrides
//!      DELETE /menu-price-overrides
//!   D. Live per-size cost from the NEW tables:
//!      GET    /menu-items/{id}/cost

use actix_web::HttpMessage;
use actix_web::{HttpRequest, HttpResponse, web};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    auth::{guards::require_same_org, jwt::Claims},
    errors::{AppError, AppErrorResponse},
    menu::studio::{self, ItemOptionOut},
    permissions::checker::check_permission,
};
use utoipa::ToSchema;

// ── Shared response shapes ────────────────────────────────────────────

/// A modifier option as returned by the reusable-group endpoints (org-scoped,
/// no per-item `included`/cost context — that belongs to the studio aggregate).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GroupOptionOut {
    pub id: Uuid,
    pub name: String,
    pub name_translations: serde_json::Value,
    pub price: i32,
    pub sort: i32,
    pub is_default: bool,
    pub is_active: bool,
    pub replaces_ingredient_id: Option<Uuid>,
}

/// A reusable modifier group with its options (org-scoped catalog view).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct GroupOut {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub name_translations: serde_json::Value,
    pub selection_type: String,
    pub min_selections: i32,
    pub max_selections: Option<i32>,
    pub is_required: bool,
    pub sort: i32,
    pub is_active: bool,
    pub legacy_addon_type: Option<String>,
    pub options: Vec<GroupOptionOut>,
}

// ── Request payloads ─────────────────────────────────────────────────

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateGroupRequest {
    pub name: String,
    #[serde(default)]
    pub name_translations: Option<serde_json::Value>,
    /// 'single' | 'multi'.
    pub selection_type: String,
    #[serde(default)]
    pub min_selections: i32,
    pub max_selections: Option<i32>,
    #[serde(default)]
    pub is_required: bool,
    #[serde(default)]
    pub sort: i32,
    /// The legacy addon type this group is presented as to OLD clients through
    /// the compat shim (the managed addon-type dropdown, e.g. `milk_type` /
    /// `coffee_type` / `extra`). Swap-family behavior keys on it. `null` = a
    /// custom group with no legacy lineage — INVISIBLE to old clients (the shim
    /// projects `type` from this value, and the old wire requires it), so set
    /// it whenever the pre-teardown fleet must see the group's options.
    #[serde(default)]
    pub legacy_addon_type: Option<String>,
}

/// Every field optional — only present keys are updated. `Option<Option<T>>` (with
/// `deserialize_with`) is avoided; nullable columns that must be clearable
/// (`max_selections`) are handled by a dedicated presence flag pattern below.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct PatchGroupRequest {
    pub name: Option<String>,
    pub name_translations: Option<serde_json::Value>,
    pub selection_type: Option<String>,
    pub min_selections: Option<i32>,
    pub max_selections: Option<i32>,
    pub is_required: Option<bool>,
    pub sort: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateOptionRequest {
    pub name: String,
    #[serde(default)]
    pub name_translations: Option<serde_json::Value>,
    pub price: i32,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default = "default_true")]
    pub is_active: bool,
    pub replaces_ingredient_id: Option<Uuid>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct PatchOptionRequest {
    pub name: Option<String>,
    #[serde(default)]
    #[schema(value_type = Object)]
    pub name_translations: Option<serde_json::Value>,
    pub price: Option<i32>,
    pub is_default: Option<bool>,
    pub is_active: Option<bool>,
    pub replaces_ingredient_id: Option<Uuid>,
}

/// One recipe line as submitted to the option-recipe replace endpoint. `quantity`
/// may be 0 (a swap marker). Server normalizes to the ingredient base unit.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct OptionRecipeLineInput {
    pub ingredient_id: Uuid,
    pub quantity: f64,
    pub unit: String,
}

// ── Endpoint-B (item options) payload ────────────────────────────────

/// One priced optional in the item's per-item `Options` set. `id` present ⇒ update
/// that option; absent ⇒ create a new one. `recipe` null ⇒ leave the option with no
/// recipe lines; else the replace-set of its lines.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ItemOptionInput {
    pub id: Option<Uuid>,
    pub name: String,
    pub price: i32,
    #[serde(default = "default_true")]
    pub is_active: bool,
    /// `null` = keep no recipe; else the option's replace-set of recipe lines.
    pub recipe: Option<Vec<OptionRecipeLineInput>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PutItemOptionsRequest {
    pub options: Vec<ItemOptionInput>,
}

// ── Endpoint-C (override) payload ────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PriceOverrideRequest {
    /// 'branch' | 'channel' | 'branch_channel'.
    pub scope: String,
    pub branch_id: Option<Uuid>,
    /// delivery_channel: 'in_mall' | 'outside' | 'umbrella' | 'pickup'.
    pub channel: Option<String>,
    /// 'menu_item_size' | 'modifier_option'.
    pub target_type: String,
    pub target_id: Uuid,
    pub price: Option<i32>,
    pub is_available: Option<bool>,
}

/// The persisted override row (returned by the upsert).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PriceOverrideOut {
    pub id: Uuid,
    pub scope: String,
    pub branch_id: Option<Uuid>,
    pub channel: Option<String>,
    pub target_type: String,
    pub target_id: Uuid,
    pub price: Option<i32>,
    pub is_available: Option<bool>,
}

// ── Endpoint-D (cost) response ───────────────────────────────────────

/// Live per-size cost of an item from the NEW tables.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SizeCostOut {
    pub size_id: Uuid,
    pub label: String,
    /// Recipe cost rollup in piastres. `null` = unknown (no priced ingredient),
    /// never 0. A partial rollup returns the sum-so-far with `cost_incomplete=true`.
    pub cost_piastres: Option<i64>,
    pub cost_incomplete: bool,
}

// ── Helpers ──────────────────────────────────────────────────────────

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

const VALID_CHANNELS: [&str; 4] = ["in_mall", "outside", "umbrella", "pickup"];

/// Load a group's `(org_id, is_active)` — the auth + soft/hard-delete gate.
async fn fetch_group_org(pool: &PgPool, gid: Uuid) -> Result<Option<Uuid>, AppError> {
    let org: Option<Uuid> = sqlx::query_scalar("SELECT org_id FROM modifier_groups WHERE id = $1")
        .bind(gid)
        .fetch_optional(pool)
        .await?;
    Ok(org)
}

/// Resolve an option → its group's org id (for auth on option-scoped endpoints).
async fn fetch_option_org(pool: &PgPool, oid: Uuid) -> Result<Option<(Uuid, Uuid)>, AppError> {
    // (group_id, org_id)
    let row: Option<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT mo.group_id, mg.org_id FROM modifier_options mo \
         JOIN modifier_groups mg ON mg.id = mo.group_id WHERE mo.id = $1",
    )
    .bind(oid)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Load one group with its options (used by list + the create/patch return shape).
#[allow(clippy::type_complexity)]
async fn load_groups(pool: &PgPool, group_ids: &[Uuid]) -> Result<Vec<GroupOut>, AppError> {
    if group_ids.is_empty() {
        return Ok(Vec::new());
    }

    let group_rows: Vec<(
        Uuid,
        Uuid,
        String,
        serde_json::Value,
        String,
        i32,
        Option<i32>,
        bool,
        i32,
        bool,
        Option<String>,
    )> = sqlx::query_as(
        "SELECT id, org_id, name, name_translations, selection_type, min_selections, \
                max_selections, is_required, sort, is_active, legacy_addon_type \
         FROM modifier_groups WHERE id = ANY($1) ORDER BY sort, name",
    )
    .bind(group_ids)
    .fetch_all(pool)
    .await?;

    let opt_rows: Vec<(
        Uuid,
        Uuid,
        String,
        serde_json::Value,
        i32,
        i32,
        bool,
        bool,
        Option<Uuid>,
    )> = sqlx::query_as(
        "SELECT group_id, id, name, name_translations, price, sort, is_default, is_active, \
                replaces_ingredient_id \
         FROM modifier_options WHERE group_id = ANY($1) ORDER BY sort, name",
    )
    .bind(group_ids)
    .fetch_all(pool)
    .await?;

    let mut opts_by_group: std::collections::HashMap<Uuid, Vec<GroupOptionOut>> =
        std::collections::HashMap::new();
    for (group_id, id, name, name_translations, price, sort, is_default, is_active, replaces) in
        opt_rows
    {
        opts_by_group
            .entry(group_id)
            .or_default()
            .push(GroupOptionOut {
                id,
                name,
                name_translations,
                price,
                sort,
                is_default,
                is_active,
                replaces_ingredient_id: replaces,
            });
    }

    Ok(group_rows
        .into_iter()
        .map(|r| {
            let (
                id,
                org_id,
                name,
                name_translations,
                selection_type,
                min_selections,
                max_selections,
                is_required,
                sort,
                is_active,
                legacy_addon_type,
            ) = r;
            GroupOut {
                id,
                org_id,
                name,
                name_translations,
                selection_type,
                min_selections,
                max_selections,
                is_required,
                sort,
                is_active,
                legacy_addon_type,
                options: opts_by_group.remove(&id).unwrap_or_default(),
            }
        })
        .collect())
}

/// One group by id (or NotFound). Small convenience over `load_groups`.
async fn load_one_group(pool: &PgPool, gid: Uuid) -> Result<GroupOut, AppError> {
    load_groups(pool, &[gid])
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::NotFound("Modifier group not found".into()))
}

// ════════════════════════════════════════════════════════════════════
// A. Reusable modifier groups
// ════════════════════════════════════════════════════════════════════

/// Query string for `GET /modifier-groups`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ListGroupsQuery {
    pub org_id: Uuid,
}

// ── GET /modifier-groups?org_id={uuid} ───────────────────────────────

#[utoipa::path(
    get,
    path = "/modifier-groups",
    tag = "menu",
    params(("org_id" = Uuid, Query, description = "Organization whose reusable modifier groups to list")),
    responses(
        (status = 200, description = "The org's active reusable modifier groups, each with its options", body = [GroupOut]),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn list_groups(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<ListGroupsQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    require_same_org(&claims, Some(query.org_id))?;

    // Only active groups (soft-deleted groups are omitted from the catalog list);
    // options are returned as-is (active + inactive) so the editor can re-enable one.
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM modifier_groups WHERE org_id = $1 AND is_active = true ORDER BY sort, name",
    )
    .bind(query.org_id)
    .fetch_all(pool.get_ref())
    .await?;

    let groups = load_groups(pool.get_ref(), &ids).await?;
    Ok(HttpResponse::Ok().json(groups))
}

// ── POST /modifier-groups ────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/modifier-groups",
    tag = "menu",
    request_body = CreateGroupRequest,
    responses((status = 201, description = "Created reusable modifier group", body = GroupOut), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_group(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<CreateGroupRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let org_id = claims
        .org_id()
        .ok_or_else(|| AppError::Forbidden("A super admin must scope this to an org".into()))?;

    let b = body.into_inner();
    if b.selection_type != "single" && b.selection_type != "multi" {
        return Err(AppError::BadRequest(
            "selection_type must be 'single' or 'multi'".into(),
        ));
    }

    let mut tx = pool.begin().await?;
    // legacy_addon_type: the managed addon-type dropdown. Set it so the shim can
    // present this group's options to OLD clients as `type` (the old wire needs
    // a string — a NULL-typed group is invisible pre-teardown); NULL = custom
    // group for new clients only.
    let gid: Uuid = sqlx::query_scalar(
        "INSERT INTO modifier_groups \
             (org_id, name, name_translations, selection_type, min_selections, \
              max_selections, is_required, sort, legacy_addon_type) \
         VALUES ($1, $2, COALESCE($3, '{}'::jsonb), $4, $5, $6, $7, $8, $9) \
         RETURNING id",
    )
    .bind(org_id)
    .bind(&b.name)
    .bind(b.name_translations)
    .bind(&b.selection_type)
    .bind(b.min_selections)
    .bind(b.max_selections)
    .bind(b.is_required)
    .bind(b.sort)
    .bind(&b.legacy_addon_type)
    .fetch_one(&mut *tx)
    .await?;

    studio::bump_catalog_revision(&mut tx, org_id).await?;
    tx.commit().await?;

    let group = load_one_group(pool.get_ref(), gid).await?;
    Ok(HttpResponse::Created().json(group))
}

// ── PATCH /modifier-groups/{gid} ─────────────────────────────────────

#[utoipa::path(
    patch,
    path = "/modifier-groups/{gid}",
    tag = "menu",
    params(("gid" = Uuid, Path, description = "Modifier group ID")),
    request_body = PatchGroupRequest,
    responses((status = 200, description = "Updated modifier group", body = GroupOut), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn patch_group(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    gid: web::Path<Uuid>,
    body: web::Json<PatchGroupRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let org_id = fetch_group_org(pool.get_ref(), *gid)
        .await?
        .ok_or_else(|| AppError::NotFound("Modifier group not found".into()))?;
    require_same_org(&claims, Some(org_id))?;

    let b = body.into_inner();
    if let Some(st) = &b.selection_type
        && st != "single"
        && st != "multi"
    {
        return Err(AppError::BadRequest(
            "selection_type must be 'single' or 'multi'".into(),
        ));
    }

    let mut tx = pool.begin().await?;
    // COALESCE keeps the current value where a field was omitted (NULL in the bind).
    // `max_selections` is intentionally set-only here (a NULL bind keeps the existing
    // value); clearing an upper bound is done by re-creating or via the studio flow —
    // this matches the additive, non-destructive intent of a PATCH.
    sqlx::query(
        "UPDATE modifier_groups SET \
             name = COALESCE($2, name), \
             name_translations = COALESCE($3, name_translations), \
             selection_type = COALESCE($4, selection_type), \
             min_selections = COALESCE($5, min_selections), \
             max_selections = COALESCE($6, max_selections), \
             is_required = COALESCE($7, is_required), \
             sort = COALESCE($8, sort), \
             updated_at = now() \
         WHERE id = $1",
    )
    .bind(*gid)
    .bind(b.name)
    .bind(b.name_translations)
    .bind(b.selection_type)
    .bind(b.min_selections)
    .bind(b.max_selections)
    .bind(b.is_required)
    .bind(b.sort)
    .execute(&mut *tx)
    .await?;

    studio::bump_catalog_revision(&mut tx, org_id).await?;
    tx.commit().await?;

    let group = load_one_group(pool.get_ref(), *gid).await?;
    Ok(HttpResponse::Ok().json(group))
}

// ── DELETE /modifier-groups/{gid} ────────────────────────────────────

#[utoipa::path(
    delete,
    path = "/modifier-groups/{gid}",
    tag = "menu",
    params(("gid" = Uuid, Path, description = "Modifier group ID")),
    responses((status = 204, description = "Group deleted (hard) or deactivated (soft, if referenced)"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_group(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    gid: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let org_id = fetch_group_org(pool.get_ref(), *gid)
        .await?
        .ok_or_else(|| AppError::NotFound("Modifier group not found".into()))?;
    require_same_org(&claims, Some(org_id))?;

    let mut tx = pool.begin().await?;

    // Soft-delete when the group is still attached to any item, OR any of its options
    // is referenced by immutable order history (order_item_addons.addon_item_id =
    // option.id). Order history keeps resolving option ids → we must never hard-delete
    // an option a paid order line points at.
    let referenced: bool = sqlx::query_scalar(
        "SELECT EXISTS ( \
             SELECT 1 FROM menu_item_modifier_groups WHERE group_id = $1 \
         ) OR EXISTS ( \
             SELECT 1 FROM order_item_addons oia \
             JOIN modifier_options mo ON mo.id = oia.addon_item_id \
             WHERE mo.group_id = $1 \
         )",
    )
    .bind(*gid)
    .fetch_one(&mut *tx)
    .await?;

    if referenced {
        // Soft: deactivate the group (its options stay, order history still resolves).
        sqlx::query(
            "UPDATE modifier_groups SET is_active = false, updated_at = now() WHERE id = $1",
        )
        .bind(*gid)
        .execute(&mut *tx)
        .await?;
    } else {
        // Hard: no attachment, no order reference. Options + their recipe_lines go too
        // (options CASCADE on group; recipe_lines are id-keyed with no FK, so drop them
        // explicitly for the options being removed).
        sqlx::query(
            "DELETE FROM recipe_lines WHERE owner_type = 'modifier_option' AND owner_id IN \
                 (SELECT id FROM modifier_options WHERE group_id = $1)",
        )
        .bind(*gid)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM modifier_groups WHERE id = $1")
            .bind(*gid)
            .execute(&mut *tx)
            .await?;
    }

    studio::bump_catalog_revision(&mut tx, org_id).await?;
    tx.commit().await?;

    Ok(HttpResponse::NoContent().finish())
}

// ── POST /modifier-groups/{gid}/options ──────────────────────────────

#[utoipa::path(
    post,
    path = "/modifier-groups/{gid}/options",
    tag = "menu",
    params(("gid" = Uuid, Path, description = "Modifier group ID")),
    request_body = CreateOptionRequest,
    responses((status = 201, description = "Created modifier option", body = GroupOptionOut), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_option(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    gid: web::Path<Uuid>,
    body: web::Json<CreateOptionRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let org_id = fetch_group_org(pool.get_ref(), *gid)
        .await?
        .ok_or_else(|| AppError::NotFound("Modifier group not found".into()))?;
    require_same_org(&claims, Some(org_id))?;

    let b = body.into_inner();

    // A swap-out ingredient (replaces_ingredient_id) must belong to this org.
    if let Some(ing) = b.replaces_ingredient_id {
        verify_ingredient_org(pool.get_ref(), org_id, ing).await?;
    }

    let mut tx = pool.begin().await?;
    // legacy_source='addon' for new options (the FLIP shim uses this to route order
    // creation; a newly-authored reusable option is addon-shaped by convention).
    let oid: Uuid = sqlx::query_scalar(
        "INSERT INTO modifier_options \
             (group_id, name, name_translations, price, is_default, is_active, \
              replaces_ingredient_id, legacy_source) \
         VALUES ($1, $2, COALESCE($3, '{}'::jsonb), $4, $5, $6, $7, 'addon') \
         RETURNING id",
    )
    .bind(*gid)
    .bind(&b.name)
    .bind(b.name_translations)
    .bind(b.price)
    .bind(b.is_default)
    .bind(b.is_active)
    .bind(b.replaces_ingredient_id)
    .fetch_one(&mut *tx)
    .await?;

    studio::bump_catalog_revision(&mut tx, org_id).await?;
    tx.commit().await?;

    let opt = load_one_option(pool.get_ref(), oid).await?;
    Ok(HttpResponse::Created().json(opt))
}

// ── PATCH /modifier-options/{oid} ────────────────────────────────────

#[utoipa::path(
    patch,
    path = "/modifier-options/{oid}",
    tag = "menu",
    params(("oid" = Uuid, Path, description = "Modifier option ID")),
    request_body = PatchOptionRequest,
    responses((status = 200, description = "Updated modifier option", body = GroupOptionOut), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn patch_option(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    oid: web::Path<Uuid>,
    body: web::Json<PatchOptionRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let (_group_id, org_id) = fetch_option_org(pool.get_ref(), *oid)
        .await?
        .ok_or_else(|| AppError::NotFound("Modifier option not found".into()))?;
    require_same_org(&claims, Some(org_id))?;

    let b = body.into_inner();
    if let Some(ing) = b.replaces_ingredient_id {
        verify_ingredient_org(pool.get_ref(), org_id, ing).await?;
    }

    let mut tx = pool.begin().await?;
    // COALESCE keeps current values for omitted fields. `replaces_ingredient_id` is
    // set-only via this PATCH (a NULL bind keeps the existing link) — clearing a swap
    // link is an explicit action handled through the studio option flow.
    sqlx::query(
        "UPDATE modifier_options SET \
             name = COALESCE($2, name), \
             name_translations = COALESCE($3, name_translations), \
             price = COALESCE($4, price), \
             is_default = COALESCE($5, is_default), \
             is_active = COALESCE($6, is_active), \
             replaces_ingredient_id = COALESCE($7, replaces_ingredient_id), \
             updated_at = now() \
         WHERE id = $1",
    )
    .bind(*oid)
    .bind(b.name)
    .bind(b.name_translations)
    .bind(b.price)
    .bind(b.is_default)
    .bind(b.is_active)
    .bind(b.replaces_ingredient_id)
    .execute(&mut *tx)
    .await?;

    studio::bump_catalog_revision(&mut tx, org_id).await?;
    tx.commit().await?;

    let opt = load_one_option(pool.get_ref(), *oid).await?;
    Ok(HttpResponse::Ok().json(opt))
}

// ── DELETE /modifier-options/{oid} ───────────────────────────────────

#[utoipa::path(
    delete,
    path = "/modifier-options/{oid}",
    tag = "menu",
    params(("oid" = Uuid, Path, description = "Modifier option ID")),
    responses((status = 204, description = "Option deleted (hard) or deactivated (soft, if order-referenced)"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_option(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    oid: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let (_group_id, org_id) = fetch_option_org(pool.get_ref(), *oid)
        .await?
        .ok_or_else(|| AppError::NotFound("Modifier option not found".into()))?;
    require_same_org(&claims, Some(org_id))?;

    let mut tx = pool.begin().await?;

    // Soft-delete when the option id is referenced by immutable order history —
    // either as an addon (order_item_addons.addon_item_id) or as an optional
    // (order_item_optionals.optional_field_id). Both columns carry the SAME stable
    // uuid as this option (CONTRACT §4), so a paid order line must keep resolving it.
    let referenced: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM order_item_addons WHERE addon_item_id = $1) \
             OR EXISTS (SELECT 1 FROM order_item_optionals WHERE optional_field_id = $1)",
    )
    .bind(*oid)
    .fetch_one(&mut *tx)
    .await?;

    if referenced {
        sqlx::query(
            "UPDATE modifier_options SET is_active = false, updated_at = now() WHERE id = $1",
        )
        .bind(*oid)
        .execute(&mut *tx)
        .await?;
    } else {
        // Hard: drop the option's recipe_lines (id-keyed, no FK) then the option row.
        sqlx::query(
            "DELETE FROM recipe_lines WHERE owner_type = 'modifier_option' AND owner_id = $1",
        )
        .bind(*oid)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM modifier_options WHERE id = $1")
            .bind(*oid)
            .execute(&mut *tx)
            .await?;
    }

    studio::bump_catalog_revision(&mut tx, org_id).await?;
    tx.commit().await?;

    Ok(HttpResponse::NoContent().finish())
}

// ── PUT /modifier-options/{oid}/recipe ───────────────────────────────

#[utoipa::path(
    put,
    path = "/modifier-options/{oid}/recipe",
    tag = "menu",
    params(("oid" = Uuid, Path, description = "Modifier option ID")),
    request_body = [OptionRecipeLineInput],
    responses((status = 200, description = "Option recipe replaced", body = [OptionRecipeLineInput]), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_option_recipe(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    oid: web::Path<Uuid>,
    body: web::Json<Vec<OptionRecipeLineInput>>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let (_group_id, org_id) = fetch_option_org(pool.get_ref(), *oid)
        .await?
        .ok_or_else(|| AppError::NotFound("Modifier option not found".into()))?;
    require_same_org(&claims, Some(org_id))?;

    let lines = body.into_inner();

    // Reject duplicate ingredient ids (UNIQUE(owner_type,owner_id,ingredient_id)).
    let mut seen = std::collections::HashSet::new();
    for l in &lines {
        if !seen.insert(l.ingredient_id) {
            return Err(AppError::BadRequest(
                "Duplicate ingredient in option recipe".into(),
            ));
        }
    }

    // Normalize each line to the ingredient base unit BEFORE opening the tx. A
    // quantity of 0 (swap marker) is allowed and passes through as 0. The helper
    // also enforces that the ingredient belongs to this org.
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
    sqlx::query("DELETE FROM recipe_lines WHERE owner_type = 'modifier_option' AND owner_id = $1")
        .bind(*oid)
        .execute(&mut *tx)
        .await?;
    for (ingredient_id, qty, base_unit) in &normalized {
        sqlx::query(
            "INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit) \
             VALUES ('modifier_option', $1, $2, $3, $4)",
        )
        .bind(*oid)
        .bind(ingredient_id)
        .bind(Decimal::try_from(*qty).unwrap_or(Decimal::ZERO))
        .bind(base_unit)
        .execute(&mut *tx)
        .await?;
    }

    studio::bump_catalog_revision(&mut tx, org_id).await?;
    tx.commit().await?;

    // Echo back the stored (normalized) lines.
    let stored = load_option_recipe(pool.get_ref(), *oid).await?;
    Ok(HttpResponse::Ok().json(stored))
}

/// One option (or NotFound) in the group-option output shape.
#[allow(clippy::type_complexity)]
async fn load_one_option(pool: &PgPool, oid: Uuid) -> Result<GroupOptionOut, AppError> {
    let row: Option<(
        Uuid,
        String,
        serde_json::Value,
        i32,
        i32,
        bool,
        bool,
        Option<Uuid>,
    )> = sqlx::query_as(
        "SELECT id, name, name_translations, price, sort, is_default, is_active, \
                replaces_ingredient_id \
         FROM modifier_options WHERE id = $1",
    )
    .bind(oid)
    .fetch_optional(pool)
    .await?;
    let (id, name, name_translations, price, sort, is_default, is_active, replaces) =
        row.ok_or_else(|| AppError::NotFound("Modifier option not found".into()))?;
    Ok(GroupOptionOut {
        id,
        name,
        name_translations,
        price,
        sort,
        is_default,
        is_active,
        replaces_ingredient_id: replaces,
    })
}

/// The stored (normalized) recipe lines of an option, echoed back after a replace.
async fn load_option_recipe(
    pool: &PgPool,
    oid: Uuid,
) -> Result<Vec<OptionRecipeLineInput>, AppError> {
    let rows: Vec<(Uuid, Decimal, String)> = sqlx::query_as(
        "SELECT ingredient_id, quantity, unit FROM recipe_lines \
         WHERE owner_type = 'modifier_option' AND owner_id = $1 ORDER BY ingredient_id",
    )
    .bind(oid)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(ingredient_id, quantity, unit)| OptionRecipeLineInput {
            ingredient_id,
            quantity: quantity.to_string().parse::<f64>().unwrap_or(0.0),
            unit,
        })
        .collect())
}

/// 400 unless the ingredient exists in `org_id`'s catalog (not soft-deleted).
async fn verify_ingredient_org(pool: &PgPool, org_id: Uuid, ing: Uuid) -> Result<(), AppError> {
    let ok: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM org_ingredients \
             WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL)",
    )
    .bind(ing)
    .bind(org_id)
    .fetch_one(pool)
    .await?;
    if !ok {
        return Err(AppError::BadRequest(
            "Ingredient not found in this organization's catalog".into(),
        ));
    }
    Ok(())
}

// ════════════════════════════════════════════════════════════════════
// B. Item priced optionals — PUT /menu-items/{id}/options
// ════════════════════════════════════════════════════════════════════

#[utoipa::path(
    put,
    path = "/menu-items/{id}/options",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Menu item ID")),
    request_body = PutItemOptionsRequest,
    responses((status = 200, description = "The item's priced optionals after the replace-set", body = [ItemOptionOut]), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_item_options(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
    body: web::Json<PutItemOptionsRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    // Item basics + auth.
    let item_org: Option<Uuid> =
        sqlx::query_scalar("SELECT org_id FROM menu_items WHERE id = $1 AND deleted_at IS NULL")
            .bind(*id)
            .fetch_optional(pool.get_ref())
            .await?;
    let org_id = item_org.ok_or_else(|| AppError::NotFound("Menu item not found".into()))?;
    require_same_org(&claims, Some(org_id))?;

    let item_id = *id;
    let incoming = body.into_inner().options;

    // Normalize every recipe line up front (needs the pool; must be outside the tx),
    // grouped per incoming option by index. A missing recipe (None) means "no lines".
    // Reject duplicate ingredient ids within one option's recipe.
    let mut normalized_recipes: Vec<Option<Vec<(Uuid, f64, String)>>> =
        Vec::with_capacity(incoming.len());
    for opt in &incoming {
        match &opt.recipe {
            None => normalized_recipes.push(None),
            Some(lines) => {
                let mut seen = std::collections::HashSet::new();
                let mut norm = Vec::with_capacity(lines.len());
                for l in lines {
                    if !seen.insert(l.ingredient_id) {
                        return Err(AppError::BadRequest(
                            "Duplicate ingredient in an option recipe".into(),
                        ));
                    }
                    let (base_unit, qty) = crate::recipes::handlers::normalize_recipe_unit(
                        pool.get_ref(),
                        org_id,
                        Some(l.ingredient_id),
                        &l.unit,
                        l.quantity,
                    )
                    .await?;
                    norm.push((l.ingredient_id, qty, base_unit));
                }
                normalized_recipes.push(Some(norm));
            }
        }
    }

    let mut tx = pool.begin().await?;

    // Resolve (or create) the item-private `Options` group: the attached group with
    // legacy_addon_type IS NULL. If absent, create one and attach it via
    // menu_item_modifier_groups (legacy_origin='options' would be a backfill marker;
    // for a freshly-created group it stays NULL, matching the studio/attach convention).
    let group_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT mg.id FROM menu_item_modifier_groups mimg \
         JOIN modifier_groups mg ON mg.id = mimg.group_id \
         WHERE mimg.menu_item_id = $1 AND mg.legacy_addon_type IS NULL \
         ORDER BY mimg.sort LIMIT 1",
    )
    .bind(item_id)
    .fetch_optional(&mut *tx)
    .await?;

    let group_id = match group_id {
        Some(g) => g,
        None => {
            // Create the per-item Options group (multi-select, no required min).
            let g: Uuid = sqlx::query_scalar(
                "INSERT INTO modifier_groups \
                     (org_id, name, selection_type, min_selections, is_required, legacy_addon_type) \
                 VALUES ($1, 'Options', 'multi', 0, false, NULL) RETURNING id",
            )
            .bind(org_id)
            .fetch_one(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO menu_item_modifier_groups (menu_item_id, group_id, sort) \
                 VALUES ($1, $2, 0)",
            )
            .bind(item_id)
            .bind(g)
            .execute(&mut *tx)
            .await?;
            g
        }
    };

    // Existing option ids in this group (to decide create/update/remove).
    let existing_ids: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM modifier_options WHERE group_id = $1")
            .bind(group_id)
            .fetch_all(&mut *tx)
            .await?;
    let existing_set: std::collections::HashSet<Uuid> = existing_ids.iter().copied().collect();

    // Upsert each incoming option; track which ids survive the replace-set.
    let mut kept: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
    for (idx, opt) in incoming.iter().enumerate() {
        let opt_id = match opt.id {
            // Update an existing option — but only if it really belongs to THIS group
            // (guards against re-pointing another item's/group's option id).
            Some(oid) if existing_set.contains(&oid) => {
                sqlx::query(
                    "UPDATE modifier_options SET name = $2, price = $3, is_active = $4, \
                         updated_at = now() WHERE id = $1",
                )
                .bind(oid)
                .bind(&opt.name)
                .bind(opt.price)
                .bind(opt.is_active)
                .execute(&mut *tx)
                .await?;
                oid
            }
            // An id that isn't in this group (or None) → create a NEW option. A stale
            // id from the client is treated as a create rather than a 404, keeping the
            // replace-set idempotent.
            _ => {
                sqlx::query_scalar(
                    "INSERT INTO modifier_options \
                         (group_id, name, price, is_active, legacy_source) \
                     VALUES ($1, $2, $3, $4, 'optional') RETURNING id",
                )
                .bind(group_id)
                .bind(&opt.name)
                .bind(opt.price)
                .bind(opt.is_active)
                .fetch_one(&mut *tx)
                .await?
            }
        };
        kept.insert(opt_id);

        // Replace this option's recipe lines when a recipe was supplied. `None` leaves
        // the existing lines untouched (a caller that omits `recipe` is editing price/
        // name only); an empty `Some([])` clears the recipe.
        if let Some(norm) = &normalized_recipes[idx] {
            sqlx::query(
                "DELETE FROM recipe_lines WHERE owner_type = 'modifier_option' AND owner_id = $1",
            )
            .bind(opt_id)
            .execute(&mut *tx)
            .await?;
            for (ingredient_id, qty, base_unit) in norm {
                sqlx::query(
                    "INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit) \
                     VALUES ('modifier_option', $1, $2, $3, $4)",
                )
                .bind(opt_id)
                .bind(ingredient_id)
                .bind(Decimal::try_from(*qty).unwrap_or(Decimal::ZERO))
                .bind(base_unit)
                .execute(&mut *tx)
                .await?;
            }
        }
    }

    // Options present before but absent from the set: soft-deactivate if order-
    // referenced (order_item_optionals/addons keep the stable id), else hard-delete.
    for old_id in &existing_ids {
        if kept.contains(old_id) {
            continue;
        }
        let referenced: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM order_item_optionals WHERE optional_field_id = $1) \
                 OR EXISTS (SELECT 1 FROM order_item_addons WHERE addon_item_id = $1)",
        )
        .bind(old_id)
        .fetch_one(&mut *tx)
        .await?;
        if referenced {
            sqlx::query(
                "UPDATE modifier_options SET is_active = false, updated_at = now() WHERE id = $1",
            )
            .bind(old_id)
            .execute(&mut *tx)
            .await?;
        } else {
            sqlx::query(
                "DELETE FROM recipe_lines WHERE owner_type = 'modifier_option' AND owner_id = $1",
            )
            .bind(old_id)
            .execute(&mut *tx)
            .await?;
            sqlx::query("DELETE FROM modifier_options WHERE id = $1")
                .bind(old_id)
                .execute(&mut *tx)
                .await?;
        }
    }

    studio::bump_catalog_revision(&mut tx, org_id).await?;
    tx.commit().await?;

    // Return the item's options[] (reusing the studio hydrator so the shape + cost
    // rollup are identical to GET /studio).
    let options = studio::fetch_item_options(pool.get_ref(), item_id).await?;
    Ok(HttpResponse::Ok().json(options))
}

// ════════════════════════════════════════════════════════════════════
// C. Pricing & availability — menu_price_overrides
// ════════════════════════════════════════════════════════════════════

/// Validate the request's scope↔(branch_id,channel) shape (mirrors the table CHECK),
/// validate the channel enum + target_type, and confirm at least one of
/// price/is_available is set. Returns the (branch_id, channel) that the query binds.
fn validate_override_shape(
    b: &PriceOverrideRequest,
) -> Result<(Option<Uuid>, Option<String>), AppError> {
    if b.target_type != "menu_item_size" && b.target_type != "modifier_option" {
        return Err(AppError::BadRequest(
            "target_type must be 'menu_item_size' or 'modifier_option'".into(),
        ));
    }
    if let Some(ch) = &b.channel
        && !VALID_CHANNELS.contains(&ch.as_str())
    {
        return Err(AppError::BadRequest(format!(
            "Invalid channel '{ch}' (expected one of: in_mall, outside, umbrella, pickup)"
        )));
    }
    match b.scope.as_str() {
        "branch" => {
            if b.branch_id.is_none() || b.channel.is_some() {
                return Err(AppError::BadRequest(
                    "scope 'branch' requires branch_id and no channel".into(),
                ));
            }
        }
        "channel" => {
            if b.branch_id.is_some() || b.channel.is_none() {
                return Err(AppError::BadRequest(
                    "scope 'channel' requires channel and no branch_id".into(),
                ));
            }
        }
        "branch_channel" => {
            if b.branch_id.is_none() || b.channel.is_none() {
                return Err(AppError::BadRequest(
                    "scope 'branch_channel' requires both branch_id and channel".into(),
                ));
            }
        }
        _ => {
            return Err(AppError::BadRequest(
                "scope must be 'branch', 'channel', or 'branch_channel'".into(),
            ));
        }
    }
    Ok((b.branch_id, b.channel.clone()))
}

/// Confirm the override target (a size or an option) belongs to `org_id`.
async fn verify_target_org(
    pool: &PgPool,
    org_id: Uuid,
    target_type: &str,
    target_id: Uuid,
) -> Result<(), AppError> {
    let ok: bool = if target_type == "menu_item_size" {
        sqlx::query_scalar(
            "SELECT EXISTS ( \
                 SELECT 1 FROM menu_item_sizes s \
                 JOIN menu_items mi ON mi.id = s.menu_item_id \
                 WHERE s.id = $1 AND mi.org_id = $2 AND mi.deleted_at IS NULL)",
        )
        .bind(target_id)
        .bind(org_id)
        .fetch_one(pool)
        .await?
    } else {
        sqlx::query_scalar(
            "SELECT EXISTS ( \
                 SELECT 1 FROM modifier_options mo \
                 JOIN modifier_groups mg ON mg.id = mo.group_id \
                 WHERE mo.id = $1 AND mg.org_id = $2)",
        )
        .bind(target_id)
        .bind(org_id)
        .fetch_one(pool)
        .await?
    };
    if !ok {
        return Err(AppError::BadRequest(
            "Override target does not exist in this organization".into(),
        ));
    }
    Ok(())
}

/// If the override is branch-scoped, confirm the branch belongs to the org too (so a
/// caller can't attach an override to another org's branch).
async fn verify_branch_org(pool: &PgPool, org_id: Uuid, branch_id: Uuid) -> Result<(), AppError> {
    let ok: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM branches WHERE id = $1 AND org_id = $2)")
            .bind(branch_id)
            .bind(org_id)
            .fetch_one(pool)
            .await?;
    if !ok {
        return Err(AppError::BadRequest(
            "Branch does not exist in this organization".into(),
        ));
    }
    Ok(())
}

// ── PUT /menu-price-overrides ────────────────────────────────────────

#[utoipa::path(
    put,
    path = "/menu-price-overrides",
    tag = "menu",
    request_body = PriceOverrideRequest,
    responses((status = 200, description = "Upserted price/availability override", body = PriceOverrideOut), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_price_override(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<PriceOverrideRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let org_id = claims
        .org_id()
        .ok_or_else(|| AppError::Forbidden("A super admin must scope this to an org".into()))?;

    let b = body.into_inner();
    if b.price.is_none() && b.is_available.is_none() {
        return Err(AppError::BadRequest(
            "an override must set at least one of price / is_available".into(),
        ));
    }
    if let Some(p) = b.price
        && p < 0
    {
        return Err(AppError::BadRequest("price must be >= 0".into()));
    }
    let (branch_id, channel) = validate_override_shape(&b)?;

    // Tenancy: the target (and the branch, if any) must belong to the caller's org.
    verify_target_org(pool.get_ref(), org_id, &b.target_type, b.target_id).await?;
    if let Some(bid) = branch_id {
        verify_branch_org(pool.get_ref(), org_id, bid).await?;
    }

    // Upsert on the per-scope partial unique key. Each scope has a distinct index
    // (menu_price_overrides_branch_uq / _channel_uq / _bc_uq) so the conflict target
    // must match the scope; we branch the SQL accordingly.
    let mut tx = pool.begin().await?;
    let id: Uuid = match b.scope.as_str() {
        "branch" => {
            sqlx::query_scalar(
                "INSERT INTO menu_price_overrides \
                     (scope, branch_id, channel, target_type, target_id, price, is_available) \
                 VALUES ('branch', $1, NULL, $2, $3, $4, $5) \
                 ON CONFLICT (target_type, target_id, branch_id) WHERE scope = 'branch' \
                 DO UPDATE SET price = EXCLUDED.price, is_available = EXCLUDED.is_available, \
                     updated_at = now() \
                 RETURNING id",
            )
            .bind(branch_id)
            .bind(&b.target_type)
            .bind(b.target_id)
            .bind(b.price)
            .bind(b.is_available)
            .fetch_one(&mut *tx)
            .await?
        }
        "channel" => {
            sqlx::query_scalar(
                "INSERT INTO menu_price_overrides \
                     (scope, branch_id, channel, target_type, target_id, price, is_available) \
                 VALUES ('channel', NULL, $1::delivery_channel, $2, $3, $4, $5) \
                 ON CONFLICT (target_type, target_id, channel) WHERE scope = 'channel' \
                 DO UPDATE SET price = EXCLUDED.price, is_available = EXCLUDED.is_available, \
                     updated_at = now() \
                 RETURNING id",
            )
            .bind(channel.as_deref())
            .bind(&b.target_type)
            .bind(b.target_id)
            .bind(b.price)
            .bind(b.is_available)
            .fetch_one(&mut *tx)
            .await?
        }
        _ => {
            // branch_channel
            sqlx::query_scalar(
                "INSERT INTO menu_price_overrides \
                     (scope, branch_id, channel, target_type, target_id, price, is_available) \
                 VALUES ('branch_channel', $1, $2::delivery_channel, $3, $4, $5, $6) \
                 ON CONFLICT (target_type, target_id, branch_id, channel) \
                     WHERE scope = 'branch_channel' \
                 DO UPDATE SET price = EXCLUDED.price, is_available = EXCLUDED.is_available, \
                     updated_at = now() \
                 RETURNING id",
            )
            .bind(branch_id)
            .bind(channel.as_deref())
            .bind(&b.target_type)
            .bind(b.target_id)
            .bind(b.price)
            .bind(b.is_available)
            .fetch_one(&mut *tx)
            .await?
        }
    };

    studio::bump_catalog_revision(&mut tx, org_id).await?;
    tx.commit().await?;

    Ok(HttpResponse::Ok().json(PriceOverrideOut {
        id,
        scope: b.scope,
        branch_id,
        channel,
        target_type: b.target_type,
        target_id: b.target_id,
        price: b.price,
        is_available: b.is_available,
    }))
}

// ── DELETE /menu-price-overrides ─────────────────────────────────────

#[utoipa::path(
    delete,
    path = "/menu-price-overrides",
    tag = "menu",
    request_body = PriceOverrideRequest,
    responses((status = 204, description = "Override row deleted (no-op if absent)"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_price_override(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<PriceOverrideRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let org_id = claims
        .org_id()
        .ok_or_else(|| AppError::Forbidden("A super admin must scope this to an org".into()))?;

    let b = body.into_inner();
    let (branch_id, channel) = validate_override_shape(&b)?;

    // Tenancy: only delete overrides on the caller's own targets.
    verify_target_org(pool.get_ref(), org_id, &b.target_type, b.target_id).await?;

    let mut tx = pool.begin().await?;
    // Match the exact row by (scope, branch_id, channel, target_type, target_id). The
    // `IS NOT DISTINCT FROM` comparisons make the NULL columns (per scope) match safely.
    sqlx::query(
        "DELETE FROM menu_price_overrides \
         WHERE scope = $1 \
           AND branch_id IS NOT DISTINCT FROM $2 \
           AND channel IS NOT DISTINCT FROM $3::delivery_channel \
           AND target_type = $4 \
           AND target_id = $5",
    )
    .bind(&b.scope)
    .bind(branch_id)
    .bind(channel.as_deref())
    .bind(&b.target_type)
    .bind(b.target_id)
    .execute(&mut *tx)
    .await?;

    studio::bump_catalog_revision(&mut tx, org_id).await?;
    tx.commit().await?;

    Ok(HttpResponse::NoContent().finish())
}

// ════════════════════════════════════════════════════════════════════
// D. Live per-size cost — GET /menu-items/{id}/cost
// ════════════════════════════════════════════════════════════════════

#[utoipa::path(
    get,
    path = "/menu-items/{id}/cost",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Menu item ID")),
    responses((status = 200, description = "Live per-size recipe cost from the new tables", body = [SizeCostOut]), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_item_cost(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;

    let item_org: Option<Uuid> =
        sqlx::query_scalar("SELECT org_id FROM menu_items WHERE id = $1 AND deleted_at IS NULL")
            .bind(*id)
            .fetch_optional(pool.get_ref())
            .await?;
    let org_id = item_org.ok_or_else(|| AppError::NotFound("Menu item not found".into()))?;
    require_same_org(&claims, Some(org_id))?;

    // The item's sizes, ordered like the studio aggregate.
    let sizes: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, label FROM menu_item_sizes WHERE menu_item_id = $1 ORDER BY sort, label",
    )
    .bind(*id)
    .fetch_all(pool.get_ref())
    .await?;

    let size_ids: Vec<Uuid> = sizes.iter().map(|s| s.0).collect();
    // Reuse the studio rollup verbatim (recipe_lines summed with round_piastres,
    // partial-tolerant, unknown = NULL) so this number equals the studio's per-size cost.
    let recipes = studio::load_recipe_lines(pool.get_ref(), "item_size", &size_ids).await?;

    let out: Vec<SizeCostOut> = sizes
        .into_iter()
        .map(|(size_id, label)| {
            let empty: Vec<studio::RawRecipeLine> = Vec::new();
            let lines = recipes.get(&size_id).unwrap_or(&empty);
            let (_hydrated, cost, incomplete) = studio::rollup_recipe(lines);
            SizeCostOut {
                size_id,
                label,
                cost_piastres: cost,
                cost_incomplete: incomplete,
            }
        })
        .collect();

    Ok(HttpResponse::Ok().json(out))
}

#[cfg(test)]
mod tests;
