//! Tests for the POS catalog-sync endpoint. These seed the NEW unified tables
//! directly via SQL (the backfill does not run here) and assert the resolved
//! catalog snapshot per CONTRACT §3 / §5.2. Seed helpers mirror
//! `menu::studio::tests` to stay consistent.

#![allow(clippy::too_many_arguments)]

use actix_web::{App, test, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::menu::catalog_sync::*;
use crate::menu::routes;
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
        .bind(format!("sync-org-{org_id}"))
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

async fn seed_ingredient(pool: &PgPool, org_id: Uuid, name: &str, unit: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO org_ingredients (id, org_id, name, unit, category, description, cost_per_unit) \
         VALUES ($1, $2, $3, $4::inventory_unit, 'veggies', 'x', NULL)",
    )
    .bind(id)
    .bind(org_id)
    .bind(name)
    .bind(unit)
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

async fn seed_option(pool: &PgPool, group: Uuid, name: &str, price: i32) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO modifier_options (id, group_id, name, price, legacy_source) \
         VALUES ($1, $2, $3, $4, 'addon')",
    )
    .bind(id)
    .bind(group)
    .bind(name)
    .bind(price)
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

async fn attach_group(
    pool: &PgPool,
    item: Uuid,
    group: Uuid,
    sort: i32,
    min_override: Option<i32>,
    max_override: Option<i32>,
    is_required_override: Option<bool>,
    included: Option<Vec<Uuid>>,
) {
    sqlx::query(
        "INSERT INTO menu_item_modifier_groups \
             (menu_item_id, group_id, sort, min_override, max_override, is_required_override, included_option_ids) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(item)
    .bind(group)
    .bind(sort)
    .bind(min_override)
    .bind(max_override)
    .bind(is_required_override)
    .bind(included.as_deref())
    .execute(pool)
    .await
    .unwrap();
}

/// Seed one `menu_price_overrides` row. `scope` drives which of branch/channel is set.
async fn seed_override(
    pool: &PgPool,
    scope: &str,
    branch_id: Option<Uuid>,
    channel: Option<&str>,
    target_type: &str,
    target_id: Uuid,
    price: Option<i32>,
    is_available: Option<bool>,
) {
    sqlx::query(
        "INSERT INTO menu_price_overrides \
             (scope, branch_id, channel, target_type, target_id, price, is_available) \
         VALUES ($1, $2, $3::delivery_channel, $4, $5, $6, $7)",
    )
    .bind(scope)
    .bind(branch_id)
    .bind(channel)
    .bind(target_type)
    .bind(target_id)
    .bind(price)
    .bind(is_available)
    .execute(pool)
    .await
    .unwrap();
}

async fn bump_revision(pool: &PgPool, org_id: Uuid) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO catalog_revision (org_id, revision) VALUES ($1, 1) \
         ON CONFLICT (org_id) DO UPDATE \
             SET revision = catalog_revision.revision + 1 RETURNING revision",
    )
    .bind(org_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn call_sync(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    token: &str,
    branch: Uuid,
    channel: &str,
    since: Option<i64>,
) -> (actix_web::http::StatusCode, CatalogSyncResponse) {
    let mut uri = format!("/catalog/sync?branch_id={branch}&channel={channel}");
    if let Some(s) = since {
        uri.push_str(&format!("&since={s}"));
    }
    let resp = test::call_service(
        app,
        test::TestRequest::get()
            .uri(&uri)
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    let status = resp.status();
    let raw = test::read_body(resp).await;
    let parsed: CatalogSyncResponse = serde_json::from_slice(&raw)
        .unwrap_or_else(|e| panic!("status {status} body {raw:?}: {e}"));
    (status, parsed)
}

// ── Test (a): effective price picks branch_channel > branch > base ───

#[sqlx::test]
async fn test_effective_price_precedence(pool: PgPool) {
    let app = app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let user = seed_user(&pool, org).await;
    grant(&pool, "menu_items", "read").await;
    let cat = seed_category(&pool, org).await;
    let item = seed_item(&pool, org, cat, "Latte", 5000).await;
    let token = org_admin_token(user, org);
    let branch = seed_branch(&pool, org).await;

    // Base price 4500.
    let size = seed_size(&pool, item, "small", 4500, 0).await;
    // channel override 4800, branch override 4200, branch_channel override 4000.
    seed_override(
        &pool,
        "channel",
        None,
        Some("outside"),
        "menu_item_size",
        size,
        Some(4800),
        None,
    )
    .await;
    seed_override(
        &pool,
        "branch",
        Some(branch),
        None,
        "menu_item_size",
        size,
        Some(4200),
        None,
    )
    .await;
    seed_override(
        &pool,
        "branch_channel",
        Some(branch),
        Some("outside"),
        "menu_item_size",
        size,
        Some(4000),
        None,
    )
    .await;

    let (status, body) = call_sync(&app, &token, branch, "outside", None).await;
    assert!(status.is_success(), "status {status}");
    assert!(body.changed);
    let it = body.items.iter().find(|i| i.id == item).unwrap();
    let sz = it.sizes.iter().find(|s| s.id == size).unwrap();
    assert_eq!(
        sz.price, 4000,
        "branch_channel override wins over branch and channel"
    );

    // With a DIFFERENT channel (no branch_channel/channel row for it), branch wins.
    let (_s2, body2) = call_sync(&app, &token, branch, "in_mall", None).await;
    let sz2 = body2
        .items
        .iter()
        .find(|i| i.id == item)
        .unwrap()
        .sizes
        .iter()
        .find(|s| s.id == size)
        .unwrap();
    assert_eq!(
        sz2.price, 4200,
        "no branch_channel/channel for in_mall → branch override wins"
    );

    // A DIFFERENT branch with only the org-wide channel override → channel wins.
    let branch2 = seed_branch(&pool, org).await;
    let (_s3, body3) = call_sync(&app, &token, branch2, "outside", None).await;
    let sz3 = body3
        .items
        .iter()
        .find(|i| i.id == item)
        .unwrap()
        .sizes
        .iter()
        .find(|s| s.id == size)
        .unwrap();
    assert_eq!(
        sz3.price, 4800,
        "only the channel override applies → channel wins over base"
    );
}

// ── Branch-only resolution: `channel` omitted (the in-store POS) ─────

#[sqlx::test]
async fn test_channel_omitted_branch_only_resolution(pool: PgPool) {
    let app = app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let user = seed_user(&pool, org).await;
    grant(&pool, "menu_items", "read").await;
    let cat = seed_category(&pool, org).await;
    let item = seed_item(&pool, org, cat, "Latte", 5000).await;
    let token = org_admin_token(user, org);
    let branch = seed_branch(&pool, org).await;

    let size = seed_size(&pool, item, "small", 4500, 0).await;
    // Channel-scoped rows must NOT apply when no channel is given; branch must.
    seed_override(
        &pool,
        "channel",
        None,
        Some("outside"),
        "menu_item_size",
        size,
        Some(4800),
        None,
    )
    .await;
    seed_override(
        &pool,
        "branch_channel",
        Some(branch),
        Some("outside"),
        "menu_item_size",
        size,
        Some(4000),
        None,
    )
    .await;
    seed_override(
        &pool,
        "branch",
        Some(branch),
        None,
        "menu_item_size",
        size,
        Some(4200),
        None,
    )
    .await;

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/catalog/sync?branch_id={branch}"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    assert!(
        resp.status().is_success(),
        "omitted channel must be accepted (branch-only resolution)"
    );
    let body: CatalogSyncResponse = test::read_body_json(resp).await;
    let sz = body
        .items
        .iter()
        .find(|i| i.id == item)
        .unwrap()
        .sizes
        .iter()
        .find(|s| s.id == size)
        .unwrap();
    assert_eq!(
        sz.price, 4200,
        "branch override wins; channel/branch_channel scopes ignored without a channel"
    );
}

// ── Test (b): effective availability resolves false via an override ──

#[sqlx::test]
async fn test_effective_availability_override_false(pool: PgPool) {
    let app = app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let user = seed_user(&pool, org).await;
    grant(&pool, "menu_items", "read").await;
    let cat = seed_category(&pool, org).await;
    let item = seed_item(&pool, org, cat, "Latte", 5000).await;
    let token = org_admin_token(user, org);
    let branch = seed_branch(&pool, org).await;

    // Two sizes: `small` made unavailable via a branch override; `large` inherits TRUE.
    let small = seed_size(&pool, item, "small", 4500, 0).await;
    let large = seed_size(&pool, item, "large", 6000, 1).await;
    // is_available=false WITHOUT setting price → price still resolves to the base default.
    seed_override(
        &pool,
        "branch",
        Some(branch),
        None,
        "menu_item_size",
        small,
        None,
        Some(false),
    )
    .await;

    let (status, body) = call_sync(&app, &token, branch, "outside", None).await;
    assert!(status.is_success(), "status {status}");
    let it = body.items.iter().find(|i| i.id == item).unwrap();

    // small is INCLUDED with is_available:false (documented include-with-flag choice).
    let small_out = it.sizes.iter().find(|s| s.id == small).unwrap();
    assert!(
        !small_out.is_available,
        "branch override resolves availability to false"
    );
    assert_eq!(
        small_out.price, 4500,
        "availability=false alone keeps the base price (independent fields)"
    );

    // large has no override → defaults to available.
    let large_out = it.sizes.iter().find(|s| s.id == large).unwrap();
    assert!(
        large_out.is_available,
        "no override → availability defaults to TRUE"
    );
}

// ── Test (c): included_option_ids filters options ────────────────────

#[sqlx::test]
async fn test_included_option_ids_filters_options(pool: PgPool) {
    let app = app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let user = seed_user(&pool, org).await;
    grant(&pool, "menu_items", "read").await;
    let cat = seed_category(&pool, org).await;
    let item = seed_item(&pool, org, cat, "Latte", 5000).await;
    let token = org_admin_token(user, org);
    let branch = seed_branch(&pool, org).await;

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
    let oat = seed_option(&pool, grp, "Oat", 1000).await;
    let soy = seed_option(&pool, grp, "Soy", 1200).await;
    let almond = seed_option(&pool, grp, "Almond", 900).await;
    // The option carries a swap-marker recipe line (qty 0) referencing an ingredient.
    let milk = seed_ingredient(&pool, org, "Milk", "l").await;
    seed_recipe_line(&pool, "modifier_option", oat, milk, "0", "l").await;

    // Attach with allowlist [oat, soy] (almond excluded) and min/max/required overrides.
    attach_group(
        &pool,
        item,
        grp,
        0,
        Some(2),
        Some(3),
        Some(false),
        Some(vec![oat, soy]),
    )
    .await;

    let (status, body) = call_sync(&app, &token, branch, "outside", None).await;
    assert!(status.is_success(), "status {status}");
    let it = body.items.iter().find(|i| i.id == item).unwrap();
    assert_eq!(it.modifier_groups.len(), 1);
    let g = &it.modifier_groups[0];
    assert_eq!(g.group_id, grp);

    // Only the allowlisted options are present.
    assert_eq!(
        g.options.len(),
        2,
        "almond is excluded by included_option_ids"
    );
    let ids: Vec<Uuid> = g.options.iter().map(|o| o.id).collect();
    assert!(ids.contains(&oat) && ids.contains(&soy));
    assert!(
        !ids.contains(&almond),
        "excluded option is not in the snapshot"
    );

    // Resolved min/max/required come from the attachment overrides.
    assert_eq!(g.min, 2, "min_override applied");
    assert_eq!(g.max, Some(3), "max_override applied");
    assert!(!g.is_required, "is_required_override applied");
    assert_eq!(g.selection_type, "single");
    assert_eq!(g.legacy_addon_type.as_deref(), Some("milk_type"));

    // Oat carries its swap-marker recipe line + the referenced ingredient is hydrated.
    let oat_out = g.options.iter().find(|o| o.id == oat).unwrap();
    assert_eq!(oat_out.recipe.len(), 1);
    assert_eq!(oat_out.recipe[0].ingredient_id, milk);
    assert_eq!(oat_out.recipe[0].unit, "l");
    assert!(oat_out.recipe[0].quantity.starts_with('0'));
    assert!(
        body.ingredients
            .iter()
            .any(|i| i.id == milk && i.name == "Milk" && i.unit == "l"),
        "ingredients[] hydrates the referenced org ingredient"
    );
}

// ── Test (d): since == current returns changed:false ─────────────────

#[sqlx::test]
async fn test_since_equal_current_returns_unchanged(pool: PgPool) {
    let app = app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let user = seed_user(&pool, org).await;
    grant(&pool, "menu_items", "read").await;
    let cat = seed_category(&pool, org).await;
    let item = seed_item(&pool, org, cat, "Latte", 5000).await;
    seed_size(&pool, item, "small", 4500, 0).await;
    let token = org_admin_token(user, org);
    let branch = seed_branch(&pool, org).await;

    // Seed a known revision (bump twice → revision 2).
    bump_revision(&pool, org).await;
    let current = bump_revision(&pool, org).await;
    assert_eq!(current, 2);

    // since == current → changed:false, empty payload.
    let (status, body) = call_sync(&app, &token, branch, "outside", Some(current)).await;
    assert!(status.is_success(), "status {status}");
    assert_eq!(body.catalog_revision, current);
    assert!(!body.changed, "since == current ⇒ changed:false");
    assert!(body.items.is_empty(), "no items on an unchanged poll");
    assert!(
        body.ingredients.is_empty(),
        "no ingredients on an unchanged poll"
    );

    // since < current → full payload with items.
    let (_s2, body2) = call_sync(&app, &token, branch, "outside", Some(current - 1)).await;
    assert!(body2.changed, "since < current ⇒ changed:true");
    assert!(
        body2.items.iter().any(|i| i.id == item),
        "stale poll returns the full catalog"
    );

    // No since → always a full payload.
    let (_s3, body3) = call_sync(&app, &token, branch, "outside", None).await;
    assert!(body3.changed, "no since ⇒ full payload");
    assert_eq!(body3.catalog_revision, current);
}

// ── Guard: cross-org sync is forbidden ───────────────────────────────

#[sqlx::test]
async fn test_catalog_sync_cross_org_forbidden(pool: PgPool) {
    let app = app(pool.clone()).await;
    let org_a = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_a).await;

    let org_b = seed_org(&pool).await;
    let user_b = seed_user(&pool, org_b).await;
    grant(&pool, "menu_items", "read").await;
    let token_b = org_admin_token(user_b, org_b);

    // org B's token asking for org A's branch → 403 (require_same_org against branch org).
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!(
                "/catalog/sync?branch_id={branch_a}&channel=outside"
            ))
            .insert_header(("Authorization", format!("Bearer {token_b}")))
            .to_request(),
    )
    .await;
    assert_eq!(
        resp.status(),
        403,
        "cross-org catalog sync must be forbidden"
    );
}

// ── Guard: unknown branch → 404; bad channel → 400 ───────────────────

#[sqlx::test]
async fn test_catalog_sync_bad_inputs(pool: PgPool) {
    let app = app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let user = seed_user(&pool, org).await;
    grant(&pool, "menu_items", "read").await;
    let branch = seed_branch(&pool, org).await;
    let token = org_admin_token(user, org);

    // Bad channel → 400.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!(
                "/catalog/sync?branch_id={branch}&channel=teleport"
            ))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 400, "an invalid channel is a 400");

    // Unknown branch → 404.
    let ghost = Uuid::new_v4();
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/catalog/sync?branch_id={ghost}&channel=outside"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 404, "an unknown branch is a 404");
}

// ── Only-active filtering: inactive item/size/group/option omitted ───

#[sqlx::test]
async fn test_only_active_included(pool: PgPool) {
    let app = app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let user = seed_user(&pool, org).await;
    grant(&pool, "menu_items", "read").await;
    let cat = seed_category(&pool, org).await;
    let token = org_admin_token(user, org);
    let branch = seed_branch(&pool, org).await;

    // Active item with one active + one INACTIVE size.
    let item = seed_item(&pool, org, cat, "Latte", 5000).await;
    let active_size = seed_size(&pool, item, "small", 4500, 0).await;
    let dead_size = seed_size(&pool, item, "gone", 6000, 1).await;
    sqlx::query("UPDATE menu_item_sizes SET is_active = false WHERE id = $1")
        .bind(dead_size)
        .execute(&pool)
        .await
        .unwrap();

    // An INACTIVE item — must not appear at all.
    let dead_item = seed_item(&pool, org, cat, "Retired", 3000).await;
    seed_size(&pool, dead_item, "small", 3000, 0).await;
    sqlx::query("UPDATE menu_items SET is_active = false WHERE id = $1")
        .bind(dead_item)
        .execute(&pool)
        .await
        .unwrap();

    // Group with one active + one inactive option.
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
    let live_opt = seed_option(&pool, grp, "Oat", 1000).await;
    let dead_opt = seed_option(&pool, grp, "Soy", 1200).await;
    sqlx::query("UPDATE modifier_options SET is_active = false WHERE id = $1")
        .bind(dead_opt)
        .execute(&pool)
        .await
        .unwrap();
    attach_group(&pool, item, grp, 0, None, None, None, None).await;

    let (status, body) = call_sync(&app, &token, branch, "outside", None).await;
    assert!(status.is_success(), "status {status}");

    // Inactive item absent.
    assert!(
        body.items.iter().all(|i| i.id != dead_item),
        "inactive item omitted"
    );
    let it = body.items.iter().find(|i| i.id == item).unwrap();

    // Only the active size present.
    assert_eq!(it.sizes.len(), 1);
    assert_eq!(it.sizes[0].id, active_size);

    // Only the active option present (allowlist NULL = all ACTIVE options).
    assert_eq!(it.modifier_groups.len(), 1);
    let opt_ids: Vec<Uuid> = it.modifier_groups[0].options.iter().map(|o| o.id).collect();
    assert!(opt_ids.contains(&live_opt));
    assert!(!opt_ids.contains(&dead_opt), "inactive option omitted");
}
