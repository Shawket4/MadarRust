#![allow(unused_imports, unused_variables, dead_code)]
use actix_web::{App, HttpMessage, test, web};
use bigdecimal::BigDecimal;
use chrono::Utc;
use sqlx::PgPool;
use std::str::FromStr;
use uuid::Uuid;

use crate::{
    auth::jwt::{JwtSecret, create_token},
    branches::handlers::{Branch, PrinterBrand},
    inventory::handlers::{BranchInventoryItem, OrgIngredient},
    menu::handlers::{AddonItem, Category, ItemSize, MenuItemFull},
    models::UserRole,
    orders::handlers::Order,
    orgs::handlers::Org,
    permissions::handlers::PermissionMatrix,
    shifts::handlers::{Shift, ShiftReportResponse},
    users::handlers::CreateUserResponse,
};

// -----------------------------------------------------------------------------
// Seeding Helpers & Utilities
// -----------------------------------------------------------------------------

fn get_secret() -> JwtSecret {
    JwtSecret("test_secret".to_string())
}

fn generate_token(
    user_id: Uuid,
    org_id: Option<Uuid>,
    role: UserRole,
    branch_id: Option<Uuid>,
) -> String {
    create_token(&get_secret(), user_id, org_id, role, branch_id, 24).unwrap()
}

fn generate_super_admin_token() -> String {
    generate_token(Uuid::new_v4(), None, UserRole::SuperAdmin, None)
}

fn generate_org_admin_token(org_id: Uuid) -> String {
    generate_token(Uuid::new_v4(), Some(org_id), UserRole::OrgAdmin, None)
}

fn multipart_body(fields: &[(&str, &str)]) -> String {
    let mut body = String::new();
    for (name, val) in fields {
        body.push_str("--boundary\r\n");
        body.push_str(&format!(
            "Content-Disposition: form-data; name=\"{}\"\r\n\r\n",
            name
        ));
        body.push_str(val);
        body.push_str("\r\n");
    }
    body.push_str("--boundary--\r\n");
    body
}

fn to_bigdecimal(val: f64) -> BigDecimal {
    BigDecimal::from_str(&val.to_string()).unwrap()
}

async fn seed_default_permissions(pool: &PgPool) {
    crate::permissions::seeder::seed_role_permissions(pool)
        .await
        .expect("Failed to seed default role permissions");
}

async fn seed_payment_methods(pool: &PgPool, org_id: Uuid) {
    sqlx::query(
        "INSERT INTO org_payment_methods (org_id, name, label_translations, color, icon, is_cash, is_active) VALUES
        ($1, 'cash', '{}', 'emerald', 'payments_outlined', true, true),
        ($1, 'card', '{}', 'blue', 'credit_card_rounded', false, true)"
    )
    .bind(org_id)
    .execute(pool)
    .await
    .unwrap();
}

// -----------------------------------------------------------------------------
// 1. Scenario A: The Merchant Setup and Operation Workflow (Happy Path)
// -----------------------------------------------------------------------------

#[sqlx::test]
async fn test_e2e_merchant_setup_and_operation_happy_path(pool: PgPool) {
    seed_default_permissions(&pool).await;

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(crate::auth::routes::configure)
            .configure(crate::orgs::routes::configure)
            .configure(crate::users::routes::configure)
            .configure(crate::permissions::routes::configure)
            .configure(crate::branches::routes::configure)
            .configure(crate::menu::routes::configure)
            .configure(crate::inventory::routes::configure)
            .configure(crate::recipes::routes::configure)
            .configure(crate::shifts::routes::configure)
            .configure(crate::orders::routes::configure),
    )
    .await;

    // STEP 1.1: SuperAdmin setup - create a new Org
    let super_token = generate_super_admin_token();
    let org_payload = multipart_body(&[
        ("name", "E2E Happy Org"),
        ("slug", "e2e-happy-slug"),
        ("currency_code", "USD"),
        ("tax_rate", "0.10"),
    ]);

    let req = test::TestRequest::post()
        .uri("/orgs")
        .insert_header(("Content-Type", "multipart/form-data; boundary=boundary"))
        .insert_header(("Authorization", format!("Bearer {}", super_token)))
        .set_payload(org_payload)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::CREATED);
    let org: Org = test::read_body_json(resp).await;
    let org_id = org.id;

    // STEP 1.2: Create an OrgAdmin user under the new Org
    let req = test::TestRequest::post()
        .uri("/users")
        .insert_header(("Authorization", format!("Bearer {}", super_token)))
        .set_json(&serde_json::json!({
            "org_id": org_id,
            "name": "E2E Org Admin",
            "email": "e2eadmin@example.com",
            "role": "org_admin",
            "password": "password123"
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::CREATED);
    let admin_res: CreateUserResponse = test::read_body_json(resp).await;
    let admin_user_id = admin_res.user.id;

    // Generate JWT for the newly created OrgAdmin
    let admin_token = generate_token(admin_user_id, Some(org_id), UserRole::OrgAdmin, None);

    // STEP 1.3: Merchant Admin Setup - create two branches
    let req = test::TestRequest::post()
        .uri("/branches")
        .insert_header(("Authorization", format!("Bearer {}", admin_token)))
        .set_json(&serde_json::json!({
            "org_id": org_id,
            "name": "Downtown Branch",
            "timezone": "Africa/Cairo"
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::CREATED);
    let branch_a: Branch = test::read_body_json(resp).await;
    let branch_a_id = branch_a.id;

    let req = test::TestRequest::post()
        .uri("/branches")
        .insert_header(("Authorization", format!("Bearer {}", admin_token)))
        .set_json(&serde_json::json!({
            "org_id": org_id,
            "name": "Subway Branch",
            "timezone": "Africa/Cairo"
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::CREATED);
    let branch_b: Branch = test::read_body_json(resp).await;
    let _branch_b_id = branch_b.id;

    // STEP 1.4: Create a Teller user assigned to Downtown Branch
    let req = test::TestRequest::post()
        .uri("/users")
        .insert_header(("Authorization", format!("Bearer {}", admin_token)))
        .set_json(&serde_json::json!({
            "org_id": org_id,
            "name": "Happy POS Teller",
            "role": "teller",
            "pin": "1234",
            "branch_ids": [branch_a_id]
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::CREATED);
    let teller_res: CreateUserResponse = test::read_body_json(resp).await;
    let teller_user_id = teller_res.user.id;

    // STEP 1.5: Roles & Permissions Mapping
    // Tellers by default do NOT have menu write permissions. Let's verify by retrieving the permission matrix
    let req = test::TestRequest::get()
        .uri(&format!("/permissions/matrix/{}", teller_user_id))
        .insert_header(("Authorization", format!("Bearer {}", admin_token)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::OK);
    let matrix: Vec<PermissionMatrix> = test::read_body_json(resp).await;

    // Verify teller has NO permission to modify categories
    let cat_write = matrix
        .iter()
        .find(|m| m.resource == "categories" && m.action == "create")
        .unwrap();
    assert_eq!(cat_write.effective, false);

    // STEP 1.6: Grant custom permission override to Teller
    let req = test::TestRequest::put()
        .uri(&format!("/permissions/user/{}", teller_user_id))
        .insert_header(("Authorization", format!("Bearer {}", admin_token)))
        .set_json(&serde_json::json!({
            "resource": "categories",
            "action": "create",
            "granted": true
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::OK);

    // Verify override in matrix now resolves to true
    let req = test::TestRequest::get()
        .uri(&format!("/permissions/matrix/{}", teller_user_id))
        .insert_header(("Authorization", format!("Bearer {}", admin_token)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    let matrix: Vec<PermissionMatrix> = test::read_body_json(resp).await;
    let cat_write_updated = matrix
        .iter()
        .find(|m| m.resource == "categories" && m.action == "create")
        .unwrap();
    assert_eq!(cat_write_updated.user_override, Some(true));
    assert_eq!(cat_write_updated.effective, true);

    // STEP 1.7: Log in / operate as the Teller user
    let teller_token = generate_token(
        teller_user_id,
        Some(org_id),
        UserRole::Teller,
        Some(branch_a_id),
    );

    // Teller gets their current info
    let req = test::TestRequest::get()
        .uri("/auth/me")
        .insert_header(("Authorization", format!("Bearer {}", teller_token)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::OK);

    // Verify Teller can now create a category thanks to the permission override!
    let req = test::TestRequest::post()
        .uri("/categories")
        .insert_header(("Authorization", format!("Bearer {}", teller_token)))
        .set_json(&serde_json::json!({
            "org_id": org_id,
            "name": "Beverages"
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::CREATED);
    let category: Category = test::read_body_json(resp).await;
    assert_eq!(category.name, "Beverages");
}

// -----------------------------------------------------------------------------
// 2. Scenario B: Multi-Tenant and Role Isolation Integrity (Attack Path)
// -----------------------------------------------------------------------------

#[sqlx::test]
async fn test_e2e_tenant_and_role_isolation_security_violation_path(pool: PgPool) {
    seed_default_permissions(&pool).await;

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(crate::auth::routes::configure)
            .configure(crate::orgs::routes::configure)
            .configure(crate::users::routes::configure)
            .configure(crate::permissions::routes::configure)
            .configure(crate::branches::routes::configure),
    )
    .await;

    // STEP 2.1: Seed Org A and Org B
    let org_a_id = Uuid::new_v4();
    let org_b_id = Uuid::new_v4();

    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org A', 'org-a'), ($2, 'Org B', 'org-b')")
        .bind(org_a_id).bind(org_b_id).execute(&pool).await.unwrap();
    seed_payment_methods(&pool, org_a_id).await;
    seed_payment_methods(&pool, org_b_id).await;

    // Create branch in Org B
    let branch_b_id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Branch B')")
        .bind(branch_b_id)
        .bind(org_b_id)
        .execute(&pool)
        .await
        .unwrap();

    // Create a Teller user in Org B
    let teller_b_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, org_id, name, email, role, pin_hash, is_active) VALUES ($1, $2, 'Teller B', 't@b.com', 'teller'::user_role, 'hash', true)")
        .bind(teller_b_id).bind(org_b_id).execute(&pool).await.unwrap();

    // OrgAdmin A token
    let admin_a_user_id = Uuid::new_v4();
    let token_admin_a = generate_token(admin_a_user_id, Some(org_a_id), UserRole::OrgAdmin, None);

    // STEP 2.2: Tenant Boundary Attack 1 - OrgAdmin A tries to list branches of Org B
    let req = test::TestRequest::get()
        .uri(&format!("/branches?org_id={}", org_b_id))
        .insert_header(("Authorization", format!("Bearer {}", token_admin_a)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::FORBIDDEN);

    // Tenant Boundary Attack 2 - OrgAdmin A tries to fetch permission matrix for Teller B (Org B)
    let req = test::TestRequest::get()
        .uri(&format!("/permissions/matrix/{}", teller_b_id))
        .insert_header(("Authorization", format!("Bearer {}", token_admin_a)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::FORBIDDEN);

    // Tenant Boundary Attack 3 - OrgAdmin A tries to inject permission override to Teller B (Org B)
    let req = test::TestRequest::put()
        .uri(&format!("/permissions/user/{}", teller_b_id))
        .insert_header(("Authorization", format!("Bearer {}", token_admin_a)))
        .set_json(&serde_json::json!({
            "resource": "inventory",
            "action": "create",
            "granted": true
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::FORBIDDEN);

    // STEP 2.3: Escalation Attacks - Teller B tries administrative actions
    let token_teller_b = generate_token(
        teller_b_id,
        Some(org_b_id),
        UserRole::Teller,
        Some(branch_b_id),
    );

    // Escalation Attempt 1 - Teller tries to list role defaults
    let req = test::TestRequest::get()
        .uri("/permissions/roles")
        .insert_header(("Authorization", format!("Bearer {}", token_teller_b)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::FORBIDDEN);

    // Escalation Attempt 2 - Teller tries to modify role permissions
    let req = test::TestRequest::put()
        .uri("/permissions/roles")
        .insert_header(("Authorization", format!("Bearer {}", token_teller_b)))
        .set_json(&serde_json::json!({
            "role": "teller",
            "resource": "permissions",
            "action": "update",
            "granted": true
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::FORBIDDEN);
}

// -----------------------------------------------------------------------------
// 3. Scenario C: Full Kitchen, Inventory, Shift, and POS Order placement Lifecycle
// -----------------------------------------------------------------------------

#[sqlx::test]
async fn test_e2e_kitchen_inventory_order_lifecycle(pool: PgPool) {
    seed_default_permissions(&pool).await;

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(crate::auth::routes::configure)
            .configure(crate::orgs::routes::configure)
            .configure(crate::users::routes::configure)
            .configure(crate::permissions::routes::configure)
            .configure(crate::branches::routes::configure)
            .configure(crate::menu::routes::configure)
            .configure(crate::inventory::routes::configure)
            .configure(crate::recipes::routes::configure)
            .configure(crate::shifts::routes::configure)
            .configure(crate::orders::routes::configure)
            .configure(crate::discounts::routes::configure)
            .configure(|cfg| crate::reports::routes::configure(cfg, web::Data::new(pool.clone()))),
    )
    .await;

    // STEP 3.1: Seed merchant setup
    let org_id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug, currency_code, tax_rate) VALUES ($1, 'Kitchen E2E Org', 'kitchen-org', 'USD', 0.10)")
        .bind(org_id).execute(&pool).await.unwrap();
    seed_payment_methods(&pool, org_id).await;

    let branch_id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'POS Kitchen Branch')")
        .bind(branch_id)
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();

    let admin_user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, org_id, name, email, role, password_hash, is_active) VALUES ($1, $2, 'Kitchen Admin', 'kadmin@example.com', 'org_admin'::user_role, 'hash', true)")
        .bind(admin_user_id).bind(org_id).execute(&pool).await.unwrap();

    let teller_user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, org_id, name, role, pin_hash, is_active) VALUES ($1, $2, 'Kitchen Teller', 'teller'::user_role, 'hash', true)")
        .bind(teller_user_id).bind(org_id).execute(&pool).await.unwrap();

    // Assign Teller to branch
    sqlx::query(
        "INSERT INTO user_branch_assignments (user_id, branch_id, assigned_by) VALUES ($1, $2, $3)",
    )
    .bind(teller_user_id)
    .bind(branch_id)
    .bind(admin_user_id)
    .execute(&pool)
    .await
    .unwrap();

    let admin_token = generate_token(admin_user_id, Some(org_id), UserRole::OrgAdmin, None);
    let teller_token = generate_token(
        teller_user_id,
        Some(org_id),
        UserRole::Teller,
        Some(branch_id),
    );

    // STEP 3.2: Configure Menu (Beverages and Bakery categories)
    let beverages_cat: Category = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/categories")
                .insert_header(("Authorization", format!("Bearer {}", admin_token)))
                .set_json(&serde_json::json!({
                    "org_id": org_id,
                    "name": "Beverages"
                }))
                .to_request(),
        )
        .await,
    )
    .await;

    let bakery_cat: Category = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/categories")
                .insert_header(("Authorization", format!("Bearer {}", admin_token)))
                .set_json(&serde_json::json!({
                    "org_id": org_id,
                    "name": "Bakery"
                }))
                .to_request(),
        )
        .await,
    )
    .await;

    // Create Menu Items
    // Espresso Macchiato (Beverages)
    let espresso_full: MenuItemFull = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/menu-items")
                .insert_header(("Authorization", format!("Bearer {}", admin_token)))
                .set_json(&serde_json::json!({
                    "org_id": org_id,
                    "category_id": beverages_cat.id,
                    "name": "Espresso Macchiato",
                    "base_price": 300
                }))
                .to_request(),
        )
        .await,
    )
    .await;
    let espresso_id = espresso_full.item.id;

    // Croissant (Bakery)
    let croissant_full: MenuItemFull = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/menu-items")
                .insert_header(("Authorization", format!("Bearer {}", admin_token)))
                .set_json(&serde_json::json!({
                    "org_id": org_id,
                    "category_id": bakery_cat.id,
                    "name": "Croissant",
                    "base_price": 250
                }))
                .to_request(),
        )
        .await,
    )
    .await;
    let croissant_id = croissant_full.item.id;

    // Create Sizes for Espresso: Medium (+100 price override)
    let _espresso_size: ItemSize = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::post()
                .uri(&format!("/menu-items/{}/sizes", espresso_id))
                .insert_header(("Authorization", format!("Bearer {}", admin_token)))
                .set_json(&serde_json::json!({
                    "label": "medium",
                    "price_override": 400
                }))
                .to_request(),
        )
        .await,
    )
    .await;

    // Create Addon item: Vanilla Syrup (syrup)
    let vanilla_addon: AddonItem = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/addon-items")
                .insert_header(("Authorization", format!("Bearer {}", admin_token)))
                .set_json(&serde_json::json!({
                    "org_id": org_id,
                    "name": "Vanilla Syrup",
                    "addon_type": "syrup",
                    "default_price": 50
                }))
                .to_request(),
        )
        .await,
    )
    .await;

    // STEP 3.3: Configure Inventory Catalog Items (ingredients)
    let espresso_beans: OrgIngredient = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::post()
                .uri(&format!("/inventory/orgs/{}/catalog", org_id))
                .insert_header(("Authorization", format!("Bearer {}", admin_token)))
                .set_json(&serde_json::json!({
                    "name": "Espresso Beans",
                    "unit": "g",
                    "category": "coffee_bean",
                    "cost_per_unit": 0.02
                }))
                .to_request(),
        )
        .await,
    )
    .await;

    let whole_milk: OrgIngredient = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::post()
                .uri(&format!("/inventory/orgs/{}/catalog", org_id))
                .insert_header(("Authorization", format!("Bearer {}", admin_token)))
                .set_json(&serde_json::json!({
                    "name": "Whole Milk",
                    "unit": "ml",
                    "category": "milk",
                    "cost_per_unit": 0.005
                }))
                .to_request(),
        )
        .await,
    )
    .await;

    let croissant_dough: OrgIngredient = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::post()
                .uri(&format!("/inventory/orgs/{}/catalog", org_id))
                .insert_header(("Authorization", format!("Bearer {}", admin_token)))
                .set_json(&serde_json::json!({
                    "name": "Croissant Dough",
                    "unit": "pcs",
                    "category": "general",
                    "cost_per_unit": 0.50
                }))
                .to_request(),
        )
        .await,
    )
    .await;

    let vanilla_flavor: OrgIngredient = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::post()
                .uri(&format!("/inventory/orgs/{}/catalog", org_id))
                .insert_header(("Authorization", format!("Bearer {}", admin_token)))
                .set_json(&serde_json::json!({
                    "name": "Vanilla Flavor",
                    "unit": "ml",
                    "category": "general",
                    "cost_per_unit": 0.01
                }))
                .to_request(),
        )
        .await,
    )
    .await;

    // STEP 3.4: Add Ingredients to Branch Stock
    let stock_beans: BranchInventoryItem = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::post()
                .uri(&format!("/inventory/branches/{}/stock", branch_id))
                .insert_header(("Authorization", format!("Bearer {}", admin_token)))
                .set_json(&serde_json::json!({
                    "org_ingredient_id": espresso_beans.id,
                    "current_stock": 1000.0,
                    "reorder_threshold": 100.0
                }))
                .to_request(),
        )
        .await,
    )
    .await;

    let stock_milk: BranchInventoryItem = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::post()
                .uri(&format!("/inventory/branches/{}/stock", branch_id))
                .insert_header(("Authorization", format!("Bearer {}", admin_token)))
                .set_json(&serde_json::json!({
                    "org_ingredient_id": whole_milk.id,
                    "current_stock": 5000.0,
                    "reorder_threshold": 500.0
                }))
                .to_request(),
        )
        .await,
    )
    .await;

    let stock_croissant: BranchInventoryItem = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::post()
                .uri(&format!("/inventory/branches/{}/stock", branch_id))
                .insert_header(("Authorization", format!("Bearer {}", admin_token)))
                .set_json(&serde_json::json!({
                    "org_ingredient_id": croissant_dough.id,
                    "current_stock": 1.0,
                    "reorder_threshold": 1.0
                }))
                .to_request(),
        )
        .await,
    )
    .await;

    let stock_vanilla: BranchInventoryItem = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::post()
                .uri(&format!("/inventory/branches/{}/stock", branch_id))
                .insert_header(("Authorization", format!("Bearer {}", admin_token)))
                .set_json(&serde_json::json!({
                    "org_ingredient_id": vanilla_flavor.id,
                    "current_stock": 100.0,
                    "reorder_threshold": 10.0
                }))
                .to_request(),
        )
        .await,
    )
    .await;

    // STEP 3.5: Map items to ingredients via Recipes
    // Espresso Macchiato (medium size): 18g beans, 150ml milk
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/recipes/drinks/{}", espresso_id))
            .insert_header(("Authorization", format!("Bearer {}", admin_token)))
            .set_json(&serde_json::json!({
                "size_label": "medium",
                "org_ingredient_id": espresso_beans.id,
                "ingredient_name": "Espresso Beans",
                "ingredient_unit": "g",
                "quantity_used": 18.0
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::OK);

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/recipes/drinks/{}", espresso_id))
            .insert_header(("Authorization", format!("Bearer {}", admin_token)))
            .set_json(&serde_json::json!({
                "size_label": "medium",
                "org_ingredient_id": whole_milk.id,
                "ingredient_name": "Whole Milk",
                "ingredient_unit": "ml",
                "quantity_used": 150.0
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::OK);

    // Croissant (one size / base): 1 croissant dough
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/recipes/drinks/{}", croissant_id))
            .insert_header(("Authorization", format!("Bearer {}", admin_token)))
            .set_json(&serde_json::json!({
                "size_label": "one_size",
                "org_ingredient_id": croissant_dough.id,
                "ingredient_name": "Croissant Dough",
                "ingredient_unit": "pcs",
                "quantity_used": 1.0
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::OK);

    // Vanilla Syrup addon: 15ml vanilla flavor
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/recipes/addons/{}", vanilla_addon.id))
            .insert_header(("Authorization", format!("Bearer {}", admin_token)))
            .set_json(&serde_json::json!({
                "org_ingredient_id": vanilla_flavor.id,
                "ingredient_name": "Vanilla Flavor",
                "ingredient_unit": "ml",
                "quantity_used": 15.0
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::OK);

    // STEP 3.6: Start POS operations - Open Shift
    let shift: Shift = test::read_body_json(
        test::call_service(
            &app,
            test::TestRequest::post()
                .uri(&format!("/shifts/branches/{}/open", branch_id))
                .insert_header(("Authorization", format!("Bearer {}", teller_token)))
                .set_json(&serde_json::json!({
                    "opening_cash": 2000
                }))
                .to_request(),
        )
        .await,
    )
    .await;

    // STEP 3.7: Place Order 1 (Happy POS flow: Espresso Macchiato medium size + Vanilla Syrup addon)
    // Setup discount rule: Seed flat discount in DB
    let discount_id = Uuid::new_v4();
    sqlx::query("INSERT INTO discounts (id, org_id, name, type, value, is_active) VALUES ($1, $2, 'POS Flat 100', 'fixed', 100, true)")
        .bind(discount_id).bind(org_id).execute(&pool).await.unwrap();

    let order_payload = serde_json::json!({
        "branch_id": branch_id,
        "shift_id": shift.id,
        "payment_method": "cash",
        "customer_name": "John Doe",
        "discount_type": "fixed",
        "discount_value": 100,
        "discount_id": discount_id,
        "amount_tendered": 1000,
        "items": [
            {
                "menu_item_id": espresso_id,
                "size_label": "medium",
                "quantity": 1,
                "addons": [{ "addon_item_id": vanilla_addon.id, "quantity": 1 }],
                "optional_field_ids": [],
                "bundle_components": []
            }
        ]
    });

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/orders")
            .insert_header(("Authorization", format!("Bearer {}", teller_token)))
            .set_json(&order_payload)
            .to_request(),
    )
    .await;
    let status = resp.status();
    if !status.is_success() {
        let body: serde_json::Value = test::read_body_json(resp).await;
        panic!("Status: {:?}, Body: {:?}", status, body);
    }
    let order: Order = test::read_body_json(resp).await;

    // Assert Pricing is correct:
    // Medium Espresso Macchiato: price_override = 400
    // Vanilla Syrup: default_price = 50
    // Subtotal: 450. Discount: 100. Total Subtotal: 350. Tax: 10% = 35. Subtotal Paid: 385 cents.
    assert_eq!(order.subtotal, 450);
    assert_eq!(order.discount_amount, 100);
    assert_eq!(order.tax_amount, 35);
    assert_eq!(order.total_amount, 385);

    // Verify stock has been accurately deducted based on the recipes!
    let beans_stock: sqlx::types::BigDecimal =
        sqlx::query_scalar("SELECT current_stock FROM branch_inventory WHERE id = $1")
            .bind(stock_beans.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let milk_stock: sqlx::types::BigDecimal =
        sqlx::query_scalar("SELECT current_stock FROM branch_inventory WHERE id = $1")
            .bind(stock_milk.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let vanilla_stock: sqlx::types::BigDecimal =
        sqlx::query_scalar("SELECT current_stock FROM branch_inventory WHERE id = $1")
            .bind(stock_vanilla.id)
            .fetch_one(&pool)
            .await
            .unwrap();

    assert_eq!(beans_stock, to_bigdecimal(982.0)); // 1000 - 18
    assert_eq!(milk_stock, to_bigdecimal(4850.0)); // 5000 - 150
    assert_eq!(vanilla_stock, to_bigdecimal(85.0)); // 100 - 15

    // STEP 3.8: Edge Case - Negative Inventory Handling (try to buy 2 croissants when only 1 croissant dough is in stock)
    let bad_order_payload = serde_json::json!({
        "branch_id": branch_id,
        "shift_id": shift.id,
        "payment_method": "cash",
        "items": [
            {
                "menu_item_id": croissant_id,
                "size_label": "one_size",
                "quantity": 2,
                "addons": [],
                "optional_field_ids": [],
                "bundle_components": []
            }
        ]
    });

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/orders")
            .insert_header(("Authorization", format!("Bearer {}", teller_token)))
            .set_json(&bad_order_payload)
            .to_request(),
    )
    .await;
    // Order placement succeeds (soft-fail / negative inventory allowed for busy merchant kitchens)
    assert_eq!(resp.status(), actix_web::http::StatusCode::CREATED);
    let bad_order: Order = test::read_body_json(resp).await;

    // Verify stock goes negative to -1.0!
    let croissant_stock: sqlx::types::BigDecimal =
        sqlx::query_scalar("SELECT current_stock FROM branch_inventory WHERE id = $1")
            .bind(stock_croissant.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(croissant_stock, to_bigdecimal(-1.0));

    // Void the bad order to restore inventory back to 1.0!
    let void_payload = serde_json::json!({
        "reason": "customer_request",
        "restore_inventory": true
    });
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/orders/{}/void", bad_order.id))
            .insert_header(("Authorization", format!("Bearer {}", admin_token)))
            .set_json(&void_payload)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::OK);

    // Croissant stock is successfully rolled back and restored to 1!
    let croissant_stock: sqlx::types::BigDecimal =
        sqlx::query_scalar("SELECT current_stock FROM branch_inventory WHERE id = $1")
            .bind(stock_croissant.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(croissant_stock, to_bigdecimal(1.0));

    // STEP 3.9: Edge Case - Order Void & Stock Rollback
    // Buy 1 croissant (valid)
    let ok_order_payload = serde_json::json!({
        "branch_id": branch_id,
        "shift_id": shift.id,
        "payment_method": "cash",
        "items": [
            {
                "menu_item_id": croissant_id,
                "size_label": "one_size",
                "quantity": 1,
                "addons": [],
                "optional_field_ids": [],
                "bundle_components": []
            }
        ]
    });

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/orders")
            .insert_header(("Authorization", format!("Bearer {}", teller_token)))
            .set_json(&ok_order_payload)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::CREATED);
    let order2: Order = test::read_body_json(resp).await;

    // Croissant stock is now 0
    let croissant_stock: sqlx::types::BigDecimal =
        sqlx::query_scalar("SELECT current_stock FROM branch_inventory WHERE id = $1")
            .bind(stock_croissant.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(croissant_stock, to_bigdecimal(0.0));

    // Void the order and verify stock restores!
    let void_payload = serde_json::json!({
        "reason": "customer_request",
        "restore_inventory": true
    });
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/orders/{}/void", order2.id))
            .insert_header(("Authorization", format!("Bearer {}", admin_token)))
            .set_json(&void_payload)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::OK);

    // Croissant stock is successfully rolled back and restored to 1!
    let croissant_stock: sqlx::types::BigDecimal =
        sqlx::query_scalar("SELECT current_stock FROM branch_inventory WHERE id = $1")
            .bind(stock_croissant.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(croissant_stock, to_bigdecimal(1.0));

    // STEP 3.10: End POS operations - close shift & audit
    // Record a cash movement (change deposit of $10 / 1000 cents)
    test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/{}/cash-movements", shift.id))
            .insert_header(("Authorization", format!("Bearer {}", teller_token)))
            .set_json(&serde_json::json!({
                "amount": 1000,
                "note": "change depot"
            }))
            .to_request(),
    )
    .await;

    // Close Shift: opening cash 2000 + order subtotal paid 385 + movement 1000 = 3385 cents. Declared: 3385.
    let close_payload = serde_json::json!({
        "closing_cash_declared": 3385,
        "cash_note": "Matches perfectly"
    });
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/{}/close", shift.id))
            .insert_header(("Authorization", format!("Bearer {}", teller_token)))
            .set_json(&close_payload)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::OK);

    // Verify shift report generates successfully
    let req = test::TestRequest::get()
        .uri(&format!("/shifts/{}/report", shift.id))
        .insert_header(("Authorization", format!("Bearer {}", teller_token)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::OK);
    let report: ShiftReportResponse = test::read_body_json(resp).await;
    assert_eq!(report.net_payments, 385); // net of discount ($4.50 - $1.00 = $3.50 + $0.35 tax)
}
// 5. Scenario E: Purchasing → Stocktake guardrail → Reports lifecycle
//    Exercises the Foodics-grade additions end to end: supplier link, PO with
//    partial + full receiving + weighted-average cost, sale deduction, waste,
//    a stock count whose large difference is blocked until a reason is given,
//    and the inventory reports reflecting all of it.
// -----------------------------------------------------------------------------

#[sqlx::test]
async fn test_e2e_purchasing_stocktake_reporting_lifecycle(pool: PgPool) {
    seed_default_permissions(&pool).await;

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(crate::auth::routes::configure)
            .configure(crate::menu::routes::configure)
            .configure(crate::inventory::routes::configure)
            .configure(crate::recipes::routes::configure)
            .configure(crate::purchasing::routes::configure)
            .configure(crate::stocktakes::routes::configure)
            .configure(crate::shifts::routes::configure)
            .configure(crate::orders::routes::configure)
            .configure(|cfg| crate::reports::routes::configure(cfg, web::Data::new(pool.clone()))),
    )
    .await;

    macro_rules! jget {
        ($app:expr, $req:expr) => {{
            let r = test::call_service(&$app, $req).await;
            let status = r.status().as_u16();
            let body: serde_json::Value = test::read_body_json(r).await;
            (status, body)
        }};
    }
    macro_rules! jstatus {
        ($app:expr, $req:expr) => {{ test::call_service(&$app, $req).await.status().as_u16() }};
    }

    // ── Setup (real users so created_by / teller_id FKs resolve) ──
    let org_id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug, currency_code, tax_rate) VALUES ($1,'Inv E2E','inv-e2e','EGP',0.0)")
        .bind(org_id).execute(&pool).await.unwrap();
    seed_payment_methods(&pool, org_id).await;
    let branch_id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1,$2,'Main')")
        .bind(branch_id)
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();
    let admin_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, org_id, name, email, role, password_hash, is_active) VALUES ($1,$2,'Admin','adm@inve2e.com','org_admin'::user_role,'h',true)")
        .bind(admin_id).bind(org_id).execute(&pool).await.unwrap();
    let teller_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, org_id, name, role, pin_hash, is_active) VALUES ($1,$2,'Teller','teller'::user_role,'h',true)")
        .bind(teller_id).bind(org_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1,$2)")
        .bind(teller_id)
        .bind(branch_id)
        .execute(&pool)
        .await
        .unwrap();
    let admin = generate_token(admin_id, Some(org_id), UserRole::OrgAdmin, None);
    let teller = generate_token(teller_id, Some(org_id), UserRole::Teller, Some(branch_id));
    let abearer = format!("Bearer {admin}");
    let tbearer = format!("Bearer {teller}");

    // ── A. Supplier ──
    let (st, v) = jget!(
        app,
        test::TestRequest::post()
            .uri(&format!("/purchasing/orgs/{org_id}/suppliers"))
            .insert_header(("Authorization", abearer.clone()))
            .set_json(serde_json::json!({"name": "Beans Co", "phone": "0100"}))
            .to_request()
    );
    assert_eq!(st, 201);
    let supplier_id = v["id"].as_str().unwrap().to_string();

    // ── B. Ingredients (linked to the supplier) ──
    let (_, v) = jget!(app, test::TestRequest::post()
        .uri(&format!("/inventory/orgs/{org_id}/catalog")).insert_header(("Authorization", abearer.clone()))
        .set_json(serde_json::json!({"name":"Beans","unit":"g","category":"coffee","cost_per_unit":10,"supplier_id":supplier_id})).to_request());
    let beans = v["id"].as_str().unwrap().to_string();
    assert_eq!(v["supplier_name"], "Beans Co");
    let (_, v) = jget!(app, test::TestRequest::post()
        .uri(&format!("/inventory/orgs/{org_id}/catalog")).insert_header(("Authorization", abearer.clone()))
        .set_json(serde_json::json!({"name":"Milk","unit":"ml","category":"dairy","cost_per_unit":5,"supplier_id":supplier_id})).to_request());
    let milk = v["id"].as_str().unwrap().to_string();

    // ── C. Branch stock + low-stock levels ──
    for (ing, stock, reorder) in [(&beans, 1000.0, 100.0), (&milk, 1000.0, 5000.0)] {
        let st = jstatus!(app, test::TestRequest::post()
            .uri(&format!("/inventory/branches/{branch_id}/stock")).insert_header(("Authorization", abearer.clone()))
            .set_json(serde_json::json!({"org_ingredient_id":ing,"current_stock":stock,"reorder_threshold":reorder})).to_request());
        assert_eq!(st, 201);
    }

    // ── D. PO for beans → full receive → weighted-average cost ──
    // Buy 1 kg (=1000 g) at 20000 piastres/kg = 20/g. Prior 1000 g @ 10/g.
    let (_, po) = jget!(app, test::TestRequest::post()
        .uri(&format!("/purchasing/branches/{branch_id}/orders")).insert_header(("Authorization", abearer.clone()))
        .set_json(serde_json::json!({"supplier_id":supplier_id,"lines":[{"org_ingredient_id":beans,"purchase_unit":"kg","quantity_ordered":1.0,"unit_cost":20000}]})).to_request());
    let beans_po = po["id"].as_str().unwrap().to_string();
    let beans_line = po["lines"][0]["id"].as_str().unwrap().to_string();
    let (st, recv) = jget!(
        app,
        test::TestRequest::post()
            .uri(&format!("/purchasing/orders/{beans_po}/receive"))
            .insert_header(("Authorization", abearer.clone()))
            .set_json(serde_json::json!({"lines":[{"line_id":beans_line,"quantity_received":1.0}]}))
            .to_request()
    );
    assert_eq!(st, 200);
    assert_eq!(recv["status"], "received");
    // (1000×10 + 1000×20)/2000 = 15 piastres/g on the BRANCH cost; stock 2000 g.
    let beans_cost: f64 = sqlx::query_scalar("SELECT cost_per_unit::float8 FROM branch_inventory WHERE branch_id=$1 AND org_ingredient_id=$2").bind(branch_id).bind(Uuid::parse_str(&beans).unwrap()).fetch_one(&pool).await.unwrap();
    assert_eq!(beans_cost, 15.0);
    let beans_stock: f64 = sqlx::query_scalar("SELECT current_stock::float8 FROM branch_inventory WHERE branch_id=$1 AND org_ingredient_id=$2").bind(branch_id).bind(Uuid::parse_str(&beans).unwrap()).fetch_one(&pool).await.unwrap();
    assert_eq!(beans_stock, 2000.0);

    // ── E. PO for milk → partial then complete receive ──
    let (_, po) = jget!(app, test::TestRequest::post()
        .uri(&format!("/purchasing/branches/{branch_id}/orders")).insert_header(("Authorization", abearer.clone()))
        .set_json(serde_json::json!({"supplier_id":supplier_id,"lines":[{"org_ingredient_id":milk,"purchase_unit":"l","quantity_ordered":2.0,"unit_cost":6000}]})).to_request());
    let milk_po = po["id"].as_str().unwrap().to_string();
    let milk_line = po["lines"][0]["id"].as_str().unwrap().to_string();
    // Receive 1 of 2 litres → partially_received.
    let (_, r1) = jget!(
        app,
        test::TestRequest::post()
            .uri(&format!("/purchasing/orders/{milk_po}/receive"))
            .insert_header(("Authorization", abearer.clone()))
            .set_json(serde_json::json!({"lines":[{"line_id":milk_line,"quantity_received":1.0}]}))
            .to_request()
    );
    assert_eq!(r1["status"], "partially_received");
    // Receive the rest → received.
    let (_, r2) = jget!(
        app,
        test::TestRequest::post()
            .uri(&format!("/purchasing/orders/{milk_po}/receive"))
            .insert_header(("Authorization", abearer.clone()))
            .set_json(serde_json::json!({"lines":[{"line_id":milk_line,"quantity_received":1.0}]}))
            .to_request()
    );
    assert_eq!(r2["status"], "received");
    let milk_stock: f64 = sqlx::query_scalar("SELECT current_stock::float8 FROM branch_inventory WHERE branch_id=$1 AND org_ingredient_id=$2").bind(branch_id).bind(Uuid::parse_str(&milk).unwrap()).fetch_one(&pool).await.unwrap();
    assert_eq!(milk_stock, 3000.0); // 1000 + 1000 + 1000

    // ── F. Menu + recipe + a sale that deducts beans ──
    let (_, cat) = jget!(
        app,
        test::TestRequest::post()
            .uri("/categories")
            .insert_header(("Authorization", abearer.clone()))
            .set_json(serde_json::json!({"org_id":org_id,"name":"Coffee"}))
            .to_request()
    );
    let cat_id = cat["id"].as_str().unwrap().to_string();
    let (mi_st, mi) = jget!(app, test::TestRequest::post()
        .uri("/menu-items").insert_header(("Authorization", abearer.clone()))
        .set_json(serde_json::json!({"org_id":org_id,"category_id":cat_id,"name":"Latte","base_price":300})).to_request());
    assert_eq!(mi_st, 201, "menu-item create: {mi:?}");
    let latte = mi["id"].as_str().unwrap().to_string();
    let st = jstatus!(app, test::TestRequest::post()
        .uri(&format!("/recipes/drinks/{latte}")).insert_header(("Authorization", abearer.clone()))
        .set_json(serde_json::json!({"size_label":"one_size","org_ingredient_id":beans,"ingredient_name":"Beans","ingredient_unit":"g","quantity_used":18.0})).to_request());
    assert_eq!(st, 200);
    let (_, shift) = jget!(
        app,
        test::TestRequest::post()
            .uri(&format!("/shifts/branches/{branch_id}/open"))
            .insert_header(("Authorization", tbearer.clone()))
            .set_json(serde_json::json!({"opening_cash":0}))
            .to_request()
    );
    let shift_id = shift["id"].as_str().unwrap().to_string();
    let st = jstatus!(app, test::TestRequest::post()
        .uri("/orders").insert_header(("Authorization", tbearer.clone()))
        .set_json(serde_json::json!({"branch_id":branch_id,"shift_id":shift_id,"payment_method":"cash",
            "items":[{"menu_item_id":latte,"size_label":"one_size","quantity":2,"addons":[],"optional_field_ids":[],"bundle_components":[]}]})).to_request());
    assert_eq!(st, 201);
    // Beans: 2000 − 2×18 = 1964; a 'sale' movement was posted.
    let beans_stock: f64 = sqlx::query_scalar("SELECT current_stock::float8 FROM branch_inventory WHERE branch_id=$1 AND org_ingredient_id=$2").bind(branch_id).bind(Uuid::parse_str(&beans).unwrap()).fetch_one(&pool).await.unwrap();
    assert_eq!(beans_stock, 1964.0);
    let sale_qty: f64 = sqlx::query_scalar("SELECT COALESCE(SUM(-quantity),0)::float8 FROM inventory_movements WHERE type='sale' AND org_ingredient_id=$1").bind(Uuid::parse_str(&beans).unwrap()).fetch_one(&pool).await.unwrap();
    assert_eq!(sale_qty, 36.0);

    // ── G. Waste milk ──
    let st = jstatus!(
        app,
        test::TestRequest::post()
            .uri(&format!("/inventory/branches/{branch_id}/waste"))
            .insert_header(("Authorization", abearer.clone()))
            .set_json(
                serde_json::json!({"org_ingredient_id":milk,"quantity":100.0,"reason":"spoiled"})
            )
            .to_request()
    );
    assert_eq!(st, 201);

    // ── H. Stock count with the variance guardrail ──
    let (st, full) = jget!(
        app,
        test::TestRequest::post()
            .uri(&format!("/stocktakes/branches/{branch_id}"))
            .insert_header(("Authorization", abearer.clone()))
            .set_json(serde_json::json!({}))
            .to_request()
    );
    assert_eq!(st, 201);
    assert_eq!(full["variance_threshold_pct"], 10.0);
    let stocktake_id = full["id"].as_str().unwrap().to_string();
    // Count beans 1964 → 1500 (≈24% shrinkage = flagged).
    let st = jstatus!(
        app,
        test::TestRequest::put()
            .uri(&format!("/stocktakes/{stocktake_id}/items"))
            .insert_header(("Authorization", abearer.clone()))
            .set_json(
                serde_json::json!({"items":[{"org_ingredient_id":beans,"counted_qty":1500.0}]})
            )
            .to_request()
    );
    assert_eq!(st, 200);
    // Finalize blocked until a reason is given.
    let st = jstatus!(
        app,
        test::TestRequest::post()
            .uri(&format!("/stocktakes/{stocktake_id}/finalize"))
            .insert_header(("Authorization", abearer.clone()))
            .to_request()
    );
    assert_eq!(st, 409);
    // Provide a reason → finalize succeeds.
    let st = jstatus!(app, test::TestRequest::put()
        .uri(&format!("/stocktakes/{stocktake_id}/items")).insert_header(("Authorization", abearer.clone()))
        .set_json(serde_json::json!({"items":[{"org_ingredient_id":beans,"counted_qty":1500.0,"variance_reason":"theft"}]})).to_request());
    assert_eq!(st, 200);
    let st = jstatus!(
        app,
        test::TestRequest::post()
            .uri(&format!("/stocktakes/{stocktake_id}/finalize"))
            .insert_header(("Authorization", abearer.clone()))
            .to_request()
    );
    assert_eq!(st, 200);
    let beans_stock: f64 = sqlx::query_scalar("SELECT current_stock::float8 FROM branch_inventory WHERE branch_id=$1 AND org_ingredient_id=$2").bind(branch_id).bind(Uuid::parse_str(&beans).unwrap()).fetch_one(&pool).await.unwrap();
    assert_eq!(beans_stock, 1500.0);

    // ── I. Reports reflect everything ──
    // Valuation (org): both ingredients have a known cost.
    let (_, val) = jget!(
        app,
        test::TestRequest::get()
            .uri(&format!("/reports/orgs/{org_id}/inventory-valuation"))
            .insert_header(("Authorization", abearer.clone()))
            .to_request()
    );
    assert_eq!(val["unknown_cost_count"], 0);
    assert!(val["total_value"].as_i64().unwrap() > 0);

    // Low-stock (org): milk (2900 < 5000) flagged, carrying its supplier.
    let (_, low) = jget!(
        app,
        test::TestRequest::get()
            .uri(&format!("/reports/orgs/{org_id}/low-stock"))
            .insert_header(("Authorization", abearer.clone()))
            .to_request()
    );
    let milk_low = low
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["org_ingredient_id"] == serde_json::json!(milk))
        .expect("milk should be low");
    assert_eq!(milk_low["supplier_name"], "Beans Co");

    // Consumption (branch): beans 36 (sale), milk 100 (waste).
    let (_, cons) = jget!(
        app,
        test::TestRequest::get()
            .uri(&format!("/reports/branches/{branch_id}/consumption"))
            .insert_header(("Authorization", abearer.clone()))
            .to_request()
    );
    let beans_cons = cons
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["org_ingredient_id"] == serde_json::json!(beans))
        .unwrap();
    assert_eq!(beans_cons["consumed_qty"].as_f64().unwrap(), 36.0);
    let milk_cons = cons
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["org_ingredient_id"] == serde_json::json!(milk))
        .unwrap();
    assert_eq!(milk_cons["consumed_qty"].as_f64().unwrap(), 100.0);

    // Shrinkage (branch): beans, reason 'theft', 464 lost.
    let (_, shr) = jget!(
        app,
        test::TestRequest::get()
            .uri(&format!("/reports/branches/{branch_id}/shrinkage"))
            .insert_header(("Authorization", abearer.clone()))
            .to_request()
    );
    let beans_shr = shr
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["org_ingredient_id"] == serde_json::json!(beans))
        .unwrap();
    assert_eq!(beans_shr["reason"], "theft");
    assert_eq!(beans_shr["shrinkage_qty"].as_f64().unwrap(), 464.0);

    // Waste report (branch): milk spoiled 100.
    let (_, wr) = jget!(
        app,
        test::TestRequest::get()
            .uri(&format!("/reports/branches/{branch_id}/waste-report"))
            .insert_header(("Authorization", abearer.clone()))
            .to_request()
    );
    let milk_waste = wr
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["org_ingredient_id"] == serde_json::json!(milk))
        .unwrap();
    assert_eq!(milk_waste["reason"], "spoiled");
    assert_eq!(milk_waste["waste_qty"].as_f64().unwrap(), 100.0);
}
