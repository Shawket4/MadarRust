//! Tests for the Menu Studio API. These seed the NEW unified tables directly via
//! SQL (the backfill does not run here) and exercise each endpoint end to end.

#![allow(clippy::too_many_arguments)]

use actix_web::{App, test, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::menu::routes;
use crate::menu::studio::*;
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
        .bind(format!("studio-org-{org_id}"))
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

async fn attach_group(
    pool: &PgPool,
    item: Uuid,
    group: Uuid,
    sort: i32,
    included: Option<Vec<Uuid>>,
) {
    sqlx::query(
        "INSERT INTO menu_item_modifier_groups (menu_item_id, group_id, sort, included_option_ids) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(item)
    .bind(group)
    .bind(sort)
    .bind(included.as_deref())
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

// ── Test 1: GET /studio full aggregate ───────────────────────────────

#[sqlx::test]
async fn test_get_studio_aggregate(pool: PgPool) {
    let app = app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let user = seed_user(&pool, org).await;
    grant(&pool, "menu_items", "read").await;
    let cat = seed_category(&pool, org).await;
    let item = seed_item(&pool, org, cat, "Latte", 5000).await;
    let token = org_admin_token(user, org);

    // Two sizes; milk (cost 10 piastres/l) costed, one uncosted ingredient too.
    let small = seed_size(&pool, item, "small", 4500, 0).await;
    let _large = seed_size(&pool, item, "large", 6000, 1).await;
    let milk = seed_ingredient(&pool, org, "Milk", "l", Some(10.0)).await;
    let mystery = seed_ingredient(&pool, org, "Mystery", "g", None).await;
    // small: 0.200 l milk (cost 2) + 5 g mystery (uncosted → partial).
    seed_recipe_line(&pool, "item_size", small, milk, "0.200", "l").await;
    seed_recipe_line(&pool, "item_size", small, mystery, "5", "g").await;

    // A reusable typed group (milk_type) with two options; one excluded via allowlist.
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
    let oat = seed_option(&pool, grp, "Oat Milk", 1000, "addon").await;
    let soy = seed_option(&pool, grp, "Soy Milk", 1200, "addon").await;
    // Oat is a swap: 0-qty swap marker recipe line (still known cost = 0, not incomplete).
    seed_recipe_line(&pool, "modifier_option", oat, milk, "0", "l").await;
    attach_group(&pool, item, grp, 0, Some(vec![oat])).await; // only oat included

    // The item-private Options group (legacy_addon_type NULL) with one priced optional.
    let opts_grp = seed_group(&pool, org, "Options", None, "multi", 0, None, false).await;
    let shot = seed_option(&pool, opts_grp, "Extra shot", 500, "optional").await;
    seed_recipe_line(&pool, "modifier_option", shot, milk, "0.050", "l").await; // cost 1 (0.05*10=0.5→1)
    attach_group(&pool, item, opts_grp, 0, None).await;

    // A per-size branch override for availability.
    let branch = seed_branch(&pool, org).await;
    sqlx::query(
        "INSERT INTO menu_price_overrides (scope, branch_id, target_type, target_id, is_available) \
         VALUES ('branch', $1, 'menu_item_size', $2, false)",
    )
    .bind(branch)
    .bind(small)
    .execute(&pool)
    .await
    .unwrap();

    // This item used in a bundle.
    let bundle = Uuid::new_v4();
    sqlx::query("INSERT INTO bundles (id, org_id, name, price) VALUES ($1, $2, 'Combo A', 9000)")
        .bind(bundle)
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO bundle_components (bundle_id, item_id, quantity, position) VALUES ($1, $2, 1, 0)",
    )
    .bind(bundle)
    .bind(item)
    .execute(&pool)
    .await
    .unwrap();

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/menu-items/{item}/studio"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    assert!(status.is_success(), "status {status}: {body:?}");
    let agg: StudioAggregate = serde_json::from_slice(&body).unwrap();

    // Basics.
    assert_eq!(agg.id, item);
    assert_eq!(agg.name, "Latte");

    // Sizes: small has a partial cost (milk 2 known, mystery unknown → incomplete=true).
    assert_eq!(agg.sizes.len(), 2);
    let small_out = agg.sizes.iter().find(|s| s.label == "small").unwrap();
    assert_eq!(small_out.recipe.len(), 2);
    assert_eq!(
        small_out.cost_piastres,
        Some(2),
        "0.200 l * 10 piastres = 2 (mystery excluded from sum)"
    );
    assert!(
        small_out.cost_incomplete,
        "an uncosted ingredient makes the rollup incomplete"
    );
    let large_out = agg.sizes.iter().find(|s| s.label == "large").unwrap();
    assert!(
        large_out.recipe.is_empty() && large_out.cost_piastres.is_none(),
        "a size with no recipe_lines has unknown cost (never 0)"
    );

    // Modifier groups: the typed group is present with resolved min/max/required.
    assert_eq!(agg.modifier_groups.len(), 1);
    let mg = &agg.modifier_groups[0];
    assert_eq!(mg.legacy_addon_type.as_deref(), Some("milk_type"));
    assert_eq!(mg.min, 1);
    assert_eq!(mg.max, Some(1));
    assert!(mg.is_required);
    assert_eq!(mg.options.len(), 2, "group offers both options");
    let oat_out = mg.options.iter().find(|o| o.id == oat).unwrap();
    assert!(oat_out.included, "oat is allowlisted");
    assert_eq!(
        oat_out.cost_piastres,
        Some(0),
        "swap marker (qty 0) costs 0"
    );
    assert!(
        !oat_out.cost_incomplete,
        "a qty-0 swap is known, not incomplete"
    );
    let soy_out = mg.options.iter().find(|o| o.id == soy).unwrap();
    assert!(!soy_out.included, "soy is not allowlisted on this item");

    // Options (item-private priced optionals).
    assert_eq!(agg.options.len(), 1);
    assert_eq!(agg.options[0].name, "Extra shot");
    assert_eq!(
        agg.options[0].cost_piastres,
        Some(1),
        "0.050 l * 10 = 0.5 → 1"
    );

    // Availability: branch override on `small` surfaced.
    assert!(agg.availability.org_active);
    let br = agg
        .availability
        .branches
        .iter()
        .find(|b| b.branch_id == branch)
        .expect("branch override present");
    assert_eq!(br.sizes.len(), 1);
    assert_eq!(br.sizes[0].size_id, small);
    assert_eq!(br.sizes[0].is_available, Some(false));

    // Bundles.
    assert_eq!(agg.used_in_bundles.len(), 1);
    assert_eq!(agg.used_in_bundles[0].name, "Combo A");
}

// ── Test 2: PUT /sizes replace-set round-trip + soft-deactivate ──────

#[sqlx::test]
async fn test_put_sizes_roundtrip_and_soft_deactivate(pool: PgPool) {
    let app = app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let user = seed_user(&pool, org).await;
    grant(&pool, "menu_items", "read").await;
    grant(&pool, "menu_items", "update").await;
    let cat = seed_category(&pool, org).await;
    let item = seed_item(&pool, org, cat, "Latte", 5000).await;
    let token = org_admin_token(user, org);

    // Pre-existing sizes: `small` (has order history) and `old` (no history).
    seed_size(&pool, item, "small", 4000, 0).await;
    seed_size(&pool, item, "old", 3000, 1).await;

    // Give `small` order history so it must be soft-deactivated, not deleted.
    // Build the minimal valid order chain (branch → teller → shift → order → line),
    // mirroring the proven pattern in costing/tests.rs.
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org).await;
    let shift: Uuid = sqlx::query_scalar(
        "INSERT INTO shifts (branch_id, teller_id) VALUES ($1, $2) RETURNING id",
    )
    .bind(branch)
    .bind(teller)
    .fetch_one(&pool)
    .await
    .unwrap();
    let cust_order: Uuid = sqlx::query_scalar(
        "INSERT INTO orders (branch_id, shift_id, teller_id, order_number, status, \
                             payment_method, subtotal, total_amount, order_ref) \
         VALUES ($1, $2, $3, 1, 'completed', 'cash', 4000, 4000, gen_random_uuid()::text) \
         RETURNING id",
    )
    .bind(branch)
    .bind(shift)
    .bind(teller)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO order_items (order_id, menu_item_id, item_name, size_label, quantity, unit_price, line_total) \
         VALUES ($1, $2, 'Latte', 'small', 1, 4000, 4000)",
    )
    .bind(cust_order)
    .bind(item)
    .execute(&pool)
    .await
    .unwrap();

    let rev_before = catalog_revision(&pool, org).await;

    // Replace with `small` (kept/updated) + `large` (new). `old` is dropped.
    let body = PutSizesRequest {
        sizes: vec![
            SizeInput {
                label: "small".into(),
                price: 4500,
                sort: 0,
                is_active: true,
            },
            SizeInput {
                label: "large".into(),
                price: 7000,
                sort: 1,
                is_active: true,
            },
        ],
    };
    let resp = test::call_service(
        &app,
        test::TestRequest::put()
            .uri(&format!("/menu-items/{item}/sizes"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(&body)
            .to_request(),
    )
    .await;
    let status = resp.status();
    let raw = test::read_body(resp).await;
    assert!(status.is_success(), "status {status}: {raw:?}");
    let agg: StudioAggregate = serde_json::from_slice(&raw).unwrap();

    // small updated, large added, both active.
    let labels: std::collections::HashMap<_, _> =
        agg.sizes.iter().map(|s| (s.label.clone(), s)).collect();
    assert_eq!(labels["small"].price, 4500);
    assert!(labels["small"].is_active);
    assert_eq!(labels["large"].price, 7000);

    // `old` had no history → hard-deleted (not present at all).
    assert!(
        !labels.contains_key("old"),
        "history-less dropped size is deleted"
    );

    // Confirm at the DB level: `small` still exists (soft rules apply if dropped later),
    // `old` is gone.
    let remaining: Vec<String> = sqlx::query_scalar(
        "SELECT label FROM menu_item_sizes WHERE menu_item_id = $1 ORDER BY label",
    )
    .bind(item)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(remaining, vec!["large".to_string(), "small".to_string()]);

    // Now drop `small` (which has history) → must be soft-deactivated, kept in the row set.
    let body = PutSizesRequest {
        sizes: vec![SizeInput {
            label: "large".into(),
            price: 7000,
            sort: 0,
            is_active: true,
        }],
    };
    let resp = test::call_service(
        &app,
        test::TestRequest::put()
            .uri(&format!("/menu-items/{item}/sizes"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(&body)
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success());
    let small_active: Option<bool> = sqlx::query_scalar(
        "SELECT is_active FROM menu_item_sizes WHERE menu_item_id = $1 AND label = 'small'",
    )
    .bind(item)
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert_eq!(
        small_active,
        Some(false),
        "a size with order history is soft-deactivated, not deleted"
    );

    // catalog_revision bumped (at least twice — two PUTs).
    let rev_after = catalog_revision(&pool, org).await;
    assert!(
        rev_after > rev_before,
        "catalog_revision must bump on size writes"
    );
}

// ── Test 3: PUT /menu-item-sizes/{size_id}/recipe round-trip + cost ──

#[sqlx::test]
async fn test_put_size_recipe_recomputes_cost_and_bumps_revision(pool: PgPool) {
    let app = app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let user = seed_user(&pool, org).await;
    grant(&pool, "menu_items", "update").await;
    let cat = seed_category(&pool, org).await;
    let item = seed_item(&pool, org, cat, "Latte", 5000).await;
    let size = seed_size(&pool, item, "one_size", 5000, 0).await;
    // Milk base unit is `l`, cost 10 piastres/l.
    let milk = seed_ingredient(&pool, org, "Milk", "l", Some(10.0)).await;
    let token = org_admin_token(user, org);

    let rev_before = catalog_revision(&pool, org).await;

    // Submit 200 ml (in ml → normalizes to 0.200 l). Cost = 0.200 * 10 = 2.
    let body = PutRecipeRequest {
        lines: vec![RecipeLineInput {
            ingredient_id: milk,
            quantity: 200.0,
            unit: "ml".into(),
        }],
    };
    let resp = test::call_service(
        &app,
        test::TestRequest::put()
            .uri(&format!("/menu-item-sizes/{size}/recipe"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(&body)
            .to_request(),
    )
    .await;
    let status = resp.status();
    let raw = test::read_body(resp).await;
    assert!(status.is_success(), "status {status}: {raw:?}");
    let result: RecipeCostResult = serde_json::from_slice(&raw).unwrap();

    assert_eq!(result.size_id, size);
    assert_eq!(result.recipe.len(), 1);
    assert_eq!(
        result.recipe[0].unit, "l",
        "normalized to the ingredient base unit"
    );
    assert_eq!(result.cost_piastres, Some(2), "0.200 l * 10 piastres/l = 2");
    assert!(!result.cost_incomplete);
    assert!(
        result.catalog_revision > rev_before,
        "recipe write bumps catalog_revision"
    );

    // Round-trip: the line landed in recipe_lines under owner_type='item_size'.
    let stored: Vec<(String, String)> = sqlx::query_as(
        "SELECT quantity::text, unit FROM recipe_lines \
         WHERE owner_type = 'item_size' AND owner_id = $1",
    )
    .bind(size)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].1, "l");
    assert!(stored[0].0.starts_with("0.2"));

    // Replace with a NEW recipe (different ingredient) → replace-set semantics.
    let sugar = seed_ingredient(&pool, org, "Sugar", "kg", Some(5.0)).await;
    let body = PutRecipeRequest {
        lines: vec![RecipeLineInput {
            ingredient_id: sugar,
            quantity: 10.0,
            unit: "g".into(),
        }],
    };
    let resp = test::call_service(
        &app,
        test::TestRequest::put()
            .uri(&format!("/menu-item-sizes/{size}/recipe"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(&body)
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success());
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM recipe_lines WHERE owner_type = 'item_size' AND owner_id = $1",
    )
    .bind(size)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1, "recipe was replaced, not appended");
}

// ── Test 4: PUT /modifier-groups attach-set ──────────────────────────

#[sqlx::test]
async fn test_put_modifier_groups_replace_set(pool: PgPool) {
    let app = app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let user = seed_user(&pool, org).await;
    grant(&pool, "menu_items", "read").await;
    grant(&pool, "menu_items", "update").await;
    let cat = seed_category(&pool, org).await;
    let item = seed_item(&pool, org, cat, "Latte", 5000).await;
    let token = org_admin_token(user, org);

    let g1 = seed_group(
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
    let g2 = seed_group(
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
    let opt1 = seed_option(&pool, g1, "Oat", 1000, "addon").await;
    let _opt2 = seed_option(&pool, g1, "Soy", 1000, "addon").await;

    // Attach g1 (with allowlist = [opt1]) and g2 with min/max/required overrides.
    let body = PutModifierGroupsRequest {
        groups: vec![
            GroupAttachInput {
                group_id: g1,
                sort: 0,
                min_override: None,
                max_override: None,
                is_required_override: None,
                included_option_ids: Some(vec![opt1]),
            },
            GroupAttachInput {
                group_id: g2,
                sort: 1,
                min_override: Some(1),
                max_override: Some(2),
                is_required_override: Some(true),
                included_option_ids: None,
            },
        ],
    };
    let resp = test::call_service(
        &app,
        test::TestRequest::put()
            .uri(&format!("/menu-items/{item}/modifier-groups"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(&body)
            .to_request(),
    )
    .await;
    let status = resp.status();
    let raw = test::read_body(resp).await;
    assert!(status.is_success(), "status {status}: {raw:?}");
    let agg: StudioAggregate = serde_json::from_slice(&raw).unwrap();
    assert_eq!(agg.modifier_groups.len(), 2);

    // g2's overrides resolved.
    let g2_out = agg
        .modifier_groups
        .iter()
        .find(|g| g.group_id == g2)
        .unwrap();
    assert_eq!(g2_out.min, 1);
    assert_eq!(g2_out.max, Some(2));
    assert!(g2_out.is_required, "is_required_override applied");

    // g1: only opt1 included.
    let g1_out = agg
        .modifier_groups
        .iter()
        .find(|g| g.group_id == g1)
        .unwrap();
    assert_eq!(g1_out.options.iter().filter(|o| o.included).count(), 1);

    // Replace with just g2 → g1 detached (delete-then-insert).
    let body = PutModifierGroupsRequest {
        groups: vec![GroupAttachInput {
            group_id: g2,
            sort: 0,
            min_override: None,
            max_override: None,
            is_required_override: None,
            included_option_ids: None,
        }],
    };
    let resp = test::call_service(
        &app,
        test::TestRequest::put()
            .uri(&format!("/menu-items/{item}/modifier-groups"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(&body)
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success());
    let attached: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM menu_item_modifier_groups WHERE menu_item_id = $1",
    )
    .bind(item)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(attached, 1, "attach-set replaced (g1 removed, g2 kept)");
}

// ── Test 5: POST /duplicate deep copy ────────────────────────────────

#[sqlx::test]
async fn test_duplicate_deep_copy_new_ids(pool: PgPool) {
    let app = app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let user = seed_user(&pool, org).await;
    grant(&pool, "menu_items", "read").await;
    grant(&pool, "menu_items", "update").await;
    let cat = seed_category(&pool, org).await;
    let item = seed_item(&pool, org, cat, "Latte", 5000).await;
    let token = org_admin_token(user, org);

    // Source: one size with a recipe, a typed group attachment, and an Options group
    // with a priced optional that has its own recipe.
    let small = seed_size(&pool, item, "small", 4500, 0).await;
    let milk = seed_ingredient(&pool, org, "Milk", "l", Some(10.0)).await;
    seed_recipe_line(&pool, "item_size", small, milk, "0.200", "l").await;

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
    let oat = seed_option(&pool, grp, "Oat", 1000, "addon").await;
    attach_group(&pool, item, grp, 0, Some(vec![oat])).await;

    let opts_grp = seed_group(&pool, org, "Options", None, "multi", 0, None, false).await;
    let shot = seed_option(&pool, opts_grp, "Extra shot", 500, "optional").await;
    seed_recipe_line(&pool, "modifier_option", shot, milk, "0.050", "l").await;
    attach_group(&pool, item, opts_grp, 0, None).await;

    // A per-size override to be copied.
    let branch = seed_branch(&pool, org).await;
    sqlx::query(
        "INSERT INTO menu_price_overrides (scope, branch_id, target_type, target_id, price) \
         VALUES ('branch', $1, 'menu_item_size', $2, 5000)",
    )
    .bind(branch)
    .bind(small)
    .execute(&pool)
    .await
    .unwrap();

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/menu-items/{item}/duplicate"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    let status = resp.status();
    let raw = test::read_body(resp).await;
    assert_eq!(status, 201, "status {status}: {raw:?}");
    let dup: StudioAggregate = serde_json::from_slice(&raw).unwrap();

    // New item id, copied basics.
    assert_ne!(dup.id, item, "duplicate is a new item");
    assert_eq!(dup.name, "Latte (Copy)");
    assert_eq!(dup.org_id, org);

    // Copied size with its own NEW id + recipe.
    assert_eq!(dup.sizes.len(), 1);
    let dup_small = &dup.sizes[0];
    assert_eq!(dup_small.label, "small");
    assert_ne!(dup_small.id, small, "the copied size has a fresh id");
    assert_eq!(dup_small.recipe.len(), 1);
    assert_eq!(
        dup_small.cost_piastres,
        Some(2),
        "size recipe copied → cost recomputes"
    );

    // Typed group attachment copied (shared group_id, same allowlist).
    assert_eq!(dup.modifier_groups.len(), 1);
    assert_eq!(
        dup.modifier_groups[0].group_id, grp,
        "reusable group is shared"
    );
    assert_eq!(
        dup.modifier_groups[0]
            .options
            .iter()
            .filter(|o| o.included)
            .count(),
        1
    );

    // Options group copied with a NEW option id (a duplicate gets fresh option uuids).
    assert_eq!(dup.options.len(), 1);
    let dup_shot = &dup.options[0];
    assert_eq!(dup_shot.name, "Extra shot");
    assert_ne!(
        dup_shot.id, shot,
        "duplicated option gets a fresh (non-stable) id"
    );
    assert_eq!(
        dup_shot.cost_piastres,
        Some(1),
        "option recipe copied → cost recomputes"
    );

    // Override copied onto the new size (re-pointed target_id).
    let ov_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM menu_price_overrides \
         WHERE target_type = 'menu_item_size' AND target_id = $1 AND price = 5000",
    )
    .bind(dup_small.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        ov_count, 1,
        "the size override was cloned onto the new size id"
    );

    // The new Options group is a DISTINCT group row (not shared with the source).
    let dup_opts_group: Option<Uuid> = sqlx::query_scalar(
        "SELECT mg.id FROM menu_item_modifier_groups mimg \
         JOIN modifier_groups mg ON mg.id = mimg.group_id \
         WHERE mimg.menu_item_id = $1 AND mg.legacy_addon_type IS NULL",
    )
    .bind(dup.id)
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert!(dup_opts_group.is_some());
    assert_ne!(
        dup_opts_group.unwrap(),
        opts_grp,
        "the item-private Options group is copied, not shared"
    );
}

// ── Auth guard: cross-org read is forbidden ──────────────────────────

#[sqlx::test]
async fn test_studio_cross_org_forbidden(pool: PgPool) {
    let app = app(pool.clone()).await;
    let org_a = seed_org(&pool).await;
    let cat = seed_category(&pool, org_a).await;
    let item = seed_item(&pool, org_a, cat, "Latte", 5000).await;

    let org_b = seed_org(&pool).await;
    let user_b = seed_user(&pool, org_b).await;
    grant(&pool, "menu_items", "read").await;
    let token_b = org_admin_token(user_b, org_b);

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/menu-items/{item}/studio"))
            .insert_header(("Authorization", format!("Bearer {token_b}")))
            .to_request(),
    )
    .await;
    assert!(
        matches!(resp.status().as_u16(), 403 | 404),
        "cross-org studio read must be denied, got {}",
        resp.status()
    );
}
