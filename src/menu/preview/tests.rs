//! B5 dry-run preview. The resolver reads the legacy relations (tables in the test
//! schema), the swap defaults read the unified ones, so the fixture writes both, the
//! way the orders resolver tests do.

#![allow(clippy::too_many_arguments)]

use actix_web::{App, test, web};
use sqlx::PgPool;
use uuid::Uuid;

use super::{PreviewDeduction, PreviewRequest, PreviewResponse, preview};
use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;

struct Fx {
    org: Uuid,
    item: Uuid,
    full_cream: Uuid,
    oat: Uuid,
    house: Uuid,
    decaf: Uuid,
    can: Uuid,
    straw: Uuid,
    milk_group: Uuid,
    bean_group: Uuid,
    opt_full: Uuid,
    opt_oat: Uuid,
    opt_almond: Uuid,
    opt_house: Uuid,
    opt_decaf: Uuid,
    opt_shot: Uuid,
}

async fn ingredient(
    pool: &PgPool,
    org: Uuid,
    name: &str,
    unit: &str,
    slug: &str,
    cost: i32,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO org_ingredients (id, org_id, name, unit, cost_per_unit, category_id) \
         VALUES ($1, $2, $3, $4::inventory_unit, $5, ingredient_category_id($2, $6))",
    )
    .bind(id)
    .bind(org)
    .bind(name)
    .bind(unit)
    .bind(cost)
    .bind(slug)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn group(pool: &PgPool, org: Uuid, name: &str, ty: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO modifier_groups (org_id, name, legacy_addon_type) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(org)
    .bind(name)
    .bind(ty)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A legacy addon + its unified option (same id), one ingredient line on both.
async fn option(
    pool: &PgPool,
    org: Uuid,
    group: Uuid,
    name: &str,
    ty: &str,
    price: i32,
    sort: i32,
    ing: (Uuid, &str, f64, &str),
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO addon_items (id, org_id, name, type, default_price) VALUES ($1, $2, $3, $4, $5)")
        .bind(id).bind(org).bind(name).bind(ty).bind(price).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO addon_item_ingredients (addon_item_id, org_ingredient_id, quantity_used, ingredient_name, ingredient_unit) VALUES ($1, $2, $3, $4, $5)")
        .bind(id).bind(ing.0).bind(ing.2).bind(ing.1).bind(ing.3).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO modifier_options (id, group_id, name, price, sort) VALUES ($1, $2, $3, $4, $5)")
        .bind(id).bind(group).bind(name).bind(price).bind(sort).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit) VALUES ('modifier_option', $1, $2, $3::numeric, $4)")
        .bind(id).bind(ing.0).bind(ing.2.to_string()).bind(ing.3).execute(pool).await.unwrap();
    id
}

async fn fixture(pool: &PgPool) -> Fx {
    let org = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Drops', $2)")
        .bind(org)
        .bind(format!("prev-{org}"))
        .execute(pool)
        .await
        .unwrap();
    let cat: Uuid = sqlx::query_scalar(
        "INSERT INTO categories (org_id, name) VALUES ($1, 'Coffee') RETURNING id",
    )
    .bind(org)
    .fetch_one(pool)
    .await
    .unwrap();
    let item: Uuid = sqlx::query_scalar("INSERT INTO menu_items (org_id, category_id, name, base_price) VALUES ($1, $2, 'Iced latte', 100) RETURNING id")
        .bind(org).bind(cat).fetch_one(pool).await.unwrap();

    let full_cream = ingredient(pool, org, "Full cream milk", "g", "milk", 1).await;
    let oat = ingredient(pool, org, "Oat milk", "g", "milk", 2).await;
    let almond = ingredient(pool, org, "Almond milk", "ml", "milk", 2).await;
    let house = ingredient(pool, org, "House beans", "g", "coffee_bean", 2).await;
    let decaf = ingredient(pool, org, "Decaf beans", "g", "coffee_bean", 3).await;
    let can = ingredient(pool, org, "Can", "pcs", "packaging", 5).await;
    let straw = ingredient(pool, org, "Straw", "pcs", "packaging", 1).await;

    // Sizes: Cup (sort 0) and Can (sort 1, the one we preview).
    //
    // Written ONCE, to `menu_item_sizes`. `item_sizes` is the old-client view
    // over that same table now, so writing both would collide on
    // (menu_item_id, label). Replacing the whole set in one transaction is also
    // what the editor does, and it is what clears the `one_size` row the item
    // was born with — leaving it would make this a three-size item.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("DELETE FROM menu_item_sizes WHERE menu_item_id = $1")
        .bind(item)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO menu_item_sizes (menu_item_id, label, price, sort) VALUES ($1, 'Cup', 150, 0)",
    )
    .bind(item)
    .execute(&mut *tx)
    .await
    .unwrap();
    let can_size: Uuid = sqlx::query_scalar("INSERT INTO menu_item_sizes (menu_item_id, label, price, sort) VALUES ($1, 'Can', 170, 1) RETURNING id")
        .bind(item).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    for (ing, name, qty, unit) in [
        (full_cream, "Full cream milk", 250.0, "g"),
        (house, "House beans", 18.0, "g"),
        (can, "Can", 1.0, "pcs"),
        (straw, "Straw", 1.0, "pcs"),
    ] {
        sqlx::query("INSERT INTO menu_item_recipes (menu_item_id, org_ingredient_id, quantity_used, size_label, ingredient_name, ingredient_unit) VALUES ($1, $2, $3, 'Can', $4, $5)")
            .bind(item).bind(ing).bind(qty).bind(name).bind(unit).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit) VALUES ('item_size', $1, $2, $3::numeric, $4)")
            .bind(can_size).bind(ing).bind(qty.to_string()).bind(unit).execute(pool).await.unwrap();
    }

    let milk_group = group(pool, org, "Milk", "milk_type").await;
    let bean_group = group(pool, org, "Beans", "coffee_type").await;
    let extras = group(pool, org, "Extras", "extra").await;
    let opt_full = option(
        pool,
        org,
        milk_group,
        "Full cream",
        "milk_type",
        0,
        0,
        (full_cream, "Full cream milk", 1.0, "g"),
    )
    .await;
    let opt_oat = option(
        pool,
        org,
        milk_group,
        "Oat",
        "milk_type",
        55,
        1,
        (oat, "Oat milk", 1.0, "g"),
    )
    .await;
    let opt_almond = option(
        pool,
        org,
        milk_group,
        "Almond",
        "milk_type",
        55,
        2,
        (almond, "Almond milk", 1.0, "ml"),
    )
    .await;
    let opt_house = option(
        pool,
        org,
        bean_group,
        "House",
        "coffee_type",
        0,
        0,
        (house, "House beans", 1.0, "g"),
    )
    .await;
    let opt_decaf = option(
        pool,
        org,
        bean_group,
        "Decaf",
        "coffee_type",
        30,
        1,
        (decaf, "Decaf beans", 1.0, "g"),
    )
    .await;
    let opt_shot = option(
        pool,
        org,
        extras,
        "Extra Shot",
        "extra",
        40,
        0,
        (house, "House beans", 18.0, "g"),
    )
    .await;
    for (g, sort) in [(milk_group, 0), (bean_group, 1), (extras, 2)] {
        sqlx::query("INSERT INTO menu_item_modifier_groups (menu_item_id, group_id, sort) VALUES ($1, $2, $3)")
            .bind(item).bind(g).bind(sort).execute(pool).await.unwrap();
    }

    Fx {
        org,
        item,
        full_cream,
        oat,
        house,
        decaf,
        can,
        straw,
        milk_group,
        bean_group,
        opt_full,
        opt_oat,
        opt_almond,
        opt_house,
        opt_decaf,
        opt_shot,
    }
}

fn req(options: Vec<Uuid>, mode: &str) -> PreviewRequest {
    PreviewRequest {
        size_label: Some("Can".into()),
        option_ids: options,
        quantity: 1,
        service_mode: Some(mode.into()),
        branch_id: None,
    }
}

fn find(r: &PreviewResponse, id: Uuid, source: &str) -> PreviewDeduction {
    r.deductions
        .iter()
        .find(|d| d.ingredient_id == Some(id) && d.source == source)
        .cloned()
        .unwrap_or_else(|| panic!("no {source} deduction of {id}"))
}

#[sqlx::test]
async fn preview_iced_latte_can_oat_decaf_extra_shot(pool: PgPool) {
    let fx = fixture(&pool).await;
    let r = preview(
        &pool,
        fx.org,
        fx.item,
        &req(vec![fx.opt_oat, fx.opt_decaf, fx.opt_shot], "takeaway"),
    )
    .await
    .unwrap();

    // Price = Can + the differences over the defaults + the additive shot.
    assert_eq!(r.price.base, 170);
    let reasons: Vec<(Uuid, i32, &str)> = r
        .price
        .options
        .iter()
        .map(|o| (o.option_id, o.price_delta, o.reason.as_str()))
        .collect();
    assert_eq!(
        reasons,
        vec![
            (fx.opt_oat, 55, "swap over Full cream"),
            (fx.opt_decaf, 30, "swap over House"),
            (fx.opt_shot, 40, "adds"),
        ]
    );
    assert_eq!(r.price.total, 170 + 55 + 30 + 40);

    let oat = find(&r, fx.oat, "swap");
    assert_eq!((oat.quantity, oat.category_slug.as_str()), (250.0, "milk"));
    assert_eq!(oat.note.as_deref(), Some("swapped from Full cream milk"));
    assert!(
        !r.deductions
            .iter()
            .any(|d| d.ingredient_id == Some(fx.full_cream))
    );
    let decaf = find(&r, fx.decaf, "swap");
    assert_eq!(decaf.quantity, 18.0);
    // The extra shot follows the chosen bean.
    let shot = find(&r, fx.decaf, "option");
    assert_eq!(shot.quantity, 18.0);
    assert_eq!(shot.note.as_deref(), Some("follows the chosen Decaf beans"));
    assert!(
        !r.deductions
            .iter()
            .any(|d| d.ingredient_id == Some(fx.house))
    );
    let can = find(&r, fx.can, "packaging");
    assert!(!can.skipped && can.note.is_none());

    // Cost: oat 250×2 + decaf 18×3 twice + can 5 + straw 1.
    assert_eq!(r.cost.total, 500 + 108 + 6);
    assert!(!r.cost.cost_missing);
    assert!(r.warnings.iter().all(|w| w.rule != "unit_conversion"));

    // Defaults are the recipe's ingredients, regardless of the choice.
    assert_eq!(r.defaults.get(&fx.milk_group), Some(&fx.opt_full));
    assert_eq!(r.defaults.get(&fx.bean_group), Some(&fx.opt_house));
}

#[sqlx::test]
async fn preview_dine_in_skips_packaging(pool: PgPool) {
    let fx = fixture(&pool).await;
    let r = preview(&pool, fx.org, fx.item, &req(vec![], "dine_in"))
        .await
        .unwrap();
    for id in [fx.can, fx.straw] {
        let d = find(&r, id, "packaging");
        assert!(d.skipped);
        assert_eq!(d.note.as_deref(), Some("skipped on dine-in"));
    }
    // Full cream 250×1 + house 18×2; no packaging cost.
    assert_eq!(r.cost.total, 250 + 36);
    assert_eq!(r.price.total, 170);
    assert_eq!(r.price.options.len(), 0);
}

#[sqlx::test]
async fn preview_swap_with_unit_conversion_failure_warns_and_does_not_deduct(pool: PgPool) {
    let fx = fixture(&pool).await;
    let r = preview(
        &pool,
        fx.org,
        fx.item,
        &req(vec![fx.opt_almond], "takeaway"),
    )
    .await
    .unwrap();
    assert!(
        r.warnings.iter().any(|w| w.rule == "unit_conversion"),
        "warnings: {:?}",
        r.warnings
    );
    assert!(
        !r.deductions.iter().any(|d| d.category_slug == "milk"),
        "no milk deducted: {:?}",
        r.deductions.iter().map(|d| &d.name).collect::<Vec<_>>()
    );
}

#[sqlx::test]
async fn preview_endpoint_is_org_scoped(pool: PgPool) {
    let fx = fixture(&pool).await;
    let user = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, org_id, name, email, password_hash, role) VALUES ($1, $2, 'U', $3, 'hash', 'org_admin'::user_role)")
        .bind(user).bind(fx.org).bind(format!("u-{user}@t.com")).execute(&pool).await.unwrap();
    let secret = JwtSecret("secret".to_string());
    let token =
        crate::auth::jwt::create_token(&secret, user, Some(fx.org), UserRole::OrgAdmin, None, 24)
            .unwrap();
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret))
            .configure(crate::menu::routes::configure),
    )
    .await;
    let call = || {
        test::TestRequest::post()
            .uri(&format!("/menu-items/{}/preview", fx.item))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(serde_json::json!({ "size_label": "Can", "option_ids": [fx.opt_oat] }))
            .to_request()
    };
    sqlx::query("INSERT INTO role_permissions (role, resource, action, granted) VALUES ('org_admin', 'menu_items', 'read', true) ON CONFLICT DO NOTHING")
        .execute(&pool).await.unwrap();
    // Another org's admin cannot preview this item.
    let other = crate::auth::jwt::create_token(
        &JwtSecret("secret".to_string()),
        user,
        Some(Uuid::new_v4()),
        UserRole::OrgAdmin,
        None,
        24,
    )
    .unwrap();
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/menu-items/{}/preview", fx.item))
            .insert_header(("Authorization", format!("Bearer {other}")))
            .set_json(serde_json::json!({}))
            .to_request(),
    )
    .await;
    assert!(
        matches!(resp.status().as_u16(), 403 | 404),
        "cross-org preview is refused: {}",
        resp.status()
    );
    let resp = test::call_service(&app, call()).await;
    assert!(resp.status().is_success(), "{}", resp.status());
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["price"]["total"], 225);
    assert!(body["deductions"].is_array() && body["defaults"].is_object());
}

/// `POST /menu-items/{id}/preview` is `menu.items.read`: refused to a person with a
/// per-person deny row, served to a teller (core `omtw`).
#[sqlx::test]
async fn preview_is_refused_without_menu_read_and_served_to_a_teller(pool: PgPool) {
    let fx = fixture(&pool).await;
    let mut tokens = Vec::new();
    for (role, kind, deny) in [
        ("org_admin", UserRole::OrgAdmin, true),
        ("teller", UserRole::Teller, false),
    ] {
        let user: Uuid = sqlx::query_scalar(
            "INSERT INTO users (org_id, name, email, password_hash, role) \
             VALUES ($1, $2, $3, 'h', $2::user_role) RETURNING id",
        )
        .bind(fx.org)
        .bind(role)
        .bind(format!("{}@prev.test", Uuid::new_v4()))
        .fetch_one(&pool)
        .await
        .unwrap();
        if deny {
            sqlx::query(
                "INSERT INTO permissions (user_id, resource, action, granted) \
                 VALUES ($1, 'menu_items'::permission_resource, 'read'::permission_action, false)",
            )
            .bind(user)
            .execute(&pool)
            .await
            .unwrap();
        }
        tokens.push(
            crate::auth::jwt::create_token(
                &JwtSecret("secret".into()),
                user,
                Some(fx.org),
                kind,
                None,
                24,
            )
            .unwrap(),
        );
    }
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(JwtSecret("secret".to_string())))
            .configure(crate::menu::routes::configure),
    )
    .await;
    for (who, token, want) in [("denied", &tokens[0], 403), ("teller", &tokens[1], 200)] {
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri(&format!("/menu-items/{}/preview", fx.item))
                .insert_header(("Authorization", format!("Bearer {token}")))
                .set_json(serde_json::json!({ "size_label": "Can", "option_ids": [fx.opt_oat] }))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status().as_u16(), want, "{who}");
    }
}

/// With no size in the request (the Studio's first render), the price, the
/// deductions and the reported size all describe the item's first size as listed.
/// Before the fix the price came from `base_price` (100) while `size_label` said
/// the first size, so the panel showed one size's name with another's price.
#[sqlx::test]
async fn preview_without_a_size_prices_and_deducts_the_reported_size(pool: PgPool) {
    let fx = fixture(&pool).await;
    let mut body = req(vec![], "takeaway");
    body.size_label = None;
    let r = preview(&pool, fx.org, fx.item, &body).await.unwrap();

    // Cup is sort 0, so it is the first size as listed.
    assert_eq!(r.size_label.as_deref(), Some("Cup"));
    assert_eq!(r.price.base, 150);
    // The fixture's recipe lines are Can-only: nothing of Can's may be deducted.
    assert!(!r.deductions.iter().any(|d| d.ingredient_id == Some(fx.can)));

    // An explicit size is unchanged.
    let can = preview(&pool, fx.org, fx.item, &req(vec![], "takeaway"))
        .await
        .unwrap();
    assert_eq!(
        (can.size_label.as_deref(), can.price.base),
        (Some("Can"), 170)
    );
}
