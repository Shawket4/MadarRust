//! `POST /menu/bulk`: the self-serve menu import (Madar MENU_IMPORT_PLAN.md).
//! The body is what a dashboard builds with madar-menu; these tests send it as
//! raw JSON, the way a client does, and read the tables back.

use actix_web::{App, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::JwtSecret;
use madar_rust::menu::routes;
use madar_rust::models::UserRole;

fn secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn token(user: Uuid, org: Uuid, role: UserRole) -> String {
    madar_rust::auth::jwt::create_token(&secret(), user, Some(org), role, None, 24).unwrap()
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
            .app_data(web::Data::new(secret()))
            .configure(routes::configure),
    )
    .await
}

const ORG_NAME: &str = "Test Org";

async fn seed_org(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(ORG_NAME)
        .bind(format!("bulk-{id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn seed_user(pool: &PgPool, org: Uuid, role: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, 'Person', $3, 'hash', $4::user_role)",
    )
    .bind(id)
    .bind(org)
    .bind(format!("u-{id}@test.com"))
    .bind(role)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// A fresh org and its owner, who holds every menu write by default.
async fn owner(pool: &PgPool) -> (Uuid, Uuid, String) {
    let org = seed_org(pool).await;
    let user = seed_user(pool, org, "org_admin").await;
    (org, user, token(user, org, UserRole::OrgAdmin))
}

/// Two categories; a milk swap with an inactive option; extras with no
/// maximum; a sized item with optionals, a one-price item with a tighter
/// extras link, an inactive item.
fn menu() -> Value {
    json!({
        "version": 1,
        "categories": [
            { "key": "c1", "name": "Coffee", "name_ar": "قهوة" },
            { "key": "c2", "name": "Bakery" }
        ],
        "groups": [
            { "key": "g1", "name": "Milk", "name_ar": "الحليب", "min": 1, "max": 1, "family": "milk_type",
              "options": [
                  { "name": "Full Cream", "price": 0, "is_active": true },
                  { "name": "Oat", "name_ar": "شوفان", "price": 4500, "is_active": true },
                  { "name": "Soy", "price": 4000, "is_active": false }
              ] },
            { "key": "g2", "name": "Extras", "min": 0, "max": null, "family": "extra",
              "options": [ { "name": "Shot", "price": 2000, "is_active": true } ] }
        ],
        "items": [
            { "name": "Latte", "name_ar": "لاتيه", "category": "c1", "price": 9000, "is_active": true,
              "sizes": [
                  { "label": "Small", "price": 9000, "is_active": true },
                  { "label": "Large", "price": 11000, "is_active": true }
              ],
              "groups": [
                  { "group": "g1", "min": 1, "max": 1 },
                  { "group": "g2", "min": 0, "max": null }
              ],
              "optionals": [ { "name": "Honey", "price": 500 } ] },
            { "name": "Espresso", "category": "c1", "price": 6000, "is_active": true,
              "description": "Double shot",
              "groups": [ { "group": "g2", "min": 0, "max": 1 } ] },
            { "name": "Cookie", "category": "c2", "price": 3500, "is_active": false }
        ]
    })
}

async fn post(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    token: &str,
    query: &str,
    body: Value,
) -> (u16, Value) {
    let req = test::TestRequest::post()
        .uri(&format!("/menu/bulk{query}"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(body)
        .to_request();
    let resp = test::call_service(app, req).await;
    let status = resp.status().as_u16();
    let bytes = test::read_body(resp).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn add(menu: Value) -> Value {
    json!({ "menu": menu })
}

async fn scalar(pool: &PgPool, sql: &str, org: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .bind(org)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn live_items(pool: &PgPool, org: Uuid) -> i64 {
    scalar(
        pool,
        "SELECT count(*) FROM menu_items WHERE org_id = $1 AND deleted_at IS NULL",
        org,
    )
    .await
}

#[sqlx::test]
async fn a_whole_menu_is_written_into_an_empty_org(pool: PgPool) {
    let (org, _, token) = owner(&pool).await;
    // The changefeed fans out per branch; a shop always has its first one.
    sqlx::query("INSERT INTO branches (org_id, name) VALUES ($1, 'Main')")
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
    let app = app(pool.clone()).await;
    let (status, body) = post(&app, &token, "", add(menu())).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["dry_run"], false);
    assert_eq!(
        body["summary"],
        json!({ "categories": 2, "items": 3, "sizes": 2, "groups": 2, "options": 4, "links": 3, "optionals": 1 })
    );
    let import_id: Uuid = serde_json::from_value(body["import_id"].clone()).unwrap();

    // Categories, in the file's order, with their Arabic.
    let cats: Vec<(String, i32, Value)> = sqlx::query_as(
        "SELECT name, display_order, name_translations FROM categories \
          WHERE org_id = $1 AND deleted_at IS NULL ORDER BY display_order",
    )
    .bind(org)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        cats,
        [
            ("Coffee".into(), 0, json!({ "ar": "قهوة" })),
            ("Bakery".into(), 1, json!({}))
        ]
    );

    // Items: the sized one's base price follows its lowest size; the one-price
    // item keeps its hidden size; the inactive one is off.
    let items: Vec<(String, i32, bool, Value, Option<String>)> = sqlx::query_as(
        "SELECT name, base_price, is_active, name_translations, description FROM menu_items \
          WHERE org_id = $1 ORDER BY name",
    )
    .bind(org)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        items,
        [
            ("Cookie".into(), 3500, false, json!({}), None),
            (
                "Espresso".into(),
                6000,
                true,
                json!({}),
                Some("Double shot".into())
            ),
            ("Latte".into(), 9000, true, json!({ "ar": "لاتيه" }), None),
        ]
    );
    let sizes: Vec<(String, String, i32)> = sqlx::query_as(
        "SELECT m.name, z.label, z.price FROM menu_item_sizes z JOIN menu_items m ON m.id = z.menu_item_id \
          WHERE m.org_id = $1 ORDER BY m.name, z.sort, z.label",
    )
    .bind(org)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        sizes,
        [
            ("Cookie".into(), "one_size".into(), 3500),
            ("Espresso".into(), "one_size".into(), 6000),
            ("Latte".into(), "Small".into(), 9000),
            ("Latte".into(), "Large".into(), 11000),
        ]
    );

    // Groups: milk is a single required swap; extras pick-many; the Latte's
    // optionals are its own private group.
    let groups: Vec<(String, String, i32, Option<i32>, bool, Option<String>)> = sqlx::query_as(
        "SELECT name, selection_type, min_selections, max_selections, is_required, legacy_addon_type \
           FROM modifier_groups WHERE org_id = $1 ORDER BY sort, name",
    )
    .bind(org)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        groups,
        [
            (
                "Milk".into(),
                "single".into(),
                1,
                Some(1),
                true,
                Some("milk_type".into())
            ),
            (
                "Extras".into(),
                "multi".into(),
                0,
                None,
                false,
                Some("extra".into())
            ),
            ("Options".into(), "multi".into(), 0, None, false, None),
        ]
    );
    let options: Vec<(String, String, i32, bool, String)> = sqlx::query_as(
        "SELECT g.name, o.name, o.price, o.is_active, o.legacy_source FROM modifier_options o \
           JOIN modifier_groups g ON g.id = o.group_id WHERE g.org_id = $1 ORDER BY g.sort, o.sort",
    )
    .bind(org)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        options,
        [
            ("Milk".into(), "Full Cream".into(), 0, true, "addon".into()),
            ("Milk".into(), "Oat".into(), 4500, true, "addon".into()),
            ("Milk".into(), "Soy".into(), 4000, false, "addon".into()),
            ("Extras".into(), "Shot".into(), 2000, true, "addon".into()),
            (
                "Options".into(),
                "Honey".into(),
                500,
                true,
                "optional".into()
            ),
        ]
    );

    // Links: overrides only where the item differs; provenance and the option
    // lists filled by trigger, the inactive option included.
    let links: Vec<(
        String,
        String,
        i32,
        Option<i32>,
        Option<i32>,
        Option<String>,
        i32,
    )> = sqlx::query_as(
        "SELECT m.name, g.name, l.sort, l.min_override, l.max_override, l.legacy_origin, \
                cardinality(l.included_option_ids) \
           FROM menu_item_modifier_groups l \
           JOIN menu_items m ON m.id = l.menu_item_id JOIN modifier_groups g ON g.id = l.group_id \
          WHERE m.org_id = $1 ORDER BY m.name, l.sort",
    )
    .bind(org)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        links,
        [
            (
                "Espresso".into(),
                "Extras".into(),
                0,
                None,
                Some(1),
                Some("allowlist".into()),
                1
            ),
            (
                "Latte".into(),
                "Milk".into(),
                0,
                None,
                None,
                Some("slot".into()),
                3
            ),
            (
                "Latte".into(),
                "Extras".into(),
                1,
                None,
                None,
                Some("allowlist".into()),
                1
            ),
            (
                "Latte".into(),
                "Options".into(),
                2,
                None,
                None,
                Some("options".into()),
                1
            ),
        ]
    );

    // Price epochs as the item and size routes write them.
    let epochs: Vec<(String, Option<String>, i32)> = sqlx::query_as(
        "SELECT m.name, e.size_label, e.price FROM menu_item_price_epochs e \
           JOIN menu_items m ON m.id = e.menu_item_id \
          WHERE m.org_id = $1 AND e.effective_until IS NULL ORDER BY m.name, e.size_label NULLS FIRST",
    )
    .bind(org)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        epochs,
        [
            ("Cookie".into(), None, 3500),
            ("Espresso".into(), None, 6000),
            ("Latte".into(), None, 9000),
            ("Latte".into(), Some("Large".into()), 11000),
            ("Latte".into(), Some("Small".into()), 9000),
        ]
    );

    // The tills hear of it, and the import is on record.
    assert!(
        scalar(
            &pool,
            "SELECT revision FROM catalog_revision WHERE org_id = $1",
            org
        )
        .await
            >= 1
    );
    assert!(
        scalar(
            &pool,
            "SELECT count(*) FROM sync_changes WHERE org_id = $1",
            org
        )
        .await
            > 0
    );
    let (mode, sha): (String, String) =
        sqlx::query_as("SELECT mode, body_sha256 FROM menu_imports WHERE id = $1 AND org_id = $2")
            .bind(import_id)
            .bind(org)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(mode, "add");
    assert_eq!(sha.len(), 64);
}

#[sqlx::test]
async fn a_dry_run_answers_like_the_real_one_and_keeps_nothing(pool: PgPool) {
    let (org, _, token) = owner(&pool).await;
    let app = app(pool.clone()).await;
    let (status, body) = post(&app, &token, "?dry_run=true", add(menu())).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["dry_run"], true);
    assert_eq!(body["summary"]["items"], 3);
    assert_eq!(body["import_id"], Value::Null);
    for table in [
        "categories",
        "menu_items",
        "modifier_groups",
        "menu_imports",
    ] {
        let n = scalar(
            &pool,
            &format!("SELECT count(*) FROM {table} WHERE org_id = $1"),
            org,
        )
        .await;
        assert_eq!(n, 0, "{table}");
    }
    // A dry run is not one of the day's imports, and the real one still runs.
    let (status, _) = post(&app, &token, "", add(menu())).await;
    assert_eq!(status, 201);
}

#[sqlx::test]
async fn add_refuses_a_menu_that_is_not_empty(pool: PgPool) {
    let (org, _, token) = owner(&pool).await;
    sqlx::query("INSERT INTO categories (org_id, name) VALUES ($1, 'Mine')")
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
    let app = app(pool.clone()).await;
    for query in ["", "?dry_run=true"] {
        let (status, body) = post(&app, &token, query, add(menu())).await;
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["code"], "MENU_NOT_EMPTY");
        assert_eq!(
            body["vars"],
            json!({ "categories": 1, "items": 0, "groups": 0 })
        );
    }
    assert_eq!(live_items(&pool, org).await, 0);
}

#[sqlx::test]
async fn a_menu_the_shared_checks_refuse_is_refused_with_their_reason(pool: PgPool) {
    let (org, _, token) = owner(&pool).await;
    let app = app(pool.clone()).await;

    let mut m = menu();
    m["items"][1]["groups"][0]["group"] = json!("g404");
    let (status, body) = post(&app, &token, "", add(m)).await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["code"], "BULK_MENU_INVALID");
    assert_eq!(
        body["vars"],
        json!({ "refusal": "unknown_group", "item": "Espresso", "group": "g404" })
    );

    let mut m = menu();
    m["groups"][0]["max"] = json!(2);
    let (_, body) = post(&app, &token, "", add(m)).await;
    assert_eq!(body["vars"]["refusal"], "swap_not_single");

    let mut m = menu();
    m["version"] = json!(2);
    let (status, body) = post(&app, &token, "", add(m)).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (422, Some("BULK_MENU_UNSUPPORTED_VERSION"))
    );

    // Not even JSON of the right shape.
    let (status, _) = post(
        &app,
        &token,
        "",
        json!({ "menu": { "version": 1, "items": "x" } }),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(live_items(&pool, org).await, 0);
}

#[sqlx::test]
async fn replace_needs_the_typed_sentence_then_archives_the_old_menu(pool: PgPool) {
    let (org, _, token) = owner(&pool).await;
    let app = app(pool.clone()).await;
    assert_eq!(post(&app, &token, "", add(menu())).await.0, 201);
    let old_latte: Uuid =
        sqlx::query_scalar("SELECT id FROM menu_items WHERE org_id = $1 AND name = 'Latte'")
            .bind(org)
            .fetch_one(&pool)
            .await
            .unwrap();

    let mut next = menu();
    next["items"][0]["name"] = json!("Flat White");
    let replace = |confirm: Option<&str>| json!({ "menu": next.clone(), "mode": "replace", "confirm": confirm });

    // No sentence, or the wrong one: refused, with the sentence to type.
    for confirm in [None, Some("Replace the menu of Test"), Some(ORG_NAME)] {
        let (status, body) = post(&app, &token, "", replace(confirm)).await;
        assert_eq!(status, 422, "{body}");
        assert_eq!(body["code"], "MENU_REPLACE_NOT_CONFIRMED");
        assert_eq!(body["vars"]["sentence_en"], "Replace the menu of Test Org");
        assert_eq!(body["vars"]["sentence_ar"], "استبدل قائمة Test Org");
    }

    // A dry run needs no sentence, says what it would archive, and keeps it all.
    let (status, body) = post(&app, &token, "?dry_run=true", replace(None)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["archived"],
        json!({ "categories": 2, "items": 3, "groups": 3 })
    );
    assert_eq!(live_items(&pool, org).await, 3);

    // The Arabic sentence, typed loosely, does it.
    let (status, body) = post(&app, &token, "", replace(Some("  استبدل  قائمة test org "))).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["mode"], "replace");
    assert_eq!(
        body["archived"],
        json!({ "categories": 2, "items": 3, "groups": 3 })
    );

    // The old menu is put away, never deleted: an old order line still finds
    // its item. Its open price epochs are closed.
    let (deleted, epochs_open): (bool, i64) = sqlx::query_as(
        "SELECT m.deleted_at IS NOT NULL, \
                (SELECT count(*) FROM menu_item_price_epochs e \
                  WHERE e.menu_item_id = m.id AND e.effective_until IS NULL) \
           FROM menu_items m WHERE m.id = $1",
    )
    .bind(old_latte)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(deleted);
    assert_eq!(epochs_open, 0);
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM menu_items WHERE org_id = $1 AND deleted_at IS NULL ORDER BY name",
    )
    .bind(org)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(names, ["Cookie", "Espresso", "Flat White"]);
    assert_eq!(
        scalar(
            &pool,
            "SELECT count(*) FROM modifier_groups WHERE org_id = $1 AND NOT is_active",
            org
        )
        .await,
        3
    );
    assert_eq!(
        scalar(
            &pool,
            "SELECT count(*) FROM categories WHERE org_id = $1 AND deleted_at IS NULL",
            org
        )
        .await,
        2
    );
    let mode: String = sqlx::query_scalar(
        "SELECT mode FROM menu_imports WHERE org_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(mode, "replace");
}

#[sqlx::test]
async fn each_mode_needs_its_permissions(pool: PgPool) {
    let (org, owner_id, owner_token) = owner(&pool).await;
    // A branch manager holds none of the menu writes.
    let manager = seed_user(&pool, org, "branch_manager").await;
    let manager_token = token(manager, org, UserRole::BranchManager);
    // The owner, denied deleting items, may add a menu but not replace one.
    madar_rust::authz::sync_catalogue(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO user_overrides (org_id, user_id, capability_id, effect, reason) \
         SELECT $1, $2, id, 'deny', 'test' FROM capabilities WHERE key = 'menu.items.delete'",
    )
    .bind(org)
    .bind(owner_id)
    .execute(&pool)
    .await
    .unwrap();
    let app = app(pool.clone()).await;
    assert_eq!(
        post(&app, &manager_token, "?dry_run=true", add(menu()))
            .await
            .0,
        403
    );
    let body =
        json!({ "menu": menu(), "mode": "replace", "confirm": "Replace the menu of Test Org" });
    assert_eq!(post(&app, &owner_token, "?dry_run=true", body).await.0, 403);
    assert_eq!(post(&app, &owner_token, "", add(menu())).await.0, 201);
}

#[sqlx::test]
async fn an_import_lands_in_the_tokens_org_only(pool: PgPool) {
    let (org_a, _, token_a) = owner(&pool).await;
    let (org_b, _, token_b) = owner(&pool).await;
    let app = app(pool.clone()).await;
    assert_eq!(post(&app, &token_b, "", add(menu())).await.0, 201);
    // B's menu does not make A's non-empty, and A's import leaves B as it was.
    assert_eq!(post(&app, &token_a, "", add(menu())).await.0, 201);
    assert_eq!(live_items(&pool, org_a).await, 3);
    assert_eq!(live_items(&pool, org_b).await, 3);
    let body =
        json!({ "menu": menu(), "mode": "replace", "confirm": "Replace the menu of Test Org" });
    assert_eq!(post(&app, &token_a, "", body).await.0, 201);
    assert_eq!(
        scalar(
            &pool,
            "SELECT count(*) FROM menu_items WHERE org_id = $1 AND deleted_at IS NOT NULL",
            org_b
        )
        .await,
        0
    );
}

#[sqlx::test]
async fn an_org_gets_its_imports_a_day(pool: PgPool) {
    let (org, _, token) = owner(&pool).await;
    for _ in 0..madar_rust::menu::bulk::IMPORTS_PER_DAY {
        sqlx::query(
            "INSERT INTO menu_imports (org_id, mode, summary, body_sha256) VALUES ($1, 'add', '{}', 'x')",
        )
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
    }
    let app = app(pool.clone()).await;
    let (status, body) = post(&app, &token, "", add(menu())).await;
    assert_eq!(status, 429, "{body}");
    assert_eq!(body["code"], "MENU_IMPORT_LIMIT");
    // Dry runs are still answered.
    assert_eq!(
        post(&app, &token, "?dry_run=true", add(menu())).await.0,
        200
    );
}

#[sqlx::test]
async fn a_body_over_the_limit_is_refused_before_it_is_read(pool: PgPool) {
    let (_, _, token) = owner(&pool).await;
    let app = app(pool.clone()).await;
    let mut m = menu();
    m["items"][0]["description"] = json!("x".repeat(madar_rust::menu::bulk::MAX_BODY_BYTES));
    assert_eq!(post(&app, &token, "", add(m)).await.0, 400);
}

#[sqlx::test]
async fn two_imports_at_once_write_one_menu(pool: PgPool) {
    let (org, _, token) = owner(&pool).await;
    let app = app(pool.clone()).await;
    let (a, b) = tokio::join!(
        post(&app, &token, "", add(menu())),
        post(&app, &token, "", add(menu()))
    );
    let mut statuses = [a.0, b.0];
    statuses.sort_unstable();
    assert_eq!(statuses, [201, 409], "{} / {}", a.1, b.1);
    assert_eq!(live_items(&pool, org).await, 3);
}
