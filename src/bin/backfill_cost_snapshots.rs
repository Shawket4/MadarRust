//! Operator CLI: reprice historical order cost snapshots at CURRENT
//! ingredient costs. Deliberately not exposed over HTTP — run it on the VPS
//! next to the server (reads DATABASE_URL from the environment / .env).
//!
//! Usage:
//!   backfill-cost-snapshots --org <uuid>    [--dry-run]
//!   backfill-cost-snapshots --branch <uuid> [--dry-run]
//!
//! Exactly one of --org / --branch is required. --dry-run computes and
//! prints the full summary, then rolls back.
//!
//! Dev:  cargo run --bin backfill-cost-snapshots -- --org <uuid> --dry-run
//! VPS:  ./backfill-cost-snapshots --branch <uuid>

use std::env;
use std::process::ExitCode;

use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use madar_rust::costing::backfill::{BackfillScope, backfill_cost_snapshots};

const USAGE: &str = "\
Reprices historical order cost snapshots at CURRENT ingredient costs.

USAGE:
    backfill-cost-snapshots (--org <uuid> | --branch <uuid>) [--dry-run]

OPTIONS:
    --org <uuid>      Reprice every branch of this organization
    --branch <uuid>   Reprice this branch only
    --dry-run         Compute and print the summary, then roll back

This REWRITES financial history (order_items.unit_cost/line_cost, addon,
optional, and bundle-component costs) as if each line were ordered TODAY:
current recipes and addon ingredients at current catalog costs. Lines whose
item/addon cannot be costed today become cost_missing. Run --dry-run first.";

fn parse_args() -> Result<(BackfillScope, bool), String> {
    let mut org: Option<Uuid> = None;
    let mut branch: Option<Uuid> = None;
    let mut dry_run = false;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
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

    let scope = match (org, branch) {
        (Some(o), None) => BackfillScope::Org(o),
        (None, Some(b)) => BackfillScope::Branch(b),
        _ => return Err("pass exactly one of --org <uuid> or --branch <uuid>".into()),
    };
    Ok((scope, dry_run))
}

#[tokio::main]
async fn main() -> ExitCode {
    dotenvy::dotenv().ok();

    let (scope, dry_run) = match parse_args() {
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
    let pool = match PgPoolOptions::new()
        .max_connections(5)
        .connect(&db_url)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: failed to connect to PostgreSQL: {e}");
            return ExitCode::FAILURE;
        }
    };

    match scope {
        BackfillScope::Org(id) => println!("Scope:  org {id}"),
        BackfillScope::Branch(id) => println!("Scope:  branch {id}"),
    }
    println!(
        "Mode:   {}",
        if dry_run {
            "DRY RUN (rolls back)"
        } else {
            "LIVE (commits)"
        }
    );

    let summary = match backfill_cost_snapshots(&pool, scope, dry_run).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let fmt_egp = |p: i64| format!("EGP {:.2}", p as f64 / 100.0);
    println!();
    println!("Branches in scope:        {}", summary.branches);
    println!("Order lines in scope:     {}", summary.order_lines_in_scope);
    println!("Order lines updated:      {}", summary.order_lines_updated);
    println!("Addon rows updated:       {}", summary.addon_rows_updated);
    println!(
        "Optional rows updated:    {}",
        summary.optional_rows_updated
    );
    println!(
        "Bundle components updated:{}",
        summary.bundle_component_rows_updated
    );
    println!(
        "Σ line_cost:              {} -> {}",
        fmt_egp(summary.line_cost_total_before),
        fmt_egp(summary.line_cost_total_after)
    );
    println!(
        "Lines missing cost:       {} -> {}",
        summary.lines_cost_missing_before, summary.lines_cost_missing_after
    );
    println!();
    if dry_run {
        println!("Dry run — no changes were committed.");
    } else {
        println!("Done — snapshots repriced at current ingredient costs.");
    }
    ExitCode::SUCCESS
}
