//! Tests for the reusable-modifier + pricing/availability API. These seed the NEW
//! unified tables directly via SQL (the backfill does not run here) and exercise each
//! endpoint end to end. They mirror the seed helpers + harness of `menu::studio::tests`.

#![allow(clippy::too_many_arguments)]

use actix_web::{App, test, web};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::menu::modifiers::*;
use crate::menu::routes;
use crate::menu::studio::ItemOptionOut;
use crate::models::UserRole;

fn get_secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn org_admin_token(user_id: Uuid, org_id: Uuid) -> String {
    crate::auth::jwt::create_token(
        &get_secret(),
        user_id,
        Some(org_id),
        UserRole::OrgAdmin,
        None,
        24,
    )
    .unwrap()
}

async fn app(
    pool: PgPool,
) -> impl actix_web::dev::Service<
    actix_http::Request,
    Response = actix_web::dev::ServiceResponse,
    Error = actix_web::Error,
> {
    test::init_service(
        App::new()
            .app_data(web::Data::new(pool))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await
}

// ── direct-SQL seed helpers on the NEW tables ────────────────────────

async fn seed_org(pool: &PgPool) -> Uuid {
    let org_id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Test Org', $2)")
        .bind(org_id)
        .bind(format!("mods-org-{org_id}"))
        .execute(pool)
        .await
        .unwrap();
    org_id
}

async fn seed_branch(pool: &PgPool, org_id: Uuid) -> Uuid {
    let branch_id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(branch_id)
        .bind(org_id)
        .bind(format!("Branch {branch_id}"))
        .execute(pool)
        .await
        .unwrap();
    branch_id
}

async fn seed_user(pool: &PgPool, org_id: Uuid) -> Uuid {
    let user_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, 'Test User', $3, 'hash', 'org_admin'::user_role)",
    )
    .bind(user_id)
    .bind(org_id)
    .bind(format!("u-{user_id}@test.com"))
    .execute(pool)
    .await
    .unwrap();
    user_id
}

async fn grant(pool: &PgPool, resource: &str, action: &str) {
    sqlx::query(
        "INSERT INTO role_permissions (role, resource, action, granted) \
         VALUES ('org_admin'::user_role, $1::permission_resource, $2::permission_action, true) \
         ON CONFLICT DO NOTHING",
    )
    .bind(resource)
    .bind(action)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_category(pool: &PgPool, org_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1, $2, 'Coffee')")
        .bind(id)
        .bind(org_id)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn seed_item(pool: &PgPool, org_id: Uuid, cat: Uuid, name: &str, price: i32) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO menu_items (id, org_id, category_id, name, base_price) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(org_id)
    .bind(cat)
    .bind(name)
    .bind(price)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// Seed an org ingredient. `cost` is the per-unit cost in piastres (NULL = uncosted).
async fn seed_ingredient(
    pool: &PgPool,
    org_id: Uuid,
    name: &str,
    unit: &str,
    cost: Option<f64>,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO org_ingredients (id, org_id, name, unit, category, description, cost_per_unit) \
         VALUES ($1, $2, $3, $4::inventory_unit, 'veggies', 'x', $5)",
    )
    .bind(id)
    .bind(org_id)
    .bind(name)
    .bind(unit)
    .bind(cost)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn seed_size(pool: &PgPool, item: Uuid, label: &str, price: i32, sort: i32) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO menu_item_sizes (id, menu_item_id, label, price, sort, is_active) \
         VALUES ($1, $2, $3, $4, $5, true)",
    )
    .bind(id)
    .bind(item)
    .bind(label)
    .bind(price)
    .bind(sort)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn seed_recipe_line(
    pool: &PgPool,
    owner_type: &str,
    owner_id: Uuid,
    ingredient_id: Uuid,
    qty: &str,
    unit: &str,
) {
    sqlx::query(
        "INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit) \
         VALUES ($1, $2, $3, $4::numeric, $5)",
    )
    .bind(owner_type)
    .bind(owner_id)
    .bind(ingredient_id)
    .bind(qty)
    .bind(unit)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_group(
    pool: &PgPool,
    org_id: Uuid,
    name: &str,
    legacy_addon_type: Option<&str>,
    selection_type: &str,
    min: i32,
    max: Option<i32>,
    required: bool,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO modifier_groups \
             (id, org_id, name, selection_type, min_selections, max_selections, is_required, legacy_addon_type) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(id)
    .bind(org_id)
    .bind(name)
    .bind(selection_type)
    .bind(min)
    .bind(max)
    .bind(required)
    .bind(legacy_addon_type)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn seed_option(
    pool: &PgPool,
    group: Uuid,
    name: &str,
    price: i32,
    legacy_source: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO modifier_options (id, group_id, name, price, legacy_source) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(group)
    .bind(name)
    .bind(price)
    .bind(legacy_source)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn attach_group(pool: &PgPool, item: Uuid, group: Uuid, sort: i32) {
    sqlx::query(
        "INSERT INTO menu_item_modifier_groups (menu_item_id, group_id, sort) VALUES ($1, $2, $3)",
    )
    .bind(item)
    .bind(group)
    .bind(sort)
    .execute(pool)
    .await
    .unwrap();
}

async fn catalog_revision(pool: &PgPool, org_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT revision FROM catalog_revision WHERE org_id = $1")
        .bind(org_id)
        .fetch_optional(pool)
        .await
        .unwrap()
        .unwrap_or(0)
}

/// Build a minimal completed order line referencing `option_id` as an addon (so the
/// stable-id order-history soft-delete path can be exercised). Mirrors the order chain
/// in the studio tests.
///
/// Pre-FLIP, `order_item_addons.addon_item_id` still has a hard FK to the legacy
/// `addon_items` table (dropped only by the Wave-2 shim). The stable-id invariant
/// (CONTRACT §4) is exactly that `modifier_options.id == addon_items.id`, so we seed a
/// matching `addon_items` row with the SAME id to satisfy the legacy FK — modelling the
/// post-backfill reality the soft-delete check relies on.
async fn seed_order_with_addon(pool: &PgPool, org: Uuid, item: Uuid, option_id: Uuid) {
    sqlx::query(
        "INSERT INTO addon_items (id, org_id, name, type, default_price) \
         VALUES ($1, $2, 'Oat', 'milk_type', 1000) ON CONFLICT (id) DO NOTHING",
    )
    .bind(option_id)
    .bind(org)
    .execute(pool)
    .await
    .unwrap();

    let branch = seed_branch(pool, org).await;
    let teller = seed_user(pool, org).await;
    let shift: Uuid = sqlx::query_scalar(
        "INSERT INTO shifts (branch_id, teller_id) VALUES ($1, $2) RETURNING id",
    )
    .bind(branch)
    .bind(teller)
    .fetch_one(pool)
    .await
    .unwrap();
    let order: Uuid = sqlx::query_scalar(
        "INSERT INTO orders (branch_id, shift_id, teller_id, order_number, status, \
                             payment_method, subtotal, total_amount, order_ref) \
         VALUES ($1, $2, $3, 1, 'completed', 'cash', 4000, 4000, gen_random_uuid()::text) \
         RETURNING id",
    )
    .bind(branch)
    .bind(shift)
    .bind(teller)
    .fetch_one(pool)
    .await
    .unwrap();
    let line: Uuid = sqlx::query_scalar(
        "INSERT INTO order_items (order_id, menu_item_id, item_name, size_label, quantity, unit_price, line_total) \
         VALUES ($1, $2, 'Latte', 'small', 1, 4000, 4000) RETURNING id",
    )
    .bind(order)
    .bind(item)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO order_item_addons (order_item_id, addon_item_id, addon_name, unit_price, quantity, line_total) \
         VALUES ($1, $2, 'Oat', 1000, 1, 1000)",
    )
    .bind(line)
    .bind(option_id)
    .execute(pool)
    .await
    .unwrap();
}

// ════════════════════════════════════════════════════════════════════
// Test 1: create group → option → recipe round-trip
// ════════════════════════════════════════════════════════════════════

#[sqlx::test]
async fn test_group_option_recipe_roundtrip(pool: PgPool) {
    let app = app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let user = seed_user(&pool, org).await;
    grant(&pool, "menu_items", "read").await;
    grant(&pool, "menu_items", "update").await;
    let milk = seed_ingredient(&pool, org, "Milk", "l", Some(10.0)).await;
    let token = org_admin_token(user, org);

    let rev_before = catalog_revision(&pool, org).await;

    // 1. Create a reusable group.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/modifier-groups")
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(json!({
                "name": "milk_type",
                "selection_type": "single",
                "min_selections": 1,
                "max_selections": 1,
                "is_required": true,
                "sort": 2
            }))
            .to_request(),
    )
    .await;
    let status = resp.status();
    let raw = test::read_body(resp).await;
    assert_eq!(status, 201, "create group status {status}: {raw:?}");
    let group: GroupOut = serde_json::from_slice(&raw).unwrap();
    assert_eq!(group.name, "milk_type");
    assert_eq!(group.selection_type, "single");
    assert_eq!(group.min_selections, 1);
    assert_eq!(group.max_selections, Some(1));
    assert!(group.is_required);
    assert_eq!(group.sort, 2);
    assert_eq!(group.org_id, org);
    assert!(
        group.legacy_addon_type.is_none(),
        "new groups have legacy_addon_type NULL"
    );
    assert!(group.options.is_empty());

    // 2. Create an option in the group.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/modifier-groups/{}/options", group.id))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(json!({ "name": "Oat Milk", "price": 1000, "replaces_ingredient_id": milk }))
            .to_request(),
    )
    .await;
    let status = resp.status();
    let raw = test::read_body(resp).await;
    assert_eq!(status, 201, "create option status {status}: {raw:?}");
    let opt: GroupOptionOut = serde_json::from_slice(&raw).unwrap();
    assert_eq!(opt.name, "Oat Milk");
    assert_eq!(opt.price, 1000);
    assert_eq!(opt.replaces_ingredient_id, Some(milk));

    // legacy_source='addon' persisted for the new option.
    let ls: String = sqlx::query_scalar("SELECT legacy_source FROM modifier_options WHERE id = $1")
        .bind(opt.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ls, "addon");

    // 3. Replace the option's recipe: 200 ml milk (normalizes to 0.200 l) + a 0-qty swap.
    let coffee = seed_ingredient(&pool, org, "Coffee", "kg", Some(50.0)).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::put()
            .uri(&format!("/modifier-options/{}/recipe", opt.id))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(json!([
                { "ingredient_id": milk, "quantity": 200.0, "unit": "ml" },
                { "ingredient_id": coffee, "quantity": 0.0, "unit": "kg" }
            ]))
            .to_request(),
    )
    .await;
    let status = resp.status();
    let raw = test::read_body(resp).await;
    assert!(status.is_success(), "put recipe status {status}: {raw:?}");
    let stored: Vec<OptionRecipeLineInput> = serde_json::from_slice(&raw).unwrap();
    assert_eq!(stored.len(), 2);

    // Round-trip at the DB level: normalized to base units, qty 0 preserved as a swap.
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT quantity::text, unit FROM recipe_lines \
         WHERE owner_type = 'modifier_option' AND owner_id = $1 ORDER BY unit",
    )
    .bind(opt.id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    // kg line is the 0-qty swap marker; l line is 0.200.
    let kg = rows.iter().find(|r| r.1 == "kg").unwrap();
    assert!(kg.0.starts_with('0'), "swap marker qty is 0, got {}", kg.0);
    let l = rows.iter().find(|r| r.1 == "l").unwrap();
    assert!(l.0.starts_with("0.2"), "200 ml → 0.200 l, got {}", l.0);

    // 4. GET the list and confirm the group + option surface with the recipe attached.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/modifier-groups?org_id={org}"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success());
    let raw = test::read_body(resp).await;
    let groups: Vec<GroupOut> = serde_json::from_slice(&raw).unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].options.len(), 1);
    assert_eq!(groups[0].options[0].id, opt.id);

    // Every write bumped catalog_revision.
    let rev_after = catalog_revision(&pool, org).await;
    assert!(
        rev_after >= rev_before + 3,
        "3 writes (group, option, recipe) each bump revision: {rev_before} -> {rev_after}"
    );
}

// ════════════════════════════════════════════════════════════════════
// Test 2: PUT /menu-items/{id}/options — create + update + deactivate
// ════════════════════════════════════════════════════════════════════

#[sqlx::test]
async fn test_put_item_options_create_update_deactivate(pool: PgPool) {
    let app = app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let user = seed_user(&pool, org).await;
    grant(&pool, "menu_items", "read").await;
    grant(&pool, "menu_items", "update").await;
    let cat = seed_category(&pool, org).await;
    let item = seed_item(&pool, org, cat, "Latte", 5000).await;
    let milk = seed_ingredient(&pool, org, "Milk", "l", Some(10.0)).await;
    let token = org_admin_token(user, org);

    // First PUT: create two options from scratch (server creates the Options group).
    let resp = test::call_service(
        &app,
        test::TestRequest::put()
            .uri(&format!("/menu-items/{item}/options"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(json!({ "options": [
                { "name": "Extra shot", "price": 500,
                  "recipe": [ { "ingredient_id": milk, "quantity": 50.0, "unit": "ml" } ] },
                { "name": "Whipped cream", "price": 300, "recipe": null }
            ]}))
            .to_request(),
    )
    .await;
    let status = resp.status();
    let raw = test::read_body(resp).await;
    assert!(status.is_success(), "first PUT status {status}: {raw:?}");
    let opts: Vec<ItemOptionOut> = serde_json::from_slice(&raw).unwrap();
    assert_eq!(opts.len(), 2);
    let shot = opts.iter().find(|o| o.name == "Extra shot").unwrap();
    assert_eq!(
        shot.cost_piastres,
        Some(1),
        "0.050 l * 10 = 0.5 → 1 piastre"
    );
    let shot_id = shot.id;
    let cream_id = opts.iter().find(|o| o.name == "Whipped cream").unwrap().id;

    // A single Options group (legacy_addon_type NULL) was created + attached.
    let grp_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM menu_item_modifier_groups mimg \
         JOIN modifier_groups mg ON mg.id = mimg.group_id \
         WHERE mimg.menu_item_id = $1 AND mg.legacy_addon_type IS NULL",
    )
    .bind(item)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(grp_count, 1);

    // Second PUT: keep+update `Extra shot` (by id, new price + recipe), DROP `Whipped
    // cream` (has no order history → hard-deleted), ADD a new `Caramel`.
    let resp = test::call_service(
        &app,
        test::TestRequest::put()
            .uri(&format!("/menu-items/{item}/options"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(json!({ "options": [
                { "id": shot_id, "name": "Extra shot", "price": 700,
                  "recipe": [ { "ingredient_id": milk, "quantity": 100.0, "unit": "ml" } ] },
                { "name": "Caramel", "price": 400, "recipe": null }
            ]}))
            .to_request(),
    )
    .await;
    let status = resp.status();
    let raw = test::read_body(resp).await;
    assert!(status.is_success(), "second PUT status {status}: {raw:?}");
    let opts: Vec<ItemOptionOut> = serde_json::from_slice(&raw).unwrap();
    let names: std::collections::HashSet<_> = opts.iter().map(|o| o.name.as_str()).collect();
    assert!(names.contains("Extra shot") && names.contains("Caramel"));
    assert!(
        !names.contains("Whipped cream"),
        "dropped, history-less option is removed"
    );

    // `Extra shot` updated in place (SAME id) with new price + recomputed cost.
    let shot = opts.iter().find(|o| o.name == "Extra shot").unwrap();
    assert_eq!(shot.id, shot_id, "existing option updated, not recreated");
    assert_eq!(shot.price, 700);
    assert_eq!(shot.cost_piastres, Some(1), "0.100 l * 10 = 1");

    // `Whipped cream` truly gone from the table (no order history).
    let cream_exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM modifier_options WHERE id = $1)")
            .bind(cream_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!cream_exists, "history-less dropped option is hard-deleted");
}

// ════════════════════════════════════════════════════════════════════
// Test 3: menu-price-overrides upsert precedence shape + delete
// ════════════════════════════════════════════════════════════════════

#[sqlx::test]
async fn test_price_overrides_upsert_shape_and_delete(pool: PgPool) {
    let app = app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let user = seed_user(&pool, org).await;
    grant(&pool, "menu_items", "update").await;
    let cat = seed_category(&pool, org).await;
    let item = seed_item(&pool, org, cat, "Latte", 5000).await;
    let size = seed_size(&pool, item, "small", 4500, 0).await;
    let branch = seed_branch(&pool, org).await;
    let token = org_admin_token(user, org);

    // A branch override sets a price on the size.
    let resp = test::call_service(
        &app,
        test::TestRequest::put()
            .uri("/menu-price-overrides")
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(json!({
                "scope": "branch", "branch_id": branch,
                "target_type": "menu_item_size", "target_id": size, "price": 5200
            }))
            .to_request(),
    )
    .await;
    let status = resp.status();
    let raw = test::read_body(resp).await;
    assert!(
        status.is_success(),
        "branch upsert status {status}: {raw:?}"
    );
    let out: PriceOverrideOut = serde_json::from_slice(&raw).unwrap();
    assert_eq!(out.price, Some(5200));

    // Upsert the SAME (scope, branch, target) → updates in place (one row, new value).
    let resp = test::call_service(
        &app,
        test::TestRequest::put()
            .uri("/menu-price-overrides")
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(json!({
                "scope": "branch", "branch_id": branch,
                "target_type": "menu_item_size", "target_id": size,
                "price": 5300, "is_available": false
            }))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success());
    let branch_rows: Vec<(Option<i32>, Option<bool>)> = sqlx::query_as(
        "SELECT price, is_available FROM menu_price_overrides \
         WHERE scope = 'branch' AND branch_id = $1 AND target_type = 'menu_item_size' AND target_id = $2",
    )
    .bind(branch)
    .bind(size)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(branch_rows.len(), 1, "upsert replaced, not appended");
    assert_eq!(branch_rows[0], (Some(5300), Some(false)));

    // A branch_channel override is a DISTINCT row (different scope/key) — precedence
    // is layered, so both the branch and the branch_channel rows coexist.
    let resp = test::call_service(
        &app,
        test::TestRequest::put()
            .uri("/menu-price-overrides")
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(json!({
                "scope": "branch_channel", "branch_id": branch, "channel": "outside",
                "target_type": "menu_item_size", "target_id": size, "price": 6000
            }))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success());
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM menu_price_overrides WHERE target_type = 'menu_item_size' AND target_id = $1",
    )
    .bind(size)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        total, 2,
        "branch + branch_channel are distinct override rows"
    );

    // Bad shape: scope 'branch' with a channel is rejected (mirrors the table CHECK).
    let resp = test::call_service(
        &app,
        test::TestRequest::put()
            .uri("/menu-price-overrides")
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(json!({
                "scope": "branch", "branch_id": branch, "channel": "outside",
                "target_type": "menu_item_size", "target_id": size, "price": 1
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 400, "scope/branch+channel shape must 400");

    // Empty override (no price, no availability) is rejected.
    let resp = test::call_service(
        &app,
        test::TestRequest::put()
            .uri("/menu-price-overrides")
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(json!({
                "scope": "branch", "branch_id": branch,
                "target_type": "menu_item_size", "target_id": size
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 400, "an override with nothing set must 400");

    // DELETE the branch_channel row only; the branch row survives.
    let resp = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri("/menu-price-overrides")
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(json!({
                "scope": "branch_channel", "branch_id": branch, "channel": "outside",
                "target_type": "menu_item_size", "target_id": size
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 204, "delete returns 204");
    let remaining: Vec<String> = sqlx::query_scalar(
        "SELECT scope FROM menu_price_overrides WHERE target_type = 'menu_item_size' AND target_id = $1",
    )
    .bind(size)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        remaining,
        vec!["branch".to_string()],
        "only the branch_channel override was deleted"
    );
}

// ════════════════════════════════════════════════════════════════════
// Test 4: DELETE option soft-deactivates when order-referenced
// ════════════════════════════════════════════════════════════════════

#[sqlx::test]
async fn test_delete_option_soft_when_order_referenced(pool: PgPool) {
    let app = app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let user = seed_user(&pool, org).await;
    grant(&pool, "menu_items", "update").await;
    let cat = seed_category(&pool, org).await;
    let item = seed_item(&pool, org, cat, "Latte", 5000).await;
    let token = org_admin_token(user, org);

    let grp = seed_group(
        &pool,
        org,
        "milk_type",
        Some("milk_type"),
        "single",
        1,
        Some(1),
        true,
    )
    .await;
    let referenced_opt = seed_option(&pool, grp, "Oat", 1000, "addon").await;
    let free_opt = seed_option(&pool, grp, "Soy", 1200, "addon").await;

    // Give `referenced_opt` immutable order history (order_item_addons.addon_item_id).
    seed_order_with_addon(&pool, org, item, referenced_opt).await;

    // DELETE the referenced option → soft-deactivate (row stays, is_active=false).
    let resp = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri(&format!("/modifier-options/{referenced_opt}"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 204);
    let active: Option<bool> =
        sqlx::query_scalar("SELECT is_active FROM modifier_options WHERE id = $1")
            .bind(referenced_opt)
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert_eq!(
        active,
        Some(false),
        "an order-referenced option is soft-deactivated, not deleted"
    );

    // DELETE the un-referenced option → hard delete (row gone).
    let resp = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri(&format!("/modifier-options/{free_opt}"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 204);
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM modifier_options WHERE id = $1)")
            .bind(free_opt)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!exists, "an un-referenced option is hard-deleted");
}

// ════════════════════════════════════════════════════════════════════
// Test 5: DELETE group soft when attached / hard when free
// ════════════════════════════════════════════════════════════════════

#[sqlx::test]
async fn test_delete_group_soft_when_attached(pool: PgPool) {
    let app = app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let user = seed_user(&pool, org).await;
    grant(&pool, "menu_items", "update").await;
    let cat = seed_category(&pool, org).await;
    let item = seed_item(&pool, org, cat, "Latte", 5000).await;
    let token = org_admin_token(user, org);

    // Attached group → soft-deactivate on delete.
    let attached = seed_group(
        &pool,
        org,
        "milk_type",
        Some("milk_type"),
        "single",
        1,
        Some(1),
        true,
    )
    .await;
    attach_group(&pool, item, attached, 0).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri(&format!("/modifier-groups/{attached}"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 204);
    let active: Option<bool> =
        sqlx::query_scalar("SELECT is_active FROM modifier_groups WHERE id = $1")
            .bind(attached)
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert_eq!(active, Some(false), "attached group is soft-deactivated");

    // Free group (no attachment, no order-referenced options) → hard delete.
    let free = seed_group(
        &pool,
        org,
        "syrup",
        Some("syrup"),
        "multi",
        0,
        Some(3),
        false,
    )
    .await;
    let _o = seed_option(&pool, free, "Vanilla", 300, "addon").await;
    let resp = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri(&format!("/modifier-groups/{free}"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 204);
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM modifier_groups WHERE id = $1)")
            .bind(free)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!exists, "a free group is hard-deleted");
}

// ════════════════════════════════════════════════════════════════════
// Test 6: GET /menu-items/{id}/cost — per-size rollup from the new tables
// ════════════════════════════════════════════════════════════════════

#[sqlx::test]
async fn test_get_item_cost_per_size(pool: PgPool) {
    let app = app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let user = seed_user(&pool, org).await;
    grant(&pool, "menu_items", "read").await;
    let cat = seed_category(&pool, org).await;
    let item = seed_item(&pool, org, cat, "Latte", 5000).await;
    let token = org_admin_token(user, org);

    let small = seed_size(&pool, item, "small", 4500, 0).await;
    let large = seed_size(&pool, item, "large", 6000, 1).await;
    let milk = seed_ingredient(&pool, org, "Milk", "l", Some(10.0)).await;
    let mystery = seed_ingredient(&pool, org, "Mystery", "g", None).await;

    // small: 0.200 l milk (cost 2) + 5 g mystery (uncosted → incomplete).
    seed_recipe_line(&pool, "item_size", small, milk, "0.200", "l").await;
    seed_recipe_line(&pool, "item_size", small, mystery, "5", "g").await;
    // large: no recipe → unknown cost (NULL, never 0).

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/menu-items/{item}/cost"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    let status = resp.status();
    let raw = test::read_body(resp).await;
    assert!(status.is_success(), "cost status {status}: {raw:?}");
    let costs: Vec<SizeCostOut> = serde_json::from_slice(&raw).unwrap();
    assert_eq!(costs.len(), 2);

    let small_c = costs.iter().find(|c| c.label == "small").unwrap();
    assert_eq!(
        small_c.cost_piastres,
        Some(2),
        "0.200 l * 10 = 2 (mystery excluded from sum)"
    );
    assert!(small_c.cost_incomplete, "uncosted ingredient → incomplete");

    let large_c = costs.iter().find(|c| c.label == "large").unwrap();
    assert!(
        large_c.cost_piastres.is_none(),
        "a recipe-less size has unknown cost (never 0)"
    );
    assert!(!large_c.cost_incomplete);
    let _ = large;
}

// ════════════════════════════════════════════════════════════════════
// Test 7: cross-org write is forbidden
// ════════════════════════════════════════════════════════════════════

#[sqlx::test]
async fn test_modifier_group_cross_org_forbidden(pool: PgPool) {
    let app = app(pool.clone()).await;
    let org_a = seed_org(&pool).await;
    let grp = seed_group(
        &pool,
        org_a,
        "milk_type",
        Some("milk_type"),
        "single",
        1,
        Some(1),
        true,
    )
    .await;

    let org_b = seed_org(&pool).await;
    let user_b = seed_user(&pool, org_b).await;
    grant(&pool, "menu_items", "update").await;
    let token_b = org_admin_token(user_b, org_b);

    // org B cannot PATCH org A's group.
    let resp = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/modifier-groups/{grp}"))
            .insert_header(("Authorization", format!("Bearer {token_b}")))
            .set_json(json!({ "name": "hijacked" }))
            .to_request(),
    )
    .await;
    assert!(
        matches!(resp.status().as_u16(), 403 | 404),
        "cross-org group patch must be denied, got {}",
        resp.status()
    );

    // org B cannot list org A's groups.
    grant(&pool, "menu_items", "read").await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/modifier-groups?org_id={org_a}"))
            .insert_header(("Authorization", format!("Bearer {token_b}")))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 403, "cross-org group list must be forbidden");
}
