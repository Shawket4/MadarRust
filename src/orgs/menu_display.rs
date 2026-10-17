//! The shop's menu WITHOUT prices — a public page every plan has.
//!
//! The owner's brief (2026-10-17): the menu on a screen, a table card or a bio
//! link with no figure on it anywhere. Category tabs, names, photos,
//! descriptions, sizes and options by name; a customer can build a list and
//! show it to a waiter, and nothing is ever ordered from it.
//!
//! Three things hold it together:
//!
//!   * **No prices on the wire.** The public answer is its OWN set of types
//!     ([`MenuDisplayItem`] and friends) that have no price, cost, surcharge,
//!     discount or deal field at all — not the priced menu with the numbers
//!     nulled. The data still comes from [`load_public_menu`] (the dine-in menu,
//!     availability and all), so the two menus cannot disagree about what is
//!     on; the conversion below simply has nowhere to put a price.
//!   * **Every plan.** Neither the settings nor the page ask for `branding` or
//!     `online`: a Free shop can switch it on and its customers can open it.
//!     Off is a 404, the same answer as no shop.
//!   * **Not on the links page.** The settings live in `org_links_pages` beside
//!     the links page's own, but the links page never shows a button for it
//!     and `GET /orgs/{id}/links-page` and `GET /public/orgs/links` are
//!     unchanged by it. It has its own small editor endpoints so a Free shop
//!     (which has no links page) can reach them.

use actix_web::{HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::auth::guards::require_same_org;
use crate::authz::Cap;
use crate::authz::require::require;
use crate::delivery::public::{DeliveryMenu, DeliveryMenuCategory, load_public_menu};
use crate::errors::{AppError, AppErrorResponse};
use crate::orgs::handlers::extract_claims;
use crate::qr_card::handlers::menu_display_url;

use super::public::{BrandQuery, PublicBrand, brand_of, resolve_org_any_plan};

/// The page's own title — it has to fit on one line of a phone. Matches the
/// column CHECKs.
pub const MAX_TITLE_CHARS: usize = 60;

// ── Settings ────────────────────────────────────────────────────────────────

/// What is stored, read leniently: no row is "off".
#[derive(Debug, Clone, Default)]
pub struct Stored {
    pub enabled: bool,
    pub title_en: Option<String>,
    pub title_ar: Option<String>,
    pub branch_id: Option<Uuid>,
}

pub(crate) async fn load_stored(pool: &PgPool, org_id: Uuid) -> Result<Stored, AppError> {
    let row = sqlx::query!(
        r#"SELECT menu_display_enabled, menu_display_title_en, menu_display_title_ar,
                  menu_display_branch_id
             FROM org_links_pages WHERE org_id = $1"#,
        org_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(row
        .map(|r| Stored {
            enabled: r.menu_display_enabled,
            title_en: r.menu_display_title_en,
            title_ar: r.menu_display_title_ar,
            branch_id: r.menu_display_branch_id,
        })
        .unwrap_or_default())
}

/// The branch the page shows: the shop's choice when it is still one of its
/// open branches, else its first one — the rule the read-only menu and the
/// tenant shell already follow (`tenant_shell::shop_branch`).
async fn page_branch(pool: &PgPool, org_id: Uuid, wanted: Option<Uuid>) -> Option<Uuid> {
    crate::tenant_shell::shop_branch(pool, org_id, wanted).await
}

/// A branch, as the editor's picker lists it.
#[derive(Debug, Serialize, ToSchema)]
pub struct MenuDisplayBranchOption {
    pub id: Uuid,
    pub name: String,
}

/// The editor's view.
#[derive(Debug, Serialize, ToSchema)]
pub struct MenuDisplaySettings {
    pub enabled: bool,
    pub title_en: Option<String>,
    pub title_ar: Option<String>,
    /// The branch the shop picked; `None` = the default (its first branch).
    pub branch_id: Option<Uuid>,
    /// The branch the page shows right now (the pick, else the default).
    /// `None` when the shop has no open branch.
    pub effective_branch_id: Option<Uuid>,
    /// Every open branch, for the picker.
    pub branches: Vec<MenuDisplayBranchOption>,
    /// Where the page is: `<slug>.madar-pos.cloud/menu` on a shop's own host,
    /// else the generic host's `/menu/<org id>`. `None` when no host is
    /// configured on this server.
    pub public_url: Option<String>,
}

/// What the editor saves.
#[derive(Debug, Deserialize, ToSchema)]
pub struct MenuDisplayInput {
    pub enabled: bool,
    #[serde(default)]
    pub title_en: Option<String>,
    #[serde(default)]
    pub title_ar: Option<String>,
    /// `None` = the default branch.
    #[serde(default)]
    pub branch_id: Option<Uuid>,
}

fn clean_title(v: Option<&str>) -> Result<Option<String>, AppError> {
    let v = v
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if v.as_ref()
        .is_some_and(|s| s.chars().count() > MAX_TITLE_CHARS)
    {
        return Err(AppError::BadRequest(format!(
            "A menu title can be at most {MAX_TITLE_CHARS} characters"
        )));
    }
    Ok(v)
}

async fn settings_view(pool: &PgPool, org_id: Uuid) -> Result<MenuDisplaySettings, AppError> {
    let stored = load_stored(pool, org_id).await?;
    let branches = sqlx::query_as!(
        MenuDisplayBranchOption,
        "SELECT id, name FROM branches \
          WHERE org_id = $1 AND is_active AND deleted_at IS NULL AND kind = 'branch' \
          ORDER BY created_at, id",
        org_id
    )
    .fetch_all(pool)
    .await?;
    Ok(MenuDisplaySettings {
        enabled: stored.enabled,
        title_en: stored.title_en,
        title_ar: stored.title_ar,
        branch_id: stored.branch_id,
        effective_branch_id: page_branch(pool, org_id, stored.branch_id).await,
        branches,
        public_url: menu_display_url(pool, org_id).await.ok(),
    })
}

/// The menu-without-prices settings.
///
/// Every plan: no `branding` or `online` check, so a Free shop can reach it.
/// The organisation settings capabilities, as the links page.
#[utoipa::path(get, path = "/orgs/{id}/links-page/menu-display", tag = "orgs",
    operation_id = "get_menu_display",
    params(("id" = Uuid, Path, description = "Organization ID")),
    responses((status = 200, body = MenuDisplaySettings), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn get_menu_display(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require_same_org(&claims, Some(*org_id))?;
    require(pool.get_ref(), &claims, Cap::OrgSettingsRead, None).await?;
    Ok(HttpResponse::Ok().json(settings_view(pool.get_ref(), *org_id).await?))
}

/// Save the menu-without-prices settings. Writes ONLY its own columns: a
/// shop's links page (and a Free shop's lack of one) is left exactly as it was.
#[utoipa::path(put, path = "/orgs/{id}/links-page/menu-display", tag = "orgs",
    operation_id = "put_menu_display",
    params(("id" = Uuid, Path, description = "Organization ID")),
    request_body = MenuDisplayInput,
    responses((status = 200, body = MenuDisplaySettings), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn put_menu_display(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
    body: web::Json<MenuDisplayInput>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let org_id = *org_id;
    require_same_org(&claims, Some(org_id))?;
    require(pool.get_ref(), &claims, Cap::OrgSettingsEdit, None).await?;
    let body = body.into_inner();
    let title_en = clean_title(body.title_en.as_deref())?;
    let title_ar = clean_title(body.title_ar.as_deref())?;
    if let Some(b) = body.branch_id {
        let ours = sqlx::query_scalar!(
            r#"SELECT EXISTS(SELECT 1 FROM branches
                 WHERE id = $1 AND org_id = $2 AND is_active AND deleted_at IS NULL
                   AND kind = 'branch') AS "ours!""#,
            b,
            org_id
        )
        .fetch_one(pool.get_ref())
        .await?;
        if !ours {
            return Err(AppError::BadRequest(
                "That branch is not one of this shop's".into(),
            ));
        }
    }

    // A shop with no links-page row gets one with the links page's defaults
    // (which read exactly as no row: see `links_page::load_stored`), and only
    // the menu columns are ever written.
    sqlx::query!(
        "INSERT INTO org_links_pages \
             (org_id, menu_display_enabled, menu_display_title_en, menu_display_title_ar, \
              menu_display_branch_id) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (org_id) DO UPDATE SET \
             menu_display_enabled = EXCLUDED.menu_display_enabled, \
             menu_display_title_en = EXCLUDED.menu_display_title_en, \
             menu_display_title_ar = EXCLUDED.menu_display_title_ar, \
             menu_display_branch_id = EXCLUDED.menu_display_branch_id, \
             updated_at = now()",
        org_id,
        body.enabled,
        title_en,
        title_ar,
        body.branch_id
    )
    .execute(pool.get_ref())
    .await?;
    crate::tenant_shell::invalidate(org_id);

    Ok(HttpResponse::Ok().json(settings_view(pool.get_ref(), org_id).await?))
}

// ── The public shape: no price field anywhere ───────────────────────────────

#[derive(Debug, Serialize, ToSchema)]
pub struct MenuDisplaySize {
    pub label: String,
}

/// An item's optional toggle ("Extra hot"), by name.
#[derive(Debug, Serialize, ToSchema)]
pub struct MenuDisplayOptional {
    pub id: Uuid,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    /// Set when it only applies to one size.
    pub size_label: Option<String>,
}

/// One add-on of the org's catalogue (milk, coffee, extras), by name.
#[derive(Debug, Serialize, ToSchema)]
pub struct MenuDisplayAddon {
    pub addon_item_id: Uuid,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    /// `milk_type` | `coffee_type` | `extra` | a custom group's type.
    pub r#type: String,
    pub is_available: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MenuDisplayModifierOption {
    pub option_id: Uuid,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
}

/// A per-item modifier group, constraints resolved, by name.
#[derive(Debug, Serialize, ToSchema)]
pub struct MenuDisplayModifierGroup {
    pub group_id: Uuid,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    /// "single" | "multi".
    pub selection_type: String,
    pub min_selections: i32,
    pub max_selections: Option<i32>,
    pub is_required: bool,
    pub addon_type: Option<String>,
    pub options: Vec<MenuDisplayModifierOption>,
}

/// One item a combo slot offers. Unavailable choices are left out.
#[derive(Debug, Serialize, ToSchema)]
pub struct MenuDisplayComboChoice {
    pub menu_item_id: Uuid,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    #[serde(serialize_with = "crate::uploads::handlers::serialize_opt_url")]
    pub image_url: Option<String>,
    pub included_size_label: String,
    /// In the order the priced menu offers them (cheapest first).
    pub sizes: Vec<MenuDisplaySize>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MenuDisplayComboSlot {
    pub id: Uuid,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub sort: i32,
    pub min: i16,
    pub max: i16,
    pub default_item_id: Option<Uuid>,
    pub default_size_label: Option<String>,
    pub choices: Vec<MenuDisplayComboChoice>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MenuDisplayCombo {
    pub is_fixed: bool,
    pub slots: Vec<MenuDisplayComboSlot>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MenuDisplayItem {
    pub id: Uuid,
    pub category_id: Option<Uuid>,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub description: Option<String>,
    #[serde(serialize_with = "crate::uploads::handlers::serialize_opt_url")]
    pub image_url: Option<String>,
    pub sizes: Vec<MenuDisplaySize>,
    pub optionals: Vec<MenuDisplayOptional>,
    /// The item's base milk, pre-selected as on the till.
    pub default_milk_addon_id: Option<Uuid>,
    /// The item's add-on allowlist (empty = no restriction).
    pub allowed_addon_ids: Vec<Uuid>,
    /// Non-empty = the item's add-ons are exactly these groups' options.
    pub modifier_groups: Vec<MenuDisplayModifierGroup>,
    /// `item` | `combo`.
    pub kind: String,
    /// A `combo` row's slots.
    pub combo: Option<MenuDisplayCombo>,
}

#[derive(Serialize, ToSchema)]
pub struct MenuDisplayMenu {
    /// In the POS's order.
    pub categories: Vec<DeliveryMenuCategory>,
    pub items: Vec<MenuDisplayItem>,
    pub addons: Vec<MenuDisplayAddon>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MenuDisplayBranch {
    pub id: Uuid,
    pub name: String,
}

/// Everything the menu-without-prices page shows, in one request.
#[derive(Serialize, ToSchema)]
pub struct PublicMenuDisplay {
    /// The shop's header, with the branding tier already applied (a shop off
    /// the tier wears Madar's colours under its own name).
    pub brand: PublicBrand,
    pub title_en: Option<String>,
    pub title_ar: Option<String>,
    pub branch: MenuDisplayBranch,
    pub menu: MenuDisplayMenu,
}

/// The priced menu, as the page may show it: every price, cost, surcharge,
/// discount, deal and pricing view dropped by construction — the target types
/// have no field to hold one. Unavailable combo choices go too.
pub fn strip_prices(menu: DeliveryMenu) -> MenuDisplayMenu {
    let items = menu
        .items
        .into_iter()
        .map(|i| MenuDisplayItem {
            id: i.id,
            category_id: i.category_id,
            name: i.name,
            name_translations: i.name_translations,
            description: i.description,
            image_url: i.image_url,
            sizes: i
                .sizes
                .into_iter()
                .map(|s| MenuDisplaySize { label: s.label })
                .collect(),
            optionals: i
                .optionals
                .into_iter()
                .map(|o| MenuDisplayOptional {
                    id: o.id,
                    name: o.name,
                    name_translations: o.name_translations,
                    size_label: o.size_label,
                })
                .collect(),
            default_milk_addon_id: i.default_milk_addon_id,
            allowed_addon_ids: i.allowed_addon_ids,
            modifier_groups: i
                .modifier_groups
                .into_iter()
                .map(|g| MenuDisplayModifierGroup {
                    group_id: g.group_id,
                    name: g.name,
                    name_translations: g.name_translations,
                    selection_type: g.selection_type,
                    min_selections: g.min_selections,
                    max_selections: g.max_selections,
                    is_required: g.is_required,
                    addon_type: g.addon_type,
                    options: g
                        .options
                        .into_iter()
                        .map(|o| MenuDisplayModifierOption {
                            option_id: o.option_id,
                            name: o.name,
                            name_translations: o.name_translations,
                        })
                        .collect(),
                })
                .collect(),
            kind: i.kind,
            combo: i.combo.map(|c| MenuDisplayCombo {
                is_fixed: c.is_fixed,
                slots: c
                    .slots
                    .into_iter()
                    .map(|s| MenuDisplayComboSlot {
                        id: s.id,
                        name: s.name,
                        name_translations: s.name_translations,
                        sort: s.sort,
                        min: s.min,
                        max: s.max,
                        default_item_id: s.default_item_id,
                        default_size_label: s.default_size_label,
                        choices: s
                            .choices
                            .into_iter()
                            .filter(|ch| ch.available)
                            .map(|ch| {
                                let mut sizes = ch.sizes;
                                // The priced picker lists sizes cheapest
                                // first; keep that order, drop the figures.
                                sizes.sort_by_key(|z| z.price);
                                MenuDisplayComboChoice {
                                    menu_item_id: ch.menu_item_id,
                                    name: ch.name,
                                    name_translations: ch.name_translations,
                                    image_url: ch.image_url,
                                    included_size_label: ch.included_size_label,
                                    sizes: sizes
                                        .into_iter()
                                        .map(|z| MenuDisplaySize { label: z.label })
                                        .collect(),
                                }
                            })
                            .collect(),
                    })
                    .collect(),
            }),
        })
        .collect();
    let addons = menu
        .addons
        .into_iter()
        .filter(|a| a.is_available)
        .map(|a| MenuDisplayAddon {
            addon_item_id: a.addon_item_id,
            name: a.name,
            name_translations: a.name_translations,
            r#type: a.r#type,
            is_available: a.is_available,
        })
        .collect();
    MenuDisplayMenu {
        categories: menu.categories,
        items,
        addons,
    }
}

/// Whether a shop's page is switched on (and the shop is live): what the
/// tenant shell asks before it serves `/menu`.
pub(crate) async fn is_on(pool: &PgPool, org_id: Uuid) -> Result<bool, AppError> {
    let live = sqlx::query_scalar!(
        r#"SELECT EXISTS(SELECT 1 FROM organizations
             WHERE id = $1 AND is_active AND deleted_at IS NULL) AS "live!""#,
        org_id
    )
    .fetch_one(pool)
    .await?;
    Ok(live && load_stored(pool, org_id).await?.enabled)
}

/// The shop's menu without prices.
///
/// `org_id` or `slug` names the shop. Every plan, Free included. A shop that
/// has not switched the page on answers 404, exactly like a shop that does not
/// exist. The answer carries no price, cost, surcharge, discount or deal field.
#[utoipa::path(get, path = "/public/menu-display", tag = "orgs",
    operation_id = "public_menu_display", params(BrandQuery),
    responses((status = 200, description = "The menu without prices. Carries an `ETag` and `Cache-Control: public, no-cache`.", body = PublicMenuDisplay),
              (status = 304, description = "`If-None-Match` held the current menu's ETag. No body."),
              AppErrorResponse))]
pub async fn public_menu_display(
    req: HttpRequest,
    pdb: crate::db::PublicDb,
    cache: Option<web::Data<crate::menu::cache::MenuCache>>,
    query: web::Query<BrandQuery>,
) -> Result<HttpResponse, AppError> {
    use crate::menu::cache;
    let no_page = || AppError::NotFound("No menu at that address".into());
    let org_id = match resolve_org_any_plan(pdb.resolver(), &query).await {
        Ok(id) => id,
        Err(AppError::NotFound(_)) => return Err(no_page()),
        Err(e) => return Err(e),
    };
    let pool = pdb.scoped(org_id).await;
    let pool = pool.get_ref();
    // Everything below depends only on rows the public fingerprint covers (the
    // catalogue, the org and branch rows, the page's settings, the clock for
    // sale windows), so a hit skips it all and answers from the stored,
    // pre-compressed body. A miss that ends in a 404 is not stored.
    let key = match &cache {
        Some(c) if c.usable(pool).await => Some(format!(
            "pub|display|{}",
            cache::public_fingerprint(pool, org_id).await?
        )),
        _ => None,
    };
    let cache = cache.filter(|_| key.is_some());
    if let (Some(c), Some(k)) = (&cache, &key)
        && let Some(entry) = c.get_entry(org_id, k).await
    {
        return Ok(cache::respond_as(&req, entry, cache::PUBLIC, true).await);
    }
    if !is_on(pool, org_id).await? {
        return Err(no_page());
    }
    let stored = load_stored(pool, org_id).await?;
    let branch_id = page_branch(pool, org_id, stored.branch_id)
        .await
        .ok_or_else(no_page)?;
    let branch_name = sqlx::query_scalar!("SELECT name FROM branches WHERE id = $1", branch_id)
        .fetch_one(pool)
        .await?;
    let slug = sqlx::query_scalar!("SELECT slug FROM organizations WHERE id = $1", org_id)
        .fetch_one(pool)
        .await?;
    let brand = crate::orgs::branding::load(pool, org_id).await?;
    // The dine-in menu (`channel: None`): branch availability, no channel.
    let menu = load_public_menu(pool, org_id, branch_id, None).await?;

    let body = serde_json::to_vec(&PublicMenuDisplay {
        brand: brand_of(org_id, slug, brand),
        title_en: stored.title_en,
        title_ar: stored.title_ar,
        branch: MenuDisplayBranch {
            id: branch_id,
            name: branch_name,
        },
        menu: strip_prices(menu),
    })
    .map_err(|_| AppError::Internal)?;
    let body = web::Bytes::from(body);
    // `public, no-cache` + ETag: a shop checking its own edit sees it on the
    // next load (a 304 when nothing changed), and a crowd scanning one table
    // card is served from the menu cache, not a query storm.
    let (entry, stored) = match (&cache, &key) {
        (Some(c), Some(k)) => (c.put_entry(org_id, k, body).await, true),
        _ => (std::sync::Arc::new(cache::CachedBody::new(body)), false),
    };
    Ok(cache::respond_as(&req, entry, cache::PUBLIC, stored).await)
}
