//! The drawer carryover, this server's SQL against madar-shared's picker.
//!
//! `last_close_declared` picks the carryover in SQL; the till picks it from
//! its rows with `madar_till::carryover::last_close_declared`. Every case in
//! `madar_till::vectors::CARRYOVER` is seeded here as real tills and asked of
//! the SQL, which must name the same close (fix T2 included: a device-less
//! close never beats this device's own).
use madar_till::carryover::vectors::CarryoverVector;
use sqlx::PgPool;
use uuid::Uuid;

#[sqlx::test]
async fn the_sql_carryover_is_the_shared_picker(pool: PgPool) {
    let vectors: Vec<CarryoverVector> =
        serde_json::from_str(madar_till::vectors::CARRYOVER).unwrap();
    assert!(vectors.len() >= 8);
    for (n, v) in vectors.iter().enumerate() {
        let org = Uuid::new_v4();
        let branch = Uuid::new_v4();
        let user = Uuid::new_v4();
        sqlx::raw_sql(&format!(
            "INSERT INTO organizations (id, name, slug) VALUES ('{org}', 'Carry {n}', 'carry-{org}');
             INSERT INTO branches (id, org_id, name) VALUES ('{branch}', '{org}', 'Carry');
             INSERT INTO users (id, org_id, name, email, password_hash, role)
                  VALUES ('{user}', '{org}', 'Sara', 'sara-{org}@carry.test', 'x', 'teller');"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let mut devices: Vec<String> = Vec::new();
        for t in &v.tills {
            if let Some(d) = &t.device_id
                && !devices.contains(d)
            {
                sqlx::query("INSERT INTO devices (id, org_id, branch_id, code) VALUES ($1::uuid, $2, $3, $4)")
                    .bind(d)
                    .bind(org)
                    .bind(branch)
                    .bind(format!("D{}", devices.len()))
                    .execute(&pool)
                    .await
                    .unwrap();
                devices.push(d.clone());
            }
            sqlx::query(
                "INSERT INTO tills (id, branch_id, teller_id, status, opening_cash, opened_at, closed_at, \
                                    closing_cash_declared, device_id) \
                 VALUES ($1, $2, $3, $4::till_status, 0, $5::timestamptz, \
                         CASE WHEN $4 = 'open' THEN NULL ELSE $5::timestamptz + interval '8 hours' END, \
                         $6, $7::uuid)",
            )
            .bind(Uuid::new_v4())
            .bind(branch)
            .bind(user)
            .bind(&t.status)
            .bind(&t.opened_at)
            .bind(t.closing_cash_declared.map(|c| c as i32))
            .bind(t.device_id.as_deref())
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("{}: {e}", v.name));
        }
        let device = v.device_id.as_deref().map(|d| Uuid::parse_str(d).unwrap());
        let got = madar_rust::tills::handlers::last_close_declared(&pool, branch, device)
            .await
            .unwrap()
            .map(i64::from);
        assert_eq!(got, v.expected, "{}", v.name);
        // Old devices' device rows must not leak into the next case.
        sqlx::query("DELETE FROM tills WHERE branch_id = $1")
            .bind(branch)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM devices WHERE org_id = $1")
            .bind(org)
            .execute(&pool)
            .await
            .unwrap();
    }
}
