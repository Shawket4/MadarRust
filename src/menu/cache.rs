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
//! read-only views over the unified tables (`deploy/menu_unification_shim.sql`).
//! On a database where they are still writable tables (fresh dev and test
//! databases, until Phase 2.4), writes to them bump nothing, so the cache
//! switches itself off there ([`MenuCache::usable`]).
//!
//! `MENU_CACHE_TTL_SECS`: default 600 (a memory-only backstop; freshness comes
//! from the key), `0` = OFF. Apps that never register the cache (every test that
//! does not ask for it) get `None` and the live DB path.

use actix_web::web::Bytes;
use moka::future::Cache;
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;
use uuid::Uuid;

/// The TTL when `MENU_CACHE_TTL_SECS` is unset.
pub const DEFAULT_TTL_SECS: u64 = 600;

pub struct MenuCache {
    enabled: bool,
    /// Whether the database has the prod catalog shape (see the module doc),
    /// checked once.
    shape_ok: tokio::sync::OnceCell<bool>,
    bodies: Cache<String, Bytes>,
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
        if !self.enabled {
            return None;
        }
        self.bodies.get(&self.key(org, variant)).await
    }

    pub async fn put(&self, org: Uuid, variant: &str, body: Bytes) {
        if !self.enabled {
            return;
        }
        self.bodies.insert(self.key(org, variant), body).await;
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
             (now() AT TIME ZONE 'UTC')::date AS "day!""#,
        org
    )
    .fetch_one(pool)
    .await?;
    let t = |v: Option<chrono::DateTime<chrono::Utc>>| v.map_or(0, |t| t.timestamp_micros());
    Ok(format!(
        "{}.{}.{}.{}.{}",
        r.rev,
        t(r.ingredients),
        t(r.assets),
        t(r.presets),
        r.day
    ))
}
