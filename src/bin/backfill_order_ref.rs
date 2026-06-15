//! Operator CLI: backfill the human-readable `orders.order_ref` for historical
//! orders, and seed `order_ref_counters` so live numbering continues from the
//! right high-water mark. Deliberately not exposed over HTTP — run it on the VPS
//! next to the server (reads DATABASE_URL from the environment / .env).
//!
//! Run this AFTER migration `20260614020000_order_ref.sql` (which adds the
//! nullable column + counter table) and BEFORE `20260614030000_order_ref_finalize.sql`
//! (which adds UNIQUE + NOT NULL). Always `--dry-run` first.
//!
//! IMPORTANT — gate order creation (maintenance mode) while this runs. New orders
//! mint a ref from `order_ref_counters` the moment migration A is live; if live
//! orders interleave with un-backfilled historical orders in the same
//! (branch, business-day), their sequence numbers would collide. This tool
//! ABORTS up front if it detects such a mixed group.
//!
//! Usage:
//!   backfill-order-ref --all            [--dry-run]
//!   backfill-order-ref --org <uuid>     [--dry-run]
//!   backfill-order-ref --branch <uuid>  [--dry-run]
//!
//! Numbering matches true ring-up order: per (branch, local business date),
//! ORDER BY shifts.opened_at, orders.order_number, orders.id.

use std::env;
use std::process::ExitCode;

use sqlx::postgres::PgPoolOptions;
use sqlx::Row;
use uuid::Uuid;

const USAGE: &str = "\
Backfills orders.order_ref for historical orders and seeds order_ref_counters.

USAGE:
    backfill-order-ref (--all | --org <uuid> | --branch <uuid>) [--dry-run]

OPTIONS:
    --all             Every order in every organization
    --org <uuid>      Every order in this organization
    --branch <uuid>   Every order in this branch only
    --dry-run         Compute and print the summary, then roll back

Gate order creation while this runs (see file header). Always --dry-run first.";

/// (org filter, branch filter) — at most one is Some; --all leaves both None.
fn parse_args() -> Result<(Option<Uuid>, Option<Uuid>, bool), String> {
    let mut org: Option<Uuid> = None;
    let mut branch: Option<Uuid> = None;
    let mut all = false;
    let mut dry_run = false;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--all" => all = true,
            "--org" => {
                let v = args.next().ok_or("--org requires a uuid")?;
                org = Some(Uuid::parse_str(&v).map_err(|_| format!("invalid org uuid: {v}"))?);
            }
            "--branch" => {
                let v = args.next().ok_or("--branch requires a uuid")?;
                branch =
                    Some(Uuid::parse_str(&v).map_err(|_| format!("invalid branch uuid: {v}"))?);
            }
            "--dry-run" => dry_run = true,
            "--help" | "-h" => return Err(String::new()),
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    match (all, org, branch) {
        (true, None, None) => Ok((None, None, dry_run)),
        (false, Some(o), None) => Ok((Some(o), None, dry_run)),
        (false, None, Some(b)) => Ok((None, Some(b), dry_run)),
        _ => Err("pass exactly one of --all, --org <uuid>, or --branch <uuid>".into()),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    dotenvy::dotenv().ok();

    let (org, branch, dry_run) = match parse_args() {
        Ok(parsed) => parsed,
        Err(msg) => {
            if !msg.is_empty() {
                eprintln!("error: {msg}\n");
            }
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    let db_url = match env::var("DATABASE_URL") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("error: DATABASE_URL must be set (env or .env)");
            return ExitCode::from(2);
        }
    };
    let pool = match PgPoolOptions::new().max_connections(5).connect(&db_url).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: failed to connect to PostgreSQL: {e}");
            return ExitCode::FAILURE;
        }
    };

    match (org, branch) {
        (Some(o), _) => println!("Scope:  org {o}"),
        (_, Some(b)) => println!("Scope:  branch {b}"),
        _ => println!("Scope:  ALL organizations"),
    }
    println!("Mode:   {}", if dry_run { "DRY RUN (rolls back)" } else { "LIVE (commits)" });

    match run(&pool, org, branch, dry_run).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(
    pool: &sqlx::PgPool,
    org: Option<Uuid>,
    branch: Option<Uuid>,
    dry_run: bool,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Pre-flight: a (branch, business-day) group must not contain BOTH a
    // null and a non-null order_ref — that means live traffic already minted refs
    // for that day, so renumbering the historical NULLs from 1 would collide.
    let mixed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (
            SELECT 1
            FROM orders o JOIN branches b ON b.id = o.branch_id
            WHERE ($1::uuid IS NULL OR b.org_id = $1)
              AND ($2::uuid IS NULL OR o.branch_id = $2)
            GROUP BY o.branch_id, (o.created_at AT TIME ZONE b.timezone)::date
            HAVING bool_or(o.order_ref IS NULL) AND bool_or(o.order_ref IS NOT NULL)
         ) m",
    )
    .bind(org)
    .bind(branch)
    .fetch_one(&mut *tx)
    .await?;
    if mixed > 0 {
        return Err(sqlx::Error::Protocol(format!(
            "{mixed} (branch, day) group(s) already contain live order_refs mixed with \
             un-backfilled orders. Gate order creation, then re-run. Aborting (no changes)."
        )));
    }

    // Step B — number historical orders by true ring-up order.
    let updated = sqlx::query(
        "WITH numbered AS (
            SELECT o.id,
                   b.code AS branch_code,
                   (o.created_at AT TIME ZONE b.timezone)::date AS biz_date,
                   row_number() OVER (
                       PARTITION BY o.branch_id, (o.created_at AT TIME ZONE b.timezone)::date
                       ORDER BY s.opened_at, o.order_number, o.id
                   ) AS seq
            FROM orders o
            JOIN branches b ON b.id = o.branch_id
            JOIN shifts   s ON s.id = o.shift_id
            WHERE o.order_ref IS NULL
              AND ($1::uuid IS NULL OR b.org_id   = $1)
              AND ($2::uuid IS NULL OR o.branch_id = $2)
        )
        UPDATE orders o
        SET order_ref = n.branch_code || '-' || to_char(n.biz_date, 'YYMMDD')
                        || '-' || lpad(n.seq::text, 4, '0')
        FROM numbered n
        WHERE o.id = n.id",
    )
    .bind(org)
    .bind(branch)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // Step C — seed the counter for EVERY (branch, business-day) group so a later
    // (even backdated) live order continues from N+1 instead of restarting at 1.
    let seeded = sqlx::query(
        "INSERT INTO order_ref_counters (branch_id, business_date, last_seq)
         SELECT o.branch_id,
                (o.created_at AT TIME ZONE b.timezone)::date,
                count(*)
         FROM orders o JOIN branches b ON b.id = o.branch_id
         WHERE ($1::uuid IS NULL OR b.org_id   = $1)
           AND ($2::uuid IS NULL OR o.branch_id = $2)
         GROUP BY o.branch_id, (o.created_at AT TIME ZONE b.timezone)::date
         ON CONFLICT (branch_id, business_date)
         DO UPDATE SET last_seq = GREATEST(order_ref_counters.last_seq, EXCLUDED.last_seq)",
    )
    .bind(org)
    .bind(branch)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // Safety: no duplicate refs anywhere (the stage-2 UNIQUE index would reject them).
    let dupes: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (
            SELECT order_ref FROM orders WHERE order_ref IS NOT NULL
            GROUP BY order_ref HAVING count(*) > 1
         ) d",
    )
    .fetch_one(&mut *tx)
    .await?;

    // Remaining un-backfilled rows in scope (should be 0 — branches.code is NOT NULL).
    let remaining: i64 = sqlx::query(
        "SELECT count(*) AS c
         FROM orders o JOIN branches b ON b.id = o.branch_id
         WHERE o.order_ref IS NULL
           AND ($1::uuid IS NULL OR b.org_id   = $1)
           AND ($2::uuid IS NULL OR o.branch_id = $2)",
    )
    .bind(org)
    .bind(branch)
    .fetch_one(&mut *tx)
    .await?
    .get("c");

    println!();
    println!("Orders backfilled:        {updated}");
    println!("Counter groups seeded:    {seeded}");
    println!("Duplicate refs found:     {dupes}");
    println!("Orders still missing ref: {remaining}");
    println!();

    if dupes > 0 {
        tx.rollback().await?;
        return Err(sqlx::Error::Protocol(
            "duplicate order_refs detected — rolled back, no changes committed".into(),
        ));
    }

    if dry_run {
        tx.rollback().await?;
        println!("Dry run — no changes were committed.");
    } else {
        tx.commit().await?;
        println!("Done — order_ref backfilled. Now apply 20260614030000_order_ref_finalize.sql.");
    }
    Ok(())
}
