//! DB Phase 2.6: the sales reports read the closed-day rollups and answer
//! exactly what the live computation answers — the same JSON, field for field —
//! for closed days, today, days a late write made dirty, and windows that cut
//! a day in half.

use chrono::{DateTime, Duration, Utc};
use madar_rust::reports::sales::{self, Source};
use madar_rust::rollups;
use sqlx::PgPool;
use uuid::Uuid;

struct Shop {
    org: Uuid,
    branch: Uuid,
    till: Uuid,
    teller: Uuid,
    tz: String,
}

async fn shop(pool: &PgPool) -> Shop {
    let (org, branch, till, teller) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations (id, name, slug) VALUES ('{org}', 'Rollup Org', 'ru-{org}');
         INSERT INTO users (id, name, role, org_id, pin_hash) VALUES ('{teller}', 'T', 'teller', '{org}', 'x');
         INSERT INTO branches (id, org_id, name, code) VALUES ('{branch}', '{org}', 'Main', 'MAIN');
         INSERT INTO tills (id, branch_id, teller_id) VALUES ('{till}', '{branch}', '{teller}');
         INSERT INTO categories (id, org_id, name) VALUES (md5('{org}cat')::uuid, '{org}', 'Coffee');"
    ))
    .execute(pool)
    .await
    .unwrap();
    let tz: String = sqlx::query_scalar("SELECT effective_timezone($1)")
        .bind(branch)
        .fetch_one(pool)
        .await
        .unwrap();
    Shop { org, branch, till, teller, tz }
}

/// Noon, `days_ago` local days back, at the branch.
async fn local_noon(pool: &PgPool, s: &Shop, days_ago: i32) -> DateTime<Utc> {
    sqlx::query_scalar(
        "SELECT (((now() AT TIME ZONE $1)::date - $2)::timestamp + interval '12 hours') AT TIME ZONE $1",
    )
    .bind(&s.tz)
    .bind(days_ago)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A sale with a line, an add-on, a payment and a tip.
#[allow(clippy::too_many_arguments)]
async fn sale(pool: &PgPool, s: &Shop, at: DateTime<Utc>, status: &str, total: i32, item: &str, method: &str) -> Uuid {
    let id = Uuid::new_v4();
    let line = Uuid::new_v4();
    sqlx::raw_sql(&format!(
        "INSERT INTO orders (id, branch_id, till_id, teller_id, order_number, payment_method, order_ref,
                             subtotal, tax_amount, discount_amount, total_amount, tip_amount, status, created_at, service_mode)
              VALUES ('{id}', '{b}', '{t}', '{u}', (SELECT count(*) + 1 FROM orders), '{method}', 'R-{id}',
                      {total}, 10, 5, {total} + 5, 3, '{status}', '{at}', 'takeaway');
         INSERT INTO menu_items (id, org_id, name, category_id)
              VALUES (md5('{org}{item}')::uuid, '{org}', '{item}', md5('{org}cat')::uuid)
              ON CONFLICT (id) DO NOTHING;
         INSERT INTO order_items (id, order_id, menu_item_id, item_name, unit_price, quantity, line_total, name_translations)
              VALUES ('{line}', '{id}', md5('{org}{item}')::uuid, '{item}', {total}, 2, {total}, '{{\"ar\": \"{item}\"}}');
         INSERT INTO order_item_addons (order_item_id, addon_item_id, addon_name, unit_price, quantity, line_total)
              VALUES ('{line}', gen_random_uuid(), 'Oat', 5, 1, 5);
         INSERT INTO order_payments (order_id, method, amount, till_id) VALUES ('{id}', '{method}', {total}, '{t}');",
        b = s.branch,
        org = s.org,
        t = s.till,
        u = s.teller,
        at = at.to_rfc3339(),
    ))
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn refund(pool: &PgPool, s: &Shop, order: Uuid, amount: i32) {
    sqlx::query(
        "INSERT INTO order_refunds (org_id, branch_id, order_id, till_id, amount, method, is_cash, reason, issued_by, tax_amount)
         VALUES ($1, $2, $3, $4, $5, 'cash', true, 'goodwill', $6, 1)",
    )
    .bind(s.org)
    .bind(s.branch)
    .bind(order)
    .bind(s.till)
    .bind(amount)
    .bind(s.teller)
    .execute(pool)
    .await
    .unwrap();
}

async fn tick(pool: &PgPool) -> rollups::ReportTick {
    rollups::report_tick(pool, std::time::Duration::ZERO, i64::MAX)
        .await
        .unwrap()
}

/// Rollup days the plan would read for this window.
async fn rollup_days(pool: &PgPool, s: &Shop, from: Option<DateTime<Utc>>, to: Option<DateTime<Utc>>) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM sales_daily_plan($1, $2, $3, true) WHERE day IS NOT NULL")
        .bind(vec![s.branch])
        .bind(from)
        .bind(to)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Every switched report, old vs new, over one window.
async fn assert_same(pool: &PgPool, s: &Shop, from: Option<DateTime<Utc>>, to: Option<DateTime<Utc>>, what: &str) {
    let ids = [s.branch];
    for ex in [None, Some(vec![Uuid::new_v4()])] {
        let run = |src| {
            sales::branch_sales_report(pool, &ids, s.branch, "Main".into(), from, to, None, ex.clone(), src)
        };
        let live = serde_json::to_value(run(Source::Live).await.unwrap()).unwrap();
        let rolled = serde_json::to_value(run(Source::Rollups).await.unwrap()).unwrap();
        assert_eq!(live, rolled, "branch sales, {what}");
    }
    for g in ["daily", "monthly", "hourly"] {
        let live = sales::sales_timeseries(pool, &ids, &s.tz, Some(g), from, to, Source::Live)
            .await
            .unwrap();
        let rolled = sales::sales_timeseries(pool, &ids, &s.tz, Some(g), from, to, Source::Rollups)
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(live).unwrap(),
            serde_json::to_value(rolled).unwrap(),
            "timeseries {g}, {what}"
        );
    }
    let live = sales::org_comparison(pool, s.org, from, to, Source::Live).await.unwrap();
    let rolled = sales::org_comparison(pool, s.org, from, to, Source::Rollups).await.unwrap();
    assert_eq!(
        serde_json::to_value(live).unwrap(),
        serde_json::to_value(rolled).unwrap(),
        "comparison, {what}"
    );
}

/// The dashboard's window for `days_ago .. 0` (today included).
async fn dashboard_window(pool: &PgPool, s: &Shop, days_ago: i32) -> (DateTime<Utc>, DateTime<Utc>) {
    let r: (DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
        "SELECT ((now() AT TIME ZONE $1)::date - $2)::timestamp AT TIME ZONE $1,
                (((now() AT TIME ZONE $1)::date + 1)::timestamp AT TIME ZONE $1) - interval '1 millisecond'",
    )
    .bind(&s.tz)
    .bind(days_ago)
    .fetch_one(pool)
    .await
    .unwrap();
    r
}

async fn seed_history(pool: &PgPool, s: &Shop) -> Vec<Uuid> {
    let mut ids = Vec::new();
    for days in [5, 4, 3, 1] {
        let at = local_noon(pool, s, days).await;
        ids.push(sale(pool, s, at, "completed", 300, "Latte", "cash").await);
        ids.push(sale(pool, s, at + Duration::hours(1), "completed", 200, "Mocha", "card").await);
        ids.push(sale(pool, s, at + Duration::hours(2), "voided", 900, "Latte", "cash").await);
        ids.push(sale(pool, s, at + Duration::hours(3), "refunded", 150, "Tea", "cash").await);
    }
    // Refunds against old sales, on the sale's day.
    refund(pool, s, ids[0], 100).await;
    refund(pool, s, ids[5], 50).await;
    // Today.
    let now = local_noon(pool, s, 0).await;
    sale(pool, s, now.min(Utc::now() - Duration::minutes(1)), "completed", 400, "Latte", "cash").await;
    ids
}

#[sqlx::test]
async fn closed_days_come_from_the_rollups_and_match_the_live_reports(pool: PgPool) {
    let s = shop(&pool).await;
    seed_history(&pool, &s).await;
    let t = tick(&pool).await;
    assert!(t.days_covered >= 4 && t.days_recomputed >= 4, "{t:?}");
    assert_eq!(tick(&pool).await, rollups::ReportTick::default(), "a second pass does nothing");

    let (from, to) = dashboard_window(&pool, &s, 6).await;
    assert!(rollup_days(&pool, &s, Some(from), Some(to)).await >= 4, "closed days are read from rollups");
    assert_same(&pool, &s, Some(from), Some(to), "last 7 days").await;
    assert_same(&pool, &s, None, None, "all history").await;
    // A window that starts and ends mid-day: those days are partly live.
    assert_same(&pool, &s, Some(from + Duration::hours(13)), Some(to - Duration::hours(30)), "mid-day").await;
    // Today alone is never a rollup day.
    let (today_from, today_to) = dashboard_window(&pool, &s, 0).await;
    assert_eq!(rollup_days(&pool, &s, Some(today_from), Some(today_to)).await, 0);
    assert_same(&pool, &s, Some(today_from), Some(today_to), "today").await;
}

#[sqlx::test]
async fn a_late_write_to_a_closed_day_is_read_live_until_recomputed(pool: PgPool) {
    let s = shop(&pool).await;
    let ids = seed_history(&pool, &s).await;
    tick(&pool).await;
    let (from, to) = dashboard_window(&pool, &s, 6).await;
    let before = rollup_days(&pool, &s, Some(from), Some(to)).await;

    // A queued offline sale replayed into day -3, a refund of a day -5 sale,
    // and a void of a day -4 sale: each marks its SALE's day dirty.
    let at = local_noon(&pool, &s, 3).await + Duration::minutes(30);
    sale(&pool, &s, at, "completed", 700, "Flat white", "card").await;
    refund(&pool, &s, ids[1], 20).await;
    sqlx::query("UPDATE orders SET status = 'voided' WHERE id = $1")
        .bind(ids[4])
        .execute(&pool)
        .await
        .unwrap();
    let dirty: i64 = sqlx::query_scalar("SELECT count(*) FROM sales_daily_dirty WHERE branch_id = $1")
        .bind(s.branch)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(dirty, 3);
    assert_eq!(rollup_days(&pool, &s, Some(from), Some(to)).await, before - 3);
    assert_same(&pool, &s, Some(from), Some(to), "with dirty days").await;

    // The worker recomputes them; the rollups are read again, still exact.
    let t = tick(&pool).await;
    assert_eq!(t.days_recomputed, 3);
    assert_eq!(rollup_days(&pool, &s, Some(from), Some(to)).await, before);
    assert_same(&pool, &s, Some(from), Some(to), "recomputed").await;
}

#[sqlx::test]
async fn todays_writes_mark_nothing(pool: PgPool) {
    let s = shop(&pool).await;
    seed_history(&pool, &s).await;
    tick(&pool).await;
    sale(&pool, &s, Utc::now(), "completed", 100, "Latte", "cash").await;
    let dirty: i64 = sqlx::query_scalar("SELECT count(*) FROM sales_daily_dirty")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(dirty, 0);
}

#[sqlx::test]
async fn a_timezone_change_drops_the_branchs_rollups(pool: PgPool) {
    let s = shop(&pool).await;
    seed_history(&pool, &s).await;
    tick(&pool).await;
    sqlx::query("UPDATE branches SET timezone = 'Asia/Dubai' WHERE id = $1")
        .bind(s.branch)
        .execute(&pool)
        .await
        .unwrap();
    let left: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM sales_daily_coverage WHERE branch_id = $1),
                (SELECT count(*) FROM sales_daily_sold WHERE branch_id = $1)",
    )
    .bind(s.branch)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(left, (0, 0));
    let s = Shop { tz: "Asia/Dubai".into(), ..s };
    let (from, to) = dashboard_window(&pool, &s, 6).await;
    assert_eq!(rollup_days(&pool, &s, Some(from), Some(to)).await, 0, "all live until rebuilt");
    assert_same(&pool, &s, Some(from), Some(to), "after the zone change").await;
    tick(&pool).await;
    assert!(rollup_days(&pool, &s, Some(from), Some(to)).await > 0);
    assert_same(&pool, &s, Some(from), Some(to), "rebuilt in the new zone").await;
}

/// The plan's one allowance: the dashboard's day ends at midnight − 1 ms; a
/// sale in that last millisecond keeps the day live (it is outside the
/// window, and the rollup would count it).
#[sqlx::test]
async fn a_sale_in_the_last_millisecond_keeps_the_day_live(pool: PgPool) {
    let s = shop(&pool).await;
    seed_history(&pool, &s).await;
    let (day_from, day_end): (DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
        "SELECT ((now() AT TIME ZONE $1)::date - 4)::timestamp AT TIME ZONE $1,
                ((now() AT TIME ZONE $1)::date - 3)::timestamp AT TIME ZONE $1",
    )
    .bind(&s.tz)
    .fetch_one(&pool)
    .await
    .unwrap();
    sale(&pool, &s, day_end - Duration::microseconds(300), "completed", 999, "Late", "cash").await;
    tick(&pool).await;
    let to = day_end - Duration::milliseconds(1);
    assert_eq!(rollup_days(&pool, &s, Some(day_from), Some(to)).await, 0);
    assert_same(&pool, &s, Some(day_from), Some(to), "sliver").await;
}

/// Tenants read the rollups through RLS like any report table.
#[sqlx::test]
async fn the_rollup_tables_are_tenant_scoped(pool: PgPool) {
    let a = shop(&pool).await;
    let b = shop(&pool).await;
    seed_history(&pool, &a).await;
    seed_history(&pool, &b).await;
    tick(&pool).await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('app.org_id', $1, true)")
        .bind(a.org.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("SET LOCAL ROLE madar_app").execute(&mut *tx).await.unwrap();
    for t in ["sales_daily_sold", "sales_daily_sold_item", "sales_daily_sold_method", "sales_daily_coverage"] {
        let other: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {t} WHERE org_id <> $1"))
            .bind(a.org)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(other, 0, "{t}");
    }
}

// ── maintenance stays off the order write path (k6 2026-10-17) ─────────────

/// Replaces `sales_daily_mark` in this test's database with one that only
/// logs its calls, so a test can count every dirty-mark any trigger makes.
async fn log_marks(pool: &PgPool) {
    sqlx::raw_sql(
        "CREATE TABLE mark_log (branches uuid[], ats timestamptz[]);
         GRANT INSERT ON mark_log TO madar_app;
         CREATE OR REPLACE FUNCTION sales_daily_mark(p_branch uuid[], p_at timestamptz[])
         RETURNS void LANGUAGE sql SECURITY DEFINER SET search_path = public AS
         $fn$ INSERT INTO mark_log VALUES (p_branch, p_at) $fn$;",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn marks(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM mark_log")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// An org admin's token, their open till and a priced item, for `POST /orders`.
async fn till_shop(pool: &PgPool, s: &Shop) -> (String, Uuid, Uuid) {
    madar_rust::permissions::seeder::seed_role_permissions(pool).await.unwrap();
    let admin = Uuid::new_v4();
    sqlx::raw_sql(&format!(
        "INSERT INTO org_payment_methods (org_id, name, color, icon, is_cash, is_active)
              VALUES ('{org}', 'cash', '#000', 'x', true, true);
         INSERT INTO users (id, org_id, name, email, password_hash, role)
              VALUES ('{admin}', '{org}', 'Owner', '{admin}@t.com', 'h', 'org_admin');
         INSERT INTO menu_items (id, org_id, name, category_id, base_price, is_active)
              VALUES (md5('{org}latte')::uuid, '{org}', 'Latte', md5('{org}cat')::uuid, 5000, true);",
        org = s.org,
    ))
    .execute(pool)
    .await
    .unwrap();
    let jwt = madar_rust::auth::jwt::create_token(
        &madar_rust::auth::jwt::JwtSecret("secret".into()),
        admin,
        Some(s.org),
        madar_rust::models::UserRole::OrgAdmin,
        None,
        24,
    )
    .unwrap();
    let item: Uuid = sqlx::query_scalar("SELECT md5($1 || 'latte')::uuid")
        .bind(s.org.to_string())
        .fetch_one(pool)
        .await
        .unwrap();
    let till: Uuid = sqlx::query_scalar(
        "INSERT INTO tills (branch_id, teller_id, status, opening_cash) VALUES ($1, $2, 'open', 0) RETURNING id",
    )
    .bind(s.branch)
    .bind(admin)
    .fetch_one(pool)
    .await
    .unwrap();
    (jwt, till, item)
}

/// Creating an order (its INSERTs into orders, order_items, order_payments
/// and the UPDATEs the create path makes) does no rollup work: no trigger
/// gets as far as a dirty-mark, and no INSERT trigger of the rollups is left
/// on the line, add-on or payment tables. A sale replayed into a closed day
/// still marks it.
#[sqlx::test]
async fn order_create_does_no_rollup_work(pool: PgPool) {
    use actix_web::{App, test, web};
    let s = shop(&pool).await;
    seed_history(&pool, &s).await;
    tick(&pool).await;
    let (jwt, till, item) = till_shop(&pool, &s).await;
    log_marks(&pool).await;

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(madar_rust::auth::jwt::JwtSecret("secret".into())))
            .app_data(web::Data::new(madar_rust::realtime::hub::BranchEventHub::new()))
            .configure(madar_rust::orders::routes::configure),
    )
    .await;
    for qty in [1, 3] {
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/orders")
                .insert_header(("Authorization", format!("Bearer {jwt}")))
                .set_json(serde_json::json!({
                    "branch_id": s.branch, "till_id": till, "payment_method": "cash",
                    "items": [{ "menu_item_id": item, "quantity": qty }],
                    "idempotency_key": Uuid::new_v4(),
                }))
                .to_request(),
        )
        .await;
        let status = resp.status();
        let body: serde_json::Value = test::read_body_json(resp).await;
        assert!(status.is_success(), "POST /orders → {status}: {body}");
    }
    let lines: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM order_items oi JOIN orders o ON o.id = oi.order_id WHERE o.till_id = $1",
    )
    .bind(till)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(lines, 2, "both orders were booked");
    assert_eq!(marks(&pool).await, 0, "order create made a rollup dirty-mark");
    let dirty: i64 = sqlx::query_scalar("SELECT count(*) FROM sales_daily_dirty")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(dirty, 0);

    // No rollup INSERT trigger on the child tables; the orders one is a row
    // trigger whose WHEN is false for a sale rung up now.
    let ins: Vec<(String, String)> = sqlx::query_as(
        "SELECT tgrelid::regclass::text, pg_get_triggerdef(oid) FROM pg_trigger
          WHERE tgname LIKE 'sales_daily_%' AND NOT tgisinternal AND (tgtype & 4) <> 0
            AND tgrelid::regclass::text IN ('orders', 'order_items', 'order_payments', 'order_item_addons')",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(ins.len(), 1, "{ins:?}");
    assert_eq!(ins[0].0, "orders");
    assert!(
        ins[0].1.contains("FOR EACH ROW WHEN ((new.created_at < (now() - '00:30:00'::interval)))"),
        "{}",
        ins[0].1
    );

    // The control: the same log sees a sale replayed into day -3, and a void
    // of an old sale.
    let at = local_noon(&pool, &s, 3).await;
    let late = sale(&pool, &s, at, "completed", 700, "Flat white", "card").await;
    assert_eq!(marks(&pool).await, 1, "a late sale marks its day");
    sqlx::query("UPDATE orders SET status = 'voided' WHERE id = $1")
        .bind(late)
        .execute(&pool)
        .await
        .unwrap();
    assert!(marks(&pool).await >= 2, "a void of an old sale marks its day");
}

/// The coverage never reaches within 30 minutes of now, whatever lag a caller
/// passes (the orders INSERT trigger's WHEN relies on it).
#[sqlx::test]
async fn coverage_keeps_a_30_minute_margin(pool: PgPool) {
    let s = shop(&pool).await;
    seed_history(&pool, &s).await;
    tick(&pool).await;
    let (through, expect): (chrono::NaiveDate, chrono::NaiveDate) = sqlx::query_as(
        "SELECT c.through, ((now() - interval '30 minutes') AT TIME ZONE $2)::date - 1
           FROM sales_daily_coverage c WHERE c.branch_id = $1",
    )
    .bind(s.branch)
    .bind(&s.tz)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(through, expect);
}

/// Once D-2 is two hours closed, a rollup of it computed before then is
/// recomputed one more time (a straggling writer); after that, nothing.
#[sqlx::test]
async fn the_catch_up_recomputes_d2_once_more(pool: PgPool) {
    let s = shop(&pool).await;
    seed_history(&pool, &s).await;
    let at = local_noon(&pool, &s, 2).await;
    sale(&pool, &s, at, "completed", 250, "Latte", "cash").await;
    tick(&pool).await;
    // As if D-2 had been computed an hour after it closed.
    sqlx::query(
        "UPDATE sales_daily_sold SET computed_at = ((business_date + 1)::timestamp AT TIME ZONE $2) + interval '1 hour'
          WHERE branch_id = $1 AND business_date = (now() AT TIME ZONE $2)::date - 2",
    )
    .bind(s.branch)
    .bind(&s.tz)
    .execute(&pool)
    .await
    .unwrap();
    let t = tick(&pool).await;
    assert_eq!((t.days_covered, t.days_recomputed), (1, 1), "{t:?}");
    assert_eq!(tick(&pool).await, rollups::ReportTick::default(), "then nothing");
    let (from, to) = dashboard_window(&pool, &s, 6).await;
    assert_same(&pool, &s, Some(from), Some(to), "after catch-up").await;
}

/// A till close recomputes its branch's dirty days at once.
#[sqlx::test]
async fn a_till_close_settles_its_branch(pool: PgPool) {
    let s = shop(&pool).await;
    let ids = seed_history(&pool, &s).await;
    tick(&pool).await;
    sqlx::query("UPDATE orders SET status = 'voided' WHERE id = $1")
        .bind(ids[4])
        .execute(&pool)
        .await
        .unwrap();
    let at = local_noon(&pool, &s, 3).await + Duration::minutes(30);
    sale(&pool, &s, at, "completed", 700, "Flat white", "card").await;
    assert_eq!(rollups::settle_till(&pool, s.till).await.unwrap(), 2);
    let dirty: i64 = sqlx::query_scalar("SELECT count(*) FROM sales_daily_dirty")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(dirty, 0);
    assert_eq!(rollups::settle_till(&pool, s.till).await.unwrap(), 0, "idempotent");
    let (from, to) = dashboard_window(&pool, &s, 6).await;
    assert_same(&pool, &s, Some(from), Some(to), "after the close").await;
}
