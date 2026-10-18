//! Reconcile the rollup-backed sales reports (DB Phase 2.6) against the live
//! computation they replace, per org and per day, on a copy of prod.
//!
//! For every org, every branch and every covered closed day, it runs the OLD
//! computation (`Source::Live`, the reports' original SQL) and the NEW one
//! (`Source::Rollups`) of every switched endpoint and compares the JSON:
//!   * `GET /reports/branches/{id}/sales`        — each branch, and "all branches"
//!   * `GET /reports/branches/{id}/sales/timeseries` (daily, monthly)
//!   * `GET /reports/orgs/{id}/comparison`
//!   * `GET /reports/branches/{id}/sales/peak-hours` and `…/peak-days`
//!   * `GET /reports/branches/{id}/addons`
//! over the dashboard's day window (local midnight to midnight − 1 ms), each
//! calendar month, all history (no bounds), and a window that starts and ends
//! mid-day (so the live intervals are exercised too).
//!
//! ```text
//! DATABASE_URL=postgres://.../<prod copy> cargo run --bin report-rollup-reconcile
//! ```
//! Prints one line per difference and a summary; exits 1 when anything differs.

use chrono::{DateTime, Duration, Utc};
use madar_rust::reports::peaks;
use madar_rust::reports::sales::{self, Source};
use sqlx::PgPool;
use uuid::Uuid;

struct Tally {
    compared: u64,
    diffs: u64,
    org_days: u64,
    /// Time spent in each computation, all-history windows only (where the
    /// rollups matter most).
    live_all: std::time::Duration,
    rolled_all: std::time::Duration,
}

impl Tally {
    fn check(&mut self, what: &str, live: serde_json::Value, rolled: serde_json::Value) {
        self.compared += 1;
        if live != rolled {
            self.diffs += 1;
            if self.diffs <= 20 {
                println!("DIFF {what}\n  live:   {live}\n  rolled: {rolled}");
            }
        }
    }
}

type Window = (String, Option<DateTime<Utc>>, Option<DateTime<Utc>>);

async fn branch_sales_pair(
    pool: &PgPool,
    t: &mut Tally,
    label: &str,
    ids: &[Uuid],
    scope: Uuid,
    w: &Window,
) {
    let run = |src| {
        sales::branch_sales_report(
            pool,
            ids,
            scope,
            String::new(),
            w.1,
            w.2,
            Some(1000),
            None,
            src,
        )
    };
    let t0 = std::time::Instant::now();
    let live = run(Source::Live).await.expect("live branch sales");
    let t1 = std::time::Instant::now();
    let rolled = run(Source::Rollups).await.expect("rolled branch sales");
    if w.1.is_none() {
        t.live_all += t1 - t0;
        t.rolled_all += t1.elapsed();
    }
    t.check(
        &format!("branch_sales {label} {}", w.0),
        serde_json::to_value(live).unwrap(),
        serde_json::to_value(rolled).unwrap(),
    );
    // The `exclude_items` path of total_line_items, with the top item left out.
    let top = sales::branch_sales_report(pool, ids, scope, String::new(), w.1, w.2, Some(1), None, Source::Live)
        .await
        .expect("top");
    if let Some(item) = top.top_items.first() {
        let ex = Some(vec![item.menu_item_id]);
        let live = sales::branch_sales_report(pool, ids, scope, String::new(), w.1, w.2, None, ex.clone(), Source::Live)
            .await
            .expect("live excl");
        let rolled = sales::branch_sales_report(pool, ids, scope, String::new(), w.1, w.2, None, ex, Source::Rollups)
            .await
            .expect("rolled excl");
        t.check(
            &format!("branch_sales(exclude) {label} {}", w.0),
            serde_json::to_value(live).unwrap(),
            serde_json::to_value(rolled).unwrap(),
        );
    }
}

async fn timeseries_pair(
    pool: &PgPool,
    t: &mut Tally,
    label: &str,
    ids: &[Uuid],
    tz: &str,
    w: &Window,
) {
    for g in ["daily", "monthly"] {
        let live = sales::sales_timeseries(pool, ids, tz, Some(g), w.1, w.2, Source::Live)
            .await
            .expect("live ts");
        let rolled = sales::sales_timeseries(pool, ids, tz, Some(g), w.1, w.2, Source::Rollups)
            .await
            .expect("rolled ts");
        t.check(
            &format!("timeseries {g} {label} {}", w.0),
            serde_json::to_value(live).unwrap(),
            serde_json::to_value(rolled).unwrap(),
        );
    }
}

async fn peaks_pair(pool: &PgPool, t: &mut Tally, label: &str, ids: &[Uuid], tz: &str, w: &Window) {
    let live = peaks::peak_hours(pool, ids, tz, w.1, w.2, Source::Live).await.expect("live peak hours");
    let rolled = peaks::peak_hours(pool, ids, tz, w.1, w.2, Source::Rollups).await.expect("rolled peak hours");
    t.check(
        &format!("peak_hours {label} {}", w.0),
        serde_json::to_value(live).unwrap(),
        serde_json::to_value(rolled).unwrap(),
    );
    let live = peaks::peak_days(pool, ids, tz, w.1, w.2, Source::Live).await.expect("live peak days");
    let rolled = peaks::peak_days(pool, ids, tz, w.1, w.2, Source::Rollups).await.expect("rolled peak days");
    t.check(
        &format!("peak_days {label} {}", w.0),
        serde_json::to_value(live).unwrap(),
        serde_json::to_value(rolled).unwrap(),
    );
    let live = peaks::addon_sales(pool, ids, w.1, w.2, Source::Live).await.expect("live addons");
    let rolled = peaks::addon_sales(pool, ids, w.1, w.2, Source::Rollups).await.expect("rolled addons");
    t.check(
        &format!("addons {label} {}", w.0),
        serde_json::to_value(live).unwrap(),
        serde_json::to_value(rolled).unwrap(),
    );
}

async fn comparison_pair(pool: &PgPool, t: &mut Tally, org: Uuid, w: &Window) {
    let live = sales::org_comparison(pool, org, w.1, w.2, Source::Live)
        .await
        .expect("live comparison");
    let rolled = sales::org_comparison(pool, org, w.1, w.2, Source::Rollups)
        .await
        .expect("rolled comparison");
    t.check(
        &format!("comparison {org} {}", w.0),
        serde_json::to_value(live).unwrap(),
        serde_json::to_value(rolled).unwrap(),
    );
}

/// The dashboard's day window, each month, all history, and a mid-day one.
async fn windows(pool: &PgPool, tz: &str, since: chrono::NaiveDate, through: chrono::NaiveDate) -> Vec<Window> {
    let ms = Duration::milliseconds(1);
    let days = sqlx::query!(
        r#"SELECT g::date AS "day!",
                  (g::date::timestamp AT TIME ZONE $1) AS "lo!",
                  ((g::date + 1)::timestamp AT TIME ZONE $1) AS "hi!"
             FROM generate_series($2::date::timestamp, $3::date::timestamp, interval '1 day') g"#,
        tz,
        since,
        through
    )
    .fetch_all(pool)
    .await
    .expect("days");
    let mut out: Vec<Window> = days
        .iter()
        .map(|d| (d.day.to_string(), Some(d.lo), Some(d.hi - ms)))
        .collect();
    let months = sqlx::query!(
        r#"SELECT to_char(m, 'YYYY-MM') AS "label!",
                  (m AT TIME ZONE $1) AS "lo!",
                  ((m + interval '1 month') AT TIME ZONE $1) AS "hi!"
             FROM generate_series(date_trunc('month', $2::date::timestamp), $3::date::timestamp, interval '1 month') m"#,
        tz,
        since,
        through
    )
    .fetch_all(pool)
    .await
    .expect("months");
    for m in months {
        out.push((format!("month {}", m.label), Some(m.lo), Some(m.hi - ms)));
    }
    out.push(("all history".into(), None, None));
    if let (Some(first), Some(last)) = (days.first(), days.last()) {
        out.push((
            "mid-day to mid-day".into(),
            Some(first.lo + Duration::hours(13)),
            Some(last.lo + Duration::hours(9)),
        ));
    }
    out
}

#[tokio::main]
async fn main() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let pool = PgPool::connect(&url).await.expect("connect");
    let started = std::time::Instant::now();
    let mut t = Tally {
        compared: 0,
        diffs: 0,
        org_days: 0,
        live_all: Default::default(),
        rolled_all: Default::default(),
    };

    let orgs = sqlx::query!(
        r#"SELECT o.id, COALESCE(o.timezone::text, 'Africa/Cairo') AS "tz!" FROM organizations o ORDER BY o.id"#
    )
    .fetch_all(&pool)
    .await
    .expect("orgs");
    for org in orgs {
        let branches = sqlx::query!(
            r#"SELECT b.id, effective_timezone(b.id) AS "tz!", c.since AS "since?", c.through AS "through?"
                 FROM branches b LEFT JOIN sales_daily_coverage c ON c.branch_id = b.id
                WHERE b.org_id = $1 AND b.deleted_at IS NULL ORDER BY b.id"#,
            org.id
        )
        .fetch_all(&pool)
        .await
        .expect("branches");
        let ids: Vec<Uuid> = branches.iter().map(|b| b.id).collect();
        let mut org_since: Option<chrono::NaiveDate> = None;
        let mut org_through: Option<chrono::NaiveDate> = None;
        for b in &branches {
            let (Some(since), Some(through)) = (b.since, b.through) else { continue };
            if since > through {
                continue;
            }
            org_since = Some(org_since.map_or(since, |s| s.min(since)));
            org_through = Some(org_through.map_or(through, |s| s.max(through)));
            for w in windows(&pool, &b.tz, since, through).await {
                branch_sales_pair(&pool, &mut t, &b.id.to_string(), &[b.id], b.id, &w).await;
                timeseries_pair(&pool, &mut t, &b.id.to_string(), &[b.id], &b.tz, &w).await;
                peaks_pair(&pool, &mut t, &b.id.to_string(), &[b.id], &b.tz, &w).await;
            }
        }
        // Org-wide: "all branches", the comparison, the org-zone timeseries.
        let (Some(since), Some(through)) = (org_since, org_through) else { continue };
        let ws = windows(&pool, &org.tz, since, through).await;
        t.org_days += ws.iter().filter(|w| !w.0.contains(' ')).count() as u64;
        for w in &ws {
            branch_sales_pair(&pool, &mut t, "all-branches", &ids, Uuid::nil(), w).await;
            timeseries_pair(&pool, &mut t, "all-branches", &ids, &org.tz, w).await;
            peaks_pair(&pool, &mut t, "all-branches", &ids, &org.tz, w).await;
            comparison_pair(&pool, &mut t, org.id, w).await;
        }
    }
    println!(
        "org-days {} · comparisons {} · differences {} · {:?}",
        t.org_days,
        t.compared,
        t.diffs,
        started.elapsed()
    );
    println!(
        "branch sales over all history: live {:?} vs rollups {:?} (sum over scopes)",
        t.live_all, t.rolled_all
    );
    if t.diffs > 0 {
        std::process::exit(1);
    }
}
