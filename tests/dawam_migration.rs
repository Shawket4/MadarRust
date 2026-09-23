//! Dawam Phase A's migrations: the data move from users + staff_profiles to
//! employees keeps every record, and the tenant role can reach every table it
//! needs — granted explicitly, not by a database's default privileges.

use std::borrow::Cow;

use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor, PgPool};
use uuid::Uuid;

const PHASE_A: i64 = 20260927000000;

/// A fresh, empty database on the same cluster (the per-test one is born
/// fully migrated from the template), migrated up to — not including — Phase A.
async fn before_phase_a(pool: &PgPool) -> (PgPool, String) {
    let name = format!("dawam_mig_{}", Uuid::new_v4().simple());
    pool.execute(format!("CREATE DATABASE {name} TEMPLATE template0").as_str())
        .await
        .unwrap();
    let opts = pool.connect_options().as_ref().clone().database(&name);
    let db = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(opts)
        .await
        .unwrap();
    let all = sqlx::migrate!("./migrations");
    let mut older = sqlx::migrate!("./migrations");
    older.migrations = Cow::Owned(
        all.migrations
            .iter()
            .filter(|m| m.version < PHASE_A)
            .cloned()
            .collect(),
    );
    older.run(&db).await.unwrap();
    (db, name)
}

async fn drop_db(pool: &PgPool, db: PgPool, name: &str) {
    db.close().await;
    pool.execute(format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)").as_str())
        .await
        .unwrap();
}

async fn scalar<T>(db: &PgPool, sql: &str) -> T
where
    T: Send + Unpin + for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    sqlx::query_scalar::<_, T>(sql).fetch_one(db).await.unwrap()
}

/// The owner's prod data, as the August HR module and the Dawam branch left
/// it, moved to employees without losing a row.
#[sqlx::test]
async fn the_data_migration_keeps_every_staff_record(pool: PgPool) {
    let (db, name) = before_phase_a(&pool).await;

    // Two businesses: a restaurant still on the old default (both modules),
    // and a Dawam-only one set by hand.
    let (org, dawam_only) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations (id, name, slug, modules) VALUES
            ('{org}', 'Rest', 'rest-{org}', '{{pos,dawam}}'),
            ('{dawam_only}', 'Clinic', 'clinic-{dawam_only}', '{{dawam}}');"
    ))
    .execute(&db)
    .await
    .unwrap();
    let (a, b, solo) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    sqlx::raw_sql(&format!(
        "INSERT INTO branches (id, org_id, name) VALUES
            ('{a}', '{org}', 'A'), ('{b}', '{org}', 'B'), ('{solo}', '{dawam_only}', 'Only');"
    ))
    .execute(&db)
    .await
    .unwrap();
    // People: a cashier on payroll (branch A), the owner on payroll (no
    // branch), a deactivated cashier on payroll sharing the cashier's number,
    // a deleted one, a manager whose only history is attendance at B (no
    // profile), and the clinic's nurse (no branch, one-branch business).
    let (cashier, owner, idle, gone, old, nurse) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let users = [
        (
            cashier,
            org,
            "Cash",
            "teller",
            Some("01012345678"),
            true,
            false,
        ),
        (
            owner,
            org,
            "Boss",
            "org_admin",
            Some("+20 100 000 0001"),
            true,
            false,
        ),
        (
            idle,
            org,
            "Idle",
            "teller",
            Some("01012345678"),
            false,
            false,
        ),
        (gone, org, "Gone", "teller", Some("01099999999"), true, true),
        (old, org, "Old", "branch_manager", None, true, false),
        (
            nurse,
            dawam_only,
            "Nurse",
            "teller",
            Some("01055555555"),
            true,
            false,
        ),
    ];
    for (id, o, n, role, phone, active, deleted) in users {
        sqlx::query(
            "INSERT INTO users (id, org_id, name, email, phone, password_hash, role, is_active, deleted_at) \
             VALUES ($1, $2, $3, $4, $5, 'h', $6::user_role, $7, CASE WHEN $8 THEN now() END)",
        )
        .bind(id)
        .bind(o)
        .bind(n)
        .bind(format!("{id}@m.test"))
        .bind(phone)
        .bind(role)
        .bind(active)
        .bind(deleted)
        .execute(&db)
        .await
        .unwrap();
    }
    sqlx::raw_sql(&format!(
        "INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ('{cashier}', '{a}'), ('{idle}', '{a}');
         INSERT INTO staff_profiles (user_id, org_id, job_title, hire_date, base_salary_piastres,
                                     national_id, gender, pay_method, pay_account, pref_time,
                                     cant_work_days, employee_code, notes)
         VALUES ('{cashier}', '{org}', 'Barista', '2025-01-01', 600000, '2990', 'f', 'wallet',
                 '0100', 'evening', '{{5}}', 'E-1', 'keys'),
                ('{owner}', '{org}', 'Owner', '2024-01-01', 0, NULL, 'm', 'cash', NULL, NULL, '{{}}', NULL, NULL),
                ('{idle}', '{org}', NULL, '2025-02-01', 500000, NULL, NULL, 'cash', NULL, NULL, '{{}}', NULL, NULL),
                ('{gone}', '{org}', NULL, '2025-03-01', 400000, NULL, NULL, 'cash', NULL, NULL, '{{}}', NULL, NULL),
                ('{nurse}', '{dawam_only}', 'Nurse', '2025-04-01', 700000, NULL, 'f', 'bank', NULL, NULL, '{{}}', NULL, NULL);"
    ))
    .execute(&db)
    .await
    .unwrap();
    let shift: Uuid = scalar(
        &db,
        &format!(
            "INSERT INTO work_shifts (org_id, branch_id, name, start_time, end_time) \
             VALUES ('{org}', '{a}', 'Day', '09:00', '17:00') RETURNING id"
        ),
    )
    .await;
    let period: Uuid = scalar(
        &db,
        &format!(
            "INSERT INTO payroll_periods (org_id, name, start_date, end_date, status) \
             VALUES ('{org}', 'Aug', '2026-08-01', '2026-08-31', 'paid') RETURNING id"
        ),
    )
    .await;
    let leave: Uuid = scalar(
        &db,
        &format!("INSERT INTO leave_types (org_id, name) VALUES ('{org}', 'Annual') RETURNING id"),
    )
    .await;
    // History of every kind. `old` has attendance at B and a payslip, and
    // never had a profile.
    let record: Uuid = scalar(
        &db,
        &format!(
            "INSERT INTO attendance_records (org_id, user_id, branch_id, business_date, status, check_in_at)
             VALUES ('{org}', '{cashier}', '{a}', '2026-08-10', 'present', '2026-08-10T09:00:00Z') RETURNING id"
        ),
    )
    .await;
    sqlx::raw_sql(&format!(
        "INSERT INTO attendance_records (org_id, user_id, branch_id, business_date, status, check_in_at,
                                         covered_user_id, cover_status, work_shift_id)
         VALUES ('{org}', '{old}', '{b}', '2026-08-11', 'present', '2026-08-11T09:00:00Z',
                 '{cashier}', 'confirmed', '{shift}');
         INSERT INTO attendance_records (org_id, user_id, branch_id, business_date, status)
         VALUES ('{org}', '{old}', '{b}', '2026-08-12', 'absent');
         INSERT INTO attendance_pings (org_id, user_id, attendance_record_id, inside)
         VALUES ('{org}', '{cashier}', '{record}', true);
         INSERT INTO attendance_flags (org_id, user_id, branch_id, attendance_record_id, kind)
         VALUES ('{org}', '{cashier}', '{a}', '{record}', 'suspicious');
         INSERT INTO staff_requests (org_id, user_id, kind, on_date, end_date, status, decided_at)
         VALUES ('{org}', '{cashier}', 'leave', '2026-08-20', '2026-08-21', 'approved', now());
         INSERT INTO leave_balances (org_id, user_id, leave_type_id, year, entitled_days, used_days)
         VALUES ('{org}', '{cashier}', '{leave}', 2026, 21, 2);
         INSERT INTO payroll_deductions (org_id, user_id, amount_piastres, reason, effective_date, source)
         VALUES ('{org}', '{cashier}', 5000, 'late', '2026-08-10', 'late_penalty');
         INSERT INTO payroll_bonuses (org_id, user_id, amount_piastres, reason, effective_date)
         VALUES ('{org}', '{cashier}', 7000, 'week', '2026-08-15');
         INSERT INTO salary_advances (org_id, user_id, amount_piastres, installments,
                                      monthly_installment_piastres, remaining_piastres)
         VALUES ('{org}', '{cashier}', 20000, 2, 10000, 10000);
         INSERT INTO payslips (org_id, payroll_period_id, user_id, base_salary_piastres, worked_days,
             absent_days, leave_days, late_minutes, overtime_minutes, overtime_piastres,
             bonuses_piastres, deductions_piastres, advance_installment_piastres, net_piastres, breakdown)
         VALUES ('{org}', '{period}', '{cashier}', 600000, 20, 0, 2, 5, 0, 0, 7000, 5000, 10000, 592000, '{{}}'),
                ('{org}', '{period}', '{old}', 300000, 1, 1, 0, 0, 0, 0, 0, 0, 0, 290000, '{{}}');
         INSERT INTO staff_schedules (org_id, user_id, work_shift_id, effective_from)
         VALUES ('{org}', '{cashier}', '{shift}', '2026-01-01');
         INSERT INTO staff_schedule_overrides (org_id, user_id, on_date, reason)
         VALUES ('{org}', '{cashier}', '2026-08-25', 'off');
         INSERT INTO staff_documents (org_id, user_id, title, file_url)
         VALUES ('{org}', '{cashier}', 'ID', '/uploads/id');
         INSERT INTO expense_advances (org_id, user_id, branch_id, amount_piastres, purpose)
         VALUES ('{org}', '{cashier}', '{a}', 3000, 'Milk');
         INSERT INTO staff_open_shifts (org_id, branch_id, work_shift_id, on_date, status, claimed_by)
         VALUES ('{org}', '{a}', '{shift}', '2026-08-30', 'claimed', '{cashier}');
         INSERT INTO staff_swaps (org_id, requester_id, requester_date, requester_shift_id,
                                  peer_id, peer_date, peer_shift_id)
         VALUES ('{org}', '{cashier}', '2026-08-28', '{shift}', '{old}', '2026-08-29', '{shift}');
         INSERT INTO staff_devices (org_id, user_id, token_hash) VALUES ('{org}', '{cashier}', 'h1');
         INSERT INTO staff_devices (org_id, user_id, token_hash, revoked_at)
         VALUES ('{org}', '{old}', 'h2', now());
         INSERT INTO staff_notifications (org_id, user_id, key) VALUES ('{org}', '{cashier}', 'staff.n_paid');
         INSERT INTO push_devices (org_id, user_id, app, token) VALUES ('{org}', '{cashier}', 'dawam', 't-dawam');
         INSERT INTO push_devices (org_id, user_id, app, token) VALUES ('{org}', '{owner}', 'manager', 't-mgr');
         INSERT INTO staff_suggestion_events (org_id, branch_id, suggestion, user_id, on_date, accepted)
         VALUES ('{org}', '{a}', 'add|x', '{cashier}', '2026-08-30', true);"
    ))
    .execute(&db)
    .await
    .unwrap();

    // The migration itself.
    sqlx::migrate!("./migrations").run(&db).await.unwrap();

    // One employee per profile, plus one for the history without a profile —
    // each keeping its user's id and linked to that user.
    let people: Vec<(Uuid, Option<Uuid>, String, Option<String>, bool, String)> = sqlx::query_as(
        "SELECT id, user_id, name, phone, app_access, employment_status FROM employees ORDER BY name",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    let find = |id: Uuid| people.iter().find(|p| p.0 == id).cloned().unwrap();
    assert_eq!(people.len(), 6, "{people:?}");
    for id in [cashier, owner, idle, gone, old, nurse] {
        assert_eq!(find(id).1, Some(id), "linked to its own user");
    }
    let c = find(cashier);
    assert_eq!(
        (c.2.as_str(), c.3.as_deref(), c.4),
        ("Cash", Some("+201012345678"), true)
    );
    assert_eq!(
        find(owner).3.as_deref(),
        Some("+201000000001"),
        "canonical number"
    );
    assert!(
        !find(idle).4,
        "an inactive account had no app, and its number is the cashier's"
    );
    assert_eq!(
        find(gone).5,
        "terminated",
        "a deleted account's employment ended"
    );
    assert!(!find(gone).4);
    assert_eq!(
        find(old).5,
        "terminated",
        "history without a profile: kept, not paid"
    );
    assert!(find(nurse).4);
    // Every profile field came across.
    let (title, salary, nid, gender, method, account, pref, cant, code, notes): (
        String,
        i64,
        String,
        String,
        String,
        String,
        String,
        Vec<i16>,
        String,
        String,
    ) = sqlx::query_as(
        "SELECT job_title, base_salary_piastres, national_id, gender, pay_method, pay_account, \
                pref_time, cant_work_days, employee_code, notes FROM employees WHERE id = $1",
    )
    .bind(cashier)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(
        (
            title.as_str(),
            salary,
            nid.as_str(),
            gender.as_str(),
            method.as_str(),
            account.as_str(),
            pref.as_str(),
            cant,
            code.as_str(),
            notes.as_str()
        ),
        (
            "Barista",
            600000,
            "2990",
            "f",
            "wallet",
            "0100",
            "evening",
            vec![5i16],
            "E-1",
            "keys"
        )
    );

    // Branches: assignments, where they punched, and a one-branch business.
    let branches = |e: Uuid| {
        let db = db.clone();
        async move {
            sqlx::query_scalar::<_, Uuid>(
                "SELECT branch_id FROM employee_branches WHERE employee_id = $1 ORDER BY branch_id",
            )
            .bind(e)
            .fetch_all(&db)
            .await
            .unwrap()
        }
    };
    assert_eq!(branches(cashier).await, vec![a]);
    assert_eq!(branches(old).await, vec![b], "where they punched");
    assert_eq!(
        branches(nurse).await,
        vec![solo],
        "the business's only branch"
    );
    assert!(
        branches(owner).await.is_empty(),
        "an owner with no branch stays so"
    );

    // Every record is still there, now the employee's.
    for (table, col, want) in [
        ("attendance_records", "employee_id", 3i64),
        ("attendance_pings", "employee_id", 1),
        ("attendance_flags", "employee_id", 1),
        ("staff_requests", "employee_id", 1),
        ("leave_balances", "employee_id", 1),
        ("payroll_deductions", "employee_id", 1),
        ("payroll_bonuses", "employee_id", 1),
        ("salary_advances", "employee_id", 1),
        ("payslips", "employee_id", 2),
        ("staff_schedules", "employee_id", 1),
        ("staff_schedule_overrides", "employee_id", 1),
        ("staff_documents", "employee_id", 1),
        ("expense_advances", "employee_id", 1),
        ("staff_devices", "employee_id", 2),
        ("staff_notifications", "employee_id", 1),
        ("staff_suggestion_events", "employee_id", 1),
    ] {
        let n: i64 = scalar(
            &db,
            &format!("SELECT COUNT(*) FROM {table} t JOIN employees e ON e.id = t.{col}"),
        )
        .await;
        assert_eq!(n, want, "{table}");
    }
    let covered: Option<Uuid> = scalar(
        &db,
        "SELECT covered_employee_id FROM attendance_records WHERE cover_status = 'confirmed'",
    )
    .await;
    assert_eq!(covered, Some(cashier));
    let (claimer, requester, peer): (Option<Uuid>, Uuid, Uuid) = sqlx::query_as(
        "SELECT (SELECT claimed_by FROM staff_open_shifts), requester_id, peer_id FROM staff_swaps",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!((claimer, requester, peer), (Some(cashier), cashier, old));
    // The staff app's pushes are the employee's; another app's stay the user's.
    let dawam: (Option<Uuid>, Option<Uuid>) =
        sqlx::query_as("SELECT user_id, employee_id FROM push_devices WHERE token = 't-dawam'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(dawam, (None, Some(cashier)));
    let other: (Option<Uuid>, Option<Uuid>) =
        sqlx::query_as("SELECT user_id, employee_id FROM push_devices WHERE token = 't-mgr'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(other, (Some(owner), None));

    // staff_profiles is folded in and gone.
    let gone_table: Option<String> =
        scalar(&db, "SELECT to_regclass('staff_profiles')::text").await;
    assert_eq!(gone_table, None);

    // Modules: the old default goes back to POS; a deliberate Dawam-only stays.
    let modules: Vec<(Uuid, Vec<String>)> =
        sqlx::query_as("SELECT id, modules FROM organizations ORDER BY name")
            .fetch_all(&db)
            .await
            .unwrap();
    assert!(
        modules.contains(&(org, vec!["pos".to_string()])),
        "{modules:?}"
    );
    assert!(modules.contains(&(dawam_only, vec!["dawam".to_string()])));

    // The rules capability and its preset grant.
    let rules: i64 = scalar(
        &db,
        "SELECT COUNT(*) FROM org_role_grants g JOIN org_roles r ON r.id = g.org_role_id \
          WHERE g.capability_id = 236 AND r.kind::text = 'org_admin'",
    )
    .await;
    assert!(rules >= 1);
    drop_db(&pool, db, &name).await;
}

/// Tables the tenant role must NOT reach, each with why.
const TENANT_DENIED: &[&str] = &[
    // Sign-in codes are read before the org is known, by the owner pool only.
    "staff_otp",
];

/// Every table in `public` is reachable by `madar_app` (the RLS role every
/// tenant request runs as), except the ones above.
#[sqlx::test]
async fn every_table_is_reachable_by_the_tenant_role(pool: PgPool) {
    let missing: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
          WHERE n.nspname = 'public' AND c.relkind IN ('r', 'p') AND NOT c.relispartition \
            AND c.relname <> '_sqlx_migrations' \
            AND NOT (has_table_privilege('madar_app', c.oid, 'SELECT') \
                 AND has_table_privilege('madar_app', c.oid, 'INSERT') \
                 AND has_table_privilege('madar_app', c.oid, 'UPDATE') \
                 AND has_table_privilege('madar_app', c.oid, 'DELETE')) \
          ORDER BY 1",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(missing, TENANT_DENIED, "tables madar_app cannot reach");
}

/// The Dawam grants are EXPLICIT. A database restored without default
/// privileges (a prod copy) gives the tenant role nothing on a new table;
/// Phase A's grants migration must restore every Dawam table by itself.
#[sqlx::test]
async fn the_dawam_grants_do_not_lean_on_default_privileges(pool: PgPool) {
    sqlx::raw_sql("REVOKE ALL ON ALL TABLES IN SCHEMA public FROM madar_app")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!(
        "../migrations/20260927000100_dawam_grants_modules.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let dawam = [
        "staff_devices",
        "attendance_pings",
        "attendance_flags",
        "staff_week_publications",
        "staff_open_shifts",
        "staff_swaps",
        "staff_holidays",
        "staff_suggestion_events",
        "expense_advances",
        "staff_notifications",
        "push_devices",
        "staff_coverage_needs",
        "staff_suggestion_cache",
        "employees",
        "employee_branches",
        "attendance_records",
        "attendance_settings",
        "staff_requests",
        "leave_types",
        "leave_balances",
        "payroll_deductions",
        "payroll_bonuses",
        "payroll_periods",
        "payslips",
        "salary_advances",
        "staff_schedules",
        "staff_schedule_overrides",
        "staff_documents",
        "work_shifts",
        "departments",
    ];
    for t in dawam {
        let ok: bool = sqlx::query_scalar(
            "SELECT has_table_privilege('madar_app', $1, 'SELECT') \
                AND has_table_privilege('madar_app', $1, 'INSERT') \
                AND has_table_privilege('madar_app', $1, 'UPDATE') \
                AND has_table_privilege('madar_app', $1, 'DELETE')",
        )
        .bind(t)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(ok, "{t} is granted explicitly");
    }
    let otp: bool =
        sqlx::query_scalar("SELECT has_table_privilege('madar_app', 'staff_otp', 'SELECT')")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!otp, "the tenant role never reads sign-in codes");
    // And every Dawam table the tenant reaches is tenant-isolated.
    let open: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
          WHERE n.nspname = 'public' AND c.relname = ANY($1) AND NOT c.relrowsecurity ORDER BY 1",
    )
    .bind(dawam.to_vec())
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(open.is_empty(), "without RLS: {open:?}");
}
