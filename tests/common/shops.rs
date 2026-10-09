//! A shop with a branch and a menu, as the guest-facing tests need it
//! (tenant_shell_tests, mcp_tests).

use sqlx::PgPool;
use uuid::Uuid;

pub async fn shop(pool: &PgPool, slug: &str, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, is_active, custom_branding) \
         VALUES ($1, $2, $3, true, true)",
    )
    .bind(id)
    .bind(name)
    .bind(slug)
    .execute(pool)
    .await
    .unwrap();
    id
}

pub async fn branch(pool: &PgPool, org: Uuid, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO branches (id, org_id, name, code, address, phone) \
         VALUES ($1, $2, $3, 'MA', '14 Road 9, Maadi', '+20 100 555 0192')",
    )
    .bind(id)
    .bind(org)
    .bind(name)
    .execute(pool)
    .await
    .unwrap();
    id
}

pub async fn pickup(pool: &PgPool, branch: Uuid) {
    sqlx::query(
        "INSERT INTO branch_delivery_settings (branch_id, pickup_enabled) VALUES ($1, true)",
    )
    .bind(branch)
    .execute(pool)
    .await
    .unwrap();
}

pub async fn menu_item(pool: &PgPool, org: Uuid, category: &str, name: &str, piastres: i32) {
    let cat: Uuid =
        sqlx::query_scalar("INSERT INTO categories (org_id, name) VALUES ($1, $2) RETURNING id")
            .bind(org)
            .bind(category)
            .fetch_one(pool)
            .await
            .unwrap();
    sqlx::query(
        "INSERT INTO menu_items (org_id, category_id, name, base_price, is_active) \
         VALUES ($1, $2, $3, $4, true)",
    )
    .bind(org)
    .bind(cat)
    .bind(name)
    .bind(piastres)
    .execute(pool)
    .await
    .unwrap();
}
