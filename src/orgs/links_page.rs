//! The shop's links page — the root of its own address.
//!
//! A Linktree-shaped page, but made of things the shop already has. Almost
//! nothing on it is stored here:
//!
//!   * the header is the brand ([`crate::orgs::branding::load`], tier gate and
//!     all — a shop off the branding tier gets Madar's colours, as every other
//!     guest page does);
//!   * the icons under the buttons are `organizations.social_links`;
//!   * "Visit us" is the shop's branches;
//!   * whether a module CAN be offered comes from the switch that already
//!     governs it: an ordering channel on at some branch, online booking on at
//!     some branch, the org's loyalty programme, any active branch for the
//!     menu.
//!
//! What IS stored (`org_links_pages`) is only what the page adds: the order of
//! the buttons and which are hidden, taglines, custom links, whether to use the
//! card image as a cover, and per-branch visibility and Maps link. The page can
//! hide a module that is on; it can never show one that is off.
//!
//! Every URL a module button opens is built by the functions that build the QR
//! codes (`qr_card::handlers::links_module_href`), so a button and a printed
//! code cannot disagree about where the menu is.

use std::collections::{BTreeMap, HashSet};

use actix_web::{HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::auth::guards::require_same_org;
use crate::authz::Cap;
use crate::authz::require::require;
use crate::errors::{AppError, AppErrorResponse};
use crate::orgs::handlers::extract_claims;
use crate::qr_card::handlers::{
    links_module_href, links_module_path, links_shop_origin, org_links_url,
};

use super::public::{BrandQuery, PublicBrand, brand_of, resolve_org};
// The loyalty card's own type: the page and the card list the same links in
// the same shape, from the same column.
use crate::loyalty::public::PublicSocialLink;

/// At most this many custom links. A links page with forty buttons is a
/// directory nobody scrolls.
pub const MAX_CUSTOM_LINKS: usize = 20;
/// A button title — it has to fit on one line of a phone.
pub const MAX_TITLE_CHARS: usize = 60;
/// Matches the column CHECKs.
pub const MAX_TAGLINE_CHARS: usize = 160;

/// What a button is. The four modules are a closed list, like the social
/// platforms: each one is a page we serve, and `custom` is the shop's own link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LinksItemKind {
    Order,
    Menu,
    Rewards,
    Book,
    Custom,
}

impl LinksItemKind {
    /// The modules, in the order a page with no settings shows them. Ordering
    /// first: when it is on it is the thing most visitors came for, and the
    /// first visible button is drawn as the big one.
    pub const MODULES: [LinksItemKind; 4] = [
        LinksItemKind::Order,
        LinksItemKind::Menu,
        LinksItemKind::Rewards,
        LinksItemKind::Book,
    ];

    fn key(self) -> &'static str {
        match self {
            LinksItemKind::Order => "order",
            LinksItemKind::Menu => "menu",
            LinksItemKind::Rewards => "rewards",
            LinksItemKind::Book => "book",
            LinksItemKind::Custom => "custom",
        }
    }
}

fn yes() -> bool {
    true
}

/// One button, as stored and as edited.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct LinksPageItem {
    pub kind: LinksItemKind,
    /// Off = kept in the list (and its place) but not shown.
    #[serde(default = "yes")]
    pub visible: bool,
    /// Custom links only: a stable id, so the editor can tell two links with
    /// the same title apart. Minted by the server when missing.
    #[serde(default)]
    pub id: Option<Uuid>,
    /// Custom links only.
    #[serde(default)]
    pub title_en: Option<String>,
    /// Custom links only. Falls back to the English title when empty.
    #[serde(default)]
    pub title_ar: Option<String>,
    /// Custom links only — a full `https://` address.
    #[serde(default)]
    pub url: Option<String>,
}

impl LinksPageItem {
    fn module(kind: LinksItemKind) -> Self {
        Self {
            kind,
            visible: true,
            id: None,
            title_en: None,
            title_ar: None,
            url: None,
        }
    }
}

/// Per-branch settings for "Visit us".
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct LinksPageBranchInput {
    pub branch_id: Uuid,
    #[serde(default = "yes")]
    pub visible: bool,
    /// A Google Maps (or any https) link, so Directions opens the shop's own
    /// pin. Without one, Directions searches the branch's coordinates or
    /// address.
    #[serde(default)]
    pub maps_url: Option<String>,
}

/// What the editor saves.
#[derive(Debug, Deserialize, ToSchema)]
pub struct LinksPageInput {
    pub items: Vec<LinksPageItem>,
    #[serde(default)]
    pub tagline_en: Option<String>,
    #[serde(default)]
    pub tagline_ar: Option<String>,
    #[serde(default = "yes")]
    pub show_cover: bool,
    #[serde(default = "yes")]
    pub show_branches: bool,
    #[serde(default)]
    pub branches: Vec<LinksPageBranchInput>,
    /// The organisation's social links — the SAME map `PATCH /orgs/{id}`
    /// takes, stored in the same column, under the same closed list and
    /// https rule. Omitted = unchanged.
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    pub social_links: Option<serde_json::Value>,
}

/// Whether a module can be shown, and why — the editor's hint line.
#[derive(Debug, Serialize, ToSchema)]
pub struct LinksModuleStatus {
    pub kind: LinksItemKind,
    /// The switch that governs it is on somewhere. A hidden module that is
    /// available is the shop's choice; an unavailable one cannot be shown.
    pub available: bool,
    /// The branches where it is on (menu: every active branch; rewards: none,
    /// the programme is the org's).
    pub branch_names: Vec<String>,
    /// Where it opens on the shop's own host.
    pub path: String,
}

/// A branch, as the editor lists it.
#[derive(Debug, Serialize, ToSchema)]
pub struct LinksPageBranch {
    pub id: Uuid,
    pub name: String,
    pub address: Option<String>,
    pub phone: Option<String>,
    pub visible: bool,
    pub maps_url: Option<String>,
}

/// The editor's view: what is saved, plus what it is built from.
#[derive(Debug, Serialize, ToSchema)]
pub struct LinksPageSettings {
    pub items: Vec<LinksPageItem>,
    pub tagline_en: Option<String>,
    pub tagline_ar: Option<String>,
    pub show_cover: bool,
    pub show_branches: bool,
    /// Every active branch, with its settings.
    pub branches: Vec<LinksPageBranch>,
    /// `organizations.social_links`, as stored.
    #[schema(value_type = Object)]
    pub social_links: serde_json::Value,
    pub modules: Vec<LinksModuleStatus>,
    /// Where the page is, or `None` when there is nowhere to put it yet (a
    /// shop with no own host on a deployment with no generic links host).
    pub public_url: Option<String>,
    /// Whether the shop wears its own colours on the page.
    pub custom_branding: bool,
    /// The shop's card image, which the page uses as its cover.
    pub card_image_url: Option<String>,
    pub loyalty_mode: Option<String>,
}

// ── Public shape ────────────────────────────────────────────────────────────

/// One button on the public page, already resolved.
#[derive(Debug, Serialize, ToSchema)]
pub struct PublicLinksItem {
    pub kind: LinksItemKind,
    /// Absolute. For a module: the shop's own host when it has one, else the
    /// generic host. For a custom link: the shop's URL.
    pub href: String,
    /// Modules only: the same place as a path on the shop's own host, for a
    /// page being read ON that host.
    pub path: Option<String>,
    /// Custom links only (a module's title is the page's own words).
    pub title_en: Option<String>,
    pub title_ar: Option<String>,
    /// Order / book: the branches where it is on.
    pub branch_names: Vec<String>,
    /// Order: the channels on anywhere — `pickup`, `delivery`, `in_mall`,
    /// `umbrella`.
    pub channels: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PublicLinksBranch {
    pub id: Uuid,
    pub name: String,
    pub address: Option<String>,
    pub phone: Option<String>,
    /// The shop's Maps link, else a search for the coordinates, else for the
    /// address. `None` when the branch has none of the three.
    pub directions_url: Option<String>,
}

/// Everything the links page shows, in one request.
#[derive(Serialize, ToSchema)]
pub struct PublicLinksPage {
    pub brand: PublicBrand,
    pub tagline_en: Option<String>,
    pub tagline_ar: Option<String>,
    /// The card image, when the shop uses it as the cover (and is on the
    /// branding tier — the loader already applied that).
    pub cover_image_url: Option<String>,
    /// Visible, available buttons, in the shop's order.
    pub items: Vec<PublicLinksItem>,
    pub socials: Vec<PublicSocialLink>,
    /// Empty when "Visit us" is off.
    pub branches: Vec<PublicLinksBranch>,
    /// `points` or `visits`, when the rewards button is shown.
    pub loyalty_mode: Option<String>,
}

// ── Stored row ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct BranchSetting {
    #[serde(default)]
    hidden: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    maps_url: Option<String>,
}

/// The saved settings, read leniently: a row that predates a rule, or a hand
/// edit, must not take the shop's front page down.
struct Stored {
    items: Vec<LinksPageItem>,
    tagline_en: Option<String>,
    tagline_ar: Option<String>,
    show_cover: bool,
    show_branches: bool,
    branches: BTreeMap<Uuid, BranchSetting>,
}

async fn load_stored(pool: &PgPool, org_id: Uuid) -> Result<Stored, AppError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        items: serde_json::Value,
        tagline_en: Option<String>,
        tagline_ar: Option<String>,
        show_cover: bool,
        show_branches: bool,
        branches: serde_json::Value,
    }
    let row: Option<Row> = sqlx::query_as(
        "SELECT items, tagline_en, tagline_ar, show_cover, show_branches, branches \
           FROM org_links_pages WHERE org_id = $1",
    )
    .bind(org_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(Stored {
            items: normalize(Vec::new()),
            tagline_en: None,
            tagline_ar: None,
            show_cover: true,
            show_branches: true,
            branches: BTreeMap::new(),
        });
    };
    let items: Vec<LinksPageItem> = serde_json::from_value(row.items).unwrap_or_default();
    // A custom link whose address no longer passes is dropped rather than
    // rendered: it is printed on a public page as a button.
    let items = items
        .into_iter()
        .filter(|i| {
            i.kind != LinksItemKind::Custom || i.url.as_deref().is_some_and(super::social::is_safe)
        })
        .collect();
    Ok(Stored {
        items: normalize(items),
        tagline_en: row.tagline_en,
        tagline_ar: row.tagline_ar,
        show_cover: row.show_cover,
        show_branches: row.show_branches,
        branches: serde_json::from_value(row.branches).unwrap_or_default(),
    })
}

/// Every module exactly once, in the shop's order, with any the shop has never
/// placed appended — visible — at the end. A module added to Madar later shows
/// up on every page without a migration, and a stored list that lost one
/// cannot hide it by accident.
pub fn normalize(items: Vec<LinksPageItem>) -> Vec<LinksPageItem> {
    let mut seen = HashSet::new();
    let mut out: Vec<LinksPageItem> = items
        .into_iter()
        .filter(|i| i.kind == LinksItemKind::Custom || seen.insert(i.kind))
        .map(|mut i| {
            if i.kind != LinksItemKind::Custom {
                i.id = None;
                i.title_en = None;
                i.title_ar = None;
                i.url = None;
            }
            i
        })
        .collect();
    for m in LinksItemKind::MODULES {
        if !seen.contains(&m) {
            out.push(LinksPageItem::module(m));
        }
    }
    out
}

fn clean_text(v: Option<&str>) -> Option<String> {
    v.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Check what a shop is trying to save, and return it tidied.
pub fn validate_items(items: Vec<LinksPageItem>) -> Result<Vec<LinksPageItem>, AppError> {
    let mut modules = HashSet::new();
    let mut ids = HashSet::new();
    let mut customs = 0usize;
    let mut out = Vec::with_capacity(items.len());
    for mut item in items {
        if item.kind != LinksItemKind::Custom {
            if !modules.insert(item.kind) {
                return Err(AppError::BadRequest(format!(
                    "\"{}\" is in the list twice",
                    item.kind.key()
                )));
            }
            out.push(item);
            continue;
        }
        customs += 1;
        if customs > MAX_CUSTOM_LINKS {
            return Err(AppError::BadRequest(format!(
                "A links page can hold at most {MAX_CUSTOM_LINKS} of your own links"
            )));
        }
        let title_en = clean_text(item.title_en.as_deref())
            .ok_or_else(|| AppError::BadRequest("Every link needs a title".into()))?;
        let title_ar = clean_text(item.title_ar.as_deref());
        for t in [Some(&title_en), title_ar.as_ref()].into_iter().flatten() {
            if t.chars().count() > MAX_TITLE_CHARS {
                return Err(AppError::BadRequest(format!(
                    "A link title can be at most {MAX_TITLE_CHARS} characters"
                )));
            }
        }
        let url = clean_text(item.url.as_deref()).unwrap_or_default();
        if !super::social::is_safe(&url) {
            return Err(AppError::BadRequest(format!(
                "\"{title_en}\" has to link to a full https:// address"
            )));
        }
        let id = item.id.unwrap_or_else(Uuid::new_v4);
        if !ids.insert(id) {
            return Err(AppError::BadRequest("Two links share one id".into()));
        }
        item.id = Some(id);
        item.title_en = Some(title_en);
        item.title_ar = title_ar;
        item.url = Some(url);
        out.push(item);
    }
    Ok(normalize(out))
}

fn validate_tagline(v: Option<&str>) -> Result<Option<String>, AppError> {
    let v = clean_text(v);
    if v.as_ref()
        .is_some_and(|s| s.chars().count() > MAX_TAGLINE_CHARS)
    {
        return Err(AppError::BadRequest(format!(
            "A tagline can be at most {MAX_TAGLINE_CHARS} characters"
        )));
    }
    Ok(v)
}

// ── What exists already ─────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct BranchRow {
    id: Uuid,
    name: String,
    address: Option<String>,
    phone: Option<String>,
    latitude: Option<f64>,
    longitude: Option<f64>,
}

/// The switches that already decide each module, read once.
struct Availability {
    branches: Vec<BranchRow>,
    /// Branch name → channels on there.
    ordering: Vec<(String, Vec<&'static str>)>,
    booking: Vec<String>,
    loyalty_mode: Option<String>,
}

impl Availability {
    fn available(&self, kind: LinksItemKind) -> bool {
        match kind {
            LinksItemKind::Order => !self.ordering.is_empty(),
            // The read-only menu needs a branch to read the menu of, and
            // nothing else: it works for a shop that takes no online orders.
            LinksItemKind::Menu => !self.branches.is_empty(),
            LinksItemKind::Rewards => self.loyalty_mode.is_some(),
            LinksItemKind::Book => !self.booking.is_empty(),
            LinksItemKind::Custom => true,
        }
    }

    fn branch_names(&self, kind: LinksItemKind) -> Vec<String> {
        match kind {
            LinksItemKind::Order => self.ordering.iter().map(|(n, _)| n.clone()).collect(),
            LinksItemKind::Menu => self.branches.iter().map(|b| b.name.clone()).collect(),
            LinksItemKind::Book => self.booking.clone(),
            _ => Vec::new(),
        }
    }

    fn channels(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for c in ["pickup", "delivery", "in_mall", "umbrella"] {
            if self.ordering.iter().any(|(_, cs)| cs.contains(&c)) {
                out.push(c.to_string());
            }
        }
        out
    }
}

async fn availability(pool: &PgPool, org_id: Uuid) -> Result<Availability, AppError> {
    let branches: Vec<BranchRow> = sqlx::query_as(
        "SELECT id, name, address, phone, latitude, longitude FROM branches \
          WHERE org_id = $1 AND is_active AND deleted_at IS NULL ORDER BY name",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await?;

    // A channel switched ON is what "ordering is available" means here — the
    // same `*_enabled` columns the ordering page offers channels from. A
    // channel paused for the evening (`*_override = 'closed'`) is still a
    // shop that takes orders; its page says so when it is opened.
    let delivery: Vec<(String, bool, bool, bool, bool)> = sqlx::query_as(
        "SELECT b.name, d.pickup_enabled, d.outside_enabled, d.in_mall_enabled, d.umbrella_enabled \
           FROM branch_delivery_settings d JOIN branches b ON b.id = d.branch_id \
          WHERE b.org_id = $1 AND b.is_active AND b.deleted_at IS NULL \
          ORDER BY b.name",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await?;
    let ordering = delivery
        .into_iter()
        .filter_map(|(name, pickup, outside, in_mall, umbrella)| {
            let mut cs = Vec::new();
            if pickup {
                cs.push("pickup");
            }
            if outside {
                cs.push("delivery");
            }
            if in_mall {
                cs.push("in_mall");
            }
            if umbrella {
                cs.push("umbrella");
            }
            (!cs.is_empty()).then_some((name, cs))
        })
        .collect();

    let booking: Vec<String> = sqlx::query_scalar(
        "SELECT b.name FROM branch_booking_settings s JOIN branches b ON b.id = s.branch_id \
          WHERE b.org_id = $1 AND b.is_active AND b.deleted_at IS NULL AND s.enabled \
          ORDER BY b.name",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await?;

    // The ORG's programme: membership belongs to the shop, and the button opens
    // the whole-shop sign-up — the same one the org's join QR checks for.
    let loyalty_mode = crate::loyalty::settings::load_scope(pool, org_id, None)
        .await?
        .filter(|s| s.enabled)
        .map(|s| s.mode);

    Ok(Availability {
        branches,
        ordering,
        booking,
        loyalty_mode,
    })
}

/// Where Directions goes: the shop's own pin, else the coordinates, else the
/// address as a search.
fn directions_url(b: &BranchRow, maps_url: Option<&str>) -> Option<String> {
    if let Some(u) = maps_url.filter(|u| super::social::is_safe(u)) {
        return Some(u.to_string());
    }
    if let (Some(lat), Some(lng)) = (b.latitude, b.longitude) {
        return Some(format!(
            "https://www.google.com/maps/search/?api=1&query={lat},{lng}"
        ));
    }
    let address = b
        .address
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())?;
    Some(format!(
        "https://www.google.com/maps/search/?api=1&query={}",
        urlencoding::encode(address)
    ))
}

// ── Dashboard: GET / PUT /orgs/{id}/links-page ──────────────────────────────

async fn settings_view(pool: &PgPool, org_id: Uuid) -> Result<LinksPageSettings, AppError> {
    let stored = load_stored(pool, org_id).await?;
    let avail = availability(pool, org_id).await?;
    let (social_links, custom_branding): (serde_json::Value, bool) =
        sqlx::query_as("SELECT social_links, custom_branding FROM organizations WHERE id = $1")
            .bind(org_id)
            .fetch_optional(pool)
            .await?
            .ok_or_else(|| AppError::NotFound("Organization not found".into()))?;
    let brand = crate::orgs::branding::load(pool, org_id).await?;
    let shop = links_shop_origin(pool, org_id).await?;

    let modules = LinksItemKind::MODULES
        .into_iter()
        .map(|k| LinksModuleStatus {
            kind: k,
            available: avail.available(k),
            branch_names: avail.branch_names(k),
            path: links_module_path(k.key()).unwrap_or("/").to_string(),
        })
        .collect();
    let branches = avail
        .branches
        .iter()
        .map(|b| {
            let s = stored.branches.get(&b.id).cloned().unwrap_or_default();
            LinksPageBranch {
                id: b.id,
                name: b.name.clone(),
                address: b.address.clone(),
                phone: b.phone.clone(),
                visible: !s.hidden,
                maps_url: s.maps_url,
            }
        })
        .collect();

    Ok(LinksPageSettings {
        items: stored.items,
        tagline_en: stored.tagline_en,
        tagline_ar: stored.tagline_ar,
        show_cover: stored.show_cover,
        show_branches: stored.show_branches,
        branches,
        social_links,
        modules,
        public_url: org_links_url(shop.as_deref(), org_id).ok(),
        custom_branding,
        card_image_url: brand.card_image_url,
        loyalty_mode: avail.loyalty_mode,
    })
}

/// The links page, as the editor sees it.
#[utoipa::path(get, path = "/orgs/{id}/links-page", tag = "orgs",
    operation_id = "get_links_page",
    params(("id" = Uuid, Path, description = "Organization ID")),
    responses((status = 200, body = LinksPageSettings), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn get_links_page(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require_same_org(&claims, Some(*org_id))?;
    require(pool.get_ref(), &claims, Cap::OrgSettingsRead, None).await?;
    Ok(HttpResponse::Ok().json(settings_view(pool.get_ref(), *org_id).await?))
}

/// Save the links page.
///
/// The organisation settings capability, as every other settings screen —
/// and the same one that lets a manager change the shop's logo. The social
/// links ride along because the page shows them; they are written to the
/// organisation's own column, under the same rules as `PATCH /orgs/{id}`.
#[utoipa::path(put, path = "/orgs/{id}/links-page", tag = "orgs",
    operation_id = "put_links_page",
    params(("id" = Uuid, Path, description = "Organization ID")),
    request_body = LinksPageInput,
    responses((status = 200, body = LinksPageSettings), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn put_links_page(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
    body: web::Json<LinksPageInput>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let org_id = *org_id;
    require_same_org(&claims, Some(org_id))?;
    require(pool.get_ref(), &claims, Cap::OrgSettingsEdit, None).await?;
    let body = body.into_inner();

    let items = validate_items(body.items)?;
    let tagline_en = validate_tagline(body.tagline_en.as_deref())?;
    let tagline_ar = validate_tagline(body.tagline_ar.as_deref())?;

    let branch_ids: HashSet<Uuid> =
        sqlx::query_scalar("SELECT id FROM branches WHERE org_id = $1 AND deleted_at IS NULL")
            .bind(org_id)
            .fetch_all(pool.get_ref())
            .await?
            .into_iter()
            .collect();
    let mut branches: BTreeMap<Uuid, BranchSetting> = BTreeMap::new();
    for b in body.branches {
        if !branch_ids.contains(&b.branch_id) {
            return Err(AppError::BadRequest(
                "That branch is not one of this shop's".into(),
            ));
        }
        let maps_url = clean_text(b.maps_url.as_deref());
        if let Some(u) = maps_url.as_deref()
            && !super::social::is_safe(u)
        {
            return Err(AppError::BadRequest(
                "A Maps link has to be a full https:// address".into(),
            ));
        }
        if b.visible && maps_url.is_none() {
            continue; // the default; nothing to store
        }
        branches.insert(
            b.branch_id,
            BranchSetting {
                hidden: !b.visible,
                maps_url,
            },
        );
    }

    let social = match &body.social_links {
        Some(v) => {
            super::social::validate(v)?;
            Some(super::social::clean(v))
        }
        None => None,
    };

    let mut tx = pool.get_ref().begin().await?;
    sqlx::query(
        "INSERT INTO org_links_pages \
             (org_id, items, tagline_en, tagline_ar, show_cover, show_branches, branches) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         ON CONFLICT (org_id) DO UPDATE SET \
             items = EXCLUDED.items, tagline_en = EXCLUDED.tagline_en, \
             tagline_ar = EXCLUDED.tagline_ar, show_cover = EXCLUDED.show_cover, \
             show_branches = EXCLUDED.show_branches, branches = EXCLUDED.branches, \
             updated_at = now()",
    )
    .bind(org_id)
    .bind(serde_json::to_value(&items).map_err(|_| AppError::Internal)?)
    .bind(&tagline_en)
    .bind(&tagline_ar)
    .bind(body.show_cover)
    .bind(body.show_branches)
    .bind(serde_json::to_value(&branches).map_err(|_| AppError::Internal)?)
    .execute(&mut *tx)
    .await?;
    if let Some(social) = social {
        sqlx::query("UPDATE organizations SET social_links = $2, updated_at = now() WHERE id = $1")
            .bind(org_id)
            .bind(social)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;

    Ok(HttpResponse::Ok().json(settings_view(pool.get_ref(), org_id).await?))
}

// ── Public: GET /public/orgs/links ──────────────────────────────────────────

/// The shop's links page, in one request.
///
/// Public for the same reason `/public/orgs/brand` is — it is the first thing
/// a customer's browser asks — and it answers "no shop" identically for a shop
/// that does not exist and one that is switched off, for the same reason.
#[utoipa::path(get, path = "/public/orgs/links", tag = "orgs",
    operation_id = "public_org_links", params(BrandQuery),
    responses((status = 200, body = PublicLinksPage), AppErrorResponse))]
pub async fn public_links(
    pool: web::Data<PgPool>,
    query: web::Query<BrandQuery>,
) -> Result<HttpResponse, AppError> {
    let pool = pool.get_ref();
    let org_id = resolve_org(pool, &query).await?;
    let slug: Option<Option<String>> = sqlx::query_scalar(
        "SELECT slug FROM organizations WHERE id = $1 AND is_active AND deleted_at IS NULL",
    )
    .bind(org_id)
    .fetch_optional(pool)
    .await?;
    let slug = slug.ok_or_else(|| AppError::NotFound("No shop at that address".into()))?;

    let org = crate::orgs::branding::load(pool, org_id).await?;
    let stored = load_stored(pool, org_id).await?;
    let avail = availability(pool, org_id).await?;
    let shop = links_shop_origin(pool, org_id).await?;

    let mut items = Vec::new();
    for item in stored.items.iter().filter(|i| i.visible) {
        if item.kind == LinksItemKind::Custom {
            let Some(url) = item.url.clone() else {
                continue;
            };
            items.push(PublicLinksItem {
                kind: item.kind,
                href: url,
                path: None,
                title_en: item.title_en.clone(),
                title_ar: item.title_ar.clone(),
                branch_names: Vec::new(),
                channels: Vec::new(),
            });
            continue;
        }
        if !avail.available(item.kind) {
            continue;
        }
        let Some(href) = links_module_href(shop.as_deref(), org_id, item.kind.key()) else {
            continue;
        };
        items.push(PublicLinksItem {
            kind: item.kind,
            href,
            path: links_module_path(item.kind.key()).map(str::to_string),
            title_en: None,
            title_ar: None,
            branch_names: avail.branch_names(item.kind),
            channels: if item.kind == LinksItemKind::Order {
                avail.channels()
            } else {
                Vec::new()
            },
        });
    }

    let branches = if stored.show_branches {
        avail
            .branches
            .iter()
            .filter_map(|b| {
                let s = stored.branches.get(&b.id).cloned().unwrap_or_default();
                (!s.hidden).then(|| PublicLinksBranch {
                    id: b.id,
                    name: b.name.clone(),
                    address: clean_text(b.address.as_deref()),
                    phone: clean_text(b.phone.as_deref()),
                    directions_url: directions_url(b, s.maps_url.as_deref()),
                })
            })
            .collect()
    } else {
        Vec::new()
    };

    let socials = org
        .social_links
        .iter()
        .map(|l| PublicSocialLink {
            key: l.key.to_string(),
            label: l.label.to_string(),
            url: l.url.clone(),
        })
        .collect();
    let loyalty_mode = items
        .iter()
        .any(|i| i.kind == LinksItemKind::Rewards)
        .then(|| avail.loyalty_mode.clone())
        .flatten();
    let cover_image_url = if stored.show_cover {
        org.card_image_url.clone()
    } else {
        None
    };

    Ok(HttpResponse::Ok()
        // A minute: short enough that a shop checking its own edit sees it,
        // long enough that a bio link shared to a crowd is one request each
        // for most of them, not a query storm.
        .insert_header(("Cache-Control", "public, max-age=60"))
        .json(PublicLinksPage {
            brand: brand_of(org_id, slug, org),
            tagline_en: stored.tagline_en,
            tagline_ar: stored.tagline_ar,
            cover_image_url,
            items,
            socials,
            branches,
            loyalty_mode,
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn custom(title: &str, url: &str) -> LinksPageItem {
        LinksPageItem {
            kind: LinksItemKind::Custom,
            visible: true,
            id: None,
            title_en: Some(title.into()),
            title_ar: None,
            url: Some(url.into()),
        }
    }

    #[test]
    fn every_module_appears_once_whatever_was_stored() {
        let items = normalize(vec![
            LinksPageItem::module(LinksItemKind::Book),
            custom("Beans", "https://beans.example"),
            LinksPageItem::module(LinksItemKind::Book),
        ]);
        let kinds: Vec<_> = items.iter().map(|i| i.kind).collect();
        assert_eq!(
            kinds,
            vec![
                LinksItemKind::Book,
                LinksItemKind::Custom,
                LinksItemKind::Order,
                LinksItemKind::Menu,
                LinksItemKind::Rewards,
            ],
            "the shop's order first, then the modules it never placed"
        );
    }

    #[test]
    fn a_custom_link_is_https_and_titled() {
        assert!(validate_items(vec![custom("Beans", "https://beans.example")]).is_ok());
        assert!(validate_items(vec![custom("Beans", "http://beans.example")]).is_err());
        assert!(validate_items(vec![custom("Beans", "javascript:alert(1)")]).is_err());
        assert!(validate_items(vec![custom("  ", "https://beans.example")]).is_err());
        let long = "x".repeat(MAX_TITLE_CHARS + 1);
        assert!(validate_items(vec![custom(&long, "https://beans.example")]).is_err());
    }

    #[test]
    fn saving_mints_ids_and_refuses_duplicate_modules() {
        let saved = validate_items(vec![custom("Beans", "https://beans.example")]).unwrap();
        assert!(saved[0].id.is_some());
        let twice = vec![
            LinksPageItem::module(LinksItemKind::Menu),
            LinksPageItem::module(LinksItemKind::Menu),
        ];
        assert!(validate_items(twice).is_err());
        let many = (0..=MAX_CUSTOM_LINKS)
            .map(|n| custom(&format!("L{n}"), "https://x.example"))
            .collect();
        assert!(validate_items(many).is_err());
    }

    #[test]
    fn directions_prefer_the_shops_pin() {
        let b = BranchRow {
            id: Uuid::nil(),
            name: "Maadi".into(),
            address: Some("14 Road 9, Maadi".into()),
            phone: None,
            latitude: Some(29.96),
            longitude: Some(31.25),
        };
        assert_eq!(
            directions_url(&b, Some("https://maps.app.goo.gl/abc")).as_deref(),
            Some("https://maps.app.goo.gl/abc")
        );
        assert!(
            directions_url(&b, None)
                .unwrap()
                .contains("query=29.96,31.25")
        );
        let no_geo = BranchRow {
            latitude: None,
            longitude: None,
            ..b
        };
        assert!(
            directions_url(&no_geo, None)
                .unwrap()
                .contains("14%20Road%209")
        );
    }
}
