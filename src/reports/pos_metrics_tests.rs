//! `GET /reports/branches/{id}/pos-metrics`: capability first, branch scope,
//! branch-local days, and figures that agree with `branch_sales`.

use actix_web::{App, test, web};
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;
use crate::reports::pos_metrics::{PosMetricsReport, average_ticket};
use crate::reports::routes;

fn secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn token(user: Uuid, org: Uuid, role: UserRole, branch: Option<Uuid>) -> String {
    crate::auth::jwt::create_token(&secret(), user, Some(org), role, branch, 24).unwrap()
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
