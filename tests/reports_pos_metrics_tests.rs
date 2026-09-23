//! `GET /reports/branches/{id}/pos-metrics`: capability first, branch scope,
//! branch-local days, and figures that agree with `branch_sales`.

use actix_web::{App, test, web};
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::JwtSecret;
use madar_rust::models::UserRole;
use madar_rust::reports::pos_metrics::{PosMetricsReport, average_ticket};
use madar_rust::reports::routes;

fn secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn token(user: Uuid, org: Uuid, role: UserRole, branch: Option<Uuid>) -> String {
    madar_rust::auth::jwt::create_token(&secret(), user, Some(org), role, branch, 24).unwrap()
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .configure(|cfg| routes::configure(cfg, web::Data::new($pool.clone()))),
        )
        .await
    };
}

async fn call(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    uri: &str,
    token: &str,
) -> (u16, Value) {
    let req = test::TestRequest::get()
        .uri(uri)
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let resp = test::call_service(app, req).await;
    let status = resp.status().as_u16();
    let body = test::read_body(resp).await;
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

async fn org(pool: &PgPool) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug) VALUES ('Metrics', $1) RETURNING id",
    )
    .bind(format!("metrics-{}", Uuid::new_v4()))
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO org_payment_methods (org_id, name, label_translations, color, icon, is_cash, is_active) VALUES
         ($1, 'cash', '{}', 'emerald', 'payments_outlined', true, true),
         ($1, 'card', '{}', 'blue', 'credit_card_rounded', false, true)",
    )
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn branch(pool: &PgPool, org: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO branches (org_id, name, timezone) VALUES ($1, $2, 'Africa/Cairo') RETURNING id",
    )
    .bind(org)
    .bind(format!("B {}", Uuid::new_v4()))
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn user(pool: &PgPool, org: Uuid, role: &str, at: Option<Uuid>) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO users (org_id, name, email, password_hash, role) VALUES ($1, 'U', $2, 'hash', $3::user_role) RETURNING id",
    )
    .bind(org)
    .bind(format!("{}@metrics.test", Uuid::new_v4()))
    .bind(role)
    .fetch_one(pool)
    .await
    .unwrap();
    if let Some(b) = at {
        sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
            .bind(id)
            .bind(b)
            .execute(pool)
            .await
            .unwrap();
    }
    id
}

async fn till(pool: &PgPool, branch: Uuid, teller: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO tills (branch_id, teller_id, status, opening_cash) VALUES ($1, $2, 'open', 0) RETURNING id",
    )
    .bind(branch)
    .bind(teller)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A completed sale at `at`, paid by `legs`, with `lines` (name, qty, line_total).
#[allow(clippy::too_many_arguments)]
async fn sale(
    pool: &PgPool,
    branch: Uuid,
    teller: Uuid,
    till: Uuid,
    n: i32,
    at: DateTime<Utc>,
    legs: &[(&str, i32)],
    lines: &[(&str, i32, i32)],
) -> Uuid {
    let total: i32 = legs.iter().map(|l| l.1).sum();
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO orders (branch_id, teller_id, till_id, idempotency_key, subtotal, discount_amount,
             tax_amount, total_amount, status, order_number, payment_method, order_ref, created_at)
         VALUES ($1, $2, $3, gen_random_uuid(), $4, 0, 0, $4, 'completed', $5, $6, gen_random_uuid()::text, $7)
         RETURNING id",
    )
    .bind(branch)
    .bind(teller)
    .bind(till)
    .bind(total)
    .bind(n)
    .bind(legs[0].0)
    .bind(at)
    .fetch_one(pool)
    .await
    .unwrap();
    for (m, a) in legs {
        sqlx::query("INSERT INTO order_payments (order_id, method, amount) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(m)
            .bind(a)
            .execute(pool)
            .await
            .unwrap();
    }
    for (name, q, total) in lines {
        // One menu item per name in the org (branch_sales reads it non-null).
        let item: Uuid = sqlx::query_scalar(
            "WITH o AS (SELECT org_id FROM branches WHERE id = $1),
                  c AS (INSERT INTO categories (org_id, name)
                        SELECT org_id, 'Cat ' || gen_random_uuid() FROM o
                        WHERE NOT EXISTS (SELECT 1 FROM menu_items m, o WHERE m.org_id = o.org_id AND m.name = $2)
                        RETURNING id, org_id),
                  i AS (INSERT INTO menu_items (org_id, category_id, name, base_price, is_active)
                        SELECT org_id, id, $2, 100, true FROM c RETURNING id)
             SELECT id FROM i
             UNION ALL SELECT m.id FROM menu_items m, o WHERE m.org_id = o.org_id AND m.name = $2",
        )
        .bind(branch)
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO order_items (order_id, menu_item_id, item_name, quantity, unit_price, line_total) VALUES ($1, $5, $2, $3, $4 / $3, $4)",
        )
        .bind(id)
        .bind(name)
        .bind(q)
        .bind(total)
        .bind(item)
        .execute(pool)
        .await
        .unwrap();
    }
    id
}

async fn refund(pool: &PgPool, order: Uuid, till: Uuid, by: Uuid, amount: i32, at: DateTime<Utc>) {
    sqlx::query(
        "INSERT INTO order_refunds (order_id, till_id, amount, method, is_cash, reason, issued_by, issued_at)
         VALUES ($1, $2, $3, 'cash', true, 'customer_request', $4, $5)",
    )
    .bind(order)
    .bind(till)
    .bind(amount)
    .bind(by)
    .bind(at)
    .execute(pool)
    .await
    .unwrap();
}

fn cairo(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
    chrono_tz::Africa::Cairo
        .with_ymd_and_hms(y, m, d, h, min, 0)
        .unwrap()
        .with_timezone(&Utc)
}

#[::core::prelude::v1::test]
fn average_ticket_rounds_half_up() {
    assert_eq!(average_ticket(0, 0), 0);
    assert_eq!(average_ticket(1000, 3), 333);
    assert_eq!(average_ticket(1001, 2), 501);
    assert_eq!(average_ticket(1000, 2), 500);
}

/// A teller (no capability) gets 403; a manager at another branch gets 403
/// (scope); a branch-bound PIN session asking about another branch gets 403;
/// a manager at the branch and the owner get 200.
#[sqlx::test]
async fn capability_then_branch_scope(pool: PgPool) {
    let app = app!(pool);
    let o = org(&pool).await;
    let mine = branch(&pool, o).await;
    let other = branch(&pool, o).await;
    let owner = user(&pool, o, "org_admin", None).await;
    let manager = user(&pool, o, "branch_manager", Some(mine)).await;
    let teller = user(&pool, o, "teller", Some(mine)).await;
    let q = "from=2026-09-01&to=2026-09-02";

    let t = token(teller, o, UserRole::Teller, Some(mine));
    let (s, _) = call(
        &app,
        &format!("/reports/branches/{mine}/pos-metrics?{q}"),
        &t,
    )
    .await;
    assert_eq!(
        s, 403,
        "a teller does not hold reports.pos_metrics by default"
    );

    let m = token(manager, o, UserRole::BranchManager, None);
    let (s, _) = call(
        &app,
        &format!("/reports/branches/{mine}/pos-metrics?{q}"),
        &m,
    )
    .await;
    assert_eq!(s, 200);
    let (s, _) = call(
        &app,
        &format!("/reports/branches/{other}/pos-metrics?{q}"),
        &m,
    )
    .await;
    assert_eq!(s, 403, "a manager is scoped to their branch");

    let pin = token(owner, o, UserRole::OrgAdmin, Some(mine));
    let (s, _) = call(
        &app,
        &format!("/reports/branches/{other}/pos-metrics?{q}"),
        &pin,
    )
    .await;
    assert_eq!(s, 403, "a branch-bound session reads only its branch");

    let web = token(owner, o, UserRole::OrgAdmin, None);
    let (s, _) = call(
        &app,
        &format!("/reports/branches/{other}/pos-metrics?{q}"),
        &web,
    )
    .await;
    assert_eq!(s, 200);

    // Another org's owner: tenant isolation hides the branch outright (404, no
    // existence leak), before any figure is read.
    let o2 = org(&pool).await;
    let stranger = user(&pool, o2, "org_admin", None).await;
    let st = token(stranger, o2, UserRole::OrgAdmin, None);
    let (s, b) = call(
        &app,
        &format!("/reports/branches/{mine}/pos-metrics?{q}"),
        &st,
    )
    .await;
    assert_eq!(s, 404, "{b}");

    // Bad windows are 400 for someone allowed.
    let (s, _) = call(
        &app,
        &format!("/reports/branches/{mine}/pos-metrics?from=2026-09-02&to=2026-09-01"),
        &web,
    )
    .await;
    assert_eq!(s, 400);
    let (s, _) = call(
        &app,
        &format!("/reports/branches/{mine}/pos-metrics?from=2025-01-01&to=2026-09-01"),
        &web,
    )
    .await;
    assert_eq!(s, 400);
}

/// The figures agree with `branch_sales` over the same instants, days are cut
/// at Cairo midnight (not UTC), and hours are Cairo hours.
#[sqlx::test]
async fn figures_agree_with_branch_sales(pool: PgPool) {
    let app = app!(pool);
    let o = org(&pool).await;
    let b = branch(&pool, o).await;
    let owner = user(&pool, o, "org_admin", None).await;
    let t = till(&pool, b, owner).await;
    let web = token(owner, o, UserRole::OrgAdmin, None);

    // Outside: 23:59 Cairo the day before, and 00:00 Cairo the day after.
    sale(
        &pool,
        b,
        owner,
        t,
        1,
        cairo(2026, 9, 9, 23, 59),
        &[("cash", 999)],
        &[("Early", 1, 999)],
    )
    .await;
    sale(
        &pool,
        b,
        owner,
        t,
        2,
        cairo(2026, 9, 12, 0, 0),
        &[("cash", 777)],
        &[("Late", 1, 777)],
    )
    .await;
    // Inside (Sept 10–11 Cairo). 00:30 Cairo is still Sept 9 in UTC.
    let a = sale(
        &pool,
        b,
        owner,
        t,
        3,
        cairo(2026, 9, 10, 0, 30),
        &[("cash", 1000)],
        &[("Latte", 2, 1000)],
    )
    .await;
    refund(&pool, a, t, owner, 300, cairo(2026, 9, 10, 1, 0)).await;
    sale(
        &pool,
        b,
        owner,
        t,
        4,
        cairo(2026, 9, 10, 9, 15),
        &[("cash", 600), ("card", 900)],
        &[("Latte", 1, 500), ("Cake", 3, 1000)],
    )
    .await;
    let full = sale(
        &pool,
        b,
        owner,
        t,
        5,
        cairo(2026, 9, 11, 9, 45),
        &[("card", 400)],
        &[("Tea", 1, 400)],
    )
    .await;
    refund(&pool, full, t, owner, 400, cairo(2026, 9, 11, 10, 0)).await;
    sqlx::query("UPDATE orders SET status = 'refunded' WHERE id = $1")
        .bind(full)
        .execute(&pool)
        .await
        .unwrap();
    let void = sale(
        &pool,
        b,
        owner,
        t,
        6,
        cairo(2026, 9, 11, 13, 0),
        &[("cash", 250)],
        &[("Tea", 1, 250)],
    )
    .await;
    sqlx::query("UPDATE orders SET status = 'voided', voided_at = now(), voided_by = $2, void_reason = 'wrong_order' WHERE id = $1")
        .bind(void)
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    // A refund issued in the window against the sale from before it.
    let early: Uuid =
        sqlx::query_scalar("SELECT id FROM orders WHERE order_number = 1 AND branch_id = $1")
            .bind(b)
            .fetch_one(&pool)
            .await
            .unwrap();
    refund(&pool, early, t, owner, 99, cairo(2026, 9, 11, 14, 0)).await;

    let (s, body) = call(
        &app,
        &format!("/reports/branches/{b}/pos-metrics?from=2026-09-10&to=2026-09-11"),
        &web,
    )
    .await;
    assert_eq!(s, 200, "{body}");
    let m: PosMetricsReport = serde_json::from_value(body).unwrap();
    assert_eq!(m.timezone, "Africa/Cairo");
    assert_eq!(m.window_from, cairo(2026, 9, 10, 0, 0));
    assert_eq!(m.window_to, cairo(2026, 9, 12, 0, 0));

    // branch_sales over the same instants (its `to` is inclusive).
    let from = m
        .window_from
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    let to = (m.window_to - Duration::microseconds(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    let (s, bs) = call(
        &app,
        &format!("/reports/branches/{b}/sales?from={from}&to={to}&limit=1000"),
        &web,
    )
    .await;
    assert_eq!(s, 200, "{bs}");
    assert_eq!(m.net_sales, bs["total_revenue"].as_i64().unwrap());
    assert_eq!(m.gross_sales, bs["gross_sales"].as_i64().unwrap());
    assert_eq!(m.refunded_amount, bs["refunded_amount"].as_i64().unwrap());
    assert_eq!(m.order_count, bs["total_orders"].as_i64().unwrap());
    assert_eq!(m.voided_count, bs["voided_orders"].as_i64().unwrap());
    let methods = bs["revenue_by_method"].as_object().unwrap();
    assert_eq!(m.tenders.len(), methods.len());
    for tender in &m.tenders {
        assert_eq!(
            Some(tender.amount),
            methods[&tender.method].as_i64(),
            "{}",
            tender.method
        );
    }
    for item in &m.top_items {
        let row = bs["top_items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["item_name"] == item.item_name.as_str())
            .unwrap();
        assert_eq!(item.quantity, row["quantity_sold"].as_i64().unwrap());
        assert_eq!(item.revenue, row["revenue"].as_i64().unwrap());
    }

    // The figures themselves.
    assert_eq!(m.order_count, 2);
    assert_eq!(m.gross_sales, 2500);
    assert_eq!(m.refunded_amount, 300);
    assert_eq!(m.net_sales, 2200);
    assert_eq!(m.average_ticket, 1100);
    assert_eq!((m.voided_count, m.voided_amount), (1, 250));
    assert_eq!(m.refunded_orders_count, 1);
    assert_eq!((m.refunds_issued_count, m.refunds_issued_amount), (3, 799));
    let tenders: Vec<_> = m
        .tenders
        .iter()
        .map(|t| (t.method.as_str(), t.amount, t.order_count))
        .collect();
    assert_eq!(tenders, vec![("cash", 1600, 2), ("card", 900, 1)]);
    let items: Vec<_> = m
        .top_items
        .iter()
        .map(|i| (i.item_name.as_str(), i.quantity, i.revenue))
        .collect();
    assert_eq!(
        items,
        vec![("Latte", 3, 1500), ("Cake", 3, 1000)],
        "quantity, then revenue"
    );
    assert_eq!(m.hourly.len(), 24);
    assert_eq!(
        (m.hourly[0].order_count, m.hourly[0].net_sales),
        (1, 700),
        "00:30 Cairo is hour 0"
    );
    assert_eq!((m.hourly[9].order_count, m.hourly[9].net_sales), (1, 1500));
    assert_eq!(
        m.hourly.iter().map(|h| h.net_sales).sum::<i64>(),
        m.net_sales
    );

    // A single day: Sept 9 holds the 23:59 sale only.
    let (_, body) = call(
        &app,
        &format!("/reports/branches/{b}/pos-metrics?from=2026-09-09&to=2026-09-09"),
        &web,
    )
    .await;
    assert_eq!(body["order_count"], 1);
    assert_eq!(body["net_sales"], 900);
    let _ = NaiveDate::from_ymd_opt(2026, 9, 9);
}

// ── Shared vectors for the POS's offline figures ─────────────────────────────
//
// The POS computes these figures from the rows `POST /sync/pull` delivers when
// the endpoint is unreachable. This seeds one scenario with fixed ids and
// times, takes its rows exactly as the feed projects them, and records what
// `compute` says for several windows. madar-core `metrics` loads the file and
// must agree field by field. Regenerate after a formula or projection change:
//
// ```sh
// MADAR_WRITE_POS_METRICS_VECTORS=1 cargo nextest run --test reports_pos_metrics_tests -E 'test(pos_metrics_vectors)'
// ```
//
// The file lives in madar-shared (`madar_money::vectors::POS_METRICS`), the
// one copy both sides read. The write goes into the madar-shared checkout
// beside this one (or `$MADAR_SHARED_DIR`); release it there with a tag.

fn vectors_out() -> std::path::PathBuf {
    let shared = std::env::var("MADAR_SHARED_DIR")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../madar-shared").into());
    std::path::Path::new(&shared).join("crates/madar-money/vectors/pos_metrics_vectors.json")
}

fn vid(label: &str) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        format!("pos-metrics-vector:{label}").as_bytes(),
    )
}

const VECTOR_SQL: &str = "
INSERT INTO organizations (id, name, slug) VALUES ('{id:org}', 'Metrics vector', 'metrics-vector');
INSERT INTO branches (id, org_id, name, code, timezone) VALUES ('{id:branch}', '{id:org}', 'Vector', 'MVC', 'Africa/Cairo');
INSERT INTO users (id, org_id, name, email, password_hash, role) VALUES
  ('{id:sara}', '{id:org}', 'Sara', 'sara@metrics-vector.test', 'x', 'teller');
INSERT INTO org_payment_methods (id, org_id, name, color, icon, is_cash, created_at) VALUES
  ('{id:pm:cash}', '{id:org}', 'cash', '#000', 'cash', true, '2026-01-01 00:00+00'),
  ('{id:pm:card}', '{id:org}', 'card', '#00f', 'card', false, '2026-01-01 00:00+00');
INSERT INTO categories (id, org_id, name) VALUES ('{id:cat}', '{id:org}', 'Drinks');
INSERT INTO menu_items (id, org_id, category_id, name, base_price, is_active) VALUES
  ('{id:latte}', '{id:org}', '{id:cat}', 'Latte', 500, true),
  ('{id:cake}',  '{id:org}', '{id:cat}', 'Cake', 333, true),
  ('{id:tea}',   '{id:org}', '{id:cat}', 'Tea', 250, true);
INSERT INTO tills (id, branch_id, teller_id, status, opening_cash, opened_at)
  VALUES ('{id:till}', '{id:branch}', '{id:sara}', 'open', 0, '2026-09-09 08:00+03');
INSERT INTO orders (id, branch_id, teller_id, till_id, idempotency_key, subtotal, discount_amount, tax_amount,
                    total_amount, status, order_number, payment_method, order_ref, created_at, updated_at) VALUES
  ('{id:o1}', '{id:branch}', '{id:sara}', '{id:till}', '{id:k1}',  999, 0, 0,  999, 'completed', 1, 'cash', 'MVC-1', '2026-09-09 23:59+03', '2026-09-09 23:59+03'),
  ('{id:o2}', '{id:branch}', '{id:sara}', '{id:till}', '{id:k2}',  777, 0, 0,  777, 'completed', 2, 'cash', 'MVC-2', '2026-09-12 00:00+03', '2026-09-12 00:00+03'),
  ('{id:o3}', '{id:branch}', '{id:sara}', '{id:till}', '{id:k3}', 1000, 0, 0, 1000, 'completed', 3, 'cash', 'MVC-3', '2026-09-10 00:30+03', '2026-09-10 00:30+03'),
  ('{id:o4}', '{id:branch}', '{id:sara}', '{id:till}', '{id:k4}', 1500, 0, 0, 1500, 'completed', 4, 'cash', 'MVC-4', '2026-09-10 09:15+03', '2026-09-10 09:15+03'),
  ('{id:o5}', '{id:branch}', '{id:sara}', '{id:till}', '{id:k5}',  400, 0, 0,  400, 'completed', 5, 'card', 'MVC-5', '2026-09-11 09:45+03', '2026-09-11 09:45+03'),
  ('{id:o6}', '{id:branch}', '{id:sara}', '{id:till}', '{id:k6}',  250, 0, 0,  250, 'completed', 6, 'cash', 'MVC-6', '2026-09-11 13:00+03', '2026-09-11 13:00+03'),
  ('{id:o7}', '{id:branch}', '{id:sara}', '{id:till}', '{id:k7}',  333, 0, 0,  333, 'completed', 7, 'card', 'MVC-7', '2026-09-11 23:30+03', '2026-09-11 23:30+03');
INSERT INTO order_payments (id, order_id, method, amount) VALUES
  ('{id:p1}', '{id:o1}', 'cash', 999), ('{id:p2}', '{id:o2}', 'cash', 777), ('{id:p3}', '{id:o3}', 'cash', 1000),
  ('{id:p4a}', '{id:o4}', 'cash', 600), ('{id:p4b}', '{id:o4}', 'card', 900), ('{id:p5}', '{id:o5}', 'card', 400),
  ('{id:p6}', '{id:o6}', 'cash', 250), ('{id:p7}', '{id:o7}', 'card', 333);
INSERT INTO order_items (id, order_id, menu_item_id, item_name, quantity, unit_price, line_total) VALUES
  ('{id:i1}', '{id:o1}', '{id:latte}', 'Latte', 2, 500, 999),
  ('{id:i2}', '{id:o2}', '{id:tea}', 'Tea', 3, 259, 777),
  ('{id:i3}', '{id:o3}', '{id:latte}', 'Latte', 2, 500, 1000),
  ('{id:i4a}', '{id:o4}', '{id:latte}', 'Latte', 1, 500, 500),
  ('{id:i4b}', '{id:o4}', '{id:cake}', 'Cake', 3, 333, 1000),
  ('{id:i5}', '{id:o5}', '{id:tea}', 'Tea', 1, 400, 400),
  ('{id:i6}', '{id:o6}', '{id:tea}', 'Tea', 1, 250, 250),
  ('{id:i7}', '{id:o7}', '{id:cake}', 'Cake', 1, 333, 333);
INSERT INTO order_refunds (id, order_id, till_id, amount, method, is_cash, reason, issued_by, issued_at, created_at) VALUES
  ('{id:r1}', '{id:o3}', '{id:till}', 300, 'cash', true, 'customer_request', '{id:sara}', '2026-09-10 01:00+03', '2026-09-10 01:00+03'),
  ('{id:r2}', '{id:o5}', '{id:till}', 400, 'card', false, 'customer_request', '{id:sara}', '2026-09-11 10:00+03', '2026-09-11 10:00+03'),
  ('{id:r3}', '{id:o1}', '{id:till}', 99, 'cash', true, 'customer_request', '{id:sara}', '2026-09-11 14:00+03', '2026-09-11 14:00+03');
UPDATE orders SET status = 'refunded' WHERE id = '{id:o5}';
UPDATE orders SET status = 'voided', voided_at = '2026-09-11 13:05+03', voided_by = '{id:sara}', void_reason = 'wrong_order'
 WHERE id = '{id:o6}';
";

/// The windows recorded, as branch-local days.
const VECTOR_WINDOWS: &[(&str, &str)] = &[
    ("2026-09-10", "2026-09-11"),
    ("2026-09-09", "2026-09-09"),
    ("2026-09-11", "2026-09-11"),
    ("2026-09-09", "2026-09-12"),
    ("2026-09-13", "2026-09-13"),
];

fn vector_scrub(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for k in ["changed_at", "updated_at", "printed_at", "seq"] {
                m.remove(k);
            }
            if let Some(Value::Array(legs)) = m.get_mut("payment_legs") {
                legs.sort_by_key(|l| {
                    (
                        l["method"].as_str().unwrap_or("").to_string(),
                        l["amount"].as_i64(),
                    )
                });
            }
            m.values_mut().for_each(vector_scrub);
        }
        Value::Array(a) => a.iter_mut().for_each(vector_scrub),
        _ => {}
    }
}

#[sqlx::test]
async fn pos_metrics_vectors(pool: PgPool) {
    let mut sql = VECTOR_SQL.to_string();
    while let Some(start) = sql.find("{id:") {
        let end = start + sql[start..].find('}').unwrap();
        let label = sql[start + 4..end].to_string();
        sql.replace_range(start..=end, &vid(&label).to_string());
    }
    sqlx::raw_sql(&sql).execute(&pool).await.expect("seed");
    let (org, branch) = (vid("org"), vid("branch"));

    let body = madar_rust::sync::pull::PullRequest {
        branch_id: branch,
        device_id: None,
        types: None,
        limit: None,
        ledger_page_size: None,
        snapshot_cursor: None,
    };
    let full = madar_rust::sync::pull::pull_core(&pool, org, &body, None)
        .await
        .unwrap();
    let rows = |ty: &str| -> Vec<Value> {
        let mut v: Vec<Value> = full.data.get(ty).cloned().unwrap_or_default();
        v.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        v
    };
    assert_eq!(rows("order").len(), 7, "every sale rides the snapshot");
    assert_eq!(rows("refund").len(), 3);

    let mut expected = Vec::new();
    for (from, to) in VECTOR_WINDOWS {
        let (from, to) = (from.parse().unwrap(), to.parse().unwrap());
        let r = madar_rust::reports::pos_metrics::compute(&pool, branch, from, to)
            .await
            .unwrap();
        expected.push(serde_json::to_value(r).unwrap());
    }
    let mut doc = serde_json::json!({
        "about": "POS metrics vectors generated by MadarRust src/reports/pos_metrics_tests.rs; \
                  rows are /sync/pull projections, expected is GET /reports/branches/{id}/pos-metrics.",
        "branch_id": branch,
        "rows": {
            "till": rows("till"),
            "order": rows("order"),
            "refund": rows("refund"),
            "payment_method": rows("payment_method"),
        },
        "expected": expected,
    });
    vector_scrub(&mut doc);
    let text = serde_json::to_string_pretty(&doc).unwrap() + "\n";
    if std::env::var("MADAR_WRITE_POS_METRICS_VECTORS").is_ok() {
        std::fs::write(vectors_out(), &text).unwrap();
        return;
    }
    let committed: Value = serde_json::from_str(madar_money::vectors::POS_METRICS)
        .expect("madar-money's pos_metrics_vectors.json parses");
    assert_eq!(
        committed, doc,
        "pos-metrics or its projections changed: regenerate the vectors into madar-shared"
    );
}
