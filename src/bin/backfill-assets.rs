//! Operator binary: move pre-asset-store image/animation files into the
//! content-addressed store (decision 19, contract §11.6).
//!
//! ```text
//! backfill-assets (--org <uuid> | --all) [--dry-run] [--limit <n>] [--run-id <uuid>]
//!                 [--verify-only] [--report <path.json>] [--uploads-dir <path>] [--assets-dir <path>]
//!                 [--step-animations-dir <path>]
//!                 [--prune-originals --i-have-verified <run-id> [--allow-partial]]
//! ```
//! Idempotent and resumable: safe to kill and re-run. Never modifies legacy URL
//! columns; deletes originals only with `--prune-originals`.

use std::path::PathBuf;

use madar_rust::assets::AssetStore;
use madar_rust::assets::backfill::{self, BackfillOptions};
use uuid::Uuid;

fn usage() -> ! {
    eprintln!(
        "usage: backfill-assets (--org <uuid> | --all) [--dry-run] [--limit <n>] [--run-id <uuid>] \
         [--verify-only] [--report <path.json>] [--uploads-dir <path>] [--assets-dir <path>] \
         [--step-animations-dir <path>] [--prune-originals --i-have-verified <run-id> [--allow-partial]]"
    );
    std::process::exit(2)
}

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    let mut args = std::env::args().skip(1);
    let mut org: Option<Uuid> = None;
    let mut all = false;
    let mut dry_run = false;
    let mut limit = None;
    let mut run_id = None;
    let mut verify_only = false;
    let mut report_path: Option<PathBuf> = None;
    let mut uploads_dir: Option<PathBuf> = None;
    let mut assets_dir: Option<PathBuf> = None;
    let mut steps_dir: Option<PathBuf> = None;
    let mut prune = false;
    let mut verified_run: Option<Uuid> = None;
    let mut allow_partial = false;
    let val = |a: &mut std::iter::Skip<std::env::Args>| a.next().unwrap_or_else(|| usage());
    while let Some(a) = args.next() {
        match a.as_str() {
            "--org" => org = Some(val(&mut args).parse().unwrap_or_else(|_| usage())),
            "--all" => all = true,
            "--dry-run" => dry_run = true,
            "--limit" => limit = Some(val(&mut args).parse().unwrap_or_else(|_| usage())),
            "--run-id" => run_id = Some(val(&mut args).parse().unwrap_or_else(|_| usage())),
            "--verify-only" => verify_only = true,
            "--report" => report_path = Some(val(&mut args).into()),
            "--uploads-dir" => uploads_dir = Some(val(&mut args).into()),
            "--assets-dir" => assets_dir = Some(val(&mut args).into()),
            "--step-animations-dir" => steps_dir = Some(val(&mut args).into()),
            "--prune-originals" => prune = true,
            "--i-have-verified" => {
                verified_run = Some(val(&mut args).parse().unwrap_or_else(|_| usage()))
            }
            "--allow-partial" => allow_partial = true,
            _ => usage(),
        }
    }
    if org.is_some() == all {
        usage();
    }
    if prune && (dry_run || verified_run.is_none()) {
        eprintln!(
            "--prune-originals needs --i-have-verified <run-id> and never runs with --dry-run"
        );
        std::process::exit(2);
    }

    let env_store = AssetStore::from_env();
    let uploads = uploads_dir.unwrap_or(env_store.uploads_dir.clone());
    let store = AssetStore::new(
        assets_dir.unwrap_or_else(|| {
            if std::env::var("ASSETS_DIR").is_ok() {
                env_store.assets_dir.clone()
            } else {
                uploads.join("assets")
            }
        }),
        uploads,
    );
    let db_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&db_url)
        .await
        .expect("database connection");

    if prune {
        match backfill::prune(&pool, &store, org, verified_run.unwrap(), allow_partial).await {
            Ok(rep) => {
                println!("{}", serde_json::to_string_pretty(&rep).unwrap());
                return;
            }
            Err(e) => {
                eprintln!("prune refused: {e}");
                std::process::exit(1);
            }
        }
    }

    let opts = BackfillOptions {
        org,
        dry_run,
        limit,
        run_id: run_id.unwrap_or_else(Uuid::new_v4),
        verify_only,
        store,
        step_animations_dir: steps_dir
            .or_else(|| Some(madar_rust::recipes::steps::animations_dir().into())),
    };
    match backfill::run(&pool, &opts).await {
        Ok(report) => {
            let json = serde_json::to_string_pretty(&report).unwrap();
            println!("{json}");
            if let Some(p) = report_path {
                std::fs::write(&p, &json).expect("write report");
            }
            if !report.dry_run {
                eprintln!("run_id: {}", report.run_id);
                match backfill::verified_runs(&pool, org).await {
                    Ok(runs) if runs.is_empty() => {
                        eprintln!("no verified items yet: nothing can be pruned");
                    }
                    Ok(runs) => {
                        for (run, n) in runs {
                            eprintln!(
                                "verified in run {run}: {n} item(s) -> prune with --prune-originals --i-have-verified {run}"
                            );
                        }
                    }
                    Err(e) => eprintln!("could not list verified runs: {e}"),
                }
            }
        }
        Err(e) => {
            eprintln!("backfill failed: {e}");
            std::process::exit(1);
        }
    }
}
