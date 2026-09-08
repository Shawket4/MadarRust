//! Loyalty end to end: settings inheritance and override, public signup with and
//! without OTP, the teller's scan, automatic earning at checkout (including the
//! replay path that must not double-award), redemption, and tenant isolation.

use actix_web::http::StatusCode;
use actix_web::{App, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::{JwtSecret, create_token};
use crate::models::UserRole;

fn secret() -> JwtSecret {
    JwtSecret("secret".into())
}
fn token(uid: Uuid, org: Uuid, role: UserRole, branch: Option<Uuid>) -> String {
    create_token(&secret(), uid, Some(org), role, branch, 24).unwrap()
}

async fn seed_org(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org', $2)")
        .bind(id)
        .bind(format!("org-{id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn seed_branch(pool: &PgPool, org: Uuid, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(org)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn seed_user(pool: &PgPool, org: Uuid, role: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, $3, $4, 'h', $5::user_role)",
    )
    .bind(id)
    .bind(org)
    .bind(format!("{role}-{id}"))
    .bind(format!("{id}@t.com"))
    .bind(role)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn seed_menu_item(pool: &PgPool, org: Uuid, name: &str, price: i32) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO menu_items (id, org_id, name, base_price) VALUES ($1,$2,$3,$4)")
        .bind(id)
        .bind(org)
        .bind(name)
        .bind(price)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn perms(pool: &PgPool) {
    crate::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
}

/// Turn the program on for an org (the org-wide default row).
async fn enable_program(pool: &PgPool, org: Uuid, rate: i32, threshold: i32, require_otp: bool) {
    enable_program_mode(pool, org, "points", rate, threshold, require_otp).await;
}

async fn enable_program_mode(
    pool: &PgPool,
    org: Uuid,
    mode: &str,
    rate: i32,
    default_cost: i32,
    require_otp: bool,
) {
    sqlx::query(
        "INSERT INTO loyalty_settings \
            (org_id, branch_id, enabled, mode, earn_piastres_per_point, default_reward_cost, \
             require_otp) \
         VALUES ($1, NULL, true, $2, $3, $4, $5)",
    )
    .bind(org)
    .bind(mode)
    .bind(rate)
    .bind(default_cost)
    .bind(require_otp)
    .execute(pool)
    .await
    .unwrap();
}

/// Put a menu item in the org-wide reward catalogue at a given price.
async fn seed_reward(pool: &PgPool, org: Uuid, item: Uuid, currency: &str, cost: i32) {
    sqlx::query(
        "INSERT INTO loyalty_reward_items (org_id, menu_item_id, cost_currency, cost_amount) \
         VALUES ($1,$2,$3,$4)",
    )
    .bind(org)
    .bind(item)
    .bind(currency)
    .bind(cost)
    .execute(pool)
    .await
    .unwrap();
}

async fn visits_of(pool: &PgPool, member: Uuid) -> i32 {
    sqlx::query_scalar("SELECT visits_balance FROM loyalty_customers WHERE id = $1")
        .bind(member)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Grant a balance directly, so a test can start from "the customer has earned".
async fn grant(pool: &PgPool, org: Uuid, member: Uuid, branch: Uuid, currency: &str, n: i32) {
    sqlx::query(
        "INSERT INTO loyalty_transactions (org_id, customer_id, branch_id, kind, currency, points) \
         VALUES ($1,$2,$3,'adjust',$4,$5)",
    )
    .bind(org)
    .bind(member)
    .bind(branch)
    .bind(currency)
    .bind(n)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_member(pool: &PgPool, org: Uuid, phone: &str, token: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO loyalty_customers (org_id, phone, name, member_token) \
         VALUES ($1,$2,'Ali',$3) RETURNING id",
    )
    .bind(org)
    .bind(phone)
    .bind(token)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn app_data(pool: &PgPool) -> (web::Data<PgPool>, web::Data<JwtSecret>) {
    (web::Data::new(pool.clone()), web::Data::new(secret()))
}

// ── Settings: the org default and the branch override ────────────────────────

#[sqlx::test]
async fn a_branch_inherits_the_org_default_until_it_overrides_it(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org, "Maadi").await;
    let admin = seed_user(&pool, org, "org_admin").await;
    enable_program(&pool, org, 1000, 100, true).await;

    let (p, s) = app_data(&pool);
    let app = test::init_service(
        App::new()
            .app_data(p)
            .app_data(s)
            .configure(super::routes::configure),
    )
    .await;
    let jwt = token(admin, org, UserRole::OrgAdmin, None);

    // Inherited: the branch reports the org's numbers.
    let req = test::TestRequest::get()
        .uri(&format!("/loyalty/settings?branch_id={branch}"))
        .insert_header(("Authorization", format!("Bearer {jwt}")))
        .to_request();
    let body: Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(body["default_reward_cost"], 100);
    assert_eq!(body["earn_piastres_per_point"], 1000);
    assert_eq!(body["mode"], "points");

    // Override just this branch.
    let req = test::TestRequest::put()
        .uri("/loyalty/settings")
        .insert_header(("Authorization", format!("Bearer {jwt}")))
        .set_json(json!({
            "org_id": org, "branch_id": branch, "enabled": true,
            "program_name": "Maadi Rewards", "mode": "visits",
            "earn_piastres_per_point": 500,
            "earn_on_discounted": true, "earn_include_tax": false,
            "default_reward_cost": 5, "require_otp": false
        }))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), StatusCode::OK);

    let req = test::TestRequest::get()
        .uri(&format!("/loyalty/settings?branch_id={branch}"))
        .insert_header(("Authorization", format!("Bearer {jwt}")))
        .to_request();
    let body: Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(body["default_reward_cost"], 5);
    // One branch may collect stamps while the org collects points.
    assert_eq!(body["mode"], "visits");
    assert_eq!(body["earn_piastres_per_point"], 500);

    // The org default is untouched — an override is local to its branch.
    let req = test::TestRequest::get()
        .uri("/loyalty/settings")
        .insert_header(("Authorization", format!("Bearer {jwt}")))
        .to_request();
    let body: Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(body["default_reward_cost"], 100);
    assert_eq!(body["mode"], "points");

    // Removing the override puts the branch back on the org default.
    let req = test::TestRequest::delete()
        .uri(&format!("/loyalty/settings?branch_id={branch}"))
        .insert_header(("Authorization", format!("Bearer {jwt}")))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::NO_CONTENT
    );
    let req = test::TestRequest::get()
        .uri(&format!("/loyalty/settings?branch_id={branch}"))
        .insert_header(("Authorization", format!("Bearer {jwt}")))
        .to_request();
    let body: Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(body["default_reward_cost"], 100);
    assert_eq!(body["mode"], "points");
}

#[sqlx::test]
async fn the_reward_catalogue_refuses_another_tenants_menu_item(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let other = seed_org(&pool).await;
    let admin = seed_user(&pool, org, "org_admin").await;
    let theirs = seed_menu_item(&pool, other, "Their Latte", 5000).await;

    let (p, s) = app_data(&pool);
    let app = test::init_service(
        App::new()
            .app_data(p)
            .app_data(s)
            .configure(super::routes::configure),
    )
    .await;
    let jwt = token(admin, org, UserRole::OrgAdmin, None);

    let req = test::TestRequest::put()
        .uri("/loyalty/reward-items")
        .insert_header(("Authorization", format!("Bearer {jwt}")))
        .set_json(json!({ "items": [{ "menu_item_id": theirs }] }))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::BAD_REQUEST
    );
}

/// The catalogue survives the program changing what it collects.
///
/// The order the dashboard's own tabs invite: add rewards, then set the mode.
/// The rows are stamped `points` and the settings then say `visits`, and every
/// read used to filter on the row's stored currency — so the till offered
/// nothing at all, and `cheapest_cost` matched nothing so the card's target
/// fell back to the program default. The dashboard relabelled the same rows
/// with the CURRENT mode, so it showed a full, healthy catalogue throughout.
/// Nothing anywhere reported a disagreement.
#[sqlx::test]
async fn a_catalogue_priced_before_the_mode_changed_is_still_claimable(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org, "Maadi").await;
    let teller = seed_user(&pool, org, "teller").await;
    // The program collects STAMPS...
    enable_program_mode(&pool, org, "visits", 1000, 8, false).await;
    let latte = seed_menu_item(&pool, org, "Latte", 5000).await;
    // ...but the catalogue was saved while it still collected points.
    seed_reward(&pool, org, latte, "points", 5).await;

    let member = seed_member(&pool, org, "201000000021", "Mstalecurrency000000001").await;
    // Comfortably more stamps than the reward costs — "they have more than
    // orders", which is what made the empty list so baffling.
    sqlx::query(
        "INSERT INTO loyalty_transactions (org_id, customer_id, branch_id, kind, currency, points) \
         VALUES ($1,$2,$3,'adjust','visits',9)",
    )
    .bind(org)
    .bind(member)
    .bind(branch)
    .execute(&pool)
    .await
    .unwrap();

    let (p, s) = app_data(&pool);
    let app = test::init_service(
        App::new()
            .app_data(p)
            .app_data(s)
            .configure(super::routes::configure),
    )
    .await;
    let jwt = token(teller, org, UserRole::Teller, Some(branch));

    let req = test::TestRequest::post()
        .uri("/loyalty/lookup")
        .insert_header(("Authorization", format!("Bearer {jwt}")))
        .set_json(json!({ "branch_id": branch, "token": "Mstalecurrency000000001" }))
        .to_request();
    let body: Value = test::call_and_read_body_json(&app, req).await;

    let rewards = body["rewards"].as_array().expect("a rewards array");
    assert_eq!(
        rewards.len(),
        1,
        "the till must offer the reward the dashboard lists: {body}"
    );
    assert_eq!(rewards[0]["menu_item_id"], latte.to_string());
    assert_eq!(
        rewards[0]["cost_currency"], "visits",
        "priced in what this branch actually collects, not what the row was stamped with"
    );
    assert_eq!(rewards[0]["cost_amount"], 5);
    // And the member's target is the reward's cost, not the program default —
    // `cheapest_cost` used to match nothing and fall back to 8.
    assert_eq!(body["member"]["next_reward_cost"], 5);
    assert_eq!(body["member"]["balance"], 9);
    assert_eq!(body["member"]["can_redeem"], true);
}

/// A shop's colours are a paid tier, and the gate is not per-surface.
///
/// The brand columns can be populated — a logo upload writes them whatever the
/// tier — so "has colours stored" must not mean "shows colours". Without the
/// gate in `orgs::branding::load` this would have to be remembered on the web
/// card, the signup page and two wallet passes, which is a rule that gets
/// missed exactly once and then ships a shop's branding to a tier it did not buy.
#[sqlx::test]
async fn a_shop_wears_madar_until_it_is_on_the_branding_tier(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org, "Maadi").await;
    enable_program(&pool, org, 1000, 100, false).await;
    sqlx::query(
        "UPDATE organizations SET logo_url = 'https://cdn.example/logos/rue.png', \
             brand_background = '#7B1E3A', brand_foreground = '#EFF3F4', \
             brand_accent = '#C8607F' WHERE id = $1",
    )
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();

    let (p, s) = app_data(&pool);
    let app = test::init_service(
        App::new()
            .app_data(p)
            .app_data(s)
            .configure(super::routes::configure),
    )
    .await;

    let info = |()| {
        test::TestRequest::get()
            .uri(&format!("/public/loyalty/join-info?branch_id={branch}"))
            .to_request()
    };
    let body: Value = test::call_and_read_body_json(&app, info(())).await;
    assert_eq!(
        body["brand"]["background_color"], "#0D6273",
        "off the tier, the card is Madar's: {body}"
    );
    assert!(
        body["brand"]["logo_url"].is_null(),
        "and wears Madar's mark"
    );
    // The shop's NAME is always its own — that is not what anyone is paying for,
    // and a card that does not say whose it is helps nobody.
    assert_eq!(body["brand"]["org_name"], "Org");

    sqlx::query("UPDATE organizations SET custom_branding = true WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
    let body: Value = test::call_and_read_body_json(&app, info(())).await;
    assert_eq!(body["brand"]["background_color"], "#7B1E3A");
    assert_eq!(
        body["brand"]["logo_url"],
        "https://cdn.example/logos/rue.png"
    );
}

/// One code for the whole shop.
///
/// A membership belongs to the SHOP — which is why the wallet pass has always
/// carried the org's programme and every branch's location. The only thing that
/// was ever per-branch was the way in, so a shop wanting one code on a poster
/// had to pick a branch and pretend.
#[sqlx::test]
async fn a_shop_can_hand_out_one_code_for_the_whole_organisation(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org, "Maadi").await;
    enable_program(&pool, org, 1000, 100, false).await;

    let (p, s) = app_data(&pool);
    let app = test::init_service(
        App::new()
            .app_data(p)
            .app_data(s)
            .configure(super::routes::configure),
    )
    .await;

    // The org code names no branch, and the page is told there is none rather
    // than being handed one it did not ask about.
    let body: Value = test::call_and_read_body_json(
        &app,
        test::TestRequest::get()
            .uri(&format!("/public/loyalty/join-info?org_id={org}"))
            .to_request(),
    )
    .await;
    assert!(body["branch_id"].is_null(), "{body}");
    assert!(body["branch_name"].is_null());
    assert_eq!(body["enabled"], true);
    assert_eq!(body["next_reward_cost"], 100);

    // Signing up through it makes a member of the SHOP.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/public/loyalty/join")
            .set_json(json!({ "org_id": org, "name": "Ali", "phone": "201000000061" }))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success());

    let (member_org, joined_branch): (Uuid, Option<Uuid>) =
        sqlx::query_as("SELECT org_id, joined_branch_id FROM loyalty_customers WHERE phone = $1")
            .bind("201000000061")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(member_org, org);
    assert!(
        joined_branch.is_none(),
        "we do not know where they were, and the membership does not need to"
    );

    // The branch code still works, and still records where they joined — the
    // cards already printed keep working untouched.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/public/loyalty/join")
            .set_json(json!({ "branch_id": branch, "name": "Sara", "phone": "201000000062" }))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success());
    let joined_branch: Option<Uuid> =
        sqlx::query_scalar("SELECT joined_branch_id FROM loyalty_customers WHERE phone = $1")
            .bind("201000000062")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(joined_branch, Some(branch));

    // Naming neither is refused rather than guessed at.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/public/loyalty/join-info")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// Every birthday the form can produce occurs every year — except one.
///
/// A (month, day) pair that survives `valid_birthday` is a real calendar day in
/// any year, because the impossible ones are refused at the door. The single
/// exception is the 29th of February, which exists in three years out of four,
/// and matching it exactly would mean a leap-day customer hears from the shop
/// once every four years. This exercises the sweep's own predicate against
/// known dates rather than trusting the reasoning above.
#[sqlx::test]
async fn a_leap_day_birthday_is_greeted_every_year(pool: PgPool) {
    // The sweep's match, with the date lifted out so it can be asked about a
    // day that is not today. Kept character-for-character in step with
    // `birthdays::run_tick` — if that changes and this does not, this test is
    // asserting about nothing.
    let matches = |month: i16, day: i16, on: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, bool>(
                "WITH t(d) AS (SELECT $3::date) \
                 SELECT ( \
                     ($1::smallint = EXTRACT(MONTH FROM t.d)::smallint \
                      AND $2::smallint = EXTRACT(DAY FROM t.d)::smallint) \
                     OR ($1::smallint = 2 AND $2::smallint = 29 \
                         AND EXTRACT(MONTH FROM t.d) = 2 AND EXTRACT(DAY FROM t.d) = 28 \
                         AND NOT (EXTRACT(DAY FROM (date_trunc('month', t.d) \
                                  + interval '1 month - 1 day')) = 29)) \
                 ) FROM t",
            )
            .bind(month)
            .bind(day)
            .bind(on)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };

    // 2024 is a leap year: the 29th exists, so it is greeted on the 29th and
    // NOT on the 28th — otherwise they would hear from the shop twice.
    assert!(
        matches(2, 29, "2024-02-29").await,
        "greeted on the real day"
    );
    assert!(
        !matches(2, 29, "2024-02-28").await,
        "not also greeted the day before, in a year that has a 29th"
    );

    // 2025 is not: the 29th never arrives, so the 28th stands in.
    assert!(
        matches(2, 29, "2025-02-28").await,
        "a leap-day customer must not wait four years"
    );
    assert!(
        !matches(2, 29, "2025-03-01").await,
        "and not on the 1st too"
    );

    // 1900 was not a leap year despite being divisible by 4. Postgres knows;
    // arithmetic on the year alone would not.
    assert!(matches(2, 29, "1900-02-28").await);
    assert!(matches(2, 29, "2000-02-29").await, "2000 WAS a leap year");
    assert!(!matches(2, 29, "2000-02-28").await);

    // Someone born on the 28th is greeted on the 28th, in both kinds of year,
    // and exactly once.
    for on in ["2024-02-28", "2025-02-28"] {
        assert!(matches(2, 28, on).await, "{on}");
    }
    assert!(!matches(2, 28, "2024-02-29").await);

    // And an ordinary birthday is unaffected by any of this.
    assert!(matches(3, 17, "2025-03-17").await);
    assert!(!matches(3, 17, "2025-03-16").await);
    assert!(matches(12, 31, "2025-12-31").await);
    assert!(matches(1, 1, "2026-01-01").await);
}

/// A field the shop turned off must not be storable by posting past the form.
#[sqlx::test]
async fn a_birthday_is_only_kept_where_the_shop_asked_for_one(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org, "Maadi").await;
    enable_program(&pool, org, 1000, 100, false).await;

    let (p, s) = app_data(&pool);
    let app = test::init_service(
        App::new()
            .app_data(p)
            .app_data(s)
            .configure(super::routes::configure),
    )
    .await;

    let join = |phone: &str| {
        test::TestRequest::post()
            .uri("/public/loyalty/join")
            .set_json(json!({
                "branch_id": branch, "name": "Ali", "phone": phone,
                "birth_month": 3, "birth_day": 17
            }))
            .to_request()
    };
    let stored = |phone: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (Option<i16>, Option<i16>)>(
                "SELECT birth_month, birth_day FROM loyalty_customers \
                  WHERE org_id = $1 AND phone = $2",
            )
            .bind(org)
            .bind(phone)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };

    // Birthdays are off, so the date is dropped however it arrived.
    assert!(
        test::call_service(&app, join("201000000051"))
            .await
            .status()
            .is_success()
    );
    assert_eq!(
        stored("201000000051").await,
        (None, None),
        "a birthday nobody asked for was kept"
    );

    // Switched on, the same post is honoured.
    sqlx::query("UPDATE loyalty_settings SET birthday_enabled = true WHERE org_id = $1")
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        test::call_service(&app, join("201000000052"))
            .await
            .status()
            .is_success()
    );
    assert_eq!(stored("201000000052").await, (Some(3), Some(17)));

    // A day that cannot exist is dropped rather than stored — 31 February is a
    // typo, and one kept would be a greeting that never fires. The signup still
    // succeeds: an optional field is not worth failing a signup over.
    let bad = test::TestRequest::post()
        .uri("/public/loyalty/join")
        .set_json(json!({
            "branch_id": branch, "name": "Sara", "phone": "201000000053",
            "birth_month": 2, "birth_day": 31
        }))
        .to_request();
    assert!(test::call_service(&app, bad).await.status().is_success());
    assert_eq!(stored("201000000053").await, (None, None));

    // Half a birthday is no birthday: a month with no day greets nobody, a day
    // with no month greets everybody twelve times.
    let half = test::TestRequest::post()
        .uri("/public/loyalty/join")
        .set_json(json!({
            "branch_id": branch, "name": "Omar", "phone": "201000000054",
            "birth_month": 5
        }))
        .to_request();
    assert!(test::call_service(&app, half).await.status().is_success());
    assert_eq!(stored("201000000054").await, (None, None));

    // The 29th of February is a real birthday and is kept as one.
    let leap = test::TestRequest::post()
        .uri("/public/loyalty/join")
        .set_json(json!({
            "branch_id": branch, "name": "Nour", "phone": "201000000055",
            "birth_month": 2, "birth_day": 29
        }))
        .to_request();
    assert!(test::call_service(&app, leap).await.status().is_success());
    assert_eq!(stored("201000000055").await, (Some(2), Some(29)));

    // And the page is told whether to show the field at all.
    let body: Value = test::call_and_read_body_json(
        &app,
        test::TestRequest::get()
            .uri(&format!("/public/loyalty/join-info?branch_id={branch}"))
            .to_request(),
    )
    .await;
    assert_eq!(body["birthday_enabled"], true);
}

// ── Public signup ────────────────────────────────────────────────────────────

#[sqlx::test]
async fn signup_without_otp_mints_a_member_and_a_token(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org, "Maadi").await;
    enable_program(&pool, org, 1000, 100, false).await;

    let (p, s) = app_data(&pool);
    let app = test::init_service(
        App::new()
            .app_data(p)
            .app_data(s)
            .configure(super::routes::configure),
    )
    .await;

    let req = test::TestRequest::post()
        .uri("/public/loyalty/join")
        .set_json(json!({ "branch_id": branch, "name": "Ali", "phone": "01000000001" }))
        .to_request();
    let body: Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(body["already_member"], false);
    assert_eq!(body["balance"], 0);
    let member_token = body["member_token"].as_str().unwrap().to_string();
    // The barcode value must not be the member's id.
    assert!(member_token.starts_with('M'));
    assert!(Uuid::parse_str(&member_token).is_err());

    // Rescanning the counter QR returns the SAME card, not a second member.
    let req = test::TestRequest::post()
        .uri("/public/loyalty/join")
        .set_json(json!({ "branch_id": branch, "name": "Ali", "phone": "01000000001" }))
        .to_request();
    let body: Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(body["already_member"], true);
    assert_eq!(body["member_token"], member_token);

    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM loyalty_customers WHERE org_id = $1")
        .bind(org)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

#[sqlx::test]
async fn signup_demands_the_otp_device_token_when_the_branch_asks_for_one(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org, "Maadi").await;
    enable_program(&pool, org, 1000, 100, true).await;

    let (p, s) = app_data(&pool);
    let app = test::init_service(
        App::new()
            .app_data(p)
            .app_data(s)
            .configure(super::routes::configure),
    )
    .await;

    let req = test::TestRequest::post()
        .uri("/public/loyalty/join")
        .set_json(json!({ "branch_id": branch, "name": "Ali", "phone": "01000000001" }))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::UNAUTHORIZED
    );

    // The very same device token the ordering flow issues is what unlocks it.
    let phone = crate::delivery::normalize_phone("01000000001").unwrap();
    let device = crate::delivery::whatsapp::issue_device_token(&secret().0, &phone).unwrap();
    let req = test::TestRequest::post()
        .uri("/public/loyalty/join")
        .set_json(json!({
            "branch_id": branch, "name": "Ali",
            "phone": "01000000001", "device_token": device
        }))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), StatusCode::OK);
}

#[sqlx::test]
async fn signup_is_refused_where_the_program_is_off(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org, "Maadi").await;
    // No settings row at all — the built-in default is "off".

    let (p, s) = app_data(&pool);
    let app = test::init_service(
        App::new()
            .app_data(p)
            .app_data(s)
            .configure(super::routes::configure),
    )
    .await;
    let req = test::TestRequest::post()
        .uri("/public/loyalty/join")
        .set_json(json!({ "branch_id": branch, "name": "Ali", "phone": "01000000001" }))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::CONFLICT
    );
}

// ── The teller's scan ────────────────────────────────────────────────────────

#[sqlx::test]
async fn a_scan_finds_the_member_and_hides_rewards_until_they_are_earned(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org, "Maadi").await;
    let teller = seed_user(&pool, org, "teller").await;
    enable_program(&pool, org, 1000, 100, false).await;
    let member = seed_member(&pool, org, "201000000001", "Mtesttoken0000000000001").await;
    let latte = seed_menu_item(&pool, org, "Latte", 5000).await;
    seed_reward(&pool, org, latte, "points", 100).await;

    let (p, s) = app_data(&pool);
    let app = test::init_service(
        App::new()
            .app_data(p)
            .app_data(s)
            .configure(super::routes::configure),
    )
    .await;
    let jwt = token(teller, org, UserRole::Teller, Some(branch));

    let scan = |body: Value| {
        test::TestRequest::post()
            .uri("/loyalty/lookup")
            .insert_header(("Authorization", format!("Bearer {jwt}")))
            .set_json(body)
            .to_request()
    };

    let body: Value = test::call_and_read_body_json(
        &app,
        scan(json!({ "branch_id": branch, "token": "Mtesttoken0000000000001" })),
    )
    .await;
    assert_eq!(body["member"]["balance"], 0);
    assert_eq!(body["member"]["mode"], "points");
    assert_eq!(body["member"]["points_to_next_reward"], 100);
    assert_eq!(body["member"]["can_redeem"], false);
    // Nothing on offer yet — the screen must not tempt a teller into giving a
    // reward that has not been earned.
    assert_eq!(body["rewards"].as_array().unwrap().len(), 0);

    // Once the balance is there, the catalogue appears.
    grant(&pool, org, member, branch, "points", 120).await;

    let body: Value = test::call_and_read_body_json(
        &app,
        scan(json!({ "branch_id": branch, "token": "Mtesttoken0000000000001" })),
    )
    .await;
    assert_eq!(body["member"]["can_redeem"], true);
    assert_eq!(body["rewards"][0]["name"], "Latte");

    // The manual fallback finds the same person.
    let body: Value = test::call_and_read_body_json(
        &app,
        scan(json!({ "branch_id": branch, "phone": "01000000001" })),
    )
    .await;
    assert_eq!(body["member"]["id"], member.to_string());
}

#[sqlx::test]
async fn one_tenants_card_is_invisible_to_another(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let other = seed_org(&pool).await;
    let branch = seed_branch(&pool, org, "Maadi").await;
    let teller = seed_user(&pool, org, "teller").await;
    enable_program(&pool, org, 1000, 100, false).await;
    // A member of a DIFFERENT tenant. The token is globally unique and carries
    // no org, so this is the case that must not resolve.
    seed_member(&pool, other, "201000000009", "Mforeigntoken000000001").await;

    let (p, s) = app_data(&pool);
    let app = test::init_service(
        App::new()
            .app_data(p)
            .app_data(s)
            .configure(super::routes::configure),
    )
    .await;
    let jwt = token(teller, org, UserRole::Teller, Some(branch));

    let req = test::TestRequest::post()
        .uri("/loyalty/lookup")
        .insert_header(("Authorization", format!("Bearer {jwt}")))
        .set_json(json!({ "branch_id": branch, "token": "Mforeigntoken000000001" }))
        .to_request();
    // "Not found", never "wrong org" — one tenant must not be able to probe
    // another's membership by scanning tokens.
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::NOT_FOUND
    );
}

// ── Redemption ───────────────────────────────────────────────────────────────

#[sqlx::test]
async fn a_teller_cannot_hand_out_points_by_hand(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org, "Maadi").await;
    let teller = seed_user(&pool, org, "teller").await;
    enable_program(&pool, org, 1000, 100, false).await;
    let member = seed_member(&pool, org, "201000000001", "Mtoken00000000000000002").await;

    let (p, s) = app_data(&pool);
    let app = test::init_service(
        App::new()
            .app_data(p)
            .app_data(s)
            .configure(super::routes::configure),
    )
    .await;

    let req = test::TestRequest::post()
        .uri("/loyalty/adjust")
        .insert_header((
            "Authorization",
            format!(
                "Bearer {}",
                token(teller, org, UserRole::Teller, Some(branch))
            ),
        ))
        .set_json(json!({ "branch_id": branch, "customer_id": member, "points": 5000 }))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::FORBIDDEN
    );

    // An admin may.
    let admin = seed_user(&pool, org, "org_admin").await;
    let req = test::TestRequest::post()
        .uri("/loyalty/adjust")
        .insert_header((
            "Authorization",
            format!("Bearer {}", token(admin, org, UserRole::OrgAdmin, None)),
        ))
        .set_json(json!({
            "branch_id": branch, "customer_id": member, "points": 20, "note": "goodwill"
        }))
        .to_request();
    let body: Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(body["points_balance"], 20);
}

// ── Earning at checkout ──────────────────────────────────────────────────────
// The heart of the program: the till sends WHO, the server decides HOW MANY.

async fn seed_cash_method(pool: &PgPool, org: Uuid) {
    sqlx::query(
        "INSERT INTO org_payment_methods (org_id, name, color, icon, is_cash, is_active) \
         VALUES ($1, 'cash', '#000', 'cash', true, true)",
    )
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
}

async fn open_shift_row(pool: &PgPool, branch: Uuid, teller: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO shifts (branch_id, teller_id, status, opening_cash) \
         VALUES ($1,$2,'open',0) RETURNING id",
    )
    .bind(branch)
    .bind(teller)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn balance_of(pool: &PgPool, member: Uuid) -> i32 {
    sqlx::query_scalar("SELECT points_balance FROM loyalty_customers WHERE id = $1")
        .bind(member)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Place a sale and return its (server id, idempotency key).
async fn place_order(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    jwt: &str,
    branch: Uuid,
    shift: Uuid,
    item: Uuid,
) -> (Uuid, Uuid) {
    let key = Uuid::new_v4();
    let req = test::TestRequest::post()
        .uri("/orders")
        .insert_header(("Authorization", format!("Bearer {jwt}")))
        .set_json(json!({
            "branch_id": branch, "shift_id": shift, "payment_method": "cash",
            "idempotency_key": key,
            "items": [{ "menu_item_id": item, "quantity": 1 }]
        }))
        .to_request();
    let resp = test::call_service(app, req).await;
    let status = resp.status();
    let body: Value = test::read_body_json(resp).await;
    assert!(status.is_success(), "order failed: {status} {body}");
    let id = Uuid::parse_str(body["id"].as_str().expect("order id")).unwrap();
    (id, key)
}

#[sqlx::test]
async fn a_sale_earns_nothing_until_the_button_is_pressed(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org, "Maadi").await;
    let teller = seed_user(&pool, org, "teller").await;
    seed_cash_method(&pool, org).await;
    let shift = open_shift_row(&pool, branch, teller).await;
    enable_program(&pool, org, 1000, 100, false).await;
    let member = seed_member(&pool, org, "201000000001", "Mawardtoken0000000001").await;
    let item = seed_menu_item(&pool, org, "Feast", 13_000).await;

    let (p, s) = app_data(&pool);
    let app = test::init_service(
        App::new()
            .app_data(p)
            .app_data(s)
            .configure(crate::orders::routes::configure)
            .configure(super::routes::configure),
    )
    .await;
    let jwt = token(teller, org, UserRole::Teller, Some(branch));

    let (order_id, _) = place_order(&app, &jwt, branch, shift, item).await;
    // Checkout alone awards NOTHING now — points are an explicit act.
    assert_eq!(balance_of(&pool, member).await, 0);

    // The button. 130 EGP at a point per 10 EGP = 13, computed from the ORDER.
    let req = test::TestRequest::post()
        .uri("/loyalty/award")
        .insert_header(("Authorization", format!("Bearer {jwt}")))
        .set_json(json!({
            "branch_id": branch, "order_id": order_id, "token": "Mawardtoken0000000001"
        }))
        .to_request();
    let body: Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(body["points_awarded"], 13);
    assert_eq!(body["already_awarded"], false);
    assert_eq!(body["member"]["points_balance"], 13);
    assert_eq!(balance_of(&pool, member).await, 13);

    // Pressing it twice is not two awards. The response still tells the truth.
    let req = test::TestRequest::post()
        .uri("/loyalty/award")
        .insert_header(("Authorization", format!("Bearer {jwt}")))
        .set_json(json!({
            "branch_id": branch, "order_id": order_id, "token": "Mawardtoken0000000001"
        }))
        .to_request();
    let body: Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(body["already_awarded"], true);
    assert_eq!(body["points_awarded"], 0);
    assert_eq!(balance_of(&pool, member).await, 13);
}

/// Scanning once, before payment, is enough.
///
/// The card was scanned at the till to spend a reward, and then had to be
/// scanned AGAIN to collect points for the same sale — two scans, two screens,
/// for one customer standing at one counter. The order now remembers who was
/// scanned, so the button on the receipt already knows.
#[sqlx::test]
async fn a_card_scanned_at_the_till_needs_no_second_scan_to_collect(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org, "Maadi").await;
    let teller = seed_user(&pool, org, "teller").await;
    seed_cash_method(&pool, org).await;
    let shift = open_shift_row(&pool, branch, teller).await;
    enable_program(&pool, org, 1000, 100, false).await;
    let member = seed_member(&pool, org, "201000000041", "Mscanoncetoken0000001").await;
    let item = seed_menu_item(&pool, org, "Feast", 13_000).await;

    let (p, s) = app_data(&pool);
    let app = test::init_service(
        App::new()
            .app_data(p)
            .app_data(s)
            .configure(crate::orders::routes::configure)
            .configure(super::routes::configure),
    )
    .await;
    let jwt = token(teller, org, UserRole::Teller, Some(branch));

    // The teller scans the card at tender. Nothing is redeemed — most sales
    // redeem nothing — but the customer has been identified.
    let key = Uuid::new_v4();
    let req = test::TestRequest::post()
        .uri("/orders")
        .insert_header(("Authorization", format!("Bearer {jwt}")))
        .set_json(json!({
            "branch_id": branch, "shift_id": shift, "payment_method": "cash",
            "idempotency_key": key,
            "loyalty_customer_id": member,
            "items": [{ "menu_item_id": item, "quantity": 1 }]
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());
    let body: Value = test::read_body_json(resp).await;
    let order_id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();

    // Checkout still awards nothing on its own.
    assert_eq!(balance_of(&pool, member).await, 0);

    // The button, naming NO member — the order carries one.
    let req = test::TestRequest::post()
        .uri("/loyalty/award")
        .insert_header(("Authorization", format!("Bearer {jwt}")))
        .set_json(json!({ "branch_id": branch, "order_id": order_id }))
        .to_request();
    let body: Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(body["points_awarded"], 13, "{body}");
    assert_eq!(body["member"]["id"], member.to_string());
    assert_eq!(balance_of(&pool, member).await, 13);
}

/// A sale nobody scanned still has to say so, rather than awarding to whoever.
#[sqlx::test]
async fn a_sale_with_no_card_scanned_still_asks_for_one(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org, "Maadi").await;
    let teller = seed_user(&pool, org, "teller").await;
    seed_cash_method(&pool, org).await;
    let shift = open_shift_row(&pool, branch, teller).await;
    enable_program(&pool, org, 1000, 100, false).await;
    let item = seed_menu_item(&pool, org, "Feast", 13_000).await;

    let (p, s) = app_data(&pool);
    let app = test::init_service(
        App::new()
            .app_data(p)
            .app_data(s)
            .configure(crate::orders::routes::configure)
            .configure(super::routes::configure),
    )
    .await;
    let jwt = token(teller, org, UserRole::Teller, Some(branch));
    let (order_id, _) = place_order(&app, &jwt, branch, shift, item).await;

    let req = test::TestRequest::post()
        .uri("/loyalty/award")
        .insert_header(("Authorization", format!("Bearer {jwt}")))
        .set_json(json!({ "branch_id": branch, "order_id": order_id }))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::BAD_REQUEST
    );
}

#[sqlx::test]
async fn the_server_refuses_an_award_after_the_window_even_if_the_client_asks(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org, "Maadi").await;
    let teller = seed_user(&pool, org, "teller").await;
    seed_cash_method(&pool, org).await;
    let shift = open_shift_row(&pool, branch, teller).await;
    enable_program(&pool, org, 1000, 100, false).await;
    let member = seed_member(&pool, org, "201000000001", "Mstaletoken0000000001").await;
    let item = seed_menu_item(&pool, org, "Feast", 13_000).await;

    let (p, s) = app_data(&pool);
    let app = test::init_service(
        App::new()
            .app_data(p)
            .app_data(s)
            .configure(crate::orders::routes::configure)
            .configure(super::routes::configure),
    )
    .await;
    let jwt = token(teller, org, UserRole::Teller, Some(branch));
    let (order_id, _) = place_order(&app, &jwt, branch, shift, item).await;

    // Age the sale past the window. This is the client-side check "slipping":
    // the till still sends the request, and the server is the one that says no.
    sqlx::query("UPDATE orders SET created_at = now() - interval '25 hours' WHERE id = $1")
        .bind(order_id)
        .execute(&pool)
        .await
        .unwrap();

    let req = test::TestRequest::post()
        .uri("/loyalty/award")
        .insert_header(("Authorization", format!("Bearer {jwt}")))
        .set_json(json!({
            "branch_id": branch, "order_id": order_id, "token": "Mstaletoken0000000001"
        }))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::CONFLICT
    );
    assert_eq!(balance_of(&pool, member).await, 0);

    // Nor can a client buy itself back into the window by post-dating the press.
    let req = test::TestRequest::post()
        .uri("/loyalty/award")
        .insert_header(("Authorization", format!("Bearer {jwt}")))
        .set_json(json!({
            "branch_id": branch, "order_id": order_id, "token": "Mstaletoken0000000001",
            "requested_at": (chrono::Utc::now() + chrono::Duration::hours(3)).to_rfc3339(),
        }))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::CONFLICT
    );
    assert_eq!(balance_of(&pool, member).await, 0);
}

#[sqlx::test]
async fn an_award_pressed_in_time_survives_a_long_offline_drain(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org, "Maadi").await;
    let teller = seed_user(&pool, org, "teller").await;
    seed_cash_method(&pool, org).await;
    let shift = open_shift_row(&pool, branch, teller).await;
    enable_program(&pool, org, 1000, 100, false).await;
    let member = seed_member(&pool, org, "201000000001", "Mqueuedtoken000000001").await;
    let item = seed_menu_item(&pool, org, "Feast", 13_000).await;

    let (p, s) = app_data(&pool);
    let app = test::init_service(
        App::new()
            .app_data(p)
            .app_data(s)
            .configure(crate::orders::routes::configure)
            .configure(super::routes::configure),
    )
    .await;
    let jwt = token(teller, org, UserRole::Teller, Some(branch));
    let (order_id, _) = place_order(&app, &jwt, branch, shift, item).await;

    // The sale is three days old and the till only reconnected now — but the
    // teller pressed the button an hour after the sale, and the queued op says
    // so. Losing that award would punish the customer for the till's outage.
    sqlx::query("UPDATE orders SET created_at = now() - interval '3 days' WHERE id = $1")
        .bind(order_id)
        .execute(&pool)
        .await
        .unwrap();
    let created: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT created_at FROM orders WHERE id = $1")
            .bind(order_id)
            .fetch_one(&pool)
            .await
            .unwrap();

    let req = test::TestRequest::post()
        .uri("/loyalty/award")
        .insert_header(("Authorization", format!("Bearer {jwt}")))
        .set_json(json!({
            "branch_id": branch, "order_id": order_id, "token": "Mqueuedtoken000000001",
            "requested_at": (created + chrono::Duration::hours(1)).to_rfc3339(),
        }))
        .to_request();
    let body: Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(body["points_awarded"], 13);
    assert_eq!(balance_of(&pool, member).await, 13);
}

#[sqlx::test]
async fn an_offline_sale_can_be_awarded_by_its_client_minted_key(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org, "Maadi").await;
    let teller = seed_user(&pool, org, "teller").await;
    seed_cash_method(&pool, org).await;
    let shift = open_shift_row(&pool, branch, teller).await;
    enable_program(&pool, org, 1000, 100, false).await;
    let member = seed_member(&pool, org, "201000000001", "Mbykeytoken0000000001").await;
    let item = seed_menu_item(&pool, org, "Feast", 13_000).await;

    let (p, s) = app_data(&pool);
    let app = test::init_service(
        App::new()
            .app_data(p)
            .app_data(s)
            .configure(crate::orders::routes::configure)
            .configure(super::routes::configure),
    )
    .await;
    let jwt = token(teller, org, UserRole::Teller, Some(branch));
    let (_, key) = place_order(&app, &jwt, branch, shift, item).await;

    // The till knows its own idempotency key, never the server's order id — so
    // an award queued beside an unsynced sale has to resolve through the key.
    let req = test::TestRequest::post()
        .uri("/loyalty/award")
        .insert_header(("Authorization", format!("Bearer {jwt}")))
        .set_json(json!({
            "branch_id": branch, "order_key": key, "token": "Mbykeytoken0000000001"
        }))
        .to_request();
    let body: Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(body["points_awarded"], 13);
    assert_eq!(balance_of(&pool, member).await, 13);
}

#[sqlx::test]
async fn a_voided_sale_earns_nothing(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org, "Maadi").await;
    let teller = seed_user(&pool, org, "teller").await;
    seed_cash_method(&pool, org).await;
    let shift = open_shift_row(&pool, branch, teller).await;
    enable_program(&pool, org, 1000, 100, false).await;
    let member = seed_member(&pool, org, "201000000001", "Mvoidtoken00000000001").await;
    let item = seed_menu_item(&pool, org, "Feast", 13_000).await;

    let (p, s) = app_data(&pool);
    let app = test::init_service(
        App::new()
            .app_data(p)
            .app_data(s)
            .configure(crate::orders::routes::configure)
            .configure(super::routes::configure),
    )
    .await;
    let jwt = token(teller, org, UserRole::Teller, Some(branch));
    let (order_id, _) = place_order(&app, &jwt, branch, shift, item).await;
    // The schema keeps voiding consistent: an order is voided only with a
    // status, a time AND a person.
    sqlx::query(
        "UPDATE orders SET voided_at = now(), voided_by = $2, status = 'voided', \
                void_reason = 'customer_request' WHERE id = $1",
    )
    .bind(order_id)
    .bind(teller)
    .execute(&pool)
    .await
    .unwrap();

    let req = test::TestRequest::post()
        .uri("/loyalty/award")
        .insert_header(("Authorization", format!("Bearer {jwt}")))
        .set_json(json!({
            "branch_id": branch, "order_id": order_id, "token": "Mvoidtoken00000000001"
        }))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::CONFLICT
    );
    assert_eq!(balance_of(&pool, member).await, 0);
}

#[sqlx::test]
async fn an_award_for_another_tenants_sale_is_refused(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let other = seed_org(&pool).await;
    let branch = seed_branch(&pool, org, "Maadi").await;
    let other_branch = seed_branch(&pool, other, "Theirs").await;
    let teller = seed_user(&pool, org, "teller").await;
    let their_teller = seed_user(&pool, other, "teller").await;
    seed_cash_method(&pool, org).await;
    seed_cash_method(&pool, other).await;
    let their_shift = open_shift_row(&pool, other_branch, their_teller).await;
    enable_program(&pool, org, 1000, 100, false).await;
    enable_program(&pool, other, 1000, 100, false).await;
    let member = seed_member(&pool, org, "201000000001", "Mcrossorgtoken0000001").await;
    let their_item = seed_menu_item(&pool, other, "Theirs", 13_000).await;

    let (p, s) = app_data(&pool);
    let app = test::init_service(
        App::new()
            .app_data(p)
            .app_data(s)
            .configure(crate::orders::routes::configure)
            .configure(super::routes::configure),
    )
    .await;
    let their_jwt = token(their_teller, other, UserRole::Teller, Some(other_branch));
    let (their_order, _) =
        place_order(&app, &their_jwt, other_branch, their_shift, their_item).await;

    // Our teller, naming another tenant's sale.
    let req = test::TestRequest::post()
        .uri("/loyalty/award")
        .insert_header((
            "Authorization",
            format!(
                "Bearer {}",
                token(teller, org, UserRole::Teller, Some(branch))
            ),
        ))
        .set_json(json!({
            "branch_id": branch, "order_id": their_order, "token": "Mcrossorgtoken0000001"
        }))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(balance_of(&pool, member).await, 0);
}

// ── Redemption: rewards covering lines of a cart ─────────────────────────────

/// Place a sale that spends a balance on some of its lines.
async fn place_with_rewards(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    jwt: &str,
    branch: Uuid,
    shift: Uuid,
    items: Value,
    member: Uuid,
    redemptions: Value,
) -> (actix_web::http::StatusCode, Value) {
    let req = test::TestRequest::post()
        .uri("/orders")
        .insert_header(("Authorization", format!("Bearer {jwt}")))
        .set_json(json!({
            "branch_id": branch, "shift_id": shift, "payment_method": "cash",
            "idempotency_key": Uuid::new_v4(),
            "loyalty_customer_id": member,
            "loyalty_redemptions": redemptions,
            "items": items
        }))
        .to_request();
    let resp = test::call_service(app, req).await;
    let status = resp.status();
    (status, test::read_body_json(resp).await)
}

#[sqlx::test]
async fn a_reward_covers_one_line_of_a_mixed_basket(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org, "Maadi").await;
    let teller = seed_user(&pool, org, "teller").await;
    seed_cash_method(&pool, org).await;
    let shift = open_shift_row(&pool, branch, teller).await;
    // A stamp card: 5 orders for a free coffee.
    enable_program_mode(&pool, org, "visits", 1000, 5, false).await;
    let member = seed_member(&pool, org, "201000000001", "Mmixedtoken000000001").await;
    let latte = seed_menu_item(&pool, org, "Latte", 5_000).await;
    let cake = seed_menu_item(&pool, org, "Cake", 9_000).await;
    seed_reward(&pool, org, latte, "visits", 5).await;
    grant(&pool, org, member, branch, "visits", 6).await;

    let (p, s) = app_data(&pool);
    let app = test::init_service(
        App::new()
            .app_data(p)
            .app_data(s)
            .configure(crate::orders::routes::configure)
            .configure(super::routes::configure),
    )
    .await;
    let jwt = token(teller, org, UserRole::Teller, Some(branch));

    // One latte (free) + one cake (paid).
    let (status, body) = place_with_rewards(
        &app,
        &jwt,
        branch,
        shift,
        json!([
            { "menu_item_id": latte, "quantity": 1 },
            { "menu_item_id": cake, "quantity": 1 }
        ]),
        member,
        json!([{ "item_index": 0 }]),
    )
    .await;
    assert!(status.is_success(), "{body}");
    // Only the cake is charged.
    assert_eq!(body["subtotal"], 9_000);
    // Five stamps spent, one left.
    assert_eq!(visits_of(&pool, member).await, 1);

    let (line_index, covered): (i32, i32) = sqlx::query_as(
        "SELECT order_line_index, points FROM loyalty_transactions \
          WHERE customer_id = $1 AND kind = 'redeem'",
    )
    .bind(member)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(line_index, 0);
    assert_eq!(covered, -5);

    // The free line is marked, so the receipt can say REWARD rather than
    // showing a mystery zero.
    let flags: Vec<(bool, i32)> = sqlx::query_as(
        "SELECT oi.is_reward, oi.line_total FROM order_items oi \
           JOIN orders o ON o.id = oi.order_id \
          WHERE o.loyalty_customer_id IS NOT NULL OR TRUE \
          ORDER BY oi.line_total",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        flags.contains(&(true, 0)),
        "the covered line is free and marked: {flags:?}"
    );
}

#[sqlx::test]
async fn two_rewards_on_one_basket_are_both_recorded(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org, "Maadi").await;
    let teller = seed_user(&pool, org, "teller").await;
    seed_cash_method(&pool, org).await;
    let shift = open_shift_row(&pool, branch, teller).await;
    enable_program_mode(&pool, org, "visits", 1000, 5, false).await;
    let member = seed_member(&pool, org, "201000000001", "Mtwotoken00000000001").await;
    let latte = seed_menu_item(&pool, org, "Latte", 5_000).await;
    let cake = seed_menu_item(&pool, org, "Cake", 9_000).await;
    // Different items at different prices — the thing one global threshold
    // could not express.
    seed_reward(&pool, org, latte, "visits", 5).await;
    seed_reward(&pool, org, cake, "visits", 10).await;
    grant(&pool, org, member, branch, "visits", 15).await;

    let (p, s) = app_data(&pool);
    let app = test::init_service(
        App::new()
            .app_data(p)
            .app_data(s)
            .configure(crate::orders::routes::configure)
            .configure(super::routes::configure),
    )
    .await;
    let jwt = token(teller, org, UserRole::Teller, Some(branch));

    let (status, body) = place_with_rewards(
        &app,
        &jwt,
        branch,
        shift,
        json!([
            { "menu_item_id": latte, "quantity": 1 },
            { "menu_item_id": cake, "quantity": 1 }
        ]),
        member,
        json!([{ "item_index": 0 }, { "item_index": 1 }]),
    )
    .await;
    assert!(status.is_success(), "{body}");
    assert_eq!(body["subtotal"], 0);
    assert_eq!(visits_of(&pool, member).await, 0);
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM loyalty_transactions WHERE customer_id = $1 AND kind = 'redeem'",
    )
    .bind(member)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 2);
}

#[sqlx::test]
async fn a_basket_that_outruns_the_balance_is_refused_whole(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org, "Maadi").await;
    let teller = seed_user(&pool, org, "teller").await;
    seed_cash_method(&pool, org).await;
    let shift = open_shift_row(&pool, branch, teller).await;
    enable_program_mode(&pool, org, "visits", 1000, 5, false).await;
    let member = seed_member(&pool, org, "201000000001", "Mshorttoken000000001").await;
    let latte = seed_menu_item(&pool, org, "Latte", 5_000).await;
    let cake = seed_menu_item(&pool, org, "Cake", 9_000).await;
    seed_reward(&pool, org, latte, "visits", 5).await;
    seed_reward(&pool, org, cake, "visits", 10).await;
    // Enough for the latte, not for both.
    grant(&pool, org, member, branch, "visits", 6).await;

    let (p, s) = app_data(&pool);
    let app = test::init_service(
        App::new()
            .app_data(p)
            .app_data(s)
            .configure(crate::orders::routes::configure)
            .configure(super::routes::configure),
    )
    .await;
    let jwt = token(teller, org, UserRole::Teller, Some(branch));

    let (status, _) = place_with_rewards(
        &app,
        &jwt,
        branch,
        shift,
        json!([
            { "menu_item_id": latte, "quantity": 1 },
            { "menu_item_id": cake, "quantity": 1 }
        ]),
        member,
        json!([{ "item_index": 0 }, { "item_index": 1 }]),
    )
    .await;
    // The whole sale fails rather than handing over half of what the teller
    // just told the customer they were getting.
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(visits_of(&pool, member).await, 6);
    let orders: i64 = sqlx::query_scalar("SELECT count(*) FROM orders")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(orders, 0, "a refused redemption leaves no order behind");
}

#[sqlx::test]
async fn an_item_that_is_not_a_reward_cannot_be_taken_as_one(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org, "Maadi").await;
    let teller = seed_user(&pool, org, "teller").await;
    seed_cash_method(&pool, org).await;
    let shift = open_shift_row(&pool, branch, teller).await;
    enable_program_mode(&pool, org, "visits", 1000, 5, false).await;
    let member = seed_member(&pool, org, "201000000001", "Mnotreward0000000001").await;
    let latte = seed_menu_item(&pool, org, "Latte", 5_000).await;
    let steak = seed_menu_item(&pool, org, "Steak", 90_000).await;
    seed_reward(&pool, org, latte, "visits", 5).await; // steak is NOT a reward
    grant(&pool, org, member, branch, "visits", 50).await;

    let (p, s) = app_data(&pool);
    let app = test::init_service(
        App::new()
            .app_data(p)
            .app_data(s)
            .configure(crate::orders::routes::configure)
            .configure(super::routes::configure),
    )
    .await;
    let jwt = token(teller, org, UserRole::Teller, Some(branch));

    let (status, _) = place_with_rewards(
        &app,
        &jwt,
        branch,
        shift,
        json!([{ "menu_item_id": steak, "quantity": 1 }]),
        member,
        json!([{ "item_index": 0 }]),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(visits_of(&pool, member).await, 50);
}

#[sqlx::test]
async fn a_reward_cannot_cover_more_units_than_the_line_holds(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org, "Maadi").await;
    let teller = seed_user(&pool, org, "teller").await;
    seed_cash_method(&pool, org).await;
    let shift = open_shift_row(&pool, branch, teller).await;
    enable_program_mode(&pool, org, "visits", 1000, 5, false).await;
    let member = seed_member(&pool, org, "201000000001", "Munits0000000000001").await;
    let latte = seed_menu_item(&pool, org, "Latte", 5_000).await;
    seed_reward(&pool, org, latte, "visits", 5).await;
    grant(&pool, org, member, branch, "visits", 100).await;

    let (p, s) = app_data(&pool);
    let app = test::init_service(
        App::new()
            .app_data(p)
            .app_data(s)
            .configure(crate::orders::routes::configure)
            .configure(super::routes::configure),
    )
    .await;
    let jwt = token(teller, org, UserRole::Teller, Some(branch));

    let (status, _) = place_with_rewards(
        &app,
        &jwt,
        branch,
        shift,
        json!([{ "menu_item_id": latte, "quantity": 2 }]),
        member,
        json!([{ "item_index": 0, "units": 3 }]),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Two of the two, on the other hand, is exactly what a customer with the
    // balance may do.
    let (status, body) = place_with_rewards(
        &app,
        &jwt,
        branch,
        shift,
        json!([{ "menu_item_id": latte, "quantity": 2 }]),
        member,
        json!([{ "item_index": 0, "units": 2 }]),
    )
    .await;
    assert!(status.is_success(), "{body}");
    assert_eq!(body["subtotal"], 0);
    assert_eq!(visits_of(&pool, member).await, 90);
}

#[sqlx::test]
async fn a_stamp_card_earns_one_per_order_whatever_the_bill(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org, "Maadi").await;
    let teller = seed_user(&pool, org, "teller").await;
    seed_cash_method(&pool, org).await;
    let shift = open_shift_row(&pool, branch, teller).await;
    enable_program_mode(&pool, org, "visits", 1000, 5, false).await;
    let member = seed_member(&pool, org, "201000000001", "Mstamp00000000000001").await;
    let item = seed_menu_item(&pool, org, "Feast", 130_000).await;

    let (p, s) = app_data(&pool);
    let app = test::init_service(
        App::new()
            .app_data(p)
            .app_data(s)
            .configure(crate::orders::routes::configure)
            .configure(super::routes::configure),
    )
    .await;
    let jwt = token(teller, org, UserRole::Teller, Some(branch));
    let (order_id, _) = place_order(&app, &jwt, branch, shift, item).await;

    let req = test::TestRequest::post()
        .uri("/loyalty/award")
        .insert_header(("Authorization", format!("Bearer {jwt}")))
        .set_json(json!({
            "branch_id": branch, "order_id": order_id, "token": "Mstamp00000000000001"
        }))
        .to_request();
    let body: Value = test::call_and_read_body_json(&app, req).await;
    // A 1300 EGP bill is still one stamp — and no points at all.
    assert_eq!(body["points_awarded"], 1);
    assert_eq!(visits_of(&pool, member).await, 1);
    assert_eq!(balance_of(&pool, member).await, 0);
}

/// A pass surfaces at THIS org's branches and nobody else's.
///
/// The coordinates come from a table shared by every tenant, so the query's
/// `org_id` filter is the whole boundary. Getting it wrong would put a rival
/// shop's address on a customer's lock screen — quiet, and only visible to the
/// customer standing outside the wrong door.
#[sqlx::test]
async fn pass_locations_are_scoped_to_the_org(pool: PgPool) {
    let org = seed_org(&pool).await;
    let other = seed_org(&pool).await;
    let mine = seed_branch(&pool, org, "Maadi").await;
    let theirs = seed_branch(&pool, other, "Their Branch").await;
    let no_coords = seed_branch(&pool, org, "Unmapped").await;

    for (b, lat, lng) in [(mine, 30.04, 31.23), (theirs, 25.20, 55.27)] {
        sqlx::query("UPDATE branches SET latitude = $2, longitude = $3 WHERE id = $1")
            .bind(b)
            .bind(lat)
            .bind(lng)
            .execute(&pool)
            .await
            .unwrap();
    }

    let locs = crate::loyalty::wallet::locations_for_org(&pool, org)
        .await
        .unwrap();
    let names: Vec<&str> = locs.iter().map(|l| l.name.as_str()).collect();
    assert_eq!(
        names,
        ["Maadi"],
        "only this org's mapped branches: {names:?}"
    );
    let _ = no_coords; // present, unmapped, and correctly absent from the pass.

    // A soft-deleted or deactivated branch stops surfacing — a customer should
    // not be sent to a shop that has closed.
    sqlx::query("UPDATE branches SET is_active = false WHERE id = $1")
        .bind(mine)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        crate::loyalty::wallet::locations_for_org(&pool, org)
            .await
            .unwrap()
            .is_empty(),
        "a closed branch must not stay on the pass"
    );
}
