//! Operator CLI: what will the customers-unification migrations do to THIS
//! database? (CUSTOMERS_UNIFICATION_DESIGN.md §8.)
//!
//! The backfill is not a tool — it is the migrations themselves
//! (`20260925010000` … `20260925110000`), and they run on boot. So the only
//! honest dry run is to run exactly those statements and not keep them: this
//! binary opens ONE transaction, applies every migration the database has not
//! seen yet (the same embedded SQL the server would apply), measures the
//! result, and ROLLS BACK. Nothing is re-implemented, so the report cannot
//! drift from what the deploy will do. A migration that would FAIL on this
//! data fails here, with its own message, before it fails in production.
//!
//! Run it against a COPY of production, never production itself: the
//! transaction holds the migrations' table locks until it rolls back, and a
//! live server would stall behind them.
//!
//!   pg_dump "$PROD_URL" | psql "$COPY_URL"          # or restore last night's backup
//!   DATABASE_URL="$COPY_URL" cargo run --bin customers-backfill-dry-run
//!
//! The conflict list prints names and phone numbers — it is personal data, for
//! the owner's eyes; do not paste it into an issue.

use std::collections::BTreeMap;
use std::env;
use std::process::ExitCode;

use sqlx::migrate::Migrator;
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, Executor, Row};
use uuid::Uuid;

/// (org, canonical phone) → every (name, source, occurrences) seen on it.
type Groups = BTreeMap<(Uuid, String), Vec<(String, String, i64)>>;

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

const USAGE: &str = "\
Reports what the customers-unification migrations will do, WITHOUT writing.

USAGE:
    customers-backfill-dry-run [--conflicts <n>] [--i-know-this-is-not-a-copy]

Reads DATABASE_URL (environment or .env). Applies the pending migrations inside
one transaction, reports, and rolls back.

OPTIONS:
    --conflicts <n>   Conflicting phone groups to list (default 200; the COUNT
                      is always complete)
    --i-know-this-is-not-a-copy
                      Skip the refusal to run against a database named like a
                      live one. The locks are real until the rollback.";

fn folded(s: &str) -> Vec<String> {
    use unicode_normalization::UnicodeNormalization;
    s.split_whitespace()
        .map(|t| t.nfc().collect::<String>().to_lowercase())
        .collect()
}

/// "Sara" / "sara " / "Sara Mostafa" are one person spelled three ways; "Sara"
/// and "Omar" on one phone are two people, or one wrong number. Only the
/// second kind needs a human.
fn materially_different(a: &str, b: &str) -> bool {
    let (a, b) = (folded(a), folded(b));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let subset = |x: &[String], y: &[String]| x.iter().all(|t| y.contains(t));
    !(subset(&a, &b) || subset(&b, &a))
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), String> {
    let mut max_conflicts = 200usize;
    let mut not_a_copy = false;
    let mut args = env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--conflicts" => {
                max_conflicts = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or("--conflicts needs a number")?;
            }
            "--i-know-this-is-not-a-copy" => not_a_copy = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => return Err(format!("unknown argument `{other}`\n\n{USAGE}")),
        }
    }
    dotenvy::dotenv().ok();
    let url = env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set".to_string())?;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let mut conn = pool.acquire().await.map_err(|e| e.to_string())?;

    let db: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| e.to_string())?;
    if !not_a_copy
        && !["copy", "dry", "test", "staging", "restore"]
            .iter()
            .any(|w| db.contains(w))
    {
        return Err(format!(
            "database `{db}` is not named like a copy (copy/dry/test/staging/restore). \
             Run this against a COPY of production; pass --i-know-this-is-not-a-copy to override."
        ));
    }

    let has_ledger: bool =
        sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations') IS NOT NULL")
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| e.to_string())?;
    let applied: Vec<i64> = if has_ledger {
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success ORDER BY version")
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| e.to_string())?
    } else {
        vec![]
    };
    let pending: Vec<_> = MIGRATOR
        .iter()
        .filter(|m| !m.migration_type.is_down_migration() && !applied.contains(&m.version))
        .collect();
    println!("database: {db}");
    println!(
        "migrations applied: {}, pending: {}",
        applied.len(),
        pending.len()
    );
    if pending.is_empty() {
        println!(
            "\nNothing is pending: this database has already been migrated. Nothing to report."
        );
        return Ok(());
    }

    let mut tx = conn.begin().await.map_err(|e| e.to_string())?;

    // ── before ──────────────────────────────────────────────────────────────
    // Temp tables live in the session's own namespace and go with the rollback.
    let has_customers: bool =
        sqlx::query_scalar("SELECT to_regclass('public.customers') IS NOT NULL")
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
    tx.execute(
        "CREATE TEMP TABLE _dry_before (id uuid PRIMARY KEY, was_merged boolean NOT NULL) ON COMMIT DROP",
    )
    .await
    .map_err(|e| e.to_string())?;
    if has_customers {
        tx.execute("INSERT INTO _dry_before SELECT id, merged_into IS NOT NULL FROM customers")
            .await
            .map_err(|e| e.to_string())?;
    }
    // Every (org, phone, name) the system has been told, from every source,
    // read BEFORE the migrations drop the columns some of them live in.
    tx.execute(
        "CREATE TEMP TABLE _dry_people (org_id uuid, phone text, name text, source text, ref uuid) ON COMMIT DROP",
    )
    .await
    .map_err(|e| e.to_string())?;
    for (source, table, name_col, phone_col, org_expr, join) in [
        ("customers", "customers", "name", "phone", "x.org_id", ""),
        (
            "loyalty",
            "loyalty_customers",
            "name",
            "phone",
            "x.org_id",
            "",
        ),
        (
            "delivery",
            "delivery_orders",
            "customer_name",
            "customer_phone",
            "b.org_id",
            "JOIN branches b ON b.id = x.branch_id",
        ),
        (
            "booking",
            "bookings",
            "guest_name",
            "guest_phone",
            "b.org_id",
            "JOIN branches b ON b.id = x.branch_id",
        ),
    ] {
        let present: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM information_schema.columns
              WHERE table_schema = 'public' AND table_name = $1 AND column_name = ANY($2)",
        )
        .bind(table)
        .bind(vec![name_col, phone_col])
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        if present < 2 {
            println!("  (source `{table}` has no {name_col}/{phone_col} here — skipped)");
            continue;
        }
        tx.execute(
            format!(
                "INSERT INTO _dry_people
                 SELECT {org_expr}, x.{phone_col}, x.{name_col}, '{source}', x.id
                   FROM {table} x {join}
                  WHERE x.{phone_col} IS NOT NULL AND btrim(x.{phone_col}) <> ''"
            )
            .as_str(),
        )
        .await
        .map_err(|e| format!("reading {table}: {e}"))?;
    }

    // ── the migrations themselves ───────────────────────────────────────────
    println!(
        "\napplying {} pending migrations inside a transaction …",
        pending.len()
    );
    for m in &pending {
        let started = std::time::Instant::now();
        tx.execute(&*m.sql).await.map_err(|e| {
            format!(
                "migration {} ({}) FAILED on this data — it would fail on boot too:\n  {e}",
                m.version, m.description
            )
        })?;
        println!(
            "  ok {:>14}  {:<44} {:>7.1}s",
            m.version,
            m.description,
            started.elapsed().as_secs_f32()
        );
    }

    // ── after ───────────────────────────────────────────────────────────────
    println!("\n── customers that would be CREATED, by source ──");
    let created = sqlx::query(
        "SELECT COALESCE(c.source, '(none)') AS source, count(*) AS n,
                count(*) FILTER (WHERE c.merged_into IS NOT NULL) AS born_merged
           FROM customers c WHERE NOT EXISTS (SELECT 1 FROM _dry_before b WHERE b.id = c.id)
          GROUP BY 1 ORDER BY 2 DESC",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    let mut total = 0i64;
    for r in &created {
        let n: i64 = r.get("n");
        total += n;
        println!("  {:<12} {:>8}", r.get::<String, _>("source"), n);
    }
    println!("  {:<12} {:>8}", "TOTAL", total);

    println!("\n── existing customers that would be MERGED (same canonical phone) ──");
    let merged = sqlx::query(
        "SELECT c.org_id, count(*) AS n FROM customers c JOIN _dry_before b ON b.id = c.id
          WHERE c.merged_into IS NOT NULL AND NOT b.was_merged GROUP BY 1 ORDER BY 2 DESC",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    let merged_total: i64 = merged.iter().map(|r| r.get::<i64, _>("n")).sum();
    for r in &merged {
        println!(
            "  org {}  {:>6}",
            r.get::<Uuid, _>("org_id"),
            r.get::<i64, _>("n")
        );
    }
    println!("  TOTAL {merged_total}");

    println!("\n── rows that would be LINKED to a customer ──");
    for table in [
        "orders",
        "delivery_orders",
        "bookings",
        "open_tickets",
        "customer_addresses",
    ] {
        let (linked, all): (i64, i64) = sqlx::query_as(&format!(
            "SELECT count(*) FILTER (WHERE customer_id IS NOT NULL), count(*) FROM {table}"
        ))
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| format!("{table}: {e}"))?;
        println!("  {table:<20} {linked:>9} of {all:>9}");
    }

    // ── conflicts: one canonical phone, materially different names ──────────
    let people = sqlx::query(
        "SELECT org_id, phone_canonical(phone) AS key, name, source, count(*) AS n
           FROM _dry_people WHERE phone_canonical(phone) IS NOT NULL AND name IS NOT NULL
          GROUP BY 1, 2, 3, 4 ORDER BY 1, 2, 5 DESC",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    let unparseable: i64 =
        sqlx::query_scalar("SELECT count(*) FROM _dry_people WHERE phone_canonical(phone) IS NULL")
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
    let mut groups: Groups = BTreeMap::new();
    for r in &people {
        groups
            .entry((r.get("org_id"), r.get("key")))
            .or_default()
            .push((r.get("name"), r.get("source"), r.get("n")));
    }
    let conflicts: Vec<_> = groups
        .iter()
        .filter(|(_, names)| {
            names
                .iter()
                .any(|a| names.iter().any(|b| materially_different(&a.0, &b.0)))
        })
        .collect();
    println!("\n── CONFLICTS for manual review: one phone, materially different names ──");
    println!("  (the migrations keep ONE customer per phone and never rename them; each row's");
    println!(
        "   own name stays on it as a snapshot. Review = is this one person or a shared/wrong number?)"
    );
    println!(
        "  {} conflicting phones; {} rows whose phone is not a phone (left unlinked)",
        conflicts.len(),
        unparseable
    );
    for ((org, key), names) in conflicts.iter().take(max_conflicts) {
        println!("  org {org}  +{key}");
        for (name, source, n) in names.iter() {
            println!("      {n:>5} × {source:<9} {name}");
        }
    }
    if conflicts.len() > max_conflicts {
        println!(
            "  … {} more (raise --conflicts)",
            conflicts.len() - max_conflicts
        );
    }

    tx.rollback().await.map_err(|e| e.to_string())?;
    println!("\nROLLED BACK. Nothing was written to `{db}`.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::materially_different;

    #[test]
    fn a_fuller_spelling_is_not_a_conflict_and_another_person_is() {
        assert!(!materially_different("Sara", " sara "));
        assert!(!materially_different("Sara", "Sara Mostafa"));
        assert!(!materially_different("", "Omar"));
        assert!(materially_different("Sara", "Omar"));
        assert!(materially_different("Sara Ali", "Sara Mostafa"));
    }
}
