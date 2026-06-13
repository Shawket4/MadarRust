//! Operator CLI: normalize existing recipe quantities to each linked
//! ingredient's base stock unit. Not exposed over HTTP — run on the VPS next
//! to the server (reads DATABASE_URL from the environment / .env).
//!
//! Usage:
//!   backfill-recipe-units --org <uuid>    [--dry-run]
//!   backfill-recipe-units --branch <uuid> [--dry-run]
//!
//! Always --dry-run first and review the "unconvertible" list — those rows
//! (cross-family or unknown units) are NOT touched and must be fixed by hand
//! in the dashboard.

use std::env;
use std::process::ExitCode;

use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use sufrix_rust::recipes::backfill::{backfill_recipe_units, BackfillScope};

const USAGE: &str = "\
Normalizes existing recipe quantities to each ingredient's base stock unit.

USAGE:
    backfill-recipe-units (--org <uuid> | --branch <uuid>) [--dry-run]

OPTIONS:
    --org <uuid>      Normalize every recipe of this organization
    --branch <uuid>   Normalize recipes of this branch's organization
    --dry-run         Compute and print the summary, then roll back

Convertible unit mismatches (e.g. recipe 'g' for a 'kg' ingredient) are
rewritten (quantity ×factor, unit set to base). Cross-family / unknown-unit
mismatches are left untouched and listed for manual correction. Historical
order snapshots are NOT changed. Run --dry-run first.";

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
                branch = Some(Uuid::parse_str(&v).map_err(|_| format!("invalid branch uuid: {v}"))?);
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
    let pool = match PgPoolOptions::new().max_connections(5).connect(&db_url).await {
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
    println!("Mode:   {}", if dry_run { "DRY RUN (rolls back)" } else { "LIVE (commits)" });

    let summary = match backfill_recipe_units(&pool, scope, dry_run).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    println!();
    println!("Recipe rows scanned:      {}", summary.scanned);
    println!("Already in base unit:     {}", summary.already_base);
    println!("Converted to base unit:   {}", summary.converted);
    println!("Unconvertible (skipped):  {}", summary.unconvertible.len());
    if !summary.unconvertible.is_empty() {
        println!();
        println!("These rows need manual correction (recipe unit vs ingredient base unit):");
        for u in &summary.unconvertible {
            println!(
                "  [{}] {} — recipe '{}' vs base '{}' (row {})",
                u.table, u.ingredient_name, u.recipe_unit, u.base_unit, u.row_id
            );
        }
    }
    println!();
    if dry_run {
        println!("Dry run — no changes were committed.");
    } else {
        println!("Done — recipe quantities normalized to base units.");
    }
    ExitCode::SUCCESS
}
