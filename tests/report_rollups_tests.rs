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
