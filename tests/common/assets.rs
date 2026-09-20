//! Asset fixtures shared by the `assets` and `assets_backfill` suites.
//!
//! They were `assets`'s own `#[cfg(test)]` helpers, and the backfill tests
//! reached across into them. Two binaries need them now, so they live here.

#![allow(dead_code)]

use sqlx::PgPool;
use uuid::Uuid;

pub async fn schema(pool: &PgPool) {
    // Schema comes from B1's migration (20260914090400_assets.sql).
    let _ = pool;
}

pub async fn seed_org(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org', $2)")
        .bind(id)
        .bind(format!("o-{}", &id.to_string()[..8]))
        .execute(pool)
        .await
        .unwrap();
    id
}

pub async fn seed_branch(pool: &PgPool, org: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'B')")
        .bind(id)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    id
}

pub async fn seed_item(pool: &PgPool, org: Uuid, image_url: Option<&str>) -> Uuid {
    let cat = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(cat)
        .bind(org)
        .bind(format!("C-{cat}"))
        .execute(pool)
        .await
        .unwrap();
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO menu_items (id, org_id, category_id, name, base_price, is_active, image_url) VALUES ($1,$2,$3,'I',100,true,$4)")
        .bind(id)
        .bind(org)
        .bind(cat)
        .bind(image_url)
        .execute(pool)
        .await
        .unwrap();
    id
}
