//! Partner analytics integration tests.
//!
//! The things that would actually hurt if they broke: a credential reading a
//! branch it was not issued for, voided/refunded orders leaking into a
//! partner's revenue, the delivery fee inflating an order total, and the
//! business-date window drifting off the branch's timezone.

use actix_web::{App, http::StatusCode, test, web};
use base64::Engine as _;
use chrono::{DateTime, Utc};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::integrations::routes;
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

fn basic(username: &str, secret: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{secret}"))
    )
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

// ── seeding ───────────────────────────────────────────────────

struct Seed {
    org: Uuid,
    branch: Uuid,
    shift: Uuid,
    teller: Uuid,
}

/// One org → branch (timezone `tz`) → till → shift → teller, ready for orders.
async fn seed(pool: &PgPool, label: &str, tz: Option<&str>) -> Seed {
    let org = Uuid::new_v4();
    let teller = Uuid::new_v4();
    let branch = Uuid::new_v4();
    let till = Uuid::new_v4();
    let shift = Uuid::new_v4();

    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, $2, $3)")
        .bind(org)
        .bind(format!("Org {label}"))
        .bind(format!("org-{}", org.simple()))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO users (id, name, role, org_id, pin_hash) VALUES ($1, $2, 'teller', $3, 'x')",
    )
    .bind(teller)
    .bind(format!("Teller {label}"))
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO branches (id, org_id, name, code, timezone)
         VALUES ($1, $2, $3, $4, $5::timezone_name)",
    )
    .bind(branch)
    .bind(org)
    .bind(format!("Branch {label}"))
    .bind(org.simple().to_string()[..6].to_uppercase())
    .bind(tz)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO tills (id, org_id, branch_id, name) VALUES ($1, $2, $3, 'Till')")
        .bind(till)
        .bind(org)
        .bind(branch)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO shifts (id, branch_id, teller_id, till_id) VALUES ($1, $2, $3, $4)")
        .bind(shift)
        .bind(branch)
        .bind(teller)
        .bind(till)
        .execute(pool)
        .await
        .unwrap();

    Seed {
        org,
        branch,
        shift,
        teller,
    }
}

#[allow(clippy::too_many_arguments)]
async fn seed_order(
    pool: &PgPool,
    s: &Seed,
    number: i32,
    status: &str,
    created_at: DateTime<Utc>,
    subtotal: i32,
    discount: i32,
    tax: i32,
    delivery_fee: i32,
    tip: i32,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO orders (id, branch_id, shift_id, teller_id, order_number, payment_method,
                             status, created_at, subtotal, discount_amount, tax_amount,
                             total_amount, delivery_fee, tip_amount, order_ref)
         VALUES ($1, $2, $3, $4, $5, 'cash', $6::order_status, $7, $8, $9, $10, $11, $12, $13, $14)",
    )
    .bind(id)
    .bind(s.branch)
    .bind(s.shift)
    .bind(s.teller)
    .bind(number)
    .bind(status)
    .bind(created_at)
    .bind(subtotal)
    .bind(discount)
    .bind(tax)
    // Mirrors the real invariant: total_amount carries the delivery fee.
    .bind(subtotal - discount + tax + delivery_fee)
    .bind(delivery_fee)
    .bind(tip)
    .bind(format!("REF-{}", &id.simple().to_string()[..8]))
    .execute(pool)
    .await
    .unwrap();
    id
}

/// Register an org payment method and say whether partners may see its orders.
async fn seed_payment_method(pool: &PgPool, s: &Seed, name: &str, visible: bool) {
    sqlx::query(
        "INSERT INTO org_payment_methods
             (org_id, name, label_translations, color, icon, is_cash, visible_in_integrations)
         VALUES ($1, $2, '{}'::jsonb, '#000000', 'money', false, $3)",
    )
    .bind(s.org)
    .bind(name)
    .bind(visible)
    .execute(pool)
    .await
    .unwrap();
}

/// Set an order's NOMINAL label (`'mixed'` for splits) and attach tender legs.
async fn seed_tender(pool: &PgPool, order: Uuid, nominal: &str, legs: &[(&str, i32)]) {
    sqlx::query("UPDATE orders SET payment_method = $2 WHERE id = $1")
        .bind(order)
        .bind(nominal)
        .execute(pool)
        .await
        .unwrap();
    for (method, amount) in legs {
        sqlx::query("INSERT INTO order_payments (order_id, method, amount) VALUES ($1, $2, $3)")
            .bind(order)
            .bind(method)
            .bind(amount)
            .execute(pool)
            .await
            .unwrap();
    }
}

/// Issue a credential directly (bypassing the HTTP surface) and return its secret.
async fn seed_credential(pool: &PgPool, s: &Seed, username: &str) -> String {
    let secret = "s3cr3t-partner-token";
    sqlx::query(
        "INSERT INTO integration_credentials (org_id, branch_id, name, username, secret_hash)
         VALUES ($1, $2, 'Partner', $3, $4)",
    )
    .bind(s.org)
    .bind(s.branch)
    .bind(username)
    .bind(bcrypt::hash(secret, 4).unwrap())
    .execute(pool)
    .await
    .unwrap();
    secret.to_string()
}

fn ts(raw: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(raw)
        .unwrap()
        .with_timezone(&Utc)
}

// ── authentication ────────────────────────────────────────────

#[sqlx::test]
async fn rejects_missing_wrong_and_revoked_credentials(pool: PgPool) {
    let s = seed(&pool, "auth", Some("Africa/Cairo")).await;
    let secret = seed_credential(&pool, &s, "partner").await;
    let app = app(pool.clone()).await;

    let url = "/integrations/analytics/orders?from=2026-06-01&to=2026-06-01".to_string();

    // No Authorization header at all.
    let req = test::TestRequest::get().uri(&url).to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::UNAUTHORIZED
    );

    // Right user, wrong secret.
    let req = test::TestRequest::get()
        .uri(&url)
        .insert_header(("Authorization", basic("partner", "nope")))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::UNAUTHORIZED
    );

    // A JWT is not a Basic credential.
    let req = test::TestRequest::get()
        .uri(&url)
        .insert_header((
            "Authorization",
            format!("Bearer {}", org_admin_token(s.teller, s.org)),
        ))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::UNAUTHORIZED
    );

    // Correct credential works …
    let req = test::TestRequest::get()
        .uri(&url)
        .insert_header(("Authorization", basic("partner", &secret)))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), StatusCode::OK);

    // … until it is revoked.
    sqlx::query("UPDATE integration_credentials SET revoked_at = now() WHERE username = 'partner'")
        .execute(&pool)
        .await
        .unwrap();
    let req = test::TestRequest::get()
        .uri(&url)
        .insert_header(("Authorization", basic("partner", &secret)))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test]
async fn username_match_is_case_insensitive(pool: PgPool) {
    let s = seed(&pool, "case", Some("Africa/Cairo")).await;
    let secret = seed_credential(&pool, &s, "Partner").await;
    let app = app(pool).await;

    let req = test::TestRequest::get()
        .uri(&"/integrations/analytics/orders?from=2026-06-01&to=2026-06-01".to_string())
        .insert_header(("Authorization", basic("PARTNER", &secret)))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), StatusCode::OK);
}

/// The credential ALONE decides the branch — there is no branch parameter to
/// pass, so a partner sees its own branch and nothing else. Two branches with
/// orders on the same day must not bleed into each other's figures, and a
/// stray `branch_id` in the query string must be inert rather than honoured.
#[sqlx::test]
async fn credential_reads_only_its_own_branch(pool: PgPool) {
    let a = seed(&pool, "a", Some("Africa/Cairo")).await;
    let b = seed(&pool, "b", Some("Africa/Cairo")).await;
    seed_order(
        &pool,
        &a,
        1,
        "completed",
        ts("2026-06-01T09:00:00Z"),
        111,
        0,
        0,
        0,
        0,
    )
    .await;
    seed_order(
        &pool,
        &b,
        1,
        "completed",
        ts("2026-06-01T09:00:00Z"),
        999,
        0,
        0,
        0,
        0,
    )
    .await;
    let secret = seed_credential(&pool, &a, "partner-a").await;
    let app = app(pool).await;

    // Plain request, and the same request with someone else's branch id bolted
    // on — both must return branch A's numbers.
    for qs in [
        "from=2026-06-01&to=2026-06-01".to_string(),
        format!("from=2026-06-01&to=2026-06-01&branch_id={}", b.branch),
    ] {
        let req = test::TestRequest::get()
            .uri(&format!("/integrations/analytics/orders?{qs}"))
            .insert_header(("Authorization", basic("partner-a", &secret)))
            .to_request();
        let body: serde_json::Value = test::call_and_read_body_json(&app, req).await;
        assert_eq!(body["branch_id"], a.branch.to_string(), "`{qs}`");
        assert_eq!(body["total_orders"], 1, "`{qs}`");
        assert_eq!(
            body["subtotal"], 111,
            "`{qs}` leaked another branch's orders"
        );
    }
}

// ── analytics payload ─────────────────────────────────────────

#[sqlx::test]
async fn excludes_voided_and_refunded_and_reports_order_money_only(pool: PgPool) {
    let s = seed(&pool, "money", Some("Africa/Cairo")).await;
    let secret = seed_credential(&pool, &s, "partner").await;

    // Sold: subtotal 10000, discount 1000, tax 1260 → order value 10260.
    // The 500 delivery fee and 300 tip must NOT appear anywhere.
    seed_order(
        &pool,
        &s,
        1,
        "completed",
        ts("2026-06-01T10:00:00Z"),
        10_000,
        1_000,
        1_260,
        500,
        300,
    )
    .await;
    // Still open on the KDS — counts as a sale (crate::orders::SOLD).
    seed_order(
        &pool,
        &s,
        2,
        "preparing",
        ts("2026-06-01T11:00:00Z"),
        2_000,
        0,
        280,
        0,
        0,
    )
    .await;
    // Neither of these may reach the partner at all.
    seed_order(
        &pool,
        &s,
        3,
        "voided",
        ts("2026-06-01T12:00:00Z"),
        9_999,
        0,
        0,
        0,
        0,
    )
    .await;
    seed_order(
        &pool,
        &s,
        4,
        "refunded",
        ts("2026-06-01T13:00:00Z"),
        8_888,
        0,
        0,
        0,
        0,
    )
    .await;

    let app = app(pool).await;
    let req = test::TestRequest::get()
        .uri(&"/integrations/analytics/orders?from=2026-06-01&to=2026-06-01".to_string())
        .insert_header(("Authorization", basic("partner", &secret)))
        .to_request();
    let body: serde_json::Value = test::call_and_read_body_json(&app, req).await;

    assert_eq!(body["total_orders"], 2);
    assert_eq!(body["subtotal"], 12_000);
    assert_eq!(body["total_discount"], 1_000);
    assert_eq!(body["total_tax"], 1_540);
    assert_eq!(body["total_service_charge"], 0);
    // 10260 + 2280 — no delivery fee, no tip.
    assert_eq!(body["total_revenue"], 12_540);
    assert_eq!(body["avg_order_total"], 6_270);

    let orders = body["orders"].as_array().unwrap();
    assert_eq!(orders.len(), 2, "voided and refunded must not be returned");
    for o in orders {
        assert!(
            o["status"] != "voided" && o["status"] != "refunded",
            "leaked a non-sale: {o}"
        );
        assert_eq!(o["service_charge"], 0);
    }
    assert_eq!(orders[0]["total_amount"], 10_260);
    assert_eq!(orders[0]["business_date"], "2026-06-01");
    assert!(orders[0]["order_ref"].is_string());
}

/// `to` is inclusive and both bounds are the BRANCH's calendar days, not UTC's.
/// Cairo is UTC+3 in June, so 2026-06-01T21:30Z is already June 2nd locally and
/// must fall outside a June-1-only window — while 2026-06-01T22:30Z on the
/// following request lands inside June 2nd.
#[sqlx::test]
async fn window_is_inclusive_and_resolved_in_the_branch_timezone(pool: PgPool) {
    let s = seed(&pool, "tz", Some("Africa/Cairo")).await;
    let secret = seed_credential(&pool, &s, "partner").await;

    // 2026-05-31 23:30 Cairo — before the window.
    seed_order(
        &pool,
        &s,
        1,
        "completed",
        ts("2026-05-31T20:30:00Z"),
        100,
        0,
        0,
        0,
        0,
    )
    .await;
    // 2026-06-01 03:00 Cairo — inside.
    seed_order(
        &pool,
        &s,
        2,
        "completed",
        ts("2026-06-01T00:00:00Z"),
        200,
        0,
        0,
        0,
        0,
    )
    .await;
    // 2026-06-02 00:30 Cairo — after a June-1-only window.
    seed_order(
        &pool,
        &s,
        3,
        "completed",
        ts("2026-06-01T21:30:00Z"),
        400,
        0,
        0,
        0,
        0,
    )
    .await;

    let app = app(pool).await;
    macro_rules! fetch {
        ($from:literal, $to:literal) => {{
            let req = test::TestRequest::get()
                .uri(&format!(
                    "/integrations/analytics/orders?from={}&to={}",
                    $from, $to
                ))
                .insert_header(("Authorization", basic("partner", &secret)))
                .to_request();
            test::call_and_read_body_json::<_, _, serde_json::Value>(&app, req).await
        }};
    }

    let june1 = fetch!("2026-06-01", "2026-06-01");
    assert_eq!(june1["total_orders"], 1);
    assert_eq!(june1["subtotal"], 200);
    assert_eq!(june1["timezone"], "Africa/Cairo");
    // 2026-06-01 00:00 Cairo == 2026-05-31T21:00Z (UTC+3 in June).
    assert_eq!(june1["from_utc"], "2026-05-31T21:00:00Z");
    assert_eq!(june1["to_utc"], "2026-06-01T21:00:00Z");

    // Extending `to` by one day pulls in the 00:30-local order — proving the
    // end bound is inclusive of its whole local day.
    let june1to2 = fetch!("2026-06-01", "2026-06-02");
    assert_eq!(june1to2["total_orders"], 2);
    assert_eq!(june1to2["subtotal"], 600);
    assert_eq!(june1to2["orders"][1]["business_date"], "2026-06-02");
}

#[sqlx::test]
async fn branch_timezone_falls_back_to_the_org(pool: PgPool) {
    let s = seed(&pool, "inherit", None).await;
    sqlx::query("UPDATE organizations SET timezone = 'Asia/Tokyo' WHERE id = $1")
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let secret = seed_credential(&pool, &s, "partner").await;
    let app = app(pool).await;

    let req = test::TestRequest::get()
        .uri(&"/integrations/analytics/orders?from=2026-06-01&to=2026-06-01".to_string())
        .insert_header(("Authorization", basic("partner", &secret)))
        .to_request();
    let body: serde_json::Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(body["timezone"], "Asia/Tokyo");
    // Tokyo is UTC+9 year-round.
    assert_eq!(body["from_utc"], "2026-05-31T15:00:00Z");
}

#[sqlx::test]
async fn empty_window_returns_zeroes_not_an_error(pool: PgPool) {
    let s = seed(&pool, "empty", Some("Africa/Cairo")).await;
    let secret = seed_credential(&pool, &s, "partner").await;
    let app = app(pool).await;

    let req = test::TestRequest::get()
        .uri(&"/integrations/analytics/orders?from=2026-06-01&to=2026-06-01".to_string())
        .insert_header(("Authorization", basic("partner", &secret)))
        .to_request();
    let body: serde_json::Value = test::call_and_read_body_json(&app, req).await;

    assert_eq!(body["total_orders"], 0);
    assert_eq!(body["total_revenue"], 0);
    // Division-by-zero guard.
    assert_eq!(body["avg_order_total"], 0);
    assert_eq!(body["orders"].as_array().unwrap().len(), 0);
}

#[sqlx::test]
async fn pagination_is_optional_and_totals_ignore_it(pool: PgPool) {
    let s = seed(&pool, "page", Some("Africa/Cairo")).await;
    let secret = seed_credential(&pool, &s, "partner").await;
    for n in 0..5 {
        seed_order(
            &pool,
            &s,
            n + 1,
            "completed",
            ts("2026-06-01T08:00:00Z") + chrono::Duration::minutes(n as i64),
            100,
            0,
            0,
            0,
            0,
        )
        .await;
    }
    let app = app(pool).await;
    let base = "/integrations/analytics/orders?from=2026-06-01&to=2026-06-01".to_string();

    // Omitted → the whole window in one response.
    let req = test::TestRequest::get()
        .uri(&base)
        .insert_header(("Authorization", basic("partner", &secret)))
        .to_request();
    let all: serde_json::Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(all["orders"].as_array().unwrap().len(), 5);
    assert_eq!(all["returned"], 5);
    assert!(all["limit"].is_null());

    // Paged → rows shrink, aggregates do not.
    let req = test::TestRequest::get()
        .uri(&format!("{base}&limit=2&offset=2"))
        .insert_header(("Authorization", basic("partner", &secret)))
        .to_request();
    let page: serde_json::Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(page["orders"].as_array().unwrap().len(), 2);
    assert_eq!(page["returned"], 2);
    assert_eq!(page["offset"], 2);
    assert_eq!(
        page["total_orders"], 5,
        "totals must cover the whole window"
    );
    assert_eq!(page["total_revenue"], 500);
    // Stable ordering: page rows are the 3rd and 4th orders.
    assert_eq!(page["orders"][0]["order_number"], 3);
    assert_eq!(page["orders"][1]["order_number"], 4);
}

#[sqlx::test]
async fn rejects_a_backwards_window_and_bad_paging(pool: PgPool) {
    let s = seed(&pool, "bad", Some("Africa/Cairo")).await;
    let secret = seed_credential(&pool, &s, "partner").await;
    let app = app(pool).await;

    for qs in [
        "from=2026-06-30&to=2026-06-01",
        "from=2026-06-01&to=2026-06-02&limit=0",
        "from=2026-06-01&to=2026-06-02&offset=-1",
    ] {
        let req = test::TestRequest::get()
            .uri(&"/integrations/analytics/orders?{qs}".to_string())
            .insert_header(("Authorization", basic("partner", &secret)))
            .to_request();
        assert_eq!(
            test::call_service(&app, req).await.status(),
            StatusCode::BAD_REQUEST,
            "`{qs}` should be rejected"
        );
    }
}

// ── payment-method visibility ─────────────────────────────────

#[sqlx::test]
async fn orders_on_a_hidden_payment_method_disappear_completely(pool: PgPool) {
    let s = seed(&pool, "pm", Some("Africa/Cairo")).await;
    let secret = seed_credential(&pool, &s, "partner").await;
    seed_payment_method(&pool, &s, "cash", true).await;
    seed_payment_method(&pool, &s, "aggregator", false).await;

    let shown = seed_order(
        &pool,
        &s,
        1,
        "completed",
        ts("2026-06-01T09:00:00Z"),
        1_000,
        0,
        140,
        0,
        0,
    )
    .await;
    seed_tender(&pool, shown, "cash", &[("cash", 1_140)]).await;

    let hidden = seed_order(
        &pool,
        &s,
        2,
        "completed",
        ts("2026-06-01T10:00:00Z"),
        5_000,
        0,
        700,
        0,
        0,
    )
    .await;
    seed_tender(&pool, hidden, "aggregator", &[("aggregator", 5_700)]).await;

    let app = app(pool).await;
    let req = test::TestRequest::get()
        .uri("/integrations/analytics/orders?from=2026-06-01&to=2026-06-01")
        .insert_header(("Authorization", basic("partner", &secret)))
        .to_request();
    let body: serde_json::Value = test::call_and_read_body_json(&app, req).await;

    // Gone from the rows AND from every aggregate — the partner's view is
    // internally consistent and gives no hint that anything was withheld.
    let orders = body["orders"].as_array().unwrap();
    assert_eq!(orders.len(), 1);
    assert_eq!(orders[0]["order_id"], shown.to_string());
    assert_eq!(body["total_orders"], 1);
    assert_eq!(body["subtotal"], 1_000);
    assert_eq!(body["total_revenue"], 1_140);
    assert_eq!(body["avg_order_total"], 1_140);
}

/// A split order carries the hidden method's money inside its own total, so
/// one tainted leg must remove the whole order.
#[sqlx::test]
async fn split_order_is_hidden_when_any_leg_used_a_hidden_method(pool: PgPool) {
    let s = seed(&pool, "split", Some("Africa/Cairo")).await;
    let secret = seed_credential(&pool, &s, "partner").await;
    seed_payment_method(&pool, &s, "cash", true).await;
    seed_payment_method(&pool, &s, "aggregator", false).await;
    seed_payment_method(&pool, &s, "mixed", true).await;

    // Nominal label is the literal 'mixed' and IS visible — only the leg is not,
    // which is exactly the case a nominal-label filter would miss.
    let order = seed_order(
        &pool,
        &s,
        1,
        "completed",
        ts("2026-06-01T09:00:00Z"),
        2_000,
        0,
        280,
        0,
        0,
    )
    .await;
    seed_tender(
        &pool,
        order,
        "mixed",
        &[("cash", 1_000), ("aggregator", 1_280)],
    )
    .await;

    let app = app(pool).await;
    let req = test::TestRequest::get()
        .uri("/integrations/analytics/orders?from=2026-06-01&to=2026-06-01")
        .insert_header(("Authorization", basic("partner", &secret)))
        .to_request();
    let body: serde_json::Value = test::call_and_read_body_json(&app, req).await;

    assert_eq!(
        body["total_orders"], 0,
        "a tainted leg must hide the whole order"
    );
    assert_eq!(body["total_revenue"], 0);
    assert_eq!(body["orders"].as_array().unwrap().len(), 0);
}

/// The column defaults to true, and the endpoint predates it — an org that has
/// never touched the toggle must keep seeing everything.
#[sqlx::test]
async fn methods_are_visible_unless_explicitly_hidden(pool: PgPool) {
    let s = seed(&pool, "default", Some("Africa/Cairo")).await;
    let secret = seed_credential(&pool, &s, "partner").await;
    seed_payment_method(&pool, &s, "cash", true).await;

    let listed = seed_order(
        &pool,
        &s,
        1,
        "completed",
        ts("2026-06-01T09:00:00Z"),
        100,
        0,
        0,
        0,
        0,
    )
    .await;
    seed_tender(&pool, listed, "cash", &[("cash", 100)]).await;
    // Tendered with a method that was never registered at all — nothing says to
    // hide it, so it stays visible rather than silently vanishing.
    let unregistered = seed_order(
        &pool,
        &s,
        2,
        "completed",
        ts("2026-06-01T10:00:00Z"),
        200,
        0,
        0,
        0,
        0,
    )
    .await;
    seed_tender(&pool, unregistered, "voucher", &[("voucher", 200)]).await;

    let app = app(pool).await;
    let req = test::TestRequest::get()
        .uri("/integrations/analytics/orders?from=2026-06-01&to=2026-06-01")
        .insert_header(("Authorization", basic("partner", &secret)))
        .to_request();
    let body: serde_json::Value = test::call_and_read_body_json(&app, req).await;

    assert_eq!(body["total_orders"], 2);
    assert_eq!(body["subtotal"], 300);
}

// ── operator surface ──────────────────────────────────────────

#[sqlx::test]
async fn create_returns_the_secret_once_and_it_authenticates(pool: PgPool) {
    let s = seed(&pool, "crud", Some("Africa/Cairo")).await;
    let admin = Uuid::new_v4();
    // `chk_login_method` requires a real login method — email + password here.
    sqlx::query(
        "INSERT INTO users (id, name, role, org_id, email, password_hash)
         VALUES ($1, 'Admin', 'org_admin', $2, 'admin-' || $1::text || '@example.com', 'x')",
    )
    .bind(admin)
    .bind(s.org)
    .execute(&pool)
    .await
    .unwrap();
    let app = app(pool.clone()).await;
    let token = org_admin_token(admin, s.org);

    let req = test::TestRequest::post()
        .uri("/integrations/credentials")
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(json!({
            "name": "Rue — One Ninety",
            "branch_id": s.branch,
            "username": "rue-one-ninety",
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created: serde_json::Value = test::read_body_json(resp).await;
    let secret = created["secret"].as_str().unwrap().to_string();
    assert!(secret.len() >= 32, "secret should be high-entropy");
    assert_eq!(created["branch_name"], format!("Branch crud"));

    // The issued secret really does open the analytics endpoint.
    let req = test::TestRequest::get()
        .uri(&"/integrations/analytics/orders?from=2026-06-01&to=2026-06-01".to_string())
        .insert_header(("Authorization", basic("rue-one-ninety", &secret)))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), StatusCode::OK);

    // Listing never exposes the secret (nor its hash).
    let req = test::TestRequest::get()
        .uri("/integrations/credentials")
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let list: serde_json::Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    let raw = list.to_string();
    assert!(!raw.contains(&secret) && !raw.contains("secret_hash"));
}

#[sqlx::test]
async fn usernames_are_unique_across_orgs(pool: PgPool) {
    let a = seed(&pool, "dup-a", Some("Africa/Cairo")).await;
    let b = seed(&pool, "dup-b", Some("Africa/Cairo")).await;
    seed_credential(&pool, &a, "shared-name").await;

    let admin = Uuid::new_v4();
    // `chk_login_method` requires a real login method — email + password here.
    sqlx::query(
        "INSERT INTO users (id, name, role, org_id, email, password_hash)
         VALUES ($1, 'Admin', 'org_admin', $2, 'admin-' || $1::text || '@example.com', 'x')",
    )
    .bind(admin)
    .bind(b.org)
    .execute(&pool)
    .await
    .unwrap();
    let app = app(pool).await;

    // Different org, same username — Basic auth has no tenant hint, so this
    // must be a clean conflict rather than an ambiguous lookup later.
    let req = test::TestRequest::post()
        .uri("/integrations/credentials")
        .insert_header((
            "Authorization",
            format!("Bearer {}", org_admin_token(admin, b.org)),
        ))
        .set_json(json!({"name": "Dup", "branch_id": b.branch, "username": "SHARED-NAME"}))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::CONFLICT
    );
}

#[sqlx::test]
async fn cannot_issue_a_credential_for_another_orgs_branch(pool: PgPool) {
    let a = seed(&pool, "own", Some("Africa/Cairo")).await;
    let b = seed(&pool, "other", Some("Africa/Cairo")).await;
    let admin = Uuid::new_v4();
    // `chk_login_method` requires a real login method — email + password here.
    sqlx::query(
        "INSERT INTO users (id, name, role, org_id, email, password_hash)
         VALUES ($1, 'Admin', 'org_admin', $2, 'admin-' || $1::text || '@example.com', 'x')",
    )
    .bind(admin)
    .bind(a.org)
    .execute(&pool)
    .await
    .unwrap();
    let app = app(pool).await;

    let req = test::TestRequest::post()
        .uri("/integrations/credentials")
        .insert_header((
            "Authorization",
            format!("Bearer {}", org_admin_token(admin, a.org)),
        ))
        .set_json(json!({"name": "Sneaky", "branch_id": b.branch, "username": "sneaky"}))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test]
async fn non_admins_cannot_touch_the_credential_surface(pool: PgPool) {
    let s = seed(&pool, "role", Some("Africa/Cairo")).await;
    let token = crate::auth::jwt::create_token(
        &get_secret(),
        s.teller,
        Some(s.org),
        UserRole::BranchManager,
        Some(s.branch),
        24,
    )
    .unwrap();
    let app = app(pool).await;

    let req = test::TestRequest::get()
        .uri("/integrations/credentials")
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test]
async fn rotate_invalidates_the_old_secret_and_revoke_ends_access(pool: PgPool) {
    let s = seed(&pool, "rot", Some("Africa/Cairo")).await;
    let old = seed_credential(&pool, &s, "partner").await;
    let id: Uuid =
        sqlx::query_scalar("SELECT id FROM integration_credentials WHERE username = 'partner'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let admin = Uuid::new_v4();
    // `chk_login_method` requires a real login method — email + password here.
    sqlx::query(
        "INSERT INTO users (id, name, role, org_id, email, password_hash)
         VALUES ($1, 'Admin', 'org_admin', $2, 'admin-' || $1::text || '@example.com', 'x')",
    )
    .bind(admin)
    .bind(s.org)
    .execute(&pool)
    .await
    .unwrap();
    let app = app(pool).await;
    let token = org_admin_token(admin, s.org);
    let url = "/integrations/analytics/orders?from=2026-06-01&to=2026-06-01".to_string();

    let req = test::TestRequest::post()
        .uri(&format!("/integrations/credentials/{id}/rotate"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let rotated: serde_json::Value = test::call_and_read_body_json(&app, req).await;
    let new = rotated["secret"].as_str().unwrap().to_string();
    assert_ne!(new, old);

    // Old secret is dead, new one works.
    let req = test::TestRequest::get()
        .uri(&url)
        .insert_header(("Authorization", basic("partner", &old)))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let req = test::TestRequest::get()
        .uri(&url)
        .insert_header(("Authorization", basic("partner", &new)))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), StatusCode::OK);

    // Revoke is a soft stamp — the row survives — and access stops immediately.
    let req = test::TestRequest::delete()
        .uri(&format!("/integrations/credentials/{id}"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::NO_CONTENT
    );
    let req = test::TestRequest::get()
        .uri(&url)
        .insert_header(("Authorization", basic("partner", &new)))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::UNAUTHORIZED
    );
}
