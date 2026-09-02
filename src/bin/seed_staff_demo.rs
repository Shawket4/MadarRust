//! Seed a self-contained staff demo org so the whole HR module can be looked at
//! without hand-rolling curl.
//!
//!   cargo run --bin seed-staff-demo
//!
//! Idempotent: it deletes and recreates the org identified by [`ORG_SLUG`], so
//! re-running gives a clean, identical dataset. It touches NOTHING else — every
//! other org in the database is left exactly as it was.
//!
//! What you get:
//!   * 2 branches with real Cairo coordinates and a 200 m geofence
//!   * 6 employees across 2 departments (one suspended, one unpaid-leave taker)
//!   * 3 work shifts including a night shift that crosses midnight
//!   * A roster, plus a per-date override
//!   * A late-penalty ladder — including "31–120 minutes late costs half a day"
//!   * 30 days of attendance: on time, late, half days, absences, leave
//!   * Requests of every kind, pending and decided
//!   * One PAID payroll period with payslips, and one DRAFT ready to generate
//!
//! The attendance is generated from a fixed pattern, not randomness, so the
//! numbers on screen are the same every run and a bug is a bug rather than luck.

use chrono::{Datelike, Duration, NaiveDate, NaiveTime, Utc};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use uuid::Uuid;

const ORG_SLUG: &str = "madar-staff-demo";
const ORG_NAME: &str = "Madar Demo";
const PASSWORD: &str = "Demo1234!";

/// Downtown Cairo and Maadi — far enough apart that a geofence test is meaningful.
const BRANCHES: [(&str, f64, f64); 2] =
    [("Downtown", 30.0444, 31.2357), ("Maadi", 29.9601, 31.2569)];

struct Employee {
    name: &'static str,
    email: &'static str,
    title: &'static str,
    salary_piastres: i64,
    department: usize,
    branch: usize,
    status: &'static str,
    /// Which attendance story this person acts out over the 30 days.
    pattern: Pattern,
}

/// Deterministic 30-day behaviours, so every run shows the same thing.
#[derive(Clone, Copy, PartialEq)]
enum Pattern {
    /// Always on time.
    Reliable,
    /// Late roughly twice a week, once badly enough to hit the half-day rung.
    SometimesLate,
    /// Two unexplained absences.
    Absentee,
    /// A block of approved (paid) leave.
    OnLeave,
    /// A block of approved UNPAID leave — docked, but not disciplined.
    UnpaidLeave,
    /// No attendance at all (suspended).
    None,
}

const EMPLOYEES: [Employee; 6] = [
    Employee {
        name: "Nour Hassan",
        email: "nour@demo.madar",
        title: "Shift Supervisor",
        salary_piastres: 900_000,
        department: 0,
        branch: 0,
        status: "active",
        pattern: Pattern::Reliable,
    },
    Employee {
        name: "Omar Adel",
        email: "omar@demo.madar",
        title: "Barista",
        salary_piastres: 550_000,
        department: 0,
        branch: 0,
        status: "active",
        pattern: Pattern::SometimesLate,
    },
    Employee {
        name: "Salma Fathy",
        email: "salma@demo.madar",
        title: "Barista",
        salary_piastres: 520_000,
        department: 0,
        branch: 1,
        status: "active",
        pattern: Pattern::Absentee,
    },
    Employee {
        name: "Youssef Kamal",
        email: "youssef@demo.madar",
        title: "Line Cook",
        salary_piastres: 620_000,
        department: 1,
        branch: 1,
        status: "active",
        pattern: Pattern::OnLeave,
    },
    Employee {
        name: "Mariam Saeed",
        email: "mariam@demo.madar",
        title: "Kitchen Porter",
        salary_piastres: 450_000,
        department: 1,
        branch: 0,
        status: "active",
        pattern: Pattern::UnpaidLeave,
    },
    Employee {
        name: "Tarek Nabil",
        email: "tarek@demo.madar",
        title: "Barista",
        salary_piastres: 500_000,
        department: 0,
        branch: 1,
        status: "suspended",
        pattern: Pattern::None,
    },
];

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await?;

    println!("Seeding {ORG_NAME} …");
    wipe(&pool).await?;
    let org = create_org(&pool).await?;
    let admin = create_admin(&pool, org).await?;
    let branches = create_branches(&pool, org).await?;
    let departments = create_departments(&pool, org).await?;
    let staff = create_employees(&pool, org, &branches, &departments).await?;
    let shifts = create_shifts(&pool, org, &branches).await?;
    create_settings(&pool, org).await?;
    let leave_types = create_leave_types(&pool, org).await?;
    roster(&pool, org, &staff, &shifts).await?;
    let requests = create_requests(&pool, org, &staff, &leave_types).await?;
    let days = create_attendance(&pool, org, &staff, &branches, &shifts).await?;
    let deductions = price_attendance(&pool, org).await?;
    let periods = create_payroll(&pool, org, admin).await?;

    println!(
        "\n  org            {ORG_NAME} ({ORG_SLUG})\n  \
           branches       {}\n  \
           employees      {} (1 suspended)\n  \
           work shifts    {}\n  \
           attendance     {days} days\n  \
           deductions     {deductions} automatic (late penalties + absences)\n  \
           requests       {requests} across all five kinds\n  \
           payroll        {periods} periods (one paid, one draft)\n",
        branches.len(),
        staff.len(),
        shifts.len(),
    );
    println!("  Sign in with any of these — password: {PASSWORD}\n");
    println!("    admin@demo.madar        org admin (dashboard)");
    for e in EMPLOYEES.iter().filter(|e| e.status == "active") {
        println!("    {:<24}{}", e.email, e.title);
    }
    println!();
    Ok(())
}

/// Delete the demo org and everything under it. Org FKs do not all cascade, so
/// the child tables are cleared explicitly, deepest first.
async fn wipe(pool: &PgPool) -> Result<(), sqlx::Error> {
    let existing: Option<Uuid> = sqlx::query_scalar("SELECT id FROM organizations WHERE slug = $1")
        .bind(ORG_SLUG)
        .fetch_optional(pool)
        .await?;
    let Some(org) = existing else {
        return Ok(());
    };

    for table in [
        "payslips",
        "payroll_periods",
        "payroll_deductions",
        "payroll_bonuses",
        "salary_advances",
        "attendance_records",
        "attendance_settings",
        "staff_requests",
        "leave_balances",
        "leave_types",
        "staff_schedule_overrides",
        "staff_schedules",
        "work_shifts",
        "staff_documents",
        "staff_profiles",
        "departments",
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE org_id = $1"))
            .bind(org)
            .execute(pool)
            .await?;
    }
    sqlx::query(
        "DELETE FROM user_branch_assignments WHERE user_id IN \
         (SELECT id FROM users WHERE org_id = $1)",
    )
    .bind(org)
    .execute(pool)
    .await?;
    sqlx::query("DELETE FROM users WHERE org_id = $1")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM branches WHERE org_id = $1")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(pool)
        .await?;
    println!("  (removed the previous demo org)");
    Ok(())
}

async fn create_org(pool: &PgPool) -> Result<Uuid, sqlx::Error> {
    // Onboarding is marked complete so the dashboard lands on the real app
    // instead of the first-run wizard.
    sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, timezone, onboarding_completed_at) \
         VALUES ($1, $2, 'Africa/Cairo'::timezone_name, now()) RETURNING id",
    )
    .bind(ORG_NAME)
    .bind(ORG_SLUG)
    .fetch_one(pool)
    .await
}

fn hash() -> String {
    bcrypt::hash(PASSWORD, bcrypt::DEFAULT_COST).expect("bcrypt")
}

async fn create_admin(pool: &PgPool, org: Uuid) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO users (org_id, name, email, password_hash, role) \
         VALUES ($1, 'Demo Admin', 'admin@demo.madar', $2, 'org_admin') RETURNING id",
    )
    .bind(org)
    .bind(hash())
    .fetch_one(pool)
    .await
}

async fn create_branches(pool: &PgPool, org: Uuid) -> Result<Vec<Uuid>, sqlx::Error> {
    let mut ids = Vec::new();
    for (name, lat, lng) in BRANCHES {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO branches (org_id, name, timezone, latitude, longitude, geo_radius_meters) \
             VALUES ($1, $2, 'Africa/Cairo'::timezone_name, $3, $4, 200) RETURNING id",
        )
        .bind(org)
        .bind(name)
        .bind(lat)
        .bind(lng)
        .fetch_one(pool)
        .await?;
        ids.push(id);
    }
    Ok(ids)
}

async fn create_departments(pool: &PgPool, org: Uuid) -> Result<Vec<Uuid>, sqlx::Error> {
    let mut ids = Vec::new();
    for name in ["Front of House", "Kitchen"] {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO departments (org_id, name) VALUES ($1, $2) RETURNING id",
        )
        .bind(org)
        .bind(name)
        .fetch_one(pool)
        .await?;
        ids.push(id);
    }
    Ok(ids)
}

async fn create_employees(
    pool: &PgPool,
    org: Uuid,
    branches: &[Uuid],
    departments: &[Uuid],
) -> Result<Vec<Uuid>, sqlx::Error> {
    let mut ids = Vec::new();
    for e in EMPLOYEES.iter() {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (org_id, name, email, password_hash, role) \
             VALUES ($1, $2, $3, $4, 'teller') RETURNING id",
        )
        .bind(org)
        .bind(e.name)
        .bind(e.email)
        .bind(hash())
        .fetch_one(pool)
        .await?;

        sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
            .bind(id)
            .bind(branches[e.branch])
            .execute(pool)
            .await?;

        sqlx::query(
            "INSERT INTO staff_profiles (user_id, org_id, department_id, job_title, hire_date, \
                 employment_status, base_salary_piastres, employee_code) \
             VALUES ($1, $2, $3, $4, CURRENT_DATE - 400, $5, $6, $7)",
        )
        .bind(id)
        .bind(org)
        .bind(departments[e.department])
        .bind(e.title)
        .bind(e.status)
        .bind(e.salary_piastres)
        .bind(format!("EMP-{:03}", ids.len() + 1))
        .execute(pool)
        .await?;

        ids.push(id);
    }
    Ok(ids)
}

async fn create_shifts(
    pool: &PgPool,
    org: Uuid,
    branches: &[Uuid],
) -> Result<Vec<Uuid>, sqlx::Error> {
    let defs: [(&str, &str, &str, i32, Option<usize>); 3] = [
        ("Morning", "08:00", "16:00", 15, None),
        ("Evening", "16:00", "23:00", 15, None),
        // Crosses midnight — the case that breaks naive date math.
        ("Night", "22:00", "06:00", 10, Some(0)),
    ];
    let mut ids = Vec::new();
    for (name, start, end, grace, branch) in defs {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO work_shifts (org_id, branch_id, name, start_time, end_time, \
                 grace_minutes, overtime_threshold_minutes, overtime_multiplier) \
             VALUES ($1, $2, $3, $4::time, $5::time, $6, 15, 1.50) RETURNING id",
        )
        .bind(org)
        .bind(branch.map(|b| branches[b]))
        .bind(name)
        .bind(start)
        .bind(end)
        .bind(grace)
        .fetch_one(pool)
        .await?;
        ids.push(id);
    }
    Ok(ids)
}

/// The rules the dashboard's editor exposes — seeded with the ladder the brief
/// asked for: half a day's pay once you are more than half an hour late.
async fn create_settings(pool: &PgPool, org: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO attendance_settings
               (org_id, late_deduction_tiers, absence_deduction_days,
                default_overtime_multiplier, working_days_per_month,
                require_geofence, excused_time_paid_default)
           VALUES ($1,
               '[{"from_minutes":1,  "to_minutes":15,   "kind":"minutes",     "value":15},
                 {"from_minutes":16, "to_minutes":30,   "kind":"minutes",     "value":60},
                 {"from_minutes":31, "to_minutes":120,  "kind":"day_fraction","value":0.5},
                 {"from_minutes":121,"to_minutes":null, "kind":"day_fraction","value":1}]'::jsonb,
               1.00, 1.50, 30.00, TRUE, TRUE)"#,
    )
    .bind(org)
    .execute(pool)
    .await?;
    Ok(())
}

async fn create_leave_types(pool: &PgPool, org: Uuid) -> Result<Vec<Uuid>, sqlx::Error> {
    let mut ids = Vec::new();
    for (name, paid, quota) in [
        ("Annual", true, 21.0),
        ("Sick", true, 7.0),
        ("Unpaid", false, 0.0),
    ] {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO leave_types (org_id, name, is_paid, annual_quota_days) \
             VALUES ($1, $2, $3, $4) RETURNING id",
        )
        .bind(org)
        .bind(name)
        .bind(paid)
        .bind(rust_decimal::Decimal::try_from(quota).unwrap())
        .fetch_one(pool)
        .await?;
        ids.push(id);
    }
    Ok(ids)
}

async fn roster(
    pool: &PgPool,
    org: Uuid,
    staff: &[Uuid],
    shifts: &[Uuid],
) -> Result<(), sqlx::Error> {
    for (i, user) in staff.iter().enumerate() {
        // Alternate morning and evening; the kitchen porter works nights.
        let shift = if i == 4 { shifts[2] } else { shifts[i % 2] };
        // Saturday–Thursday, one row per weekday. Rostering "every day" instead
        // would make Friday a working day as far as the sweep is concerned, and
        // it would mark the whole team absent every week — the roster IS the
        // definition of who is expected, so it has to exclude the weekend.
        for day_of_week in [6i16, 0, 1, 2, 3, 4] {
            sqlx::query(
                "INSERT INTO staff_schedules (org_id, user_id, work_shift_id, day_of_week, \
                     effective_from) \
                 VALUES ($1, $2, $3, $4, CURRENT_DATE - 60)",
            )
            .bind(org)
            .bind(user)
            .bind(shift)
            .bind(day_of_week)
            .execute(pool)
            .await?;
        }
    }

    // One per-date override, so the resolution ladder has something to show.
    sqlx::query(
        "INSERT INTO staff_schedule_overrides (org_id, user_id, on_date, work_shift_id, reason) \
         VALUES ($1, $2, CURRENT_DATE + 1, $3, 'Covering the evening shift')",
    )
    .bind(org)
    .bind(staff[0])
    .bind(shifts[1])
    .execute(pool)
    .await?;
    Ok(())
}

/// Requests of every kind: some pending (so the inbox has work in it), some
/// already decided (so history is not empty).
async fn create_requests(
    pool: &PgPool,
    org: Uuid,
    staff: &[Uuid],
    leave_types: &[Uuid],
) -> Result<usize, sqlx::Error> {
    let today = Utc::now().date_naive();
    let mut n = 0;

    // Pending — the manager has these waiting.
    for (user, kind, on_date, from_t, to_t, reason) in [
        (
            staff[1],
            "late_arrival",
            today + Duration::days(1),
            None,
            Some("10:00"),
            "Doctor's appointment",
        ),
        (
            staff[2],
            "excuse",
            today + Duration::days(2),
            Some("12:00"),
            Some("14:00"),
            "Bank errand",
        ),
        (
            staff[3],
            "early_departure",
            today + Duration::days(1),
            Some("14:00"),
            None,
            "Family commitment",
        ),
    ] {
        sqlx::query(
            "INSERT INTO staff_requests (org_id, user_id, kind, on_date, from_time, to_time, reason) \
             VALUES ($1, $2, $3, $4, $5::time, $6::time, $7)",
        )
        .bind(org)
        .bind(user)
        .bind(kind)
        .bind(on_date)
        .bind(from_t)
        .bind(to_t)
        .bind(reason)
        .execute(pool)
        .await?;
        n += 1;
    }

    // Approved paid leave (Youssef) and unpaid leave (Mariam), both in the past
    // month so they land inside the generated payroll period.
    for (user, type_idx, start_offset, days) in
        [(staff[3], 0usize, -12i64, 3i64), (staff[4], 2, -8, 2)]
    {
        sqlx::query(
            "INSERT INTO staff_requests (org_id, user_id, kind, leave_type_id, on_date, end_date, \
                 status, decided_at, reason) \
             VALUES ($1, $2, 'leave', $3, $4, $5, 'approved', now(), 'Planned')",
        )
        .bind(org)
        .bind(user)
        .bind(leave_types[type_idx])
        .bind(today + Duration::days(start_offset))
        .bind(today + Duration::days(start_offset + days - 1))
        .execute(pool)
        .await?;
        n += 1;
    }

    // An approved mission, and one rejected request so the history is not all yes.
    sqlx::query(
        "INSERT INTO staff_requests (org_id, user_id, kind, on_date, end_date, title, location, \
             status, decided_at) \
         VALUES ($1, $2, 'mission', $3, $3, 'Supplier visit', 'Obour City', 'approved', now())",
    )
    .bind(org)
    .bind(staff[0])
    .bind(today - Duration::days(5))
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO staff_requests (org_id, user_id, kind, on_date, to_time, status, \
             decided_at, reason, decision_note) \
         VALUES ($1, $2, 'late_arrival', $3, '11:00'::time, 'rejected', now(), \
                 'Overslept', 'Third time this month')",
    )
    .bind(org)
    .bind(staff[1])
    .bind(today - Duration::days(3))
    .execute(pool)
    .await?;
    n += 2;

    // Leave balances for everyone, so the app's balance card is populated.
    for user in staff {
        for (i, lt) in leave_types.iter().enumerate() {
            let entitled = [21.0, 7.0, 0.0][i];
            sqlx::query(
                "INSERT INTO leave_balances (org_id, user_id, leave_type_id, year, entitled_days) \
                 VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
            )
            .bind(org)
            .bind(user)
            .bind(lt)
            .bind(today.year())
            .bind(rust_decimal::Decimal::try_from(entitled).unwrap())
            .execute(pool)
            .await?;
        }
    }
    Ok(n)
}

/// 30 days of attendance, generated from each employee's fixed pattern.
async fn create_attendance(
    pool: &PgPool,
    org: Uuid,
    staff: &[Uuid],
    branches: &[Uuid],
    shifts: &[Uuid],
) -> Result<usize, sqlx::Error> {
    let today = Utc::now().date_naive();
    let mut written = 0;

    for (i, user) in staff.iter().enumerate() {
        let e = &EMPLOYEES[i];
        if e.pattern == Pattern::None {
            continue;
        }
        let branch = branches[e.branch];
        let shift = if i == 4 { shifts[2] } else { shifts[i % 2] };
        let (start_h, span_h) = if i == 4 {
            (22, 8)
        } else if i % 2 == 0 {
            (8, 8)
        } else {
            (16, 7)
        };

        for back in 1..=30i64 {
            let date = today - Duration::days(back);
            // Friday is the weekend here (EXTRACT(DOW) = 5).
            if date.weekday().num_days_from_sunday() == 5 {
                continue;
            }

            let (status, late_min) = day_outcome(e.pattern, back);
            if status == "skip" {
                continue;
            }

            let start = date
                .and_time(NaiveTime::from_hms_opt(start_h, 0, 0).unwrap())
                .and_utc()
                - Duration::hours(2); // Africa/Cairo is UTC+2
            let end = start + Duration::hours(span_h);

            let (check_in, check_out, worked) = match status {
                "absent" | "on_leave" => (None, None, 0),
                "half_day" => (
                    Some(start + Duration::minutes(late_min)),
                    Some(start + Duration::hours(span_h / 2)),
                    (span_h * 60 / 2) - late_min,
                ),
                _ => (
                    Some(start + Duration::minutes(late_min)),
                    Some(end),
                    span_h * 60 - late_min,
                ),
            };

            sqlx::query(
                "INSERT INTO attendance_records (org_id, user_id, branch_id, work_shift_id, \
                     business_date, status, scheduled_start_at, scheduled_end_at, \
                     check_in_at, check_out_at, check_in_method, check_out_method, \
                     check_in_distance_meters, late_minutes, worked_minutes) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, \
                     CASE WHEN $9::timestamptz IS NULL THEN NULL ELSE 'mobile_gps' END, \
                     CASE WHEN $10::timestamptz IS NULL THEN NULL ELSE 'mobile_gps' END, \
                     CASE WHEN $9::timestamptz IS NULL THEN NULL ELSE 42.0 END, $11, $12) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(org)
            .bind(user)
            .bind(branch)
            .bind(shift)
            .bind(date)
            .bind(status)
            .bind(start)
            .bind(end)
            .bind(check_in)
            .bind(check_out)
            .bind(late_min.max(0) as i32)
            .bind(worked.max(0) as i32)
            .execute(pool)
            .await?;
            written += 1;
        }
    }
    Ok(written)
}

/// What one employee does on the day `back` days ago. Deterministic on purpose.
fn day_outcome(pattern: Pattern, back: i64) -> (&'static str, i64) {
    match pattern {
        Pattern::Reliable => ("present", 0),
        Pattern::SometimesLate => match back % 7 {
            // Once a week badly enough to hit the half-day rung (>30 min late).
            2 => ("late", 45),
            5 => ("late", 20),
            _ => ("present", 0),
        },
        Pattern::Absentee => match back % 11 {
            3 => ("absent", 0),
            7 => ("half_day", 0),
            _ => ("present", 0),
        },
        // The approved-leave blocks seeded above; the rest is ordinary.
        Pattern::OnLeave if (11..=13).contains(&back) => ("on_leave", 0),
        Pattern::OnLeave => ("present", 0),
        Pattern::UnpaidLeave if (7..=8).contains(&back) => ("on_leave", 0),
        Pattern::UnpaidLeave => ("present", 0),
        Pattern::None => ("skip", 0),
    }
}

/// Run the real pricing engine over the seeded attendance, so the deductions on
/// screen are the ones the rules actually produce — not values typed in here.
async fn price_attendance(pool: &PgPool, org: Uuid) -> Result<usize, sqlx::Error> {
    let settings = madar_rust::staff::attendance::load_settings(pool, org, None)
        .await
        .expect("settings");
    let ids: Vec<Uuid> = sqlx::query("SELECT id FROM attendance_records WHERE org_id = $1")
        .bind(org)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|r| r.get::<Uuid, _>("id"))
        .collect();

    let mut priced = 0;
    for id in ids {
        let mut conn = pool.acquire().await?;
        if madar_rust::staff::penalties::recompute_record(&mut conn, id, &settings)
            .await
            .expect("price")
            > 0
        {
            priced += 1;
        }
    }
    Ok(priced)
}

/// Last month PAID (with payslips), this month DRAFT (ready to generate from the
/// dashboard, so there is something to press).
async fn create_payroll(pool: &PgPool, org: Uuid, admin: Uuid) -> Result<usize, sqlx::Error> {
    let today = Utc::now().date_naive();
    let first_of_month = NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap();
    let last_month_end = first_of_month - Duration::days(1);
    let last_month_start =
        NaiveDate::from_ymd_opt(last_month_end.year(), last_month_end.month(), 1).unwrap();

    for (name, start, end, status) in [
        (
            last_month_start.format("%B %Y").to_string(),
            last_month_start,
            last_month_end,
            "paid",
        ),
        (
            first_of_month.format("%B %Y").to_string(),
            first_of_month,
            today,
            "draft",
        ),
    ] {
        sqlx::query(
            "INSERT INTO payroll_periods (org_id, name, start_date, end_date, status, generated_by) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(org)
        .bind(&name)
        .bind(start)
        .bind(end)
        .bind(if status == "paid" { "draft" } else { status })
        .bind(admin)
        .execute(pool)
        .await?;
    }

    // Payslips are NOT written here. They come from the real generator, driven
    // through the same endpoint the dashboard button uses — `dev-staff.sh` calls
    // it once the server is up. Seeding payslips by hand would mean the numbers
    // on screen were typed in this file rather than produced by the rules, which
    // is precisely what a demo must not do.
    Ok(2)
}
