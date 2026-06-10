#![allow(unused_imports)]
use actix_web::{test, web, App};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;

use super::service::{AddonCost, SkuCost};

fn get_secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn admin_token(user_id: Uuid, org_id: Uuid) -> String {
    crate::auth::jwt::create_token(&get_secret(), user_id, Some(org_id), UserRole::OrgAdmin, None, 24)
        .unwrap()
}

async fn seed_basics(pool: &PgPool) -> (Uuid, Uuid, String) {
    let org_id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org', $2)")
        .bind(org_id)
        .bind(format!("costing-{org_id}"))
        .execute(pool)
        .await
        .unwrap();
    let user_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, 'U', $3, 'h', 'org_admin'::user_role)",
    )
    .bind(user_id)
    .bind(org_id)
    .bind(format!("u-{user_id}@t.com"))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO role_permissions (role, resource, action, granted) \
         VALUES ('org_admin'::user_role, 'orders'::permission_resource, 'read'::permission_action, true) \
         ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await
    .unwrap();
    let token = admin_token(user_id, org_id);
    (org_id, user_id, token)
}

#[sqlx::test]
async fn test_sku_costs_rollup_and_missing(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(super::routes::configure),
    )
    .await;
    let (org_id, _user, token) = seed_basics(&pool).await;

    let cat_id = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1, $2, 'Drinks')")
        .bind(cat_id).bind(org_id).execute(&pool).await.unwrap();

    // Costed item: 10 g @ 2.50 EGP/g → 25 EGP = 2 500 piastres.
    let costed = Uuid::new_v4();
    sqlx::query("INSERT INTO menu_items (id, org_id, category_id, name, base_price, is_active) VALUES ($1, $2, $3, 'Latte', 7000, true)")
        .bind(costed).bind(org_id).bind(cat_id).execute(&pool).await.unwrap();
    let ing = Uuid::new_v4();
    sqlx::query("INSERT INTO org_ingredients (id, org_id, name, unit, cost_per_unit, category) VALUES ($1, $2, 'Beans', 'g'::inventory_unit, 2.50, 'coffee_bean')")
        .bind(ing).bind(org_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO menu_item_recipes (menu_item_id, org_ingredient_id, quantity_used, size_label, ingredient_name, ingredient_unit) VALUES ($1, $2, 10.0, 'one_size', 'Beans', 'g')")
        .bind(costed).bind(ing).execute(&pool).await.unwrap();

    // Recipe-less item: cost must be NULL, never zero.
    let bare = Uuid::new_v4();
    sqlx::query("INSERT INTO menu_items (id, org_id, category_id, name, base_price, is_active) VALUES ($1, $2, $3, 'Water', 1000, true)")
        .bind(bare).bind(org_id).bind(cat_id).execute(&pool).await.unwrap();

    let req = test::TestRequest::get()
        .uri(&format!("/costing/menu-items?org_id={org_id}"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success(), "got {:?}", resp.status());
    let rows: Vec<SkuCost> = test::read_body_json(resp).await;

    let latte = rows.iter().find(|r| r.menu_item_id == costed).unwrap();
    assert_eq!(latte.cost, Some(2_500));
    assert!(!latte.cost_missing);
    assert!((latte.food_cost_pct.unwrap() - 2_500.0 / 7_000.0).abs() < 1e-9);

    let water = rows.iter().find(|r| r.menu_item_id == bare).unwrap();
    assert_eq!(water.cost, None);
    assert!(water.cost_missing);
}

#[sqlx::test]
async fn test_addon_costs_rollup(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(super::routes::configure),
    )
    .await;
    let (org_id, _user, token) = seed_basics(&pool).await;

    let ing = Uuid::new_v4();
    sqlx::query("INSERT INTO org_ingredients (id, org_id, name, unit, cost_per_unit, category) VALUES ($1, $2, 'Oat Milk', 'ml'::inventory_unit, 0.10, 'milk')")
        .bind(ing).bind(org_id).execute(&pool).await.unwrap();
    let addon = Uuid::new_v4();
    sqlx::query("INSERT INTO addon_items (id, org_id, name, type, default_price) VALUES ($1, $2, 'Oat', 'milk_type', 1500)")
        .bind(addon).bind(org_id).execute(&pool).await.unwrap();
    // 200 ml @ 0.10 EGP/ml → 20 EGP = 2 000 piastres.
    sqlx::query("INSERT INTO addon_item_ingredients (addon_item_id, org_ingredient_id, quantity_used, ingredient_name, ingredient_unit) VALUES ($1, $2, 200.0, 'Oat Milk', 'ml')")
        .bind(addon).bind(ing).execute(&pool).await.unwrap();

    let req = test::TestRequest::get()
        .uri(&format!("/costing/addon-items?org_id={org_id}"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());
    let rows: Vec<AddonCost> = test::read_body_json(resp).await;
    let oat = rows.iter().find(|r| r.addon_item_id == addon).unwrap();
    assert_eq!(oat.cost, Some(2_000));
    assert!(!oat.cost_missing);
}
