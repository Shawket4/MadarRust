//! Lint rule fixtures: each test builds the smallest broken menu for one rule on the
//! unified tables and asserts the finding (and that a clean twin produces none).

use actix_web::{App, test, web};
use sqlx::PgPool;
use uuid::Uuid;

use super::{LintIssue, LintSeverity, lint_org};
use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;

async fn org(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Lint', $2)")
        .bind(id)
        .bind(format!("lint-{id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn item(pool: &PgPool, org: Uuid, name: &str) -> Uuid {
    let cat: Uuid =
        sqlx::query_scalar("INSERT INTO categories (org_id, name) VALUES ($1, $2) RETURNING id")
            .bind(org)
            .bind(name)
            .fetch_one(pool)
            .await
            .unwrap();
    sqlx::query_scalar(
        "INSERT INTO menu_items (org_id, category_id, name, base_price) VALUES ($1, $2, $3, 100) RETURNING id",
    )
    .bind(org)
    .bind(cat)
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn size(pool: &PgPool, item: Uuid, label: &str, sort: i32) -> Uuid {
    crate::test_support::seed_real_size(pool, item, label, 100, sort).await
}

async fn ingredient(pool: &PgPool, org: Uuid, name: &str, unit: &str, slug: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO org_ingredients (org_id, name, unit, category_id) \
         VALUES ($1, $2, $3::inventory_unit, ingredient_category_id($1, $4)) RETURNING id",
    )
    .bind(org)
    .bind(name)
    .bind(unit)
    .bind(slug)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn group(pool: &PgPool, org: Uuid, name: &str, ty: Option<&str>) -> Uuid {
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

async fn option(pool: &PgPool, group: Uuid, name: &str, price: i32) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO modifier_options (group_id, name, price) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(group)
    .bind(name)
    .bind(price)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn line(pool: &PgPool, owner_type: &str, owner: Uuid, ing: Uuid, qty: &str, unit: &str) {
    sqlx::query(
        "INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit) \
         VALUES ($1, $2, $3, $4::numeric, $5)",
    )
    .bind(owner_type)
    .bind(owner)
    .bind(ing)
    .bind(qty)
    .bind(unit)
    .execute(pool)
    .await
    .unwrap();
}

async fn attach(pool: &PgPool, item: Uuid, group: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO menu_item_modifier_groups (menu_item_id, group_id) VALUES ($1, $2) RETURNING id",
    )
    .bind(item)
    .bind(group)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn only<'a>(issues: &'a [LintIssue], rule: &str) -> Vec<&'a LintIssue> {
    issues.iter().filter(|i| i.rule == rule).collect()
}

/// A clean latte: Cup + Can with milk, packaging on both, a Milk group whose options
/// each carry one line in the ingredient's unit, and the recipe milk offered.
async fn clean_latte(pool: &PgPool, org: Uuid) -> (Uuid, Uuid, Uuid) {
    let latte = item(pool, org, "Latte").await;
    let cup = size(pool, latte, "Cup", 0).await;
    let can = size(pool, latte, "Can", 1).await;
    let whole = ingredient(pool, org, "Full Cream Milk", "g", "milk").await;
    let oat = ingredient(pool, org, "Oat Milk", "g", "milk").await;
    let cup12 = ingredient(pool, org, "12oz cup", "pcs", "packaging").await;
    for s in [cup, can] {
        line(pool, "item_size", s, whole, "250", "g").await;
        line(pool, "item_size", s, cup12, "1", "pcs").await;
    }
    let milk = group(pool, org, "Milk", Some("milk_type")).await;
    let o_whole = option(pool, milk, "Full Cream", 0).await;
    let o_oat = option(pool, milk, "Oat", 55).await;
    line(pool, "modifier_option", o_whole, whole, "1", "g").await;
    line(pool, "modifier_option", o_oat, oat, "1", "g").await;
    attach(pool, latte, milk).await;
    (latte, milk, cup)
}

#[sqlx::test]
async fn clean_menu_has_no_findings(pool: PgPool) {
    let org = org(&pool).await;
    clean_latte(&pool, org).await;
    let issues = lint_org(&pool, org).await.unwrap();
    assert!(issues.is_empty(), "{issues:#?}");
}

#[sqlx::test]
async fn f1_f3_null_provenance_from_legacy_writers(pool: PgPool) {
    let org = org(&pool).await;
    let (latte, _, _) = clean_latte(&pool, org).await;
    let extras = group(&pool, org, "Extras", Some("extra")).await;
    option(&pool, extras, "Shot", 40).await;
    // Simulate a pre-invariant writer: the fill trigger is what stops this today.
    sqlx::query("ALTER TABLE menu_item_modifier_groups DISABLE TRIGGER mimg_fill_provenance")
        .execute(&pool)
        .await
        .unwrap();
    let att = attach(&pool, latte, extras).await;
    let issues = lint_org(&pool, org).await.unwrap();

    let f1 = only(&issues, "F1");
    assert_eq!(f1.len(), 1, "{issues:#?}");
    assert_eq!(f1[0].entity_id, att);
    assert_eq!(f1[0].entity_type, "attachment");
    assert_eq!(f1[0].severity, LintSeverity::Error);

    let f3 = only(&issues, "F3");
    assert_eq!(
        f3.len(),
        1,
        "Milk restricted (materialized) + Extras NULL: {issues:#?}"
    );
    assert_eq!(f3[0].entity_id, latte);
    assert!(f3[0].message.contains("Extras"));
}

#[sqlx::test]
async fn f4_swap_group_without_a_milk_line(pool: PgPool) {
    let org = org(&pool).await;
    let (_, milk, _) = clean_latte(&pool, org).await;
    let tea = item(&pool, org, "Tea latte").await;
    let s = size(&pool, tea, "one_size", 0).await;
    let tea_leaf = ingredient(&pool, org, "Tea", "g", "general").await;
    line(&pool, "item_size", s, tea_leaf, "5", "g").await;
    attach(&pool, tea, milk).await;

    let issues = lint_org(&pool, org).await.unwrap();
    let f4 = only(&issues, "F4");
    assert_eq!(f4.len(), 1, "{issues:#?}");
    assert_eq!(f4[0].entity_name, "Tea latte");
    assert_eq!(f4[0].size_label.as_deref(), Some("one_size"));
}

#[sqlx::test]
async fn f6_f7_swap_options_with_zero_or_many_lines(pool: PgPool) {
    let org = org(&pool).await;
    let (_, milk, _) = clean_latte(&pool, org).await;
    let almond = option(&pool, milk, "Almond", 55).await; // no line → F6
    let soy = option(&pool, milk, "Soy", 55).await; // two lines → F7
    let soy_a = ingredient(&pool, org, "Soy A", "g", "milk").await;
    let soy_b = ingredient(&pool, org, "Soy B", "g", "milk").await;
    line(&pool, "modifier_option", soy, soy_a, "1", "g").await;
    line(&pool, "modifier_option", soy, soy_b, "1", "g").await;

    let issues = lint_org(&pool, org).await.unwrap();
    let f6 = only(&issues, "F6");
    assert_eq!(f6.len(), 1, "{issues:#?}");
    assert_eq!(
        (f6[0].entity_id, f6[0].entity_type.as_str()),
        (almond, "option")
    );
    let f7 = only(&issues, "F7");
    assert_eq!(f7.len(), 1, "{issues:#?}");
    assert_eq!(f7[0].entity_id, soy);
    assert!(f7[0].message.contains("Soy A"), "{}", f7[0].message);
}

#[sqlx::test]
async fn f9_f11_f14_f15_warnings(pool: PgPool) {
    let org = org(&pool).await;
    let (latte, milk, cup) = clean_latte(&pool, org).await;
    // F11: an option line in litres for a gram ingredient (Drops' Milk options).
    let skim = ingredient(&pool, org, "Skimmed Milk", "g", "milk").await;
    let o = option(&pool, milk, "Skimmed", 0).await;
    line(&pool, "modifier_option", o, skim, "1", "l").await;
    // F9: a milk nobody offers.
    let rifi = ingredient(&pool, org, "Rifi Full Cream Milk", "g", "milk").await;
    // F14: a lid outside packaging.
    let lid = ingredient(&pool, org, "Lid 12oz", "pcs", "general").await;
    // F15: a third size without any lines.
    let _ = (latte, cup);
    size(&pool, latte, "Jug", 2).await;

    let issues = lint_org(&pool, org).await.unwrap();
    let f11 = only(&issues, "F11");
    assert_eq!(f11.len(), 1, "{issues:#?}");
    assert_eq!(f11[0].entity_id, o);
    assert_eq!(f11[0].severity, LintSeverity::Warn);
    assert!(f11[0].message.contains("unit family"), "{}", f11[0].message);
    assert_eq!(
        only(&issues, "F9")
            .iter()
            .map(|i| i.entity_id)
            .collect::<Vec<_>>(),
        vec![rifi]
    );
    assert_eq!(
        only(&issues, "F14")
            .iter()
            .map(|i| i.entity_id)
            .collect::<Vec<_>>(),
        vec![lid]
    );
    let f15 = only(&issues, "F15");
    assert_eq!(f15.len(), 1, "{issues:#?}");
    assert_eq!(f15[0].size_label.as_deref(), Some("Jug"));
    // Every rule stays org-scoped.
    let other = self::org(&pool).await;
    assert!(lint_org(&pool, other).await.unwrap().is_empty());
}

#[sqlx::test]
async fn lint_endpoint_shape_and_auth(pool: PgPool) {
    let org = org(&pool).await;
    let (_, milk, _) = clean_latte(&pool, org).await;
    option(&pool, milk, "Almond", 55).await;
    let user = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, 'U', $3, 'h', 'org_admin'::user_role)",
    )
    .bind(user)
    .bind(org)
    .bind(format!("{user}@t.com"))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO role_permissions (role, resource, action, granted) \
         VALUES ('org_admin'::user_role, 'menu_items'::permission_resource, 'read'::permission_action, true) \
         ON CONFLICT DO NOTHING",
    )
    .execute(&pool)
    .await
    .unwrap();
    let secret = JwtSecret("secret".to_string());
    let token =
        crate::auth::jwt::create_token(&secret, user, Some(org), UserRole::OrgAdmin, None, 24)
            .unwrap();
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret))
            .configure(crate::menu::routes::configure),
    )
    .await;

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/menu/lint?org_id={org}"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = test::read_body_json(resp).await;
    let f6 = body
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["rule"] == "F6")
        .expect("F6 in body");
    assert_eq!(f6["severity"], "error");
    assert_eq!(f6["entity_type"], "option");
    assert_eq!(f6["entity_name"], "Milk / Almond");
    assert!(
        f6.get("size_label").is_none(),
        "size_label omitted when not size-scoped"
    );

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/menu/lint?org_id={}", Uuid::new_v4()))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 403, "cross-org lint is forbidden");
}

/// Operator run against a restored prod copy:
/// `DATABASE_URL_DROPS=postgres://…/drops LINT_ORG=<uuid> cargo nextest run -E 'test(lint_restored_db)' --run-ignored only`
#[tokio::test]
#[ignore]
async fn lint_restored_db() {
    let url = std::env::var("DATABASE_URL_DROPS").expect("DATABASE_URL_DROPS");
    let org: Uuid = std::env::var("LINT_ORG")
        .expect("LINT_ORG")
        .parse()
        .unwrap();
    let pool = PgPool::connect(&url).await.unwrap();
    let issues = lint_org(&pool, org).await.unwrap();
    let out = std::env::var("LINT_OUT").unwrap_or_else(|_| "lint_out.json".into());
    std::fs::write(&out, serde_json::to_string_pretty(&issues).unwrap()).unwrap();
}

/// A person with an ORG role (`org_id`, `role`) and optional per-person deny of
/// `menu_items read` (the legacy `permissions` row, synced into `user_overrides`).
async fn staff_token(
    pool: &PgPool,
    org: Uuid,
    role: &str,
    kind: UserRole,
    deny_read: bool,
) -> String {
    let user: Uuid = sqlx::query_scalar(
        "INSERT INTO users (org_id, name, email, password_hash, role) \
         VALUES ($1, $2, $3, 'h', $2::user_role) RETURNING id",
    )
    .bind(org)
    .bind(role)
    .bind(format!("{}@lint.test", Uuid::new_v4()))
    .fetch_one(pool)
    .await
    .unwrap();
    if deny_read {
        sqlx::query(
            "INSERT INTO permissions (user_id, resource, action, granted) \
             VALUES ($1, 'menu_items'::permission_resource, 'read'::permission_action, false)",
        )
        .bind(user)
        .execute(pool)
        .await
        .unwrap();
    }
    crate::auth::jwt::create_token(&JwtSecret("secret".into()), user, Some(org), kind, None, 24)
        .unwrap()
}

/// `GET /menu/lint` is `menu.items.read`: refused to a person denied it, served to
/// a teller (core `omtw`).
#[sqlx::test]
async fn lint_is_refused_without_menu_read_and_served_to_a_teller(pool: PgPool) {
    let org = org(&pool).await;
    clean_latte(&pool, org).await;
    let denied = staff_token(&pool, org, "org_admin", UserRole::OrgAdmin, true).await;
    let teller = staff_token(&pool, org, "teller", UserRole::Teller, false).await;
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(JwtSecret("secret".to_string())))
            .configure(crate::menu::routes::configure),
    )
    .await;
    for (who, token, want) in [("denied", &denied, 403), ("teller", &teller, 200)] {
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/menu/lint?org_id={org}"))
                .insert_header(("Authorization", format!("Bearer {token}")))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status().as_u16(), want, "{who}");
    }
}
