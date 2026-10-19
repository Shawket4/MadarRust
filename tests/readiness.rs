//! Readiness (`GET /orgs/{id}/readiness`): a shop is ready when a cashier can
//! ring up and take payment, and live once it has an order. Every expectation
//! here is worked out by hand from what the POS needs (design doc "Onboarding
//! rebuild", 2026-10-19), not read back from the code.

use actix_web::{App, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::JwtSecret;
use madar_rust::models::UserRole;
use madar_rust::orgs::provision::{
    ProvisionBranch, ProvisionOpts, ProvisionOrgRequest, ProvisionOwner, provision,
};
use madar_rust::orgs::readiness::{Readiness, ReadinessCheck, load};

fn secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn token(user: Uuid, org: Uuid) -> String {
    madar_rust::auth::jwt::create_token(&secret(), user, Some(org), UserRole::OrgAdmin, None, 24)
        .unwrap()
}

struct Shop {
    org: Uuid,
    branch: Uuid,
    owner: Uuid,
}

/// A shop exactly as signup or the super admin makes it.
async fn new_shop(pool: &PgPool, plan: madar_plan::Plan, modules: Option<Vec<&str>>) -> Shop {
    let slug = format!("r-{}", &Uuid::new_v4().simple().to_string()[..10]);
    let p = provision(
        pool,
        ProvisionOrgRequest {
            name: "Bean There".into(),
            slug: slug.clone(),
            template: "cafe".into(),
            currency_code: None,
            timezone: None,
            tax_rate: None,
            branch: ProvisionBranch {
                name: "Maadi".into(),
                address: None,
                phone: None,
            },
            owner: ProvisionOwner {
                name: "Owner".into(),
                email: format!("{slug}@example.com"),
                password: "password123".into(),
                pin: None,
            },
            modules: modules.map(|m| m.into_iter().map(String::from).collect()),
        },
        ProvisionOpts {
            plan,
            ..ProvisionOpts::admin()
        },
    )
    .await
    .unwrap();
    Shop {
        org: p.org.id,
        branch: p.branch_id,
        owner: p.owner_id,
    }
}

fn check<'a>(r: &'a Readiness, key: &str) -> &'a ReadinessCheck {
    r.checks
        .iter()
        .find(|c| c.key == key)
        .unwrap_or_else(|| panic!("no {key} check"))
}

fn has(r: &Readiness, key: &str) -> bool {
    r.checks.iter().any(|c| c.key == key)
}

fn names(c: &ReadinessCheck) -> Vec<&str> {
    c.problems.iter().map(|p| p.name.as_str()).collect()
}

async fn category(pool: &PgPool, org: Uuid, name: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO categories (org_id, name) VALUES ($1, $2) RETURNING id")
        .bind(org)
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn item(pool: &PgPool, org: Uuid, cat: Option<Uuid>, name: &str, price: i32) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO menu_items (org_id, category_id, name, base_price, is_active) \
         VALUES ($1, $2, $3, $4, true) RETURNING id",
    )
    .bind(org)
    .bind(cat)
    .bind(name)
    .bind(price)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn set_pin(pool: &PgPool, user: Uuid) {
    sqlx::query("UPDATE users SET pin_hash = 'x' WHERE id = $1")
        .bind(user)
        .execute(pool)
        .await
        .unwrap();
}

async fn tablet(pool: &PgPool, org: Uuid, branch: Uuid) {
    sqlx::query(
        "INSERT INTO devices (id, org_id, branch_id, code, kind) VALUES ($1, $2, $3, $4, 'pos')",
    )
    .bind(Uuid::new_v4())
    .bind(org)
    .bind(branch)
    .bind(Uuid::new_v4().simple().to_string()[..6].to_uppercase())
    .execute(pool)
    .await
    .unwrap();
}

async fn second_branch(pool: &PgPool, org: Uuid) -> Uuid {
    sqlx::query_scalar("INSERT INTO branches (org_id, name) VALUES ($1, 'Zamalek') RETURNING id")
        .bind(org)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A fresh signup has a branch and three tenders, and nothing else a till needs.
/// Each fix turns exactly its own check green, and the order makes it live.
#[sqlx::test]
async fn a_new_shop_becomes_ready_then_live(pool: PgPool) {
    let s = new_shop(&pool, madar_plan::Plan::Free, None).await;

    let r = load(&pool, s.org).await.unwrap();
    assert!(!r.ready && !r.live);
    assert!(check(&r, "branch").ok);
    assert!(check(&r, "payment").ok);
    assert!(!check(&r, "menu").ok);
    assert_eq!(
        names(check(&r, "sign_in")),
        ["Maadi"],
        "signup gives the owner no PIN"
    );
    assert_eq!(names(check(&r, "tablet")), ["Maadi"]);
    assert!(!check(&r, "receipt").ok);
    assert!(
        !has(&r, "recipes") && !has(&r, "ingredient_costs"),
        "Free has no inventory"
    );

    let cat = category(&pool, s.org, "Coffee").await;
    item(&pool, s.org, Some(cat), "Latte", 7000).await;
    assert!(check(&load(&pool, s.org).await.unwrap(), "menu").ok);

    set_pin(&pool, s.owner).await;
    assert!(check(&load(&pool, s.org).await.unwrap(), "sign_in").ok);

    tablet(&pool, s.org, s.branch).await;
    let r = load(&pool, s.org).await.unwrap();
    assert!(check(&r, "tablet").ok);
    assert!(r.ready, "every blocking check passes: {:?}", r.checks);
    assert!(!r.live);

    let till: Uuid = sqlx::query_scalar(
        "INSERT INTO tills (branch_id, teller_id, opening_cash) VALUES ($1, $2, 0) RETURNING id",
    )
    .bind(s.branch)
    .bind(s.owner)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO orders (branch_id, teller_id, till_id, subtotal, tax_amount, total_amount, \
                             status, order_number, payment_method, order_ref) \
         VALUES ($1, $2, $3, 7000, 0, 7000, 'completed', 1, 'cash', 'R-1')",
    )
    .bind(s.branch)
    .bind(s.owner)
    .bind(till)
    .execute(&pool)
    .await
    .unwrap();
    let r = load(&pool, s.org).await.unwrap();
    assert!(r.ready && r.live && r.first_order_at.is_some());
}

/// Items that sell but read wrong on the till are named, never blocking.
#[sqlx::test]
async fn menu_advice_names_the_items(pool: PgPool) {
    let s = new_shop(&pool, madar_plan::Plan::Free, None).await;
    let cat = category(&pool, s.org, "Coffee").await;
    item(&pool, s.org, Some(cat), "Latte", 7000).await;
    item(&pool, s.org, None, "Loose cookie", 2500).await;
    let free = item(&pool, s.org, Some(cat), "Staff water", 0).await;
    // A sized item is priced by its sizes, so base_price 0 is fine there.
    let sized = item(&pool, s.org, Some(cat), "Mocha", 0).await;
    sqlx::query("INSERT INTO menu_item_sizes (menu_item_id, label, price) VALUES ($1, 'M', 8000)")
        .bind(sized)
        .execute(&pool)
        .await
        .unwrap();
    // A deleted category drops its items out of the grid.
    let gone = category(&pool, s.org, "Old").await;
    item(&pool, s.org, Some(gone), "Old tea", 3000).await;
    sqlx::query("UPDATE categories SET deleted_at = now() WHERE id = $1")
        .bind(gone)
        .execute(&pool)
        .await
        .unwrap();

    let r = load(&pool, s.org).await.unwrap();
    let nc = check(&r, "items_no_category");
    assert_eq!(names(nc), ["Loose cookie", "Old tea"]);
    assert_eq!(nc.problems[0].item_id, Some(nc.problems[0].entity_id));
    assert_eq!(names(check(&r, "items_zero_price")), ["Staff water"]);
    assert_eq!(check(&r, "items_zero_price").problems[0].entity_id, free);
    assert!(check(&r, "menu").ok, "advice never blocks the menu");
}

/// A required group whose options are all off: the POS hides it (F16).
#[sqlx::test]
async fn a_required_group_with_nothing_to_pick_is_named(pool: PgPool) {
    let s = new_shop(&pool, madar_plan::Plan::Free, None).await;
    let cat = category(&pool, s.org, "Coffee").await;
    let latte = item(&pool, s.org, Some(cat), "Latte", 7000).await;
    let group: Uuid = sqlx::query_scalar(
        "INSERT INTO modifier_groups (org_id, name, is_required, min_selections, max_selections) \
         VALUES ($1, 'Milk', true, 1, 1) RETURNING id",
    )
    .bind(s.org)
    .fetch_one(&pool)
    .await
    .unwrap();
    let opt: Uuid = sqlx::query_scalar(
        "INSERT INTO modifier_options (group_id, name, price, is_active) \
         VALUES ($1, 'Oat', 0, true) RETURNING id",
    )
    .bind(group)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO menu_item_modifier_groups (menu_item_id, group_id, legacy_origin) \
         VALUES ($1, $2, 'options')",
    )
    .bind(latte)
    .bind(group)
    .execute(&pool)
    .await
    .unwrap();
    let r = load(&pool, s.org).await.unwrap();
    assert!(check(&r, "required_group_empty").ok, "Oat is on");

    sqlx::query("UPDATE modifier_options SET is_active = false WHERE id = $1")
        .bind(opt)
        .execute(&pool)
        .await
        .unwrap();
    let r = load(&pool, s.org).await.unwrap();
    let c = check(&r, "required_group_empty");
    assert_eq!(names(c), ["Latte / Milk"]);
    assert_eq!(c.problems[0].rule.as_deref(), Some("F16"));
    assert_eq!(c.problems[0].group_id, Some(group));
}

/// Branch checks name each branch that fails, and a cashier who works at one
/// branch does not make the other one able to sign in.
#[sqlx::test]
async fn per_branch_checks_name_the_branch(pool: PgPool) {
    let s = new_shop(&pool, madar_plan::Plan::Paid, None).await;
    let zamalek = second_branch(&pool, s.org).await;
    tablet(&pool, s.org, s.branch).await;

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret()))
            .configure(madar_rust::users::routes::configure),
    )
    .await;
    let req = test::TestRequest::post()
        .uri("/users")
        .insert_header(("Authorization", format!("Bearer {}", token(s.owner, s.org))))
        .set_json(json!({
            "org_id": s.org, "name": "Mona", "role": "teller",
            "pin": "246810", "branch_ids": [s.branch],
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(
        resp.status().is_success(),
        "create teller: {:?}",
        resp.status()
    );

    let r = load(&pool, s.org).await.unwrap();
    let sign_in = check(&r, "sign_in");
    assert_eq!(names(sign_in), ["Zamalek"], "Mona works at Maadi only");
    assert_eq!(sign_in.total, Some(2));
    assert_eq!(names(check(&r, "tablet")), ["Zamalek"]);
    assert!(!r.ready);

    // Zamalek's allow-list names only a switched-off tender: nothing left to pay with.
    let off: Uuid = sqlx::query_scalar(
        "UPDATE org_payment_methods SET is_active = false \
          WHERE org_id = $1 AND name = 'card' RETURNING id",
    )
    .bind(s.org)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO branch_payment_methods (org_id, branch_id, payment_method_id) VALUES ($1, $2, $3)",
    )
    .bind(s.org)
    .bind(zamalek)
        .bind(off)
        .execute(&pool)
        .await
        .unwrap();
    let r = load(&pool, s.org).await.unwrap();
    assert_eq!(names(check(&r, "payment")), ["Zamalek"]);
    assert_eq!(check(&r, "team").count, 2);
    assert!(check(&r, "team").ok);
}

/// A paid shop gets the stock advice: recipes on every size, ingredient costs.
#[sqlx::test]
async fn paid_shops_get_recipe_and_cost_advice(pool: PgPool) {
    let s = new_shop(&pool, madar_plan::Plan::Paid, None).await;
    let cat = category(&pool, s.org, "Coffee").await;
    let latte = item(&pool, s.org, Some(cat), "Latte", 7000).await;
    item(&pool, s.org, Some(cat), "Tea", 3000).await;
    // Every item is born with one size (trigger menu_items_ensure_one_size).
    let size: Uuid = sqlx::query_scalar("SELECT id FROM menu_item_sizes WHERE menu_item_id = $1")
        .bind(latte)
        .fetch_one(&pool)
        .await
        .unwrap();
    let milk: Uuid = sqlx::query_scalar(
        "INSERT INTO org_ingredients (org_id, name, unit, cost_per_unit) \
         VALUES ($1, 'Milk', 'ml', 0) RETURNING id",
    )
    .bind(s.org)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit) \
         VALUES ('item_size', $1, $2, 200, 'ml')",
    )
    .bind(size)
    .bind(milk)
    .execute(&pool)
    .await
    .unwrap();

    let r = load(&pool, s.org).await.unwrap();
    let recipes = check(&r, "recipes");
    assert_eq!(
        names(recipes),
        ["Tea"],
        "Latte has a recipe on its one size"
    );
    assert_eq!(recipes.total, Some(2));
    assert_eq!(names(check(&r, "ingredient_costs")), ["Milk"]);
}

/// A Dawam-only org has no POS, so nothing to set up here and nothing blocks.
#[sqlx::test]
async fn a_dawam_only_org_has_no_pos_checks(pool: PgPool) {
    let s = new_shop(&pool, madar_plan::Plan::Paid, Some(vec!["dawam"])).await;
    let r = load(&pool, s.org).await.unwrap();
    assert!(r.checks.is_empty());
}

/// Over HTTP: the owner reads their shop; another org's id is refused; the
/// old onboarding routes answer "completed" so old Flutter builds skip their wizard.
#[sqlx::test]
async fn the_routes(pool: PgPool) {
    let s = new_shop(&pool, madar_plan::Plan::Free, None).await;
    let other = new_shop(&pool, madar_plan::Plan::Free, None).await;
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret()))
            .configure(madar_rust::orgs::routes::configure),
    )
    .await;
    let get = |uri: String, tok: String| {
        test::TestRequest::get()
            .uri(&uri)
            .insert_header(("Authorization", format!("Bearer {tok}")))
            .to_request()
    };

    let resp = test::call_service(
        &app,
        get(format!("/orgs/{}/readiness", s.org), token(s.owner, s.org)),
    )
    .await;
    assert!(resp.status().is_success(), "{:?}", resp.status());
    let body: Value = test::read_body_json(resp).await;
    assert_eq!(body["ready"], false);
    assert_eq!(body["checks"][0]["key"], "branch");
    assert_eq!(body["checks"][0]["level"], "blocking");

    let resp = test::call_service(
        &app,
        get(
            format!("/orgs/{}/readiness", other.org),
            token(s.owner, s.org),
        ),
    )
    .await;
    assert!(
        resp.status().is_client_error(),
        "another org: {:?}",
        resp.status()
    );

    let resp = test::call_service(
        &app,
        get(format!("/orgs/{}/onboarding", s.org), token(s.owner, s.org)),
    )
    .await;
    let body: Value = test::read_body_json(resp).await;
    assert_eq!(body["completed"], true);
    assert_eq!(body["steps"], json!([]));
    let req = test::TestRequest::post()
        .uri(&format!("/orgs/{}/onboarding/complete", s.org))
        .insert_header(("Authorization", format!("Bearer {}", token(s.owner, s.org))))
        .to_request();
    let body: Value = test::read_body_json(test::call_service(&app, req).await).await;
    assert_eq!(body["completed"], true);
}
