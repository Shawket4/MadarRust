//! The `GET /menu-items` cache never serves a stale body (owner, 2026-10-16):
//! every input of a cached body is either in its key or bumps it. One check per
//! input (src/menu/cache.rs lists them): write it, and the next read is fresh.

use std::time::Duration;

use actix_web::{App, test, web};
use madar_rust::menu::cache::{MenuCache, fingerprint};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::JwtSecret;
use madar_rust::models::UserRole;

struct Seed {
    org: Uuid,
    user: Uuid,
    item: Uuid,
    size: Uuid,
    group: Uuid,
    option: Uuid,
    ingredient: Uuid,
    branch: Uuid,
    preset: String,
}

async fn exec(pool: &PgPool, sql: &str, ids: &[Uuid]) {
    let mut q = sqlx::query(sql);
    for id in ids {
        q = q.bind(*id);
    }
    q.execute(pool).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
}

async fn seed(pool: &PgPool) -> Seed {
    let (org, user, cat, item, size, group, option, ingredient, branch) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let preset = format!("p1_{}", &org.simple().to_string()[..10]);
    exec(pool, "INSERT INTO organizations (id, name, slug) VALUES ($1, 'Cache Org', 'cache-' || $1::text)", &[org]).await;
    exec(pool, "INSERT INTO users (id, org_id, name, email, password_hash, role) VALUES ($1, $2, 'Admin', $1::text || '@t.test', 'x', 'org_admin')", &[user, org]).await;
    exec(pool, "INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'B')", &[branch, org]).await;
    exec(pool, "INSERT INTO categories (id, org_id, name) VALUES ($1, $2, 'Hot')", &[cat, org]).await;
    exec(pool, "INSERT INTO menu_items (id, org_id, category_id, name, base_price) VALUES ($1, $2, $3, 'Latte', 500)", &[item, org, cat]).await;
    exec(pool, "INSERT INTO menu_item_sizes (id, menu_item_id, label, price) VALUES ($1, $2, 'Large', 600)", &[size, item]).await;
    exec(pool, "INSERT INTO modifier_groups (id, org_id, name) VALUES ($1, $2, 'Milk')", &[group, org]).await;
    exec(pool, "INSERT INTO modifier_options (id, group_id, name) VALUES ($1, $2, 'Oat')", &[option, group]).await;
    exec(pool, "INSERT INTO menu_item_modifier_groups (menu_item_id, group_id) VALUES ($1, $2)", &[item, group]).await;
    exec(pool, "INSERT INTO org_ingredients (id, org_id, name, unit) VALUES ($1, $2, 'Milk', 'ml')", &[ingredient, org]).await;
    exec(pool, "INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit) VALUES ('item_size', $1, $2, 200, 'ml')", &[size, ingredient]).await;
    sqlx::query("INSERT INTO recipe_step_presets (slug, name, name_ar, sha256, bytes) VALUES ($1, 'Steam', 'بخار', 'abc', 1)")
        .bind(&preset)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO menu_item_recipe_steps (org_id, menu_item_id, position, kind, preset_slug) VALUES ($3, $1, 1, 'preset', $2)")
        .bind(item)
        .bind(&preset)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    exec(pool, "INSERT INTO role_permissions (role, resource, action, granted) VALUES ('org_admin', 'menu_items', 'read', true) ON CONFLICT DO NOTHING", &[]).await;
    Seed {
        org,
        user,
        item,
        size,
        group,
        option,
        ingredient,
        branch,
        preset,
    }
}

/// Every input the fingerprint watches moves it, through a plain SQL write
/// (so ANY write path counts, not only the handlers').
#[sqlx::test]
async fn every_input_write_moves_the_fingerprint(pool: PgPool) {
    let s = seed(&pool).await;
    let job = Uuid::new_v4();
    exec(&pool, "INSERT INTO asset_jobs (id, org_id, purpose, source_kind, staged_path, target_table, target_id, target_field) \
                 VALUES ($1, $2, 'menu_item_photo', 'upload', '/tmp/x', 'menu_items', $3, 'image_url')", &[job, s.org, s.item]).await;
    let writes: Vec<(&str, String, Vec<Uuid>)> = vec![
        ("item name", "UPDATE menu_items SET name = 'Flat white' WHERE id = $1".into(), vec![s.item]),
        ("item translations", "UPDATE menu_items SET name_translations = '{\"ar\":\"x\"}' WHERE id = $1".into(), vec![s.item]),
        ("item image_url", "UPDATE menu_items SET image_url = '/uploads/x.png' WHERE id = $1".into(), vec![s.item]),
        ("category", "UPDATE categories SET name = 'Coffee' WHERE org_id = $1".into(), vec![s.org]),
        ("size price", "UPDATE menu_item_sizes SET price = 650 WHERE id = $1".into(), vec![s.size]),
        ("group", "UPDATE modifier_groups SET name = 'Milks' WHERE id = $1".into(), vec![s.group]),
        ("option", "UPDATE modifier_options SET name = 'Almond' WHERE id = $1".into(), vec![s.option]),
        ("item group link", "UPDATE menu_item_modifier_groups SET included_option_ids = ARRAY[$2]::uuid[] WHERE menu_item_id = $1".into(), vec![s.item, s.option]),
        ("recipe line", "UPDATE recipe_lines SET quantity = 250 WHERE owner_id = $1".into(), vec![s.size]),
        ("ingredient name", "UPDATE org_ingredients SET name = 'Whole milk' WHERE id = $1".into(), vec![s.ingredient]),
        ("ingredient yield_pct", "UPDATE org_ingredients SET yield_pct = 90 WHERE id = $1".into(), vec![s.ingredient]),
        ("branch price override", "INSERT INTO menu_price_overrides (scope, branch_id, target_type, target_id, price) VALUES ('branch', $1, 'menu_item_size', $2, 700)".into(), vec![s.branch, s.size]),
        ("branch availability", "UPDATE menu_price_overrides SET is_available = false WHERE target_id = $1".into(), vec![s.size]),
        ("recipe step", "UPDATE menu_item_recipe_steps SET position = 2 WHERE menu_item_id = $1".into(), vec![s.item]),
        ("asset job running", "UPDATE asset_jobs SET status = 'running' WHERE id = $1".into(), vec![job]),
        ("asset job done", "UPDATE asset_jobs SET status = 'done' WHERE id = $1".into(), vec![job]),
    ];
    let mut before = fingerprint(&pool, s.org).await.unwrap();
    for (what, sql, ids) in writes {
        exec(&pool, &sql, &ids).await;
        let after = fingerprint(&pool, s.org).await.unwrap();
        assert_ne!(before, after, "writing {what} did not change the menu cache key");
        before = after;
    }
    // A step preset (global), as boot's reconcile and the assets backfill write it.
    sqlx::query("UPDATE recipe_step_presets SET name = 'Steam milk', updated_at = now() WHERE slug = $1")
        .bind(&s.preset)
        .execute(&pool)
        .await
        .unwrap();
    assert_ne!(before, fingerprint(&pool, s.org).await.unwrap(), "a step preset write did not change the key");
}

fn token(s: &Seed) -> String {
    madar_rust::auth::jwt::create_token(
        &JwtSecret("secret".into()),
        s.user,
        Some(s.org),
        UserRole::OrgAdmin,
        None,
        24,
    )
    .unwrap()
}

/// `GET /menu-items?org_id=…{query}` as a current POS, as JSON.
macro_rules! get {
    ($app:expr, $s:expr, $query:expr) => {{
        let req = test::TestRequest::get()
            .uri(&format!("/menu-items?org_id={}{}", $s.org, $query))
            .insert_header(("Authorization", format!("Bearer {}", token($s))))
            .insert_header(("X-Madar-Client", "pos/0.14.0 (android)"))
            .to_request();
        let res = test::call_service($app, req).await;
        assert!(res.status().is_success(), "{}", res.status());
        let v: serde_json::Value = test::read_body_json(res).await;
        v
    }};
}

/// With the cache ON (long TTL), the first read caches, and the read right
/// after each write is the fresh body — for the inputs a fresh (test) database
/// reads from the same tables prod does.
#[sqlx::test]
async fn with_the_cache_on_the_read_after_a_write_is_fresh(pool: PgPool) {
    let s = seed(&pool).await;
    let cache = MenuCache::with_ttl(Duration::from_secs(3600));
    cache.assume_prod_shape();
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(JwtSecret("secret".into())))
            .app_data(web::Data::new(cache))
            .configure(madar_rust::menu::routes::configure),
    )
    .await;

    for full in ["", "&full=true"] {
        let first = get!(&app, &s, full);
        assert_eq!(first, get!(&app, &s, full), "an unchanged menu reads the same");
    }

    exec(&pool, "UPDATE menu_items SET name = 'Cortado' WHERE id = $1", &[s.item]).await;
    assert_eq!(get!(&app, &s, "")[0]["name"], "Cortado");
    assert_eq!(get!(&app, &s, "&full=true")[0]["name"], "Cortado");

    exec(&pool, "UPDATE menu_item_sizes SET price = 777 WHERE id = $1", &[s.size]).await;
    let body = get!(&app, &s, "&full=true");
    assert!(body.to_string().contains("777"), "size price not fresh: {body}");

    exec(&pool, "UPDATE menu_items SET is_active = false WHERE id = $1", &[s.item]).await;
    assert_eq!(get!(&app, &s, "")[0]["is_active"], false);

    sqlx::query("UPDATE recipe_step_presets SET name = 'Froth milk', updated_at = now() WHERE slug = $1")
        .bind(&s.preset)
        .execute(&pool)
        .await
        .unwrap();
    let body = get!(&app, &s, "&full=true");
    assert!(body.to_string().contains("Froth milk"), "step preset not fresh: {body}");

    exec(&pool, "DELETE FROM menu_items WHERE id = $1", &[s.item]).await;
    assert_eq!(get!(&app, &s, "").as_array().map(Vec::len), Some(0));
}

/// Every database built from the migrations has prod's shape since DB Phase
/// 2.4 (the legacy catalog relations are views), so the cache is usable. One
/// where a legacy relation is still a writable table would see writes that
/// bump nothing: the cache is OFF there.
#[sqlx::test]
async fn the_cache_is_off_where_legacy_relations_are_tables(pool: PgPool) {
    assert!(MenuCache::with_ttl(Duration::from_secs(600)).usable(&pool).await);
    // A database that still has the pre-2.4 shape (a writable legacy table).
    sqlx::raw_sql(
        "DROP VIEW branch_menu_overrides;
         CREATE TABLE branch_menu_overrides (branch_id uuid, menu_item_id uuid);",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert!(!MenuCache::with_ttl(Duration::from_secs(600)).usable(&pool).await);
}
