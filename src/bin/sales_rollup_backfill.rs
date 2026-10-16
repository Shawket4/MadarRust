//! Fill the report rollups (`sales_daily_sold*`, DB Phase 2.6) for all history
//! now, instead of waiting for the server's worker: extend every branch's
//! coverage to the last day that closed over an hour ago, then recompute every
//! dirty day. Batched (50 days per transaction), resumable and idempotent: run
//! it again and it reports 0 / 0. Reports stay exact while it runs (an
//! uncovered or dirty day is read live).
//!
//! ```text
//! DATABASE_URL=postgres://.../<db> cargo run --bin sales-rollup-backfill
//! ```

#[tokio::main]
async fn main() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let pool = sqlx::PgPool::connect(&url).await.expect("connect");
    let t = std::time::Instant::now();
    let tick = madar_rust::rollups::report_tick(
        &pool,
        std::time::Duration::from_secs(3600),
        i64::MAX,
    )
    .await
    .expect("report rollups");
    println!(
        "covered {} new days, recomputed {} days in {:?}",
        tick.days_covered,
        tick.days_recomputed,
        t.elapsed()
    );
}
