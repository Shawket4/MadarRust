//! Dawam rules this server runs in SQL, pinned to madar-shared's copies — the
//! ones the staff app's core runs and this server's Rust calls.
//!
//! - A shift's instants (DW1): the roster resolver places a shift as
//!   `(date + time) AT TIME ZONE zone`, the end on the next date when it does
//!   not come after the start. Every case of `madar_dawam::vectors::SHIFT`
//!   (Cairo, Beirut, Berlin, Riyadh and UTC DST days) is placed by Postgres
//!   here and must be the crate's `shift::instants`.
//! - A percentage of a salary (DW3): `dawam_advance_cap` and an adjustment's
//!   `value_piastres` are `round(salary::numeric * percent / 100)`; every case
//!   of `madar_dawam::vectors::PERCENT` must be the crate's
//!   `pay::percent_of_salary` — and so is `staff::pricing::percent_of_salary`.
use madar_dawam::pay::percent_vectors::PercentVector;
use madar_dawam::shift::vectors::ShiftVector;
use rust_decimal::Decimal;
use sqlx::PgPool;

#[sqlx::test]
async fn postgres_places_every_shift_as_the_crate_does(pool: PgPool) {
    let vectors: Vec<ShiftVector> = serde_json::from_str(madar_dawam::vectors::SHIFT).unwrap();
    assert!(vectors.len() >= 100);
    for v in &vectors {
        let (start_at, end_at): (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>) =
            sqlx::query_as(
                "SELECT ($1::date + $2::time) AT TIME ZONE $4, \
                        ($1::date + $3::time + CASE WHEN $3::time <= $2::time THEN INTERVAL '1 day' \
                                                    ELSE INTERVAL '0 day' END) AT TIME ZONE $4",
            )
            .bind(v.date)
            .bind(v.start)
            .bind(v.end)
            .bind(&v.zone)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!((start_at, end_at), (v.start_at, v.end_at), "{v:?}");
    }
}

#[sqlx::test]
async fn a_percentage_of_a_salary_is_the_crates_in_sql_and_here(pool: PgPool) {
    let vectors: Vec<PercentVector> =
        serde_json::from_str(madar_dawam::vectors::PERCENT).unwrap();
    assert!(vectors.len() >= 100);
    // An org with no settings row: `dawam_advance_cap` takes 50 %.
    let org = uuid::Uuid::new_v4();
    let mut capped = 0;
    for v in &vectors {
        let percent: Decimal = v.percent.parse().unwrap();
        // An adjustment's value_piastres (payroll) and the advance cap's body.
        let sql: i64 = sqlx::query_scalar(
            "SELECT GREATEST(round(GREATEST($1::bigint, 0)::numeric * GREATEST($2::numeric, 0) / 100), 0)::bigint",
        )
        .bind(v.salary)
        .bind(percent)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(sql, v.value, "SQL {v:?}");
        assert_eq!(
            madar_rust::staff::pricing::percent_of_salary(v.salary, percent),
            v.value,
            "pricing {v:?}"
        );
        if v.percent == "50" {
            let cap: i64 = sqlx::query_scalar("SELECT dawam_advance_cap($1, $2)")
                .bind(org)
                .bind(v.salary)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(cap, v.value, "dawam_advance_cap {v:?}");
            capped += 1;
        }
    }
    assert!(capped >= 5);
    // DW3 as the discovery found it: 33.3 % of 1500 is 500 (the app said 499).
    assert!(
        vectors
            .iter()
            .any(|v| v.salary == 1_500 && v.percent == "33.3" && v.value == 500)
    );
}
