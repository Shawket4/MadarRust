//! The tenant shell: the HTML a shop's own address serves, with the shop in it.
//!
//! A shop's host (`rue.madar-pos.cloud`) serves three single-page apps from
//! disk: the links page and rewards (`loyalty.html`), ordering under `/order/`
//! and bookings under `/book/`. People get the React app and never notice what
//! the HTML said. Crawlers and AI agents read the HTML and nothing else, and
//! that HTML said "Madar — Rewards" for every shop on the platform.
//!
//! nginx now hands page requests on a shop host to [`shell`] (the wildcard vhost,
//! `deploy/shop/nginx-wildcard.conf`), which returns the same app entry with the
//! shop written into it:
//!  - the `<head>` between `<!-- madar:head -->` and `<!-- /madar:head -->`: the
//!    shop's title and description, canonical address, share tags and JSON-LD
//!    (a `Restaurant` with its branches, its menu and the order and booking
//!    actions that are switched on);
//!  - the `<noscript>` between `<!-- madar:noscript -->` and its end marker: the
//!    shop's links, menu and branches as plain HTML.
//!
//! Everything comes from the loaders the public API already uses
//! (`public_links_page`, `load_public_menu`), so the HTML cannot say something
//! the app does not. Unknown shops and paths the apps do not route get a real
//! 404. When the template is missing, or anything else fails, the answer is a
//! 5xx, and nginx serves the static file exactly as it did before this existed.
//!
//! The templates are the deployed app entries, mounted read-only into the
//! container (`docker-compose.yml`, `MADAR_SHELL_DIR`, default `/app/shells`):
//! `loyalty/loyalty.html`, `order/order.html`, `book/reservations.html`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, RwLock};
use std::time::{Duration, SystemTime};

use actix_web::{HttpRequest, HttpResponse, web};
use moka::future::Cache;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use utoipa::IntoParams;
use uuid::Uuid;

use crate::delivery::public::{DeliveryMenu, load_public_menu};
use crate::errors::AppError;
use crate::menu::cache::MenuCache;
use crate::orgs::links_page::{LinksItemKind, PublicLinksPage, public_links_page};
use crate::orgs::public::{BrandQuery, resolve_org};
use crate::uploads::handlers::normalize_upload_url;

/// The parent domain of every shop host.
const SHOP_DOMAIN: &str = "madar-pos.cloud";
/// Madar's own site, for the 404 pages and the "Powered by" line.
const MADAR_SITE: &str = "https://get.madar-pos.cloud/";
/// The share image when the shop has neither a cover nor a logo.
const FALLBACK_IMAGE: &str = "https://get.madar-pos.cloud/og/en-home.jpg";
/// A rendered page is reused this long at most; a brand or links edit drops it
/// at once ([`invalidate`]), a menu edit at the latest after this.
const TTL: Duration = Duration::from_secs(60);
/// Menu items written into one page, so a giant catalogue can't make a giant page.
const MAX_MENU_ITEMS: usize = 300;

// ── Which page ──────────────────────────────────────────────────────────────

/// The app entry a page is served from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bundle {
    /// `loyalty.html`: the links page, rewards, join and the member's card.
    Links,
    /// `order.html` under `/order/`.
    Order,
    /// `reservations.html` under `/book/`.
    Book,
}

/// A page the shop apps route, by what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Links,
    Rewards,
    Join,
    Card,
    Order,
    Menu,
    Track,
    OrderAgain,
    Book,
    Booking,
}

impl Page {
    pub fn bundle(self) -> Bundle {
        match self {
            Page::Links | Page::Rewards | Page::Join | Page::Card => Bundle::Links,
            Page::Order | Page::Menu | Page::Track | Page::OrderAgain => Bundle::Order,
            Page::Book | Page::Booking => Bundle::Book,
        }
    }

    /// Pages that belong to one person (a card, an order, a booking) or are a
    /// counter's sign-up form stay out of search results.
    pub fn indexable(self) -> bool {
        matches!(
            self,
            Page::Links | Page::Rewards | Page::Order | Page::Menu | Page::Book
        )
    }

    /// The address search engines should know the page by.
    fn canonical_path(self) -> &'static str {
        match self {
            Page::Links => "/",
            Page::Rewards => "/rewards",
            Page::Order => "/order/",
            Page::Menu => "/order/menu",
            Page::Book => "/book/",
            // Never canonical (not indexable); the shop root stands in.
            Page::Join | Page::Card | Page::Track | Page::OrderAgain | Page::Booking => "/",
        }
    }

    /// Whether the page carries the menu (in the JSON-LD and the noscript).
    fn shows_menu(self) -> bool {
        matches!(self, Page::Links | Page::Order | Page::Menu)
    }
}

fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn is_uuid(s: &str) -> bool {
    Uuid::parse_str(s).is_ok()
}

/// The page a path is, or `None` when no shop app routes it. Mirrors the
/// routers in MadarDashboard (`src/loyalty/main.tsx`, `src/order/main.tsx`
/// with base `/order/`, `src/reservations/main.tsx` with base `/book/`).
pub fn classify(path: &str) -> Option<Page> {
    let path = path.split(['?', '#']).next().unwrap_or("");
    if !path.starts_with('/') {
        return None;
    }
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    // A doubled slash is not an address any app links to.
    if path.contains("//") {
        return None;
    }
    match segs.as_slice() {
        [] => Some(Page::Links),
        ["rewards"] => Some(Page::Rewards),
        ["join", "org", id] if is_uuid(id) => Some(Page::Join),
        ["join", branch] if is_token(branch) => Some(Page::Join),
        ["card", token] if is_token(token) => Some(Page::Card),
        ["order"] => Some(Page::Order),
        ["order", "menu"] => Some(Page::Menu),
        ["order", "track", id] if is_token(id) => Some(Page::Track),
        ["order", "now", token] if is_token(token) => Some(Page::OrderAgain),
        ["order", "order", org] if is_uuid(org) => Some(Page::Order),
        ["order", org] if is_uuid(org) => Some(Page::Order),
        ["order", org, branch] if is_uuid(org) && is_uuid(branch) => Some(Page::Order),
        ["book"] => Some(Page::Book),
        ["book", "manage", token] if is_token(token) => Some(Page::Booking),
        ["book", org] if is_uuid(org) => Some(Page::Book),
        ["book", org, branch] if is_uuid(org) && is_uuid(branch) => Some(Page::Book),
        _ => None,
    }
}

/// The shop's slug from its host: `rue` for `rue.madar-pos.cloud`. One label
/// only; anything else is not a shop host.
pub fn slug_of_host(host: &str) -> Option<String> {
    let host = host
        .split(':')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let label = host.strip_suffix(&format!(".{SHOP_DOMAIN}"))?;
    (!label.is_empty()
        && !label.contains('.')
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
    .then(|| label.to_string())
}

// ── Templates ───────────────────────────────────────────────────────────────

/// Where the app entries are. Registered as app data in `main.rs`; a test can
/// register its own.
#[derive(Debug, Clone)]
pub struct ShellConfig {
    pub dir: PathBuf,
}

impl ShellConfig {
    pub fn from_env() -> Self {
        Self {
            dir: std::env::var("MADAR_SHELL_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("/app/shells")),
        }
    }

    fn template(&self, bundle: Bundle) -> PathBuf {
        match bundle {
            Bundle::Links => self.dir.join("loyalty/loyalty.html"),
            Bundle::Order => self.dir.join("order/order.html"),
            Bundle::Book => self.dir.join("book/reservations.html"),
        }
    }
}

/// A template's body and the modification time it was read at.
type Template = (SystemTime, Arc<str>);

/// Template bodies, re-read only when the file's modification time changes (a
/// deploy writes a new file).
static TEMPLATES: LazyLock<RwLock<HashMap<PathBuf, Template>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

async fn template(path: &Path) -> Option<Template> {
    let modified = tokio::fs::metadata(path).await.ok()?.modified().ok()?;
    if let Some((at, body)) = TEMPLATES.read().ok()?.get(path)
        && *at == modified
    {
        return Some((modified, body.clone()));
    }
    let body: Arc<str> = tokio::fs::read_to_string(path).await.ok()?.into();
    if let Ok(mut map) = TEMPLATES.write() {
        map.insert(path.to_path_buf(), (modified, body.clone()));
    }
    Some((modified, body))
}

// ── Cache ───────────────────────────────────────────────────────────────────

static RENDERED: LazyLock<Cache<String, Arc<String>>> = LazyLock::new(|| {
    Cache::builder()
        .max_capacity(5_000)
        .time_to_live(TTL)
        .build()
});

/// Per-org generation, bumped by [`invalidate`]; part of every cache key, so
/// a bump orphans the org's rendered pages at once.
static GENERATIONS: LazyLock<RwLock<HashMap<Uuid, u64>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

fn generation(org: Uuid) -> u64 {
    GENERATIONS
        .read()
        .map(|m| m.get(&org).copied().unwrap_or(0))
        .unwrap_or(0)
}

/// Drop a shop's rendered pages. Call after a write that changes its name,
/// logo, palette, social links or links page.
pub fn invalidate(org: Uuid) {
    if let Ok(mut m) = GENERATIONS.write() {
        *m.entry(org).or_insert(0) += 1;
    }
}

// ── The endpoint ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ShellQuery {
    /// The shop's host, e.g. `rue.madar-pos.cloud`. nginx sends it as the
    /// `X-Shell-Host` header; the query parameter is for tests and probes.
    pub host: Option<String>,
    /// The page's path and query, e.g. `/order/menu?branch=…`. nginx sends it
    /// as the `X-Shell-Path` header.
    pub path: Option<String>,
}

/// A shop page's HTML: the app entry with the shop's head and noscript.
///
/// Called by nginx for page requests on a shop's own host. Public by nature:
/// it returns what the shop's public pages and the public JSON endpoints
/// already show.
#[utoipa::path(get, path = "/public/tenant-shell", tag = "orgs",
    operation_id = "public_tenant_shell", params(ShellQuery),
    responses(
        (status = 200, description = "The page's HTML", content_type = "text/html"),
        (status = 404, description = "No such shop, or a path its apps don't route", content_type = "text/html"),
        (status = 503, description = "The app entry isn't deployed here (nginx serves the static file)", content_type = "text/html"),
    ))]
pub async fn shell(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    config: Option<web::Data<ShellConfig>>,
    menu_cache: Option<web::Data<MenuCache>>,
    query: web::Query<ShellQuery>,
) -> HttpResponse {
    let header = |name: &str| {
        req.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let host = header("x-shell-host")
        .or_else(|| query.host.clone())
        .unwrap_or_default();
    let full_path = header("x-shell-path")
        .or_else(|| query.path.clone())
        .unwrap_or_else(|| "/".into());
    let config = config
        .map(|c| c.get_ref().clone())
        .unwrap_or_else(ShellConfig::from_env);

    let Some(slug) = slug_of_host(&host) else {
        return not_found_page(None);
    };
    let pool = pool.get_ref();
    let org_id = match resolve_org(
        pool,
        &BrandQuery {
            org_id: None,
            slug: Some(slug.clone()),
        },
    )
    .await
    {
        Ok(id) => id,
        Err(AppError::NotFound(_)) => return not_found_page(None),
        Err(_) => return unavailable(),
    };
    let origin = format!("https://{slug}.{SHOP_DOMAIN}");
    let Some(page) = classify(&full_path) else {
        let name = public_links_page(pool, org_id)
            .await
            .map(|p| p.brand.name)
            .unwrap_or_else(|_| slug.clone());
        return not_found_page(Some((&name, &origin)));
    };

    let Some((modified, body)) = template(&config.template(page.bundle())).await else {
        return unavailable();
    };
    let menu_generation = menu_cache.as_deref().map_or(0, |c| c.version_of(org_id));
    let stamp = modified
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let key = format!(
        "{org_id}:{}:{menu_generation}:{stamp}:{page:?}:{full_path}",
        generation(org_id)
    );
    if let Some(html) = RENDERED.get(&key).await {
        return html_response(200, html.as_str().to_string());
    }

    let links = match public_links_page(pool, org_id).await {
        Ok(l) => l,
        Err(AppError::NotFound(_)) => return not_found_page(None),
        Err(_) => return unavailable(),
    };
    let menu = if page.shows_menu() {
        match menu_branch(pool, org_id, &full_path).await {
            Some(branch) => load_public_menu(pool, org_id, branch, None).await.ok(),
            None => None,
        }
    } else {
        None
    };

    let html = render(&body, page, &origin, &links, menu.as_ref());
    RENDERED.insert(key, Arc::new(html.clone())).await;
    html_response(200, html)
}

/// The branch whose menu a page shows: `?branch=` when it is one of the shop's
/// own, else the shop's first active branch.
async fn menu_branch(pool: &PgPool, org_id: Uuid, full_path: &str) -> Option<Uuid> {
    let wanted = full_path.split_once('?').and_then(|(_, q)| {
        q.split('&')
            .find_map(|kv| kv.strip_prefix("branch="))
            .and_then(|v| Uuid::parse_str(v).ok())
    });
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM branches \
          WHERE org_id = $1 AND is_active AND deleted_at IS NULL \
          ORDER BY (id = $2) DESC, created_at, id LIMIT 1",
    )
    .bind(org_id)
    .bind(wanted.unwrap_or(Uuid::nil()))
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

// ── Rendering ───────────────────────────────────────────────────────────────

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// JSON for a `<script type="application/ld+json">`: nothing in it may close the
/// script element or open a comment, so `<`, `>` and `&` travel as `\u` escapes
/// (the same value to any JSON reader).
fn ld_json(v: &Value) -> String {
    serde_json::to_string(v)
        .unwrap_or_else(|_| "{}".into())
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
}

fn module_label(kind: LinksItemKind) -> &'static str {
    match kind {
        LinksItemKind::Order => "Order online",
        LinksItemKind::Menu => "Menu",
        LinksItemKind::Rewards => "Rewards",
        LinksItemKind::Book => "Book a table",
        LinksItemKind::Custom => "",
    }
}

fn piastres(p: i32) -> String {
    format!("{}.{:02}", p / 100, (p % 100).abs())
}

fn title_of(page: Page, name: &str, links: &PublicLinksPage) -> String {
    let what = match page {
        Page::Links => {
            let modules: Vec<&str> = links
                .items
                .iter()
                .filter(|i| i.kind != LinksItemKind::Custom)
                .map(|i| match i.kind {
                    LinksItemKind::Order => "order online",
                    LinksItemKind::Menu => "menu",
                    LinksItemKind::Rewards => "rewards",
                    LinksItemKind::Book => "book a table",
                    LinksItemKind::Custom => "",
                })
                .collect();
            if modules.is_empty() {
                return name.to_string();
            }
            return format!("{name}: {}", modules.join(", "));
        }
        Page::Rewards => "Rewards",
        Page::Join => "Join the rewards",
        Page::Card => "Your rewards card",
        Page::Order => "Order online",
        Page::Menu => "Menu",
        Page::Track => "Your order",
        Page::OrderAgain => "Order again",
        Page::Book => "Book a table",
        Page::Booking => "Your booking",
    };
    format!("{name} — {what}")
}

fn description_of(name: &str, links: &PublicLinksPage) -> String {
    if let Some(t) = links
        .tagline_en
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        return t.to_string();
    }
    let parts: Vec<&str> = links
        .items
        .iter()
        .filter(|i| i.kind != LinksItemKind::Custom)
        .map(|i| match i.kind {
            LinksItemKind::Order => "order online",
            LinksItemKind::Menu => "see the menu",
            LinksItemKind::Rewards => "join the rewards",
            LinksItemKind::Book => "book a table",
            LinksItemKind::Custom => "",
        })
        .collect();
    let mut what = match parts.as_slice() {
        [] => String::new(),
        [one] => (*one).to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    };
    if let Some(first) = what.get(0..1) {
        what = first.to_uppercase() + &what[1..];
    }
    if what.is_empty() {
        format!("{name}, on Madar POS.")
    } else {
        format!("{what} at {name}.")
    }
}

/// The cover, else the logo, as an absolute address on the current uploads host
/// (a shop's stored URL can name a host it has since moved off).
fn share_image(links: &PublicLinksPage) -> Option<String> {
    links
        .cover_image_url
        .as_deref()
        .or(links.brand.logo_url.as_deref())
        .map(normalize_upload_url)
        .filter(|u| !u.is_empty())
}

fn href_of(links: &PublicLinksPage, kind: LinksItemKind) -> Option<&str> {
    links
        .items
        .iter()
        .find(|i| i.kind == kind)
        .map(|i| i.href.as_str())
}

/// The shop as schema.org sees it.
pub fn restaurant_ld(origin: &str, links: &PublicLinksPage, menu: Option<&DeliveryMenu>) -> Value {
    let brand = &links.brand;
    let mut shop = json!({
        "@context": "https://schema.org",
        "@type": "Restaurant",
        "@id": format!("{origin}/#shop"),
        "name": brand.name,
        "url": format!("{origin}/"),
    });
    let o = shop.as_object_mut().expect("object");
    if let Some(logo) = &brand.logo_url {
        o.insert("logo".into(), json!(normalize_upload_url(logo)));
    }
    if let Some(image) = share_image(links) {
        o.insert("image".into(), json!(image));
    }
    if let Some(t) = links.tagline_en.as_deref().filter(|t| !t.trim().is_empty()) {
        o.insert("description".into(), json!(t));
    }
    let same_as: Vec<&str> = links.socials.iter().map(|s| s.url.as_str()).collect();
    if !same_as.is_empty() {
        o.insert("sameAs".into(), json!(same_as));
    }
    let place = |b: &crate::orgs::links_page::PublicLinksBranch| {
        let mut p = json!({ "@type": "Restaurant", "name": b.name });
        let m = p.as_object_mut().expect("object");
        if let Some(a) = &b.address {
            m.insert(
                "address".into(),
                json!({ "@type": "PostalAddress", "streetAddress": a, "addressCountry": "EG" }),
            );
        }
        if let Some(t) = &b.phone {
            m.insert("telephone".into(), json!(t));
        }
        p
    };
    if let Some((first, rest)) = links.branches.split_first() {
        if let Some(a) = &first.address {
            o.insert(
                "address".into(),
                json!({ "@type": "PostalAddress", "streetAddress": a, "addressCountry": "EG" }),
            );
        }
        if let Some(t) = &first.phone {
            o.insert("telephone".into(), json!(t));
        }
        if !rest.is_empty() {
            o.insert(
                "department".into(),
                Value::Array(rest.iter().map(place).collect()),
            );
        }
    }
    if let Some(menu) = menu {
        let mut sections = Vec::new();
        let mut written = 0usize;
        for cat in &menu.categories {
            let items: Vec<Value> = menu
                .items
                .iter()
                .filter(|i| i.category_id == Some(cat.id))
                .take(MAX_MENU_ITEMS.saturating_sub(written))
                .map(menu_item_ld)
                .collect();
            written += items.len();
            if !items.is_empty() {
                sections.push(
                    json!({ "@type": "MenuSection", "name": cat.name, "hasMenuItem": items }),
                );
            }
        }
        if !sections.is_empty() {
            let mut m = json!({ "@type": "Menu", "name": format!("{} menu", brand.name), "hasMenuSection": sections });
            if let Some(url) = href_of(links, LinksItemKind::Menu) {
                m.as_object_mut()
                    .expect("object")
                    .insert("url".into(), json!(url));
            }
            o.insert("hasMenu".into(), m);
        }
    }
    let entry = |url: &str| {
        json!({
            "@type": "EntryPoint",
            "urlTemplate": url,
            "actionPlatform": ["https://schema.org/DesktopWebPlatform", "https://schema.org/MobileWebPlatform"],
        })
    };
    let mut actions = Vec::new();
    if let Some(url) = href_of(links, LinksItemKind::Order) {
        actions.push(json!({ "@type": "OrderAction", "target": entry(url) }));
    }
    if let Some(url) = href_of(links, LinksItemKind::Book) {
        actions.push(json!({ "@type": "ReserveAction", "target": entry(url) }));
    }
    if !actions.is_empty() {
        o.insert("potentialAction".into(), Value::Array(actions));
    }
    shop
}

fn menu_item_ld(item: &crate::delivery::public::DeliveryMenuItem) -> Value {
    let offers: Vec<Value> = if item.sizes.is_empty() {
        vec![json!({ "@type": "Offer", "price": piastres(item.price), "priceCurrency": "EGP" })]
    } else {
        item.sizes
            .iter()
            .map(|s| json!({ "@type": "Offer", "name": s.label, "price": piastres(s.price), "priceCurrency": "EGP" }))
            .collect()
    };
    let mut v = json!({ "@type": "MenuItem", "name": item.name, "offers": offers });
    if let Some(d) = item.description.as_deref().filter(|d| !d.trim().is_empty()) {
        v.as_object_mut()
            .expect("object")
            .insert("description".into(), json!(d));
    }
    v
}

fn head(page: Page, origin: &str, links: &PublicLinksPage, menu: Option<&DeliveryMenu>) -> String {
    let name = &links.brand.name;
    let title = title_of(page, name, links);
    let description = description_of(name, links);
    let canonical = format!("{origin}{}", page.canonical_path());
    let image = share_image(links).unwrap_or_else(|| FALLBACK_IMAGE.into());
    let (t, d, c, i, n) = (
        escape(&title),
        escape(&description),
        escape(&canonical),
        escape(&image),
        escape(name),
    );
    let mut h = format!("<title>{t}</title>\n<meta name=\"description\" content=\"{d}\">\n");
    if page.indexable() {
        h.push_str(&format!("<link rel=\"canonical\" href=\"{c}\">\n"));
    } else {
        h.push_str("<meta name=\"robots\" content=\"noindex\">\n");
    }
    h.push_str(&format!(
        "<meta property=\"og:type\" content=\"website\">\n\
         <meta property=\"og:site_name\" content=\"{n}\">\n\
         <meta property=\"og:title\" content=\"{t}\">\n\
         <meta property=\"og:description\" content=\"{d}\">\n\
         <meta property=\"og:url\" content=\"{c}\">\n\
         <meta property=\"og:image\" content=\"{i}\">\n\
         <meta name=\"twitter:card\" content=\"summary_large_image\">\n\
         <meta name=\"twitter:title\" content=\"{t}\">\n\
         <meta name=\"twitter:description\" content=\"{d}\">\n\
         <meta name=\"twitter:image\" content=\"{i}\">\n"
    ));
    let ld = restaurant_ld(origin, links, menu.filter(|_| page.shows_menu()));
    h.push_str(&format!(
        "<script type=\"application/ld+json\">{}</script>\n",
        ld_json(&ld)
    ));
    h
}

fn noscript(page: Page, links: &PublicLinksPage, menu: Option<&DeliveryMenu>) -> String {
    let name = escape(&links.brand.name);
    let mut b = format!(
        "<noscript><div style=\"max-width:40rem;margin:2rem auto;padding:0 1rem;font-family:system-ui,sans-serif;line-height:1.5\">\n<h1>{name}</h1>\n<p>{}</p>\n",
        escape(&description_of(&links.brand.name, links))
    );
    let items: Vec<String> = links
        .items
        .iter()
        .filter_map(|i| {
            let label = if i.kind == LinksItemKind::Custom {
                i.title_en.as_deref().unwrap_or_default().to_string()
            } else {
                module_label(i.kind).to_string()
            };
            (!label.is_empty()).then(|| {
                format!(
                    "<li><a href=\"{}\">{}</a></li>",
                    escape(&i.href),
                    escape(&label)
                )
            })
        })
        .collect();
    if !items.is_empty() {
        b.push_str(&format!("<ul>\n{}\n</ul>\n", items.join("\n")));
    }
    if let Some(menu) = menu.filter(|_| page.shows_menu()) {
        let mut written = 0usize;
        let mut sections = String::new();
        for cat in &menu.categories {
            let rows: Vec<String> = menu
                .items
                .iter()
                .filter(|i| i.category_id == Some(cat.id))
                .take(MAX_MENU_ITEMS.saturating_sub(written))
                .map(|i| {
                    let price = if i.sizes.is_empty() {
                        format!("{} EGP", piastres(i.price))
                    } else {
                        i.sizes
                            .iter()
                            .map(|s| format!("{} {} EGP", escape(&s.label), piastres(s.price)))
                            .collect::<Vec<_>>()
                            .join(" · ")
                    };
                    format!("<li>{} — {price}</li>", escape(&i.name))
                })
                .collect();
            written += rows.len();
            if !rows.is_empty() {
                sections.push_str(&format!(
                    "<h3>{}</h3>\n<ul>\n{}\n</ul>\n",
                    escape(&cat.name),
                    rows.join("\n")
                ));
            }
        }
        if !sections.is_empty() {
            b.push_str("<h2>Menu</h2>\n");
            b.push_str(&sections);
        }
    }
    if !links.branches.is_empty() {
        b.push_str("<h2>Visit us</h2>\n<ul>\n");
        for br in &links.branches {
            let mut line = escape(&br.name);
            if let Some(a) = &br.address {
                line.push_str(&format!(": {}", escape(a)));
            }
            if let Some(p) = &br.phone {
                line.push_str(&format!(
                    " · <a href=\"tel:{}\">{}</a>",
                    escape(&p.replace(' ', "")),
                    escape(p)
                ));
            }
            b.push_str(&format!("<li>{line}</li>\n"));
        }
        b.push_str("</ul>\n");
    }
    b.push_str(&format!(
        "<p><a href=\"{MADAR_SITE}\">Powered by Madar POS</a></p>\n</div></noscript>"
    ));
    b
}

/// The text between two markers replaced, or `None` when the markers aren't there.
fn replace_between(s: &str, start: &str, end: &str, with: &str) -> Option<String> {
    let a = s.find(start)?;
    let b = a + s[a..].find(end)?;
    Some(format!("{}{start}\n{with}{}", &s[..a], &s[b..]))
}

/// The template with the shop's head and noscript in it. An app entry built
/// before the markers existed gets the head appended (its own `<title>`
/// removed) and the noscript right after `<body…>`.
pub fn inject(template: &str, head: &str, noscript: &str) -> String {
    let with_head = replace_between(
        template,
        "<!-- madar:head -->",
        "<!-- /madar:head -->",
        head,
    )
    .unwrap_or_else(|| {
        let mut t = template.to_string();
        if let (Some(a), Some(b)) = (t.find("<title>"), t.find("</title>"))
            && b > a
        {
            t.replace_range(a..b + "</title>".len(), "");
        }
        match t.find("</head>") {
            Some(at) => format!("{}{head}{}", &t[..at], &t[at..]),
            None => t,
        }
    });
    replace_between(
        &with_head,
        "<!-- madar:noscript -->",
        "<!-- /madar:noscript -->",
        &format!("{noscript}\n"),
    )
    .unwrap_or_else(|| {
        let t = with_head;
        let Some(open) = t.find("<body") else {
            return t;
        };
        let Some(close) = t[open..].find('>') else {
            return t;
        };
        let at = open + close + 1;
        format!("{}\n{noscript}{}", &t[..at], &t[at..])
    })
}

pub fn render(
    template: &str,
    page: Page,
    origin: &str,
    links: &PublicLinksPage,
    menu: Option<&DeliveryMenu>,
) -> String {
    inject(
        template,
        &head(page, origin, links, menu),
        &noscript(page, links, menu),
    )
}

fn html_response(status: u16, body: String) -> HttpResponse {
    HttpResponse::build(
        actix_web::http::StatusCode::from_u16(status).unwrap_or(actix_web::http::StatusCode::OK),
    )
    .content_type("text/html; charset=utf-8")
    .insert_header(("Cache-Control", "no-cache"))
    .body(body)
}

/// A 5xx with no body worth reading: nginx answers it with the static file.
fn unavailable() -> HttpResponse {
    html_response(503, "<!doctype html><title>Unavailable</title>".into())
}

/// The 404 page: for an address no shop has (`None`), or a path a shop's apps
/// don't route (`Some((name, origin))`, with a way back to the shop).
fn not_found_page(shop: Option<(&str, &str)>) -> HttpResponse {
    let (title, heading, back) = match shop {
        None => (
            "Shop not found · Madar POS".to_string(),
            "There's no shop at this address.".to_string(),
            format!("<a href=\"{MADAR_SITE}\">Madar POS</a>"),
        ),
        Some((name, origin)) => (
            format!("Page not found · {}", escape(name)),
            "This page isn't here.".to_string(),
            format!(
                "<a href=\"{}/\">Back to {}</a>",
                escape(origin),
                escape(name)
            ),
        ),
    };
    html_response(
        404,
        format!(
            "<!doctype html>\n<html lang=\"en\" dir=\"ltr\"><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
             <meta name=\"robots\" content=\"noindex\"><title>{title}</title>\
             <style>body{{margin:0;min-height:100vh;display:grid;place-items:center;background:#14181E;color:#EFF3F4;font:500 17px/1.5 system-ui,sans-serif}}main{{padding:2rem;max-width:32rem}}a{{color:#9AA6AD}}</style>\
             </head><body><main><h1>{heading}</h1><p lang=\"ar\" dir=\"rtl\">الصفحة دي مش موجودة.</p><p>{back}</p></main></body></html>"
        ),
    )
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    // HEAD too: link checkers and some crawlers ask with it, and nginx passes
    // the method through.
    cfg.service(
        web::resource("/public/tenant-shell")
            .route(web::get().to(shell))
            .route(web::head().to(shell)),
    );
}
