//! Optional per-org cache for the heavy menu-listing reads (the busiest endpoint
//! in the SaaS read mix). The cached value is the SERIALIZED response body, keyed
//! by `(org, version, variant)` where `variant` folds in the query params that
//! change the result (category/branch/full). A menu or category write bumps the
//! org's `version`, which instantly orphans that org's cached entries; the TTL is
//! a backstop for any write path that forgets to invalidate.
//!
//! Env-gated: `MENU_CACHE_TTL_SECS` (default 0 = OFF). When off, every method is a
//! no-op, so production and the test suite are unchanged unless it's enabled. The
//! handlers extract it as `Option<web::Data<MenuCache>>`, so apps that never
//! register it (every test) simply get `None` and the live DB path.

use actix_web::web::Bytes;
use moka::future::Cache;
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;
use uuid::Uuid;

pub struct MenuCache {
    enabled: bool,
    bodies: Cache<String, Bytes>,
    versions: RwLock<HashMap<Uuid, u64>>,
}

impl MenuCache {
    /// Build from `MENU_CACHE_TTL_SECS` (unset/0 → disabled).
    pub fn from_env() -> Self {
        let ttl = std::env::var("MENU_CACHE_TTL_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        let enabled = ttl > 0;
        if enabled {
            tracing::info!("Menu cache ENABLED (ttl={ttl}s)");
        }
        Self {
            enabled,
            bodies: Cache::builder()
                .time_to_live(Duration::from_secs(ttl.max(1)))
                .max_capacity(10_000)
                .build(),
            versions: RwLock::new(HashMap::new()),
        }
    }

    fn version(&self, org: Uuid) -> u64 {
        *self.versions.read().unwrap().get(&org).unwrap_or(&0)
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
