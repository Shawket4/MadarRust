//! Menu modeling phases 4–5: recipe bases, packaging rules, per-size option amounts,
//! linked copies. Everything expands into plain `recipe_lines`, so most assertions
//! read that table directly.

use actix_web::{App, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::menu::routes;
use crate::models::UserRole;

struct Org {
    org: Uuid,
    branch: Uuid,
    token: String,
    cat: Uuid,
}

async fn setup(pool: &PgPool) -> Org {
    let org: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug) VALUES ('F Org', $1) RETURNING id",
    )
    .bind(format!("f-{}", Uuid::new_v4()))
    .fetch_one(pool)
    .await
    .unwrap();
    let branch: Uuid = sqlx::query_scalar(
        "INSERT INTO branches (org_id, name, code) VALUES ($1, 'F', 'FFF') RETURNING id",
    )
    .bind(org)
    .fetch_one(pool)
    .await
    .unwrap();
    let user: Uuid = sqlx::query_scalar(
        "INSERT INTO users (org_id, name, email, password_hash, role) \
         VALUES ($1, 'Owner', $2, 'x', 'org_admin'::user_role) RETURNING id",
    )
    .bind(org)
    .bind(format!("{}@f.test", Uuid::new_v4()))
    .fetch_one(pool)
    .await
    .unwrap();
    for action in ["create", "read", "update", "delete"] {
        sqlx::query(
            "INSERT INTO role_permissions (role, resource, action, granted) \
             VALUES ('org_admin'::user_role, 'menu_items'::permission_resource, $1::permission_action, true) \
             ON CONFLICT DO NOTHING",
        )
        .bind(action)
        .execute(pool)
        .await
        .unwrap();
    }
    let cat = category(pool, org, "Iced coffee").await;
    let token = crate::auth::jwt::create_token(
        &JwtSecret("secret".into()),
        user,
        Some(org),
        UserRole::OrgAdmin,
        None,
        24,
    )
    .unwrap();
    Org {
        org,
        branch,
        token,
        cat,
    }
}

async fn category(pool: &PgPool, org: Uuid, name: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO categories (org_id, name) VALUES ($1, $2) RETURNING id")
        .bind(org)
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn ingredient(pool: &PgPool, org: Uuid, name: &str, unit: &str, slug: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO org_ingredients (org_id, name, unit, category_id, cost_per_unit) \
         VALUES ($1, $2, $3::inventory_unit, ingredient_category_id($1, $4), 1) RETURNING id",
    )
    .bind(org)
    .bind(name)
    .bind(unit)
    .bind(slug)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn item(
    pool: &PgPool,
    org: Uuid,
    cat: Uuid,
    name: &str,
    labels: &[&str],
) -> (Uuid, Vec<Uuid>) {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO menu_items (org_id, category_id, name, base_price) VALUES ($1, $2, $3, 100) RETURNING id",
    )
    .bind(org)
    .bind(cat)
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap();
    let mut sizes = Vec::new();
    for (i, l) in labels.iter().enumerate() {
        sizes.push(
            sqlx::query_scalar(
                "INSERT INTO menu_item_sizes (menu_item_id, label, price, sort) VALUES ($1, $2, 100, $3) RETURNING id",
            )
            .bind(id)
            .bind(l)
            .bind(i as i32)
            .fetch_one(pool)
            .await
            .unwrap(),
        );
    }
    (id, sizes)
}

/// (ingredient, quantity as text, source) of a size, ordered by ingredient.
async fn lines(pool: &PgPool, size: Uuid) -> Vec<(Uuid, String, String)> {
    sqlx::query_as(
        "SELECT ingredient_id, (quantity::float8)::text, COALESCE(source, 'own') FROM recipe_lines \
         WHERE owner_type = 'item_size' AND owner_id = $1 ORDER BY ingredient_id",
    )
    .bind(size)
    .fetch_all(pool)
    .await
    .unwrap()
}

fn qty_of(ls: &[(Uuid, String, String)], ing: Uuid) -> Option<(String, String)> {
    ls.iter()
        .find(|l| l.0 == ing)
        .map(|l| (l.1.clone(), l.2.clone()))
}

macro_rules! call {
    ($pool:expr, $token:expr, $method:ident, $uri:expr) => {
        call!($pool, $token, $method, $uri, Value::Null)
    };
    ($pool:expr, $token:expr, $method:ident, $uri:expr, $body:expr) => {{
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(JwtSecret("secret".into())))
                .configure(routes::configure),
        )
        .await;
        let mut r = test::TestRequest::$method()
            .uri(&$uri)
            .insert_header(("Authorization", format!("Bearer {}", $token)));
        let b: Value = $body;
        if !b.is_null() {
            r = r.set_json(&b);
        }
        let resp = test::call_service(&app, r.to_request()).await;
        let status = resp.status().as_u16();
        let raw = test::read_body(resp).await;
        let v: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
        (status, v)
    }};
}

// ── B7 bases ─────────────────────────────────────────────────────────

#[sqlx::test]
async fn a_base_expands_by_size_label_and_re_expands_in_place(pool: PgPool) {
    let o = setup(&pool).await;
    let honey = ingredient(&pool, o.org, "Honey", "g", "general").await;
    let milk = ingredient(&pool, o.org, "Milk", "g", "milk").await;
    let (it, sizes) = item(
        &pool,
        o.org,
        o.cat,
        "Blended pistachio matcha",
        &["Cup", "Can"],
    )
    .await;
    let (cup, can) = (sizes[0], sizes[1]);

    let (st, base) = call!(
        pool,
        o.token,
        post,
        "/recipe-bases".to_string(),
        json!({
            "name": "Blended matcha",
            "lines": [
                {"ingredient_id": honey, "quantity": 10, "unit": "g"},
                {"ingredient_id": milk, "quantity": 90, "unit": "g", "size_label": "Cup"},
                {"ingredient_id": milk, "quantity": 110, "unit": "g", "size_label": "Can"},
                {"ingredient_id": milk, "quantity": 50, "unit": "g"}
            ]
        })
    );
    assert_eq!(st, 201, "{base}");
    let base_id = base["id"].as_str().unwrap().to_string();
    assert_eq!(base["lines"].as_array().unwrap().len(), 4);

    for s in [cup, can] {
        let (st, r) = call!(
            pool,
            o.token,
            put,
            format!("/menu-item-sizes/{s}/base"),
            json!({"base_id": base_id})
        );
        assert_eq!(st, 200, "{r}");
    }
    let cup_l = lines(&pool, cup).await;
    let can_l = lines(&pool, can).await;
    assert_eq!(qty_of(&cup_l, honey), Some(("10".into(), "base".into())));
    assert_eq!(
        qty_of(&cup_l, milk),
        Some(("90".into(), "base".into())),
        "labelled line beats the NULL line"
    );
    assert_eq!(qty_of(&can_l, milk), Some(("110".into(), "base".into())));
    assert_eq!(cup_l.len(), 2);

    let milk_row_before: Uuid = sqlx::query_scalar(
        "SELECT id FROM recipe_lines WHERE owner_id = $1 AND ingredient_id = $2",
    )
    .bind(cup)
    .bind(milk)
    .fetch_one(&pool)
    .await
    .unwrap();

    // Re-expansion: honey 10 → 12 on every size; unchanged milk keeps its row id.
    let (st, r) = call!(
        pool,
        o.token,
        put,
        format!("/recipe-bases/{base_id}/lines"),
        json!({"lines": [
            {"ingredient_id": honey, "quantity": 12, "unit": "g"},
            {"ingredient_id": milk, "quantity": 90, "unit": "g", "size_label": "Cup"},
            {"ingredient_id": milk, "quantity": 110, "unit": "g", "size_label": "Can"}
        ]})
    );
    assert_eq!(st, 200, "{r}");
    assert_eq!(r["sizes_changed"], 2);
    assert_eq!(
        qty_of(&lines(&pool, cup).await, honey),
        Some(("12".into(), "base".into()))
    );
    assert_eq!(
        qty_of(&lines(&pool, can).await, honey),
        Some(("12".into(), "base".into()))
    );
    let milk_row_after: Uuid = sqlx::query_scalar(
        "SELECT id FROM recipe_lines WHERE owner_id = $1 AND ingredient_id = $2",
    )
    .bind(cup)
    .bind(milk)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        milk_row_before, milk_row_after,
        "an unchanged line is not rewritten"
    );

    let (st, usage) = call!(pool, o.token, get, format!("/recipe-bases/{base_id}/usage"));
    assert_eq!(st, 200);
    assert_eq!(usage["size_count"], 2);
    assert_eq!(usage["item_count"], 1);
    assert_eq!(usage["sizes"][0]["menu_item_id"], json!(it));

    // Deactivating removes the expansion; detaching a size removes it too.
    let (st, _) = call!(
        pool,
        o.token,
        patch,
        format!("/recipe-bases/{base_id}"),
        json!({"is_active": false})
    );
    assert_eq!(st, 200);
    assert!(lines(&pool, cup).await.is_empty());
    let (st, _) = call!(
        pool,
        o.token,
        patch,
        format!("/recipe-bases/{base_id}"),
        json!({"is_active": true})
    );
    assert_eq!(st, 200);
    assert_eq!(lines(&pool, can).await.len(), 2);
    let (st, _) = call!(pool, o.token, delete, format!("/recipe-bases/{base_id}"));
    assert_eq!(st, 204);
    assert!(lines(&pool, can).await.is_empty());
    let base_ptr: Option<Uuid> =
        sqlx::query_scalar("SELECT base_id FROM menu_item_sizes WHERE id = $1")
            .bind(can)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(base_ptr.is_none());
    // Soft delete frees the name.
    let (st, _) = call!(
        pool,
        o.token,
        post,
        "/recipe-bases".to_string(),
        json!({"name": "Blended matcha"})
    );
    assert_eq!(st, 201);
}

#[sqlx::test]
async fn saving_own_lines_keeps_base_rows_and_own_overrides_base(pool: PgPool) {
    let o = setup(&pool).await;
    let honey = ingredient(&pool, o.org, "Honey", "g", "general").await;
    let matcha = ingredient(&pool, o.org, "Matcha", "g", "general").await;
    let pistachio = ingredient(&pool, o.org, "Pistachio sauce", "g", "general").await;
    let (_it, sizes) = item(&pool, o.org, o.cat, "Pistachio matcha", &["Cup"]).await;
    let cup = sizes[0];

    let (_, base) = call!(
        pool,
        o.token,
        post,
        "/recipe-bases".to_string(),
        json!({
            "name": "Matcha base",
            "lines": [{"ingredient_id": honey, "quantity": 10, "unit": "g"},
                      {"ingredient_id": matcha, "quantity": 3, "unit": "g"}]
        })
    );
    let base_id = base["id"].as_str().unwrap().to_string();
    call!(
        pool,
        o.token,
        put,
        format!("/menu-item-sizes/{cup}/base"),
        json!({"base_id": base_id})
    );

    let (st, r) = call!(
        pool,
        o.token,
        put,
        format!("/menu-item-sizes/{cup}/recipe"),
        json!({"lines": [{"ingredient_id": pistachio, "quantity": 40, "unit": "g"}]})
    );
    assert_eq!(st, 200, "{r}");
    let l = lines(&pool, cup).await;
    assert_eq!(l.len(), 3, "own save preserved the base rows: {l:?}");
    assert_eq!(qty_of(&l, pistachio), Some(("40".into(), "own".into())));
    assert_eq!(qty_of(&l, honey), Some(("10".into(), "base".into())));
    let sources: Vec<String> = r["recipe"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["source"].as_str().unwrap().to_string())
        .collect();
    assert!(sources.contains(&"base".to_string()) && sources.contains(&"own".to_string()));

    // An own line for a base ingredient wins…
    let (st, _) = call!(
        pool,
        o.token,
        put,
        format!("/menu-item-sizes/{cup}/recipe"),
        json!({"lines": [
            {"ingredient_id": pistachio, "quantity": 40, "unit": "g"},
            {"ingredient_id": honey, "quantity": 15, "unit": "g"}
        ]})
    );
    assert_eq!(st, 200);
    let l = lines(&pool, cup).await;
    assert_eq!(qty_of(&l, honey), Some(("15".into(), "own".into())));
    assert_eq!(l.len(), 3);
    // …and dropping it brings the base line back.
    call!(
        pool,
        o.token,
        put,
        format!("/menu-item-sizes/{cup}/recipe"),
        json!({"lines": [{"ingredient_id": pistachio, "quantity": 40, "unit": "g"}]})
    );
    assert_eq!(
        qty_of(&lines(&pool, cup).await, honey),
        Some(("10".into(), "base".into()))
    );

    // The studio shows the base pointer and each line's source.
    let (st, agg) = call!(pool, o.token, get, format!("/menu-items/{_it}/studio"));
    assert_eq!(st, 200);
    assert_eq!(agg["sizes"][0]["base_id"], json!(base_id));
}

// ── B8 packaging rules ───────────────────────────────────────────────

#[sqlx::test]
async fn the_most_specific_packaging_rule_wins(pool: PgPool) {
    let o = setup(&pool).await;
    let hot = category(&pool, o.org, "Hot coffee").await;
    let cup16 = ingredient(&pool, o.org, "Cup 16oz", "pcs", "packaging").await;
    let lid16 = ingredient(&pool, o.org, "Lid 16oz", "pcs", "packaging").await;
    let straw = ingredient(&pool, o.org, "Straw", "pcs", "packaging").await;
    let can = ingredient(&pool, o.org, "Can", "pcs", "packaging").await;
    let cup12 = ingredient(&pool, o.org, "Cup 12oz", "pcs", "packaging").await;
    let cup4 = ingredient(&pool, o.org, "Cup 4oz", "pcs", "packaging").await;

    let (_latte, iced_sizes) = item(&pool, o.org, o.cat, "Iced latte", &["Cup", "Can"]).await;
    let (v60, v60_sizes) = item(&pool, o.org, o.cat, "Iced V60", &["Cup"]).await;
    let (_hot, hot_sizes) = item(&pool, o.org, hot, "Espresso", &["one_size", "Can"]).await;
    let p = |i: Uuid| json!({"ingredient_id": i, "quantity": 1, "unit": "pcs"});

    let rules = [
        json!({"name": "Iced", "match_category_id": o.cat, "lines": [p(cup16), p(lid16), p(straw)]}),
        json!({"name": "Iced can", "match_category_id": o.cat, "match_size_label": "Can", "lines": [p(can), p(straw)]}),
        json!({"name": "Any can", "match_size_label": "Can", "lines": [p(straw)]}),
        json!({"name": "V60", "match_item_id": v60, "lines": [p(cup12), p(lid16)]}),
        json!({"name": "Hot", "match_category_id": hot, "sort": 5, "lines": [p(cup4)]}),
    ];
    for r in rules {
        let (st, v) = call!(pool, o.token, post, "/packaging-rules".to_string(), r);
        assert_eq!(st, 201, "{v}");
    }
    let (st, bad) = call!(
        pool,
        o.token,
        post,
        "/packaging-rules".to_string(),
        json!({"name": "x", "lines": []})
    );
    assert_eq!(st, 400, "{bad}");
    // Rules are stored, not applied, until /apply.
    assert!(lines(&pool, iced_sizes[0]).await.is_empty());

    let (st, res) = call!(pool, o.token, post, "/packaging-rules/apply".to_string());
    assert_eq!(st, 200, "{res}");
    assert_eq!(res["sizes_seen"], 5);
    assert_eq!(res["sizes_changed"], 5);
    assert_eq!(res["sizes_with_manual_packaging"], 0);

    let ids = |ls: Vec<(Uuid, String, String)>| {
        let mut v: Vec<Uuid> = ls
            .into_iter()
            .map(|l| {
                assert_eq!(l.2, "rule");
                l.0
            })
            .collect();
        v.sort();
        v
    };
    let sorted = |mut v: Vec<Uuid>| {
        v.sort();
        v
    };
    assert_eq!(
        ids(lines(&pool, iced_sizes[0]).await),
        sorted(vec![cup16, lid16, straw]),
        "category"
    );
    assert_eq!(
        ids(lines(&pool, iced_sizes[1]).await),
        sorted(vec![can, straw]),
        "category + label beats category and label"
    );
    assert_eq!(
        ids(lines(&pool, v60_sizes[0]).await),
        sorted(vec![cup12, lid16]),
        "item beats category"
    );
    assert_eq!(ids(lines(&pool, hot_sizes[0]).await), vec![cup4]);
    assert_eq!(
        ids(lines(&pool, hot_sizes[1]).await),
        sorted(vec![cup4]),
        "category beats label"
    );

    // Re-apply is a no-op; editing a rule then applying replaces only rule rows.
    let (_, res) = call!(pool, o.token, post, "/packaging-rules/apply".to_string());
    assert_eq!(res["sizes_changed"], 0);
    let (_, list) = call!(pool, o.token, get, "/packaging-rules".to_string());
    let v60_rule = list
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "V60")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (st, _) = call!(
        pool,
        o.token,
        patch,
        format!("/packaging-rules/{v60_rule}"),
        json!({"is_active": false})
    );
    assert_eq!(st, 200);
    call!(pool, o.token, post, "/packaging-rules/apply".to_string());
    assert_eq!(
        ids(lines(&pool, v60_sizes[0]).await),
        sorted(vec![cup16, lid16, straw])
    );
}

#[sqlx::test]
async fn dine_in_skips_categories_flagged_as_packaging(pool: PgPool) {
    let o = setup(&pool).await;
    let legacy = ingredient(&pool, o.org, "Lid", "pcs", "packaging").await;
    let flagged = ingredient(&pool, o.org, "Paper bag", "pcs", "takeaway_bags").await;
    let milk = ingredient(&pool, o.org, "Milk", "g", "milk").await;
    let backfilled: bool = sqlx::query_scalar(
        "SELECT is_packaging FROM ingredient_categories WHERE org_id = $1 AND slug = 'packaging'",
    )
    .bind(o.org)
    .fetch_one(&pool)
    .await
    .unwrap();
    let _ = backfilled; // created after the backfill: the slug fallback covers it.

    let set = crate::orders::handlers::packaging_ingredient_ids(&pool, o.org)
        .await
        .unwrap();
    assert!(set.contains(&legacy) && !set.contains(&flagged) && !set.contains(&milk));

    sqlx::query("UPDATE ingredient_categories SET is_packaging = true WHERE org_id = $1 AND slug = 'takeaway_bags'")
        .bind(o.org)
        .execute(&pool)
        .await
        .unwrap();
    let set = crate::orders::handlers::packaging_ingredient_ids(&pool, o.org)
        .await
        .unwrap();
    assert!(set.contains(&legacy) && set.contains(&flagged) && !set.contains(&milk));
    let _ = o.branch;
}

// ── B9 per-size option amounts ───────────────────────────────────────

#[::core::prelude::v1::test]
fn sized_option_lines_replace_generic_ones_per_ingredient() {
    use crate::orders::component_resolve::merge_sized_option_lines;
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();
    let g = vec![
        (Some(a), 25.0, "Sauce".to_string(), "g".to_string()),
        (Some(b), 15.0, "Syrup".to_string(), "g".to_string()),
    ];
    let s = vec![
        (Some(a), 40.0, "Sauce".to_string(), "g".to_string()),
        (Some(c), 1.0, "Straw".to_string(), "pcs".to_string()),
    ];
    let m = merge_sized_option_lines(g.clone(), s);
    assert_eq!(m.len(), 3);
    assert_eq!(m[0].1, 40.0);
    assert_eq!(m[1].1, 15.0);
    assert_eq!(m[2].0, Some(c));
    assert_eq!(merge_sized_option_lines(g.clone(), vec![]).len(), 2);
}

#[sqlx::test]
async fn the_resolver_prefers_the_sized_option_amount_and_the_shim_hides_it(pool: PgPool) {
    let o = setup(&pool).await;
    sqlx::raw_sql(include_str!("../../deploy/menu_unification_shim.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let sauce = ingredient(&pool, o.org, "Mango sauce", "g", "general").await;
    let (it, _sizes) = item(&pool, o.org, o.cat, "Mango mojito", &["Cup", "Can"]).await;
    let group: Uuid = sqlx::query_scalar(
        "INSERT INTO modifier_groups (org_id, name, legacy_addon_type) VALUES ($1, 'Flavour', 'flavour') RETURNING id",
    )
    .bind(o.org)
    .fetch_one(&pool)
    .await
    .unwrap();
    let opt: Uuid = sqlx::query_scalar(
        "INSERT INTO modifier_options (group_id, name, price, legacy_source) VALUES ($1, 'Mango', 0, 'addon') RETURNING id",
    )
    .bind(group)
    .fetch_one(&pool)
    .await
    .unwrap();

    let (st, r) = call!(
        pool,
        o.token,
        put,
        format!("/modifier-options/{opt}/recipe"),
        json!([
            {"ingredient_id": sauce, "quantity": 25, "unit": "g"},
            {"ingredient_id": sauce, "quantity": 40, "unit": "g", "size_label": "Can"}
        ])
    );
    assert_eq!(st, 200, "{r}");
    assert_eq!(r.as_array().unwrap().len(), 2);
    assert_eq!(r[1]["size_label"], "Can");

    let legacy_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM addon_item_ingredients WHERE addon_item_id = $1")
            .bind(opt)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(legacy_rows, 1, "old tills see only the generic amount");

    let addon = crate::orders::component_resolve::AddonInput {
        addon_item_id: opt,
        quantity: 1,
        unit_price: None,
    };
    let qty = |res: crate::orders::component_resolve::MenuItemResolution| {
        res.deductions
            .iter()
            .filter(|d| d.org_ingredient_id == Some(sauce))
            .map(|d| d.quantity)
            .sum::<f64>()
    };
    let can = crate::orders::component_resolve::resolve_menu_item_configuration(
        &pool,
        it,
        Some("Can".into()),
        1,
        std::slice::from_ref(&addon),
        &[],
        o.branch,
    )
    .await
    .unwrap();
    assert_eq!(qty(can), 40.0);
    let cup = crate::orders::component_resolve::resolve_menu_item_configuration(
        &pool,
        it,
        Some("Cup".into()),
        2,
        std::slice::from_ref(&addon),
        &[],
        o.branch,
    )
    .await
    .unwrap();
    assert_eq!(qty(cup), 50.0, "no Cup line → the generic 25 g × 2");
}

// ── B10 linked copies ────────────────────────────────────────────────

#[sqlx::test]
async fn a_linked_copy_follows_its_source_until_unlinked(pool: PgPool) {
    let o = setup(&pool).await;
    let staff = category(&pool, o.org, "Staff drinks").await;
    let beans = ingredient(&pool, o.org, "House blend", "g", "coffee_bean").await;
    let milk = ingredient(&pool, o.org, "Milk", "g", "milk").await;
    let straw = ingredient(&pool, o.org, "Straw", "pcs", "packaging").await;
    let (src, src_sizes) = item(&pool, o.org, o.cat, "Iced latte", &["Cup", "Can"]).await;
    call!(
        pool,
        o.token,
        put,
        format!("/menu-item-sizes/{}/recipe", src_sizes[0]),
        json!({"lines": [
            {"ingredient_id": beans, "quantity": 18, "unit": "g"},
            {"ingredient_id": milk, "quantity": 180, "unit": "g"}
        ]})
    );
    call!(
        pool,
        o.token,
        post,
        "/packaging-rules".to_string(),
        json!({
        "name": "Can", "match_size_label": "Can",
        "lines": [{"ingredient_id": straw, "quantity": 1, "unit": "pcs"}]})
    );
    call!(
        pool,
        o.token,
        put,
        format!("/menu-item-sizes/{}/recipe", src_sizes[1]),
        json!({"lines": [
            {"ingredient_id": beans, "quantity": 18, "unit": "g"},
            {"ingredient_id": milk, "quantity": 250, "unit": "g"}
        ]})
    );
    assert_eq!(
        lines(&pool, src_sizes[1]).await.len(),
        3,
        "rule applied on save"
    );

    let (st, r) = call!(
        pool,
        o.token,
        post,
        format!("/menu-items/{src}/linked-copy"),
        json!({"name": "Iced latte staff", "price": 0, "category_id": staff})
    );
    assert_eq!(st, 201, "{r}");
    let copy: Uuid = r["menu_item_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(r["link"]["recipe_source_item_id"], json!(src));
    assert_eq!(r["link"]["in_sync"], true);
    let copy_sizes: Vec<(Uuid, String, i32)> = sqlx::query_as(
        "SELECT id, label, price FROM menu_item_sizes WHERE menu_item_id = $1 ORDER BY sort",
    )
    .bind(copy)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        copy_sizes.iter().map(|s| s.1.as_str()).collect::<Vec<_>>(),
        vec!["Cup", "Can"]
    );
    assert!(copy_sizes.iter().all(|s| s.2 == 0));
    let groups: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM menu_item_modifier_groups WHERE menu_item_id = $1",
    )
    .bind(copy)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(groups, 0);
    let can_copy = lines(&pool, copy_sizes[1].0).await;
    assert_eq!(can_copy.len(), 3);
    assert!(can_copy.iter().all(|l| l.2 == "linked"));
    assert_eq!(qty_of(&can_copy, milk).unwrap().0, "250");

    // A save on the source propagates by label.
    call!(
        pool,
        o.token,
        put,
        format!("/menu-item-sizes/{}/recipe", src_sizes[0]),
        json!({"lines": [
            {"ingredient_id": beans, "quantity": 20, "unit": "g"},
            {"ingredient_id": milk, "quantity": 180, "unit": "g"}
        ]})
    );
    assert_eq!(
        qty_of(&lines(&pool, copy_sizes[0].0).await, beans).unwrap(),
        ("20".into(), "linked".into())
    );

    // The source's studio lists the copy; the copy can't be edited directly.
    let (_, agg) = call!(pool, o.token, get, format!("/menu-items/{src}/studio"));
    assert_eq!(agg["linked_copy_ids"], json!([copy]));
    let (st, _) = call!(
        pool,
        o.token,
        put,
        format!("/menu-item-sizes/{}/recipe", copy_sizes[0].0),
        json!({"lines": []})
    );
    assert_eq!(st, 409);

    // A copy of the copy follows the root.
    let (_, r2) = call!(
        pool,
        o.token,
        post,
        format!("/menu-items/{copy}/linked-copy"),
        json!({"name": "Loyalty iced latte", "price": 0})
    );
    assert_eq!(r2["link"]["recipe_source_item_id"], json!(src));

    // Unlink: lines become own and stop following.
    let (st, u) = call!(
        pool,
        o.token,
        delete,
        format!("/menu-items/{copy}/recipe-link")
    );
    assert_eq!(st, 200, "{u}");
    assert!(u["recipe_source_item_id"].is_null());
    assert!(
        lines(&pool, copy_sizes[0].0)
            .await
            .iter()
            .all(|l| l.2 == "own")
    );
    call!(
        pool,
        o.token,
        put,
        format!("/menu-item-sizes/{}/recipe", src_sizes[0]),
        json!({"lines": [
            {"ingredient_id": beans, "quantity": 22, "unit": "g"},
            {"ingredient_id": milk, "quantity": 180, "unit": "g"}
        ]})
    );
    assert_eq!(
        qty_of(&lines(&pool, copy_sizes[0].0).await, beans)
            .unwrap()
            .0,
        "20"
    );
}

// ── Permissions coverage (stream P) ──────────────────────────────────

async fn staff_token(pool: &PgPool, org: Uuid, role: &str, kind: UserRole) -> String {
    let user: Uuid = sqlx::query_scalar(
        "INSERT INTO users (org_id, name, email, password_hash, role) \
         VALUES ($1, $2, $3, 'x', $2::user_role) RETURNING id",
    )
    .bind(org)
    .bind(role)
    .bind(format!("{}@p.test", Uuid::new_v4()))
    .fetch_one(pool)
    .await
    .unwrap();
    crate::auth::jwt::create_token(&JwtSecret("secret".into()), user, Some(org), kind, None, 24)
        .unwrap()
}

/// Apply is `menu.packaging_rules.apply` (owner-only); base line edits are
/// `menu.items.edit` and linked copies `menu.items.create` (both owner-only by
/// default). A teller and a branch manager are refused all three; reading the
/// rules is `menu.items.read`, core for tellers and managers.
#[sqlx::test]
async fn modeling_writes_are_refused_to_tellers_and_managers(pool: PgPool) {
    let o = setup(&pool).await;
    let (src, _) = item(&pool, o.org, o.cat, "Latte", &["Cup"]).await;
    let base = Uuid::new_v4();
    let teller = staff_token(&pool, o.org, "teller", UserRole::Teller).await;
    let manager = staff_token(&pool, o.org, "branch_manager", UserRole::BranchManager).await;

    for (who, t) in [("teller", &teller), ("manager", &manager)] {
        let (st, r) = call!(pool, t, post, "/packaging-rules/apply".to_string());
        assert_eq!(st, 403, "{who} apply: {r}");
        let (st, r) = call!(
            pool,
            t,
            put,
            format!("/recipe-bases/{base}/lines"),
            json!({"lines": []})
        );
        assert_eq!(st, 403, "{who} base lines: {r}");
        let (st, r) = call!(
            pool,
            t,
            post,
            format!("/menu-items/{src}/linked-copy"),
            json!({"name": "Copy", "price": 0, "category_id": o.cat})
        );
        assert_eq!(st, 403, "{who} linked copy: {r}");
        let (st, r) = call!(pool, t, get, "/packaging-rules".to_string());
        assert_eq!(st, 200, "{who} reads the rules: {r}");
    }

    let (st, r) = call!(pool, o.token, post, "/packaging-rules/apply".to_string());
    assert_eq!(st, 200, "the owner applies: {r}");
}
