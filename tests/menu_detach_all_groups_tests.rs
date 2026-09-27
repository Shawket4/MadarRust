//! `PUT /menu-items/{id}/modifier-groups` with an EXPLICIT empty set detaches
//! every group of the item (owner, 2026-09-27; T1 finding B5).
//!
//! Before: the replace-set only deleted TYPED attachments, because an untyped
//! group was also how the item's own priced "Options" group was recognised. A
//! custom (untyped) group therefore never left the item, and `{"groups": []}`
//! answered 200 having changed nothing. Now:
//! * `[]` detaches every typed and custom group; the item's own Options group
//!   stays (it belongs to `PUT /menu-items/{id}/options`);
//! * an omitted or `null` set changes nothing (older clients, partial saves);
//! * the legacy shapes old tills read (addon slots, the allowed-addon list)
//!   are views over the same rows, so no old link survives;
//! * the till's `menu_item` feed row loses the groups, and never reads as "not
//!   set up", which a till answers by offering every add-on of the org.
//!
//! Runs on a database the contract shim has been applied to, as on the box.

use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::orders::component_resolve::{AddonInput, resolve_menu_item_configuration};

fn test_secret() -> madar_rust::auth::jwt::JwtSecret {
    madar_rust::auth::jwt::JwtSecret("detach-all-secret".into())
}

macro_rules! app {
    ($pool:expr) => {
        actix_web::test::init_service(
            actix_web::App::new()
                .app_data(actix_web::web::Data::new($pool.clone()))
                .app_data(actix_web::web::Data::new(test_secret()))
                .app_data(actix_web::web::Data::new(
                    madar_rust::realtime::hub::BranchEventHub::new(),
                ))
                .configure(madar_rust::menu::routes::configure)
                .configure(madar_rust::sync::routes::configure),
        )
        .await
    };
}

async fn http<S>(app: &S, method: &str, uri: &str, token: &str, body: Option<Value>) -> (u16, Value)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    use actix_web::test::TestRequest;
    let mut r = match method {
        "GET" => TestRequest::get(),
        "POST" => TestRequest::post(),
        "PUT" => TestRequest::put(),
        other => panic!("unsupported method {other}"),
    }
    .uri(uri)
    .insert_header(("Authorization", format!("Bearer {token}")));
    if let Some(b) = body {
        r = r.set_json(b);
    }
    let resp = actix_web::test::call_service(app, r.to_request()).await;
    let status = resp.status().as_u16();
    let bytes = actix_web::test::read_body(resp).await;
    let v = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, v)
}

struct Shop {
    org: Uuid,
    branch: Uuid,
    token: String,
    category: Uuid,
}

/// An org with a branch, a category and its owner, on a shimmed database.
async fn shop(pool: &PgPool) -> Shop {
    let org: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug) VALUES ('Detach Org', $1) RETURNING id",
    )
    .bind(format!("detach-{}", Uuid::new_v4()))
    .fetch_one(pool)
    .await
    .unwrap();
    let branch: Uuid = sqlx::query_scalar(
        "INSERT INTO branches (org_id, name, code) VALUES ($1, 'Main', 'MAIN') RETURNING id",
    )
    .bind(org)
    .fetch_one(pool)
    .await
    .unwrap();
    let owner: Uuid = sqlx::query_scalar(
        "INSERT INTO users (org_id, name, email, password_hash, role) VALUES ($1, 'Owner', $2, 'x', 'org_admin') RETURNING id",
    )
    .bind(org)
    .bind(format!("{}@detach.test", Uuid::new_v4()))
    .fetch_one(pool)
    .await
    .unwrap();
    let category: Uuid = sqlx::query_scalar(
        "INSERT INTO categories (org_id, name) VALUES ($1, 'Coffee') RETURNING id",
    )
    .bind(org)
    .fetch_one(pool)
    .await
    .unwrap();
    madar_rust::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../deploy/menu_unification_shim.sql"))
        .execute(pool)
        .await
        .unwrap();
    let token = madar_rust::auth::jwt::create_token(
        &test_secret(),
        owner,
        Some(org),
        madar_rust::models::UserRole::OrgAdmin,
        None,
        1,
    )
    .unwrap();
    Shop {
        org,
        branch,
        token,
        category,
    }
}

async fn item(pool: &PgPool, s: &Shop, name: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO menu_items (org_id, category_id, name, base_price) VALUES ($1, $2, $3, 6000) RETURNING id",
    )
    .bind(s.org)
    .bind(s.category)
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A group created the way the dashboard (typed) or an API client (custom, no
/// type) creates one, with one add-on option. Returns (group, option).
async fn group<S>(app: &S, s: &Shop, body: Value, option: &str, price: i32) -> (Uuid, Uuid)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    let (st, g) = http(app, "POST", "/modifier-groups", &s.token, Some(body)).await;
    assert_eq!(st, 201, "{g}");
    let gid: Uuid = g["id"].as_str().unwrap().parse().unwrap();
    let (st, o) = http(
        app,
        "POST",
        &format!("/modifier-groups/{gid}/options"),
        &s.token,
        Some(json!({"name": option, "price": price})),
    )
    .await;
    assert_eq!(st, 201, "{o}");
    (gid, o["id"].as_str().unwrap().parse().unwrap())
}

async fn put_groups<S>(app: &S, s: &Shop, item: Uuid, body: Value) -> (u16, Value)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    http(
        app,
        "PUT",
        &format!("/menu-items/{item}/modifier-groups"),
        &s.token,
        Some(body),
    )
    .await
}

fn set(groups: &[Uuid]) -> Value {
    json!({
        "groups": groups
            .iter()
            .enumerate()
            .map(|(i, g)| json!({"group_id": g, "sort": i}))
            .collect::<Vec<_>>()
    })
}

/// The groups attached to `item`, by group id.
async fn attached(pool: &PgPool, item: Uuid) -> Vec<Uuid> {
    sqlx::query_scalar(
        "SELECT group_id FROM menu_item_modifier_groups WHERE menu_item_id = $1 ORDER BY sort, group_id",
    )
    .bind(item)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// What an old till reads for `item`: (addon slots, allowed add-ons, optionals),
/// straight from the contract shim's views.
async fn legacy_links(pool: &PgPool, item: Uuid) -> (i64, i64, i64) {
    let n = |sql: &'static str| async move {
        sqlx::query_scalar::<_, i64>(sql)
            .bind(item)
            .fetch_one(pool)
            .await
            .unwrap()
    };
    (
        n("SELECT COUNT(*) FROM menu_item_addon_slots WHERE menu_item_id = $1").await,
        n("SELECT COUNT(*) FROM menu_item_allowed_addons WHERE menu_item_id = $1").await,
        n("SELECT COUNT(*) FROM menu_item_optional_fields WHERE menu_item_id = $1").await,
    )
}

/// The item's `menu_item` row in a full `/sync/pull` snapshot: what a till's
/// catalogue holds after its next pull.
async fn feed_row<S>(app: &S, s: &Shop, item: Uuid) -> Value
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    let (st, pull) = http(
        app,
        "POST",
        "/sync/pull",
        &s.token,
        Some(json!({"branch_id": s.branch})),
    )
    .await;
    assert_eq!(st, 200, "{pull}");
    pull["data"]["menu_item"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == item.to_string())
        .cloned()
        .unwrap_or_else(|| panic!("{item} is not in the snapshot"))
}

fn group_ids(row: &Value) -> Vec<String> {
    row["modifier_groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["group_id"].as_str().unwrap().to_string())
        .collect()
}

fn option_ids(row: &Value) -> Vec<String> {
    row["modifier_groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["options"].as_array().unwrap().iter())
        .map(|o| o["id"].as_str().unwrap().to_string())
        .collect()
}

fn studio_group_ids(agg: &Value) -> Vec<String> {
    agg["modifier_groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["group_id"].as_str().unwrap().to_string())
        .collect()
}

async fn catalog_revision(pool: &PgPool, org: Uuid) -> i64 {
    sqlx::query_scalar("SELECT COALESCE(MAX(revision), 0) FROM catalog_revision WHERE org_id = $1")
        .bind(org)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The item's own Options group, as `PUT /options` leaves it.
async fn options_group(pool: &PgPool, item: Uuid) -> Option<Uuid> {
    sqlx::query_scalar(
        "SELECT m.group_id FROM menu_item_modifier_groups m \
           JOIN modifier_groups g ON g.id = m.group_id \
          WHERE m.menu_item_id = $1 AND g.legacy_addon_type IS NULL AND g.name = 'Options'",
    )
    .bind(item)
    .fetch_optional(pool)
    .await
    .unwrap()
}

#[sqlx::test]
async fn an_explicit_empty_set_detaches_every_group_of_a_new_style_item(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let latte = item(&pool, &s, "Latte").await;
    let (typed, shot) = group(
        &app,
        &s,
        json!({"name": "Extras", "selection_type": "multi", "legacy_addon_type": "extra"}),
        "Extra shot",
        700,
    )
    .await;
    // A custom group: no legacy type, as the T1 rig made it over the API.
    let (custom, drizzle) = group(
        &app,
        &s,
        json!({"name": "Toppings", "selection_type": "multi"}),
        "Caramel drizzle",
        500,
    )
    .await;
    let (st, agg) = put_groups(&app, &s, latte, set(&[typed, custom])).await;
    assert_eq!(st, 200, "{agg}");
    // The custom group now shows in the editor, so it can be taken off there.
    assert_eq!(
        studio_group_ids(&agg),
        vec![typed.to_string(), custom.to_string()]
    );
    // The item's own priced options (the Options section).
    let (st, opts) = http(
        &app,
        "PUT",
        &format!("/menu-items/{latte}/options"),
        &s.token,
        Some(json!({"options": [{"name": "Honey", "price": 300, "is_active": true}]})),
    )
    .await;
    assert_eq!(st, 200, "{opts}");
    let own = options_group(&pool, latte).await.expect("an Options group");
    let before = feed_row(&app, &s, latte).await;
    assert_eq!(group_ids(&before).len(), 3, "{before}");

    let (st, agg) = put_groups(&app, &s, latte, json!({"groups": []})).await;
    assert_eq!(st, 200, "{agg}");
    assert_eq!(agg["modifier_groups"], json!([]), "{agg}");
    assert_eq!(
        agg["options"][0]["name"], "Honey",
        "the options stay: {agg}"
    );
    assert_eq!(
        attached(&pool, latte).await,
        vec![own],
        "only the item's options remain"
    );
    assert_eq!(
        legacy_links(&pool, latte).await,
        (0, 0, 1),
        "no old slot or allowed add-on is left; the optional stays"
    );

    // The till's row loses both groups and their add-ons.
    let after = feed_row(&app, &s, latte).await;
    assert_eq!(group_ids(&after), vec![own.to_string()], "{after}");
    let offered = option_ids(&after);
    assert!(!offered.contains(&shot.to_string()), "{after}");
    assert!(!offered.contains(&drizzle.to_string()), "{after}");
}

/// An item as the menu unification backfill left it on the box: typed groups
/// linked as an old addon slot and an old allowlist, a custom group attached
/// as the item-private kind (the T1 rig case), and the backfilled Options group
/// (its id derived from the item, sorted AFTER the custom group).
#[sqlx::test]
async fn an_explicit_empty_set_detaches_a_legacy_linked_item(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let latte = item(&pool, &s, "Latte").await;
    let (slot_group, slot_opt) = group(
        &app,
        &s,
        json!({"name": "Syrup", "selection_type": "single", "legacy_addon_type": "syrup_type"}),
        "Vanilla",
        600,
    )
    .await;
    let (allow_group, allow_opt) = group(
        &app,
        &s,
        json!({"name": "Extras", "selection_type": "multi", "legacy_addon_type": "extra"}),
        "Extra shot",
        700,
    )
    .await;
    let (custom, custom_opt) = group(
        &app,
        &s,
        json!({"name": "Toppings", "selection_type": "multi"}),
        "Caramel drizzle",
        500,
    )
    .await;
    let backfilled: Uuid = sqlx::query_scalar(
        "INSERT INTO modifier_groups (id, org_id, name, selection_type, sort, legacy_addon_type) \
         VALUES (md5($1::text || ':options')::uuid, $2, 'Options', 'multi', 100, NULL) RETURNING id",
    )
    .bind(latte)
    .bind(s.org)
    .fetch_one(&pool)
    .await
    .unwrap();
    let optional: Uuid = sqlx::query_scalar(
        "INSERT INTO modifier_options (group_id, name, price, legacy_source) VALUES ($1, 'Honey', 300, 'optional') RETURNING id",
    )
    .bind(backfilled)
    .fetch_one(&pool)
    .await
    .unwrap();
    for (group_id, sort, required, included, origin) in [
        (slot_group, 1, Some(true), vec![slot_opt], "slot"),
        (allow_group, 2, None, vec![allow_opt], "allowlist"),
        (custom, 0, None, vec![custom_opt], "options"),
        (backfilled, 100, None, vec![optional], "options"),
    ] {
        sqlx::query(
            "INSERT INTO menu_item_modifier_groups \
                 (menu_item_id, group_id, sort, is_required_override, included_option_ids, legacy_origin) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(latte)
        .bind(group_id)
        .bind(sort)
        .bind(required)
        .bind(&included)
        .bind(origin)
        .execute(&pool)
        .await
        .unwrap();
    }
    // (the allowed-add-on view lists every add-on option an attachment offers:
    // the slot's, the allowlist's and the custom group's)
    assert_eq!(
        legacy_links(&pool, latte).await,
        (1, 3, 1),
        "the old links, before"
    );

    // The custom group sorts first, yet is never read as the item's options.
    let (st, agg) = http(
        &app,
        "GET",
        &format!("/menu-items/{latte}/studio"),
        &s.token,
        None,
    )
    .await;
    assert_eq!(st, 200, "{agg}");
    assert_eq!(agg["options"].as_array().unwrap().len(), 1, "{agg}");
    assert_eq!(agg["options"][0]["id"], optional.to_string());
    let mut shown = studio_group_ids(&agg);
    shown.sort();
    let mut expected = vec![
        slot_group.to_string(),
        allow_group.to_string(),
        custom.to_string(),
    ];
    expected.sort();
    assert_eq!(shown, expected, "every other group is in the editor");

    let (st, agg) = put_groups(&app, &s, latte, json!({"groups": []})).await;
    assert_eq!(st, 200, "{agg}");
    assert_eq!(attached(&pool, latte).await, vec![backfilled]);
    assert_eq!(
        legacy_links(&pool, latte).await,
        (0, 0, 1),
        "no old link is left"
    );
    let (st, legacy) = http(&app, "GET", &format!("/menu-items/{latte}"), &s.token, None).await;
    assert_eq!(st, 200, "{legacy}");
    assert_eq!(legacy["addon_slots"], json!([]), "{legacy}");
    assert_eq!(legacy["allowed_addon_ids"], json!([]), "{legacy}");

    let row = feed_row(&app, &s, latte).await;
    assert_eq!(group_ids(&row), vec![backfilled.to_string()], "{row}");

    // Saving the Options section never touches the custom group's add-ons.
    let (st, opts) = http(
        &app,
        "PUT",
        &format!("/menu-items/{latte}/options"),
        &s.token,
        Some(json!({"options": [{"id": optional, "name": "Honey", "price": 350, "is_active": true}]})),
    )
    .await;
    assert_eq!(st, 200, "{opts}");
    let drizzle: (String, i32, Uuid) =
        sqlx::query_as("SELECT name, price, group_id FROM modifier_options WHERE id = $1")
            .bind(custom_opt)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(drizzle, ("Caramel drizzle".to_string(), 500, custom));
}

#[sqlx::test]
async fn an_omitted_or_null_set_changes_nothing(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let latte = item(&pool, &s, "Latte").await;
    let (typed, _) = group(
        &app,
        &s,
        json!({"name": "Extras", "selection_type": "multi", "legacy_addon_type": "extra"}),
        "Extra shot",
        700,
    )
    .await;
    let (custom, _) = group(
        &app,
        &s,
        json!({"name": "Toppings", "selection_type": "multi"}),
        "Drizzle",
        500,
    )
    .await;
    let (st, _) = put_groups(&app, &s, latte, set(&[typed, custom])).await;
    assert_eq!(st, 200);
    let links = attached(&pool, latte).await;
    let revision = catalog_revision(&pool, s.org).await;

    for body in [json!({}), json!({"groups": null})] {
        let (st, agg) = put_groups(&app, &s, latte, body.clone()).await;
        assert_eq!(st, 200, "{body}: {agg}");
        assert_eq!(studio_group_ids(&agg).len(), 2, "{body}: {agg}");
        assert_eq!(attached(&pool, latte).await, links, "{body}");
        assert_eq!(
            catalog_revision(&pool, s.org).await,
            revision,
            "{body}: no catalogue tick"
        );
    }
}

#[sqlx::test]
async fn groups_attach_again_after_a_detach_all(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let latte = item(&pool, &s, "Latte").await;
    let (typed, shot) = group(
        &app,
        &s,
        json!({"name": "Extras", "selection_type": "multi", "legacy_addon_type": "extra"}),
        "Extra shot",
        700,
    )
    .await;
    let (custom, drizzle) = group(
        &app,
        &s,
        json!({"name": "Toppings", "selection_type": "multi"}),
        "Drizzle",
        500,
    )
    .await;
    assert_eq!(
        put_groups(&app, &s, latte, set(&[typed, custom])).await.0,
        200
    );
    assert_eq!(
        put_groups(&app, &s, latte, json!({"groups": []})).await.0,
        200
    );
    assert!(!attached(&pool, latte).await.contains(&typed));

    let (st, agg) = put_groups(&app, &s, latte, set(&[typed, custom])).await;
    assert_eq!(st, 200, "{agg}");
    assert_eq!(
        studio_group_ids(&agg),
        vec![typed.to_string(), custom.to_string()]
    );
    // The same set again is a no-op, never a conflict (the custom group used to
    // survive the delete and collide with its own re-insert).
    let (st, agg) = put_groups(&app, &s, latte, set(&[typed, custom])).await;
    assert_eq!(st, 200, "{agg}");
    let links = attached(&pool, latte).await;
    assert!(
        links.contains(&typed) && links.contains(&custom),
        "{links:?}"
    );

    let row = feed_row(&app, &s, latte).await;
    let offered = option_ids(&row);
    assert!(offered.contains(&shot.to_string()), "{row}");
    assert!(offered.contains(&drizzle.to_string()), "{row}");
    assert_eq!(
        legacy_links(&pool, latte).await.1,
        2,
        "both add-ons are allowed again"
    );
}

/// A till reads an item whose catalogue row lists NO group as "never set up"
/// and falls back to the legacy rule for such items: an empty allowlist offers
/// every add-on of the org (madar-core `UnifiedDoc::groups_for` →
/// `Core::modifier_groups_in` → `cart::item_modifier_groups`). Once the row
/// lists any group the unified list is authoritative, and a group with no
/// option renders nothing (`cart::item_modifier_groups_unified`). So a
/// detach-all must leave the row listing a group, and no option in it.
#[sqlx::test]
async fn a_detach_all_never_leaves_a_till_offering_every_addon(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let latte = item(&pool, &s, "Latte").await;
    let (typed, _) = group(
        &app,
        &s,
        json!({"name": "Extras", "selection_type": "multi", "legacy_addon_type": "extra"}),
        "Extra shot",
        700,
    )
    .await;
    let (custom, _) = group(
        &app,
        &s,
        json!({"name": "Toppings", "selection_type": "multi"}),
        "Drizzle",
        500,
    )
    .await;
    // Another add-on of the org, never attached to the latte.
    group(
        &app,
        &s,
        json!({"name": "Sauces", "selection_type": "multi", "legacy_addon_type": "sauce"}),
        "Chocolate",
        400,
    )
    .await;
    assert_eq!(
        put_groups(&app, &s, latte, set(&[typed, custom])).await.0,
        200
    );

    for round in 0..2 {
        let (st, agg) = put_groups(&app, &s, latte, json!({"groups": []})).await;
        assert_eq!(st, 200, "{agg}");
        assert_eq!(agg["modifier_groups"], json!([]), "{agg}");
        assert_eq!(agg["options"], json!([]), "{agg}");

        let row = feed_row(&app, &s, latte).await;
        assert!(
            !group_ids(&row).is_empty(),
            "round {round}: a row with no group makes a till offer every add-on: {row}"
        );
        assert_eq!(
            option_ids(&row),
            Vec::<String>::new(),
            "round {round}: {row}"
        );
        let (st, sync) = http(
            &app,
            "GET",
            &format!("/catalog/sync?branch_id={}", s.branch),
            &s.token,
            None,
        )
        .await;
        assert_eq!(st, 200, "{sync}");
        let listed = sync["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == latte.to_string())
            .unwrap();
        assert!(!group_ids(listed).is_empty(), "{listed}");
        assert!(option_ids(listed).is_empty(), "{listed}");
    }
    // Detaching twice keeps ONE empty Options group, not one per save.
    assert_eq!(attached(&pool, latte).await.len(), 1);
    assert_eq!(legacy_links(&pool, latte).await, (0, 0, 0));
}

/// A sale rung on a till before the detach reaches the server after it (the
/// offline queue, `/sync/replay`): it still prices, add-ons from the org
/// catalogue and the item's own optionals alike. `create_order_inner` prices
/// every line through `resolve_loaded` over the same catalogue.
#[sqlx::test]
async fn a_sale_rung_before_the_detach_still_prices_after_it(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let latte = item(&pool, &s, "Latte").await;
    let (typed, shot) = group(
        &app,
        &s,
        json!({"name": "Extras", "selection_type": "multi", "legacy_addon_type": "extra"}),
        "Extra shot",
        700,
    )
    .await;
    assert_eq!(put_groups(&app, &s, latte, set(&[typed])).await.0, 200);
    let (st, opts) = http(
        &app,
        "PUT",
        &format!("/menu-items/{latte}/options"),
        &s.token,
        Some(json!({"options": [{"name": "Honey", "price": 300, "is_active": true}]})),
    )
    .await;
    assert_eq!(st, 200, "{opts}");
    let honey: Uuid = opts[0]["id"].as_str().unwrap().parse().unwrap();

    assert_eq!(
        put_groups(&app, &s, latte, json!({"groups": []})).await.0,
        200
    );

    let line = resolve_menu_item_configuration(
        &pool,
        latte,
        None,
        1,
        &[AddonInput {
            addon_item_id: shot,
            quantity: 1,
            unit_price: None,
        }],
        &[honey],
        s.branch,
    )
    .await
    .expect("the queued sale still prices");
    assert_eq!(line.addon_line, 700);
}

#[sqlx::test]
async fn another_items_options_are_never_attached_as_a_group(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let latte = item(&pool, &s, "Latte").await;
    let mocha = item(&pool, &s, "Mocha").await;
    let (st, _) = http(
        &app,
        "PUT",
        &format!("/menu-items/{latte}/options"),
        &s.token,
        Some(json!({"options": [{"name": "Honey", "price": 300, "is_active": true}]})),
    )
    .await;
    assert_eq!(st, 200);
    let own = options_group(&pool, latte).await.unwrap();
    let (typed, _) = group(
        &app,
        &s,
        json!({"name": "Extras", "selection_type": "multi", "legacy_addon_type": "extra"}),
        "Extra shot",
        700,
    )
    .await;
    let (custom, _) = group(
        &app,
        &s,
        json!({"name": "Toppings", "selection_type": "multi"}),
        "Drizzle",
        500,
    )
    .await;

    // The group library says which is which (the editor's picker hides these).
    let (st, list) = http(
        &app,
        "GET",
        &format!("/modifier-groups?org_id={}", s.org),
        &s.token,
        None,
    )
    .await;
    assert_eq!(st, 200, "{list}");
    let flag = |id: Uuid| {
        list.as_array()
            .unwrap()
            .iter()
            .find(|g| g["id"] == id.to_string())
            .map(|g| g["is_item_options"].clone())
    };
    assert_eq!(flag(own), Some(json!(true)));
    assert_eq!(flag(typed), Some(json!(false)));
    assert_eq!(flag(custom), Some(json!(false)));

    let (st, err) = put_groups(&app, &s, mocha, set(&[typed, own])).await;
    assert_eq!(st, 400, "{err}");
    assert!(attached(&pool, mocha).await.is_empty(), "refused whole");

    // Listing the item's OWN Options group in its set is a no-op, not a conflict.
    let (st, agg) = put_groups(&app, &s, latte, set(&[own, typed])).await;
    assert_eq!(st, 200, "{agg}");
    assert_eq!(studio_group_ids(&agg), vec![typed.to_string()]);
    assert_eq!(agg["options"][0]["name"], "Honey");
}
