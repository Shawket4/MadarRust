//! Operator CLI: diff the bundle 1.2× margin-floor pass/fail between the RETIRED
//! `compute_item_cost` engine and the canonical `costing::service` engine
//! (`component_cost`). Read-only — computes both costs, never writes.
//!
//! For every ACTIVE bundle in scope it computes `sum_costs` old vs new, applies the
//! floor `price >= 1.20 × sum_costs`, and reports every bundle whose pass/fail (or
//! known/unknown) result FLIPS. This is the diff required before shipping the cost
//! re-route — some bundles that passed under the old (single arbitrary size, f64,
//! recipe-less = free) engine will now fail or become unknown (recipe-less = unknown).
//!
//! Usage:
//!   bundle-margin-flip [--org <uuid> | --branch <uuid>]      (default: all orgs)
//!
//! Both sides use the ORG (standard) cost so the diff isolates engine differences,
//! not per-branch cost.

use std::env;
use std::process::ExitCode;

use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use madar_rust::bundles::handlers::{component_cost, compute_item_cost};
use madar_rust::errors::AppError;

const FLOOR: f64 = 1.20;

const USAGE: &str = "\
Diffs the bundle 1.2x margin-floor pass/fail between the retired and canonical cost engines.

USAGE:
    bundle-margin-flip [--org <uuid> | --branch <uuid>]

Default (no scope) = every organization. Read-only.";

fn floor_pass(price: i32, sum: i64) -> bool {
    (price as f64) >= (sum as f64 * FLOOR)
}

fn verdict(known: bool, price: i32, sum: i64) -> Option<bool> {
    known.then(|| floor_pass(price, sum))
}

fn label(v: Option<bool>) -> &'static str {
    match v {
        Some(true) => "PASS",
        Some(false) => "FAIL",
        None => "UNKNOWN",
    }
}

async fn run(pool: &sqlx::PgPool, org_filter: Option<Uuid>) -> Result<(), AppError> {
    let bundles: Vec<(Uuid, String, Uuid, i32)> = sqlx::query_as(
        "SELECT id, name, org_id, price FROM bundles \
         WHERE status = 'active' AND ($1::uuid IS NULL OR org_id = $1) \
         ORDER BY org_id, name",
    )
    .bind(org_filter)
    .fetch_all(pool)
    .await?;

    let examined = bundles.len();
    let mut flips: Vec<String> = Vec::new();

    for (bid, name, org_id, price) in bundles {
        let comps: Vec<(Uuid, i32)> =
            sqlx::query_as("SELECT item_id, quantity FROM bundle_components WHERE bundle_id = $1")
                .bind(bid)
                .fetch_all(pool)
                .await?;

        // OLD engine (retired): single arbitrary size, f64, recipe-less = free.
        let mut old_sum: i64 = 0;
        let mut old_known = true;
        for (item, qty) in &comps {
            match compute_item_cost(pool, *item).await? {
                Some(c) => old_sum += c as i64 * *qty as i64,
                None => old_known = false,
            }
        }

        // NEW engine (canonical): base-size recipe, Decimal, recipe-less = unknown.
        let mut new_sum: i64 = 0;
        let mut new_known = true;
        for (item, qty) in &comps {
            match component_cost(pool, org_id, *item, None).await? {
                Some(c) => new_sum += c * *qty as i64,
                None => new_known = false,
            }
        }

        let old_v = verdict(old_known, price, old_sum);
        let new_v = verdict(new_known, price, new_sum);
        if old_v != new_v {
            flips.push(format!(
                "  {name}  (bundle {bid}, org {org_id})\n     price={price}  \
                 old: {} [sum {}{}]  ->  new: {} [sum {}{}]",
                label(old_v),
                old_sum,
                if old_known { "" } else { ", incomplete" },
                label(new_v),
                new_sum,
                if new_known { "" } else { ", incomplete" },
            ));
        }
    }

    println!();
    println!("Active bundles examined:        {examined}");
    println!("Margin-floor pass/fail flips:   {}", flips.len());
    if !flips.is_empty() {
        println!();
        println!("Bundles whose 1.2x floor verdict changed (old engine -> new engine):");
        for f in &flips {
            println!("{f}");
        }
        println!();
        println!("Review each: a PASS->FAIL / *->UNKNOWN bundle was under-priced under the");
        println!("old engine and must be re-priced (or its recipes completed) before the flip.");
    } else {
        println!("No flips — every active bundle keeps its floor verdict under the new engine.");
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    dotenvy::dotenv().ok();

    let mut org_filter: Option<Uuid> = None;
    let mut branch: Option<Uuid> = None;
    let mut args = env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--org" => match args.next().and_then(|v| Uuid::parse_str(&v).ok()) {
                Some(u) => org_filter = Some(u),
                None => {
                    eprintln!("error: --org requires a uuid\n\n{USAGE}");
                    return ExitCode::from(2);
                }
            },
            "--branch" => match args.next().and_then(|v| Uuid::parse_str(&v).ok()) {
                Some(u) => branch = Some(u),
                None => {
                    eprintln!("error: --branch requires a uuid\n\n{USAGE}");
                    return ExitCode::from(2);
                }
            },
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("error: unknown argument: {other}\n\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }

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

    // Resolve --branch to its org.
    if let Some(b) = branch {
        match sqlx::query_scalar::<_, Uuid>(
            "SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(b)
        .fetch_optional(&pool)
        .await
        {
            Ok(Some(o)) => org_filter = Some(o),
            Ok(None) => {
                eprintln!("error: branch {b} not found");
                return ExitCode::FAILURE;
            }
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    match run(&pool, org_filter).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
