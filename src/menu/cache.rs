//! Per-org cache of `GET /menu-items` bodies (the busiest read the POS makes:
//! `full=true` costs ~1,000 statements per call). The value is the SERIALIZED
//! body. Owner rule (2026-10-16): **a sync must never get stale data because of
//! this cache** — the next read after ANY write to an input is fresh.
//!
//! ## Every input of a cached body, and what keeps it fresh
//! The key is `(org, in-process version, fingerprint, variant)`:
//! * `variant` = the query (`category_id`, `branch_id`, `full`) and the caller's
//!   `sells_combos` (from `X-Madar-Client` / `User-Agent`). The body depends on
//!   nothing else about the caller: no role, plan, language or Accept header.
//! * `fingerprint` ([`fingerprint`], ONE query per read) =
//!   - the org's `catalog_revision`, bumped at COMMIT by the
//!     `catalog_revision_bump` trigger on every table the body reads its catalog
//!     from: `menu_items` (names, translations — translated in the writing
//!     request, not by a job — description, `image_url`, `image_group_id`,
//!     `is_active`, `kind`, meal links), `categories`, `menu_item_sizes`
//!     (sizes, all_sizes), `modifier_groups`, `modifier_options`,
//!     `menu_item_modifier_groups` (slots, optional fields, allowed add-ons,
//!     pricing), `recipe_lines` (recipes, pricing, default milk),
//!     `org_ingredients` (names, units, density, active), `ingredient_categories`,
//!     `menu_price_overrides` (branch price + availability: the prod views
//!     `branch_menu_overrides`, `branch_menu_size_overrides`,
//!     `branch_addon_overrides` read it), `menu_item_recipe_steps`, and the combo
//!     tables (`menu_item_combos`, `combo_slots`, `combo_slot_choices`,
//!     `combo_choice_size_surcharges`, `combo_channel_settings`,
//!     `combo_channel_branch_overrides`, `sale_windows` — passed through raw,
//!     never evaluated against the clock here);
//!   - `max(org_ingredients.updated_at)`: `yield_pct` (→ `recipes[].usable_quantity`)
//!     is outside the trigger's UPDATE filter;
//!   - `max(asset_jobs.updated_at)` of the org: an image being processed or
//!     finished (`image` refs) changes job rows, and the asset worker finishes
//!     the job when it writes `assets` / `asset_groups`;
//!   - `max(recipe_step_presets.updated_at)` (global): step preset names, notes
//!     and animation hashes (rewritten at boot, and by the assets backfill);
//!   - the UTC date: signed asset URLs carry an `exp` that moves at UTC midnight
//!     (it is always ≥ 24 h ahead, far beyond any TTL).
//! * the in-process `version`: [`MenuCache::invalidate`] from the write handlers.
//! * Environment (`UPLOADS_BASE_URL`, `API_PUBLIC_URL`, the URL signing key) is
//!   fixed for the life of the process, and so is this cache.
//! * Not inputs: `branch_stock` / sold-out state (not in this body), deal rules,
//!   channel-scope overrides.
//!
//! ## Where it is OFF
//! The trigger only exists on the PROD shape of the catalog, where the legacy
//! relations (`branch_menu_overrides`, `menu_item_recipes`, `addon_items`, …) are
//! read-only views over the unified tables. Every database built from the
//! migrations has that shape since DB Phase 2.4
//! (`20261018200000_p2_converge_legacy_catalog`); one that somehow still has
//! them as writable tables would see writes that bump nothing, so the cache
//! switches itself off there ([`MenuCache::usable`]).
//!
//! `MENU_CACHE_TTL_SECS`: default 600 (a memory-only backstop; freshness comes
//! from the key), `0` = OFF. Apps that never register the cache (every test that
//! does not ask for it) get `None` and the live DB path.

use actix_web::web::Bytes;
use moka::future::Cache;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use uuid::Uuid;

/// One cached body: the JSON, its ETag (a hash of the bytes, so it is the same
/// on every instance), and the body compressed once per content coding the
/// first time a client asks for it (OPT-A: a cache hit costs no compression).
pub struct CachedBody {
    pub body: Bytes,
    /// Weak (the body also goes out compressed): `W/"m-<sha256 prefix>"`.
    pub etag: String,
    encoded: Mutex<HashMap<&'static str, Bytes>>,
}

/// Content codings a cached body is stored in, with their levels. Higher than
/// the per-response levels actix uses (brotli 3, gzip 1, zstd 3), because the
/// cost is paid once per body instead of once per response. Measured on a
/// 1.09 MB `full=true` body: brotli 5 → 51 KB in 9 ms (q3: 65 KB), gzip 6 →
/// 96 KB in 11 ms (level 1: 149 KB).
pub const STORED_CODINGS: [&str; 3] = ["br", "gzip", "zstd"];

impl CachedBody {
    pub fn new(body: Bytes) -> Self {
        use sha2::{Digest, Sha256};
        let d = Sha256::digest(&body);
        let hex: String = d[..12].iter().map(|b| format!("{b:02x}")).collect();
        Self {
            body,
            etag: format!("W/\"m-{hex}\""),
            encoded: Mutex::new(HashMap::new()),
        }
    }

    /// The body in `coding` (one of [`STORED_CODINGS`]), compressed on the
    /// first ask and kept. `None` for any other coding.
    pub async fn encoded(self: &Arc<Self>, coding: &str) -> Option<Bytes> {
        let coding: &'static str = STORED_CODINGS.into_iter().find(|c| *c == coding)?;
        if let Some(b) = self
            .encoded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(coding)
        {
            return Some(b.clone());
        }
        let me = self.clone();
        let out = actix_web::web::block(move || {
            let t = std::time::Instant::now();
            let out = compress(coding, &me.body);
            tracing::debug!(
                target: "madar::menu_build",
                coding,
                bytes = me.body.len(),
                compressed = out.as_ref().map_or(0, |b| b.len()),
                compress_us = t.elapsed().as_micros() as u64,
                "stored coding"
            );
            out
        })
        .await
        .ok()??;
        self.encoded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(coding, out.clone());
        Some(out)
    }
}

/// The content coding the `Compress` middleware would pick for this request
/// (same negotiation, same supported list), when it is a stored one.
fn negotiated(req: &actix_web::HttpRequest) -> Option<&'static str> {
    use actix_web::HttpMessage;
    use actix_web::http::header::{AcceptEncoding, Encoding};
    let accept = req.get_header::<AcceptEncoding>()?;
    let supported = [
        Encoding::identity(),
        Encoding::brotli(),
        Encoding::gzip(),
        Encoding::deflate(),
        Encoding::zstd(),
    ];
    let chosen = accept.negotiate(supported.iter())?.to_string();
    STORED_CODINGS.into_iter().find(|c| *c == chosen)
}

/// A cached `/menu-items` body as a response: `304` when the caller already
/// holds it (`If-None-Match` = its ETag), else the body in the negotiated
/// coding from the stored copy (the outer `Compress` leaves a response that
/// already has a `Content-Encoding` alone). The decoded body is the same bytes
/// a client always got; the ETag and `Cache-Control` headers are additive.
pub async fn respond(
    req: &actix_web::HttpRequest,
    entry: Arc<CachedBody>,
) -> actix_web::HttpResponse {
    respond_as(req, entry, PRIVATE, true).await
}

/// `Cache-Control` of a till's menu: only the caller may keep it, and it
/// revalidates (ETag) before every use.
pub const PRIVATE: &str = "private, no-cache";

/// `Cache-Control` of a public menu (`/public/menu-display`, the dine-in table
/// menu): anyone may keep it, but must revalidate before every use, so an
/// edit is never hidden behind a max-age; the ETag makes that revalidation a
/// bodiless 304. The server side is the menu cache, keyed on the database
/// fingerprint ([`public_fingerprint`]), so a crowd scanning one table card
/// costs one build per edit, on any instance.
pub const PUBLIC: &str = "public, no-cache";

/// [`respond`] with the given `Cache-Control`. `stored`: serve the entry's
/// stored codings (a body that lives in the cache, compressed once at the
/// stored level). `false` for a body built for this one response: it goes out
/// plain and the `Compress` middleware encodes it at its cheap per-response
/// level, as every dynamic body is.
pub async fn respond_as(
    req: &actix_web::HttpRequest,
    entry: Arc<CachedBody>,
    cache_control: &'static str,
    stored: bool,
) -> actix_web::HttpResponse {
    use actix_web::http::header;
    let held = req
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == entry.etag));
    if held {
        return actix_web::HttpResponse::NotModified()
            .insert_header((header::ETAG, entry.etag.clone()))
            .insert_header((header::CACHE_CONTROL, cache_control))
            .finish();
    }
    let mut res = actix_web::HttpResponse::Ok();
    res.content_type("application/json")
        .insert_header((header::ETAG, entry.etag.clone()))
        .insert_header((header::CACHE_CONTROL, cache_control));
    if stored
        && let Some(coding) = negotiated(req)
        && let Some(body) = entry.encoded(coding).await
    {
        res.insert_header((header::CONTENT_ENCODING, coding))
            .insert_header((header::VARY, "accept-encoding"));
        return res.body(body);
    }
    res.body(entry.body.clone())
}

/// Compress `body` in `coding` at the stored level.
pub fn compress(coding: &str, body: &[u8]) -> Option<Bytes> {
    use std::io::Write;
    let out = match coding {
        "br" => {
            let mut out = Vec::with_capacity(body.len() / 8);
            {
                let mut w = brotli::CompressorWriter::new(&mut out, 32 * 1024, 5, 22);
                w.write_all(body).ok()?;
            }
            out
        }
        "gzip" => {
            let mut w = flate2::write::GzEncoder::new(
                Vec::with_capacity(body.len() / 8),
                flate2::Compression::new(6),
            );
            w.write_all(body).ok()?;
            w.finish().ok()?
        }
        "zstd" => zstd::bulk::compress(body, 9).ok()?,
        _ => return None,
    };
    Some(Bytes::from(out))
}

/// The TTL when `MENU_CACHE_TTL_SECS` is unset.
pub const DEFAULT_TTL_SECS: u64 = 600;

pub struct MenuCache {
    enabled: bool,
    /// Whether the database has the prod catalog shape (see the module doc),
    /// checked once.
    shape_ok: tokio::sync::OnceCell<bool>,
    bodies: Cache<String, Arc<CachedBody>>,
    versions: RwLock<HashMap<Uuid, u64>>,
}

impl MenuCache {
    /// Build from `MENU_CACHE_TTL_SECS` (unset → 600 s, `0` → disabled).
    pub fn from_env() -> Self {
        let ttl = std::env::var("MENU_CACHE_TTL_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_TTL_SECS);
        let enabled = ttl > 0;
        if enabled {
            tracing::info!("Menu cache ENABLED (ttl={ttl}s)");
        }
        Self {
            enabled,
            shape_ok: tokio::sync::OnceCell::new(),
            bodies: Cache::builder()
                .time_to_live(Duration::from_secs(ttl.max(1)))
                .max_capacity(10_000)
                .build(),
            versions: RwLock::new(HashMap::new()),
        }
    }

    /// A cache that is on with `ttl`, whatever the environment says (tests).
    #[doc(hidden)]
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            enabled: true,
            shape_ok: tokio::sync::OnceCell::new(),
            bodies: Cache::builder().time_to_live(ttl).max_capacity(10_000).build(),
            versions: RwLock::new(HashMap::new()),
        }
    }

    /// Mark the database shape as checked (tests that exercise the cache on a
    /// fresh database, writing only to inputs the fingerprint covers there).
    #[doc(hidden)]
    pub fn assume_prod_shape(&self) {
        let _ = self.shape_ok.set(true);
    }

    /// On, and the database has the prod catalog shape: every legacy catalog
    /// relation is a view (or gone), so every catalog write bumps the revision.
    pub async fn usable(&self, pool: &sqlx::PgPool) -> bool {
        if !self.enabled {
            return false;
        }
        *self
            .shape_ok
            .get_or_init(|| async {
                let writable: i64 = sqlx::query_scalar!(
                    r#"SELECT count(*) AS "n!" FROM pg_class
                        WHERE relnamespace = 'public'::regnamespace AND relkind = 'r'
                          AND relname = ANY($1)"#,
                    &LEGACY_RELATIONS.map(String::from)[..]
                )
                .fetch_one(pool)
                .await
                .unwrap_or(1);
                if writable > 0 {
                    tracing::warn!("menu cache OFF: the legacy catalog relations are writable tables here (not the prod shape)");
                }
                writable == 0
            })
            .await
    }

    /// Whether the cache is on (`MENU_CACHE_TTL_SECS` > 0).
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    fn version(&self, org: Uuid) -> u64 {
        *self.versions.read().unwrap().get(&org).unwrap_or(&0)
    }

    /// The org's menu version: changes on every menu write while the cache is
    /// on, so another cache can key on it. Always 0 when the cache is off.
    pub fn version_of(&self, org: Uuid) -> u64 {
        if !self.enabled {
            return 0;
        }
        self.version(org)
    }

    /// Drop an org's cached menu by bumping its version. Call on every write that
    /// can change a menu/category listing for the org.
    pub fn invalidate(&self, org: Uuid) {
        if !self.enabled {
            return;
        }
        *self.versions.write().unwrap().entry(org).or_insert(0) += 1;
    }

    fn key(&self, org: Uuid, variant: &str) -> String {
        format!("{org}:{}:{variant}", self.version(org))
    }

    pub async fn get(&self, org: Uuid, variant: &str) -> Option<Bytes> {
        Some(self.get_entry(org, variant).await?.body.clone())
    }

    pub async fn put(&self, org: Uuid, variant: &str, body: Bytes) {
        self.put_entry(org, variant, body).await;
    }

    /// The cached body with its ETag and stored codings.
    pub async fn get_entry(&self, org: Uuid, variant: &str) -> Option<Arc<CachedBody>> {
        if !self.enabled {
            return None;
        }
        self.bodies.get(&self.key(org, variant)).await
    }

    /// Cache `body` (when on) and return it as an entry either way, so the
    /// response that built it carries the same ETag a later hit will.
    pub async fn put_entry(&self, org: Uuid, variant: &str, body: Bytes) -> Arc<CachedBody> {
        let entry = Arc::new(CachedBody::new(body));
        if self.enabled {
            self.bodies
                .insert(self.key(org, variant), entry.clone())
                .await;
        }
        entry
    }
}

/// The legacy catalog relations: read-only views in prod, writable (untriggered)
/// tables on a database that predates the shim.
pub const LEGACY_RELATIONS: [&str; 12] = [
    "addon_items",
    "addon_item_ingredients",
    "menu_item_recipes",
    "menu_item_optional_fields",
    "menu_item_addon_slots",
    "menu_item_allowed_addons",
    "menu_item_addon_overrides",
    "branch_menu_overrides",
    "branch_menu_size_overrides",
    "branch_addon_overrides",
    "branch_channel_menu_overrides",
    "branch_channel_addon_overrides",
];

/// Everything a cached `/menu-items` body depends on besides the catalog
/// revision, in one key part (see the module doc). One query.
pub async fn fingerprint(pool: &sqlx::PgPool, org: Uuid) -> Result<String, sqlx::Error> {
    let r = sqlx::query!(
        r#"SELECT
             COALESCE((SELECT revision FROM catalog_revision WHERE org_id = $1), 0) AS "rev!",
             (SELECT max(updated_at) FROM org_ingredients WHERE org_id = $1) AS ingredients,
             (SELECT max(updated_at) FROM asset_jobs WHERE org_id = $1) AS assets,
             (SELECT max(updated_at) FROM recipe_step_presets) AS presets,
             (now() AT TIME ZONE 'UTC')::date AS "day!",
             -- The database, so a key never matches another database's
             -- (process-global users of this key, e.g. the lint cache).
             current_database()::text AS "db!""#,
        org
    )
    .fetch_one(pool)
    .await?;
    let t = |v: Option<chrono::DateTime<chrono::Utc>>| v.map_or(0, |t| t.timestamp_micros());
    Ok(format!(
        "{}.{}.{}.{}.{}.{}",
        r.rev,
        t(r.ingredients),
        t(r.assets),
        t(r.presets),
        r.day,
        r.db
    ))
}

/// Everything a cached PUBLIC menu body depends on, in one query: what
/// [`fingerprint`] covers for the till's menu (the catalogue — items, sizes,
/// groups, options, recipes, branch and channel overrides, combos, deals and
/// sale windows all bump `catalog_revision` — ingredient yields, asset jobs and
/// the UTC date that moves signed image URLs), plus what only the public pages
/// read:
/// * the organization row (name, slug, branding, colours, plan, timezone,
///   active) and the org's branch rows (which branch the page shows, its name,
///   its timezone, open or not), each as a hash of the whole row, so no
///   `updated_at` discipline is assumed;
/// * the `org_links_pages` row (the menu-display switch, titles, branch pick);
/// * the clock, ONLY for an org with sale windows (combos and deals on sale
///   "now"): the current minute, as windows open and close on whole minutes
///   — down to the microsecond if any window boundary has seconds.
///
/// A public page is open to anyone, so its key never trusts this process's
/// memory alone: every read runs this on the database, which is what makes the
/// cache safe across instances (a write on another instance changes a row
/// here before its commit returns).
pub async fn public_fingerprint(pool: &sqlx::PgPool, org: Uuid) -> Result<String, sqlx::Error> {
    let base = fingerprint(pool, org).await?;
    let r = sqlx::query!(
        r#"SELECT
             (SELECT md5(o::text) FROM organizations o WHERE o.id = $1) AS org_row,
             (SELECT md5(string_agg(b::text, ',' ORDER BY b.id)) FROM branches b
               WHERE b.org_id = $1) AS branch_rows,
             (SELECT md5(p::text) FROM org_links_pages p WHERE p.org_id = $1) AS page_row,
             CASE WHEN EXISTS (SELECT 1 FROM sale_windows w WHERE w.org_id = $1)
                  THEN to_char(now() AT TIME ZONE 'UTC', 'YYYYMMDDHH24MI')
                       || CASE WHEN EXISTS (
                               SELECT 1 FROM sale_windows w
                                WHERE w.org_id = $1
                                  AND (extract(second FROM w.starts_at) <> 0
                                    OR extract(second FROM w.ends_at) <> 0))
                          THEN to_char(clock_timestamp(), 'SS.US') ELSE '' END
                  ELSE '' END AS "clock!""#,
        org
    )
    .fetch_one(pool)
    .await?;
    Ok(format!(
        "{base}.{}.{}.{}.{}",
        r.org_row.unwrap_or_default(),
        r.branch_rows.unwrap_or_default(),
        r.page_row.unwrap_or_default(),
        r.clock
    ))
}
