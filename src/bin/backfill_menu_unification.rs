//! Operator CLI: backfill the unified menu/recipe/modifier tables from the legacy
//! tables, with STABLE ids and an unmigratable-rows report. Not exposed over HTTP —
//! run on the VPS next to the server (reads DATABASE_URL from the environment / .env).
//!
//! Run AFTER migration 20260703100000_menu_unification_expand.sql.
//!
//! Usage:
//!   backfill-menu-unification --org <uuid>    [--dry-run]
//!   backfill-menu-unification --branch <uuid> [--dry-run]
//!
//! Always --dry-run first and review the "unmigratable" list — those rows are NOT
//! written to the new tables and must be fixed by hand (see MadarRust/CONTRACT.md).
//! Order history is never touched.

use std::env;
use std::process::ExitCode;

use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use madar_rust::menu_unification::backfill::backfill_menu_unification;
use madar_rust::recipes::backfill::BackfillScope;

const USAGE: &str = "\
Backfills the unified menu/recipe/modifier tables from the legacy tables.

USAGE:
    backfill-menu-unification (--org <uuid> | --branch <uuid>) [--dry-run]

OPTIONS:
    --org <uuid>      Backfill every menu item / addon / recipe of this organization
    --branch <uuid>   Backfill the organization owning this branch
    --dry-run         Compute + print the summary and report, then roll back

Preserves stable ids (modifier_options.id == old addon/optional id) so immutable order
history keeps resolving. Rows that cannot be faithfully migrated are listed, not dropped.
Idempotent (clears this org's new-table rows first). Run --dry-run first.";

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

    let summary = match backfill_menu_unification(&pool, scope, dry_run).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    println!();
    println!("menu_item_sizes  copied:      {}", summary.sizes_copied);
    println!("menu_item_sizes  synth one_size:{}", summary.one_size_synth);
    println!("modifier_groups  created:     {}", summary.groups_created);
    println!("modifier_options created:     {}", summary.options_created);
    println!(
        "item↔group attaches:          {}",
        summary.item_group_attaches
    );
    println!("recipe_lines     written:     {}", summary.recipe_lines);
    println!("menu_price_overrides written: {}", summary.price_overrides);
    println!(
        "Unmigratable (needs review):  {}",
        summary.unmigratable.len()
    );
    if !summary.unmigratable.is_empty() {
        println!();
        println!("These legacy rows were NOT migrated and need manual review:");
        for u in &summary.unmigratable {
            println!("  [{}] {} — {}", u.kind, u.source, u.detail);
        }
    }
    println!();
    if dry_run {
        println!("Dry run — no changes were committed.");
    } else {
        println!("Done — unified menu tables backfilled.");
    }
    ExitCode::SUCCESS
}
