//! The SQL copy of the refund split, pinned to the Rust one.
//!
//! A refund's tax and service charge are computed by the
//! `order_refunds_before_insert` trigger through `refund_share()` — the copy the
//! books trust. The server's and the till's Rust (`madar_money::tax::refund_split`)
//! compute the same split for refunds still queued on a device. Until now
//! nothing tied the SQL to the Rust; this runs `refund_share()` over every
//! vector in madar-money's `refund_split_vectors.json`, exactly as the trigger
//! calls it (`share(already + amount) - share(already)`).
//!
//! Vectors marked `"sql": false` have a negative earlier sum or amount — inputs
//! the trigger never passes (an amount is positive, `already` is a sum of them)
//! and on which the Rust clamps first. They are skipped, and counted.

use madar_money::tax::refund_vectors::RefundVector;
use sqlx::PgPool;

#[sqlx::test]
async fn refund_share_takes_back_what_refund_split_does(pool: PgPool) {
    let vectors: Vec<RefundVector> =
        serde_json::from_str(madar_money::vectors::REFUND_SPLIT).expect("refund vectors parse");
    assert!(vectors.len() > 300, "the fixture looks truncated");
    let mut checked = 0;
    let mut drift = Vec::new();
    for v in vectors.iter().filter(|v| v.sql) {
        let (tax, sc): (i32, i32) = sqlx::query_as(
            "SELECT refund_share($1, $4::bigint + $5::bigint, $3) - refund_share($1, $4::bigint, $3),
                    refund_share($2, $4::bigint + $5::bigint, $3) - refund_share($2, $4::bigint, $3)",
        )
        .bind(v.order_tax as i32)
        .bind(v.order_service_charge as i32)
        .bind(v.order_total as i32)
        .bind(v.refunded_before)
        .bind(v.amount)
        .fetch_one(&pool)
        .await
        .unwrap();
        if (tax as i64, sc as i64) != (v.tax, v.service_charge) {
            drift.push(format!("{v:?} -> sql ({tax}, {sc})"));
        }
        checked += 1;
    }
    assert!(
        checked > 300,
        "only {checked} vectors are in the SQL's domain"
    );
    assert!(
        drift.is_empty(),
        "refund_share() and refund_split disagree on {} of {checked} refunds:\n{}",
        drift.len(),
        drift.join("\n")
    );
}
