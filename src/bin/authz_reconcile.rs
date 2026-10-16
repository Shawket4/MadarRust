//! DB audit Phase 4: the exhaustive authz reconciliation (`authz::reconcile`).
//! For every person x capability x scope (anywhere + every live branch), the
//! decision the legacy tables imply next to the architecture E decision.
//!
//! ```text
//! DATABASE_URL=postgres://.../<prod copy> cargo run --bin authz-reconcile [-- --org <uuid>] > authz.tsv
//! ```
//! Writes the TSV (ids only) to stdout and a one-line summary to stderr. Exits
//! 1 when any decision differs. Read-only.

#[tokio::main]
async fn main() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let args: Vec<String> = std::env::args().collect();
    let org = args
        .iter()
        .position(|a| a == "--org")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.parse::<uuid::Uuid>().expect("--org takes a uuid"));
    let pool = sqlx::PgPool::connect(&url).await.expect("connect");
    let report = madar_rust::authz::reconcile::run(&pool, org)
        .await
        .expect("reconcile");
    print!("{}", report.to_tsv());
    let cells: usize = report.orgs.iter().map(|o| o.cells).sum();
    let authored: usize = report.orgs.iter().map(|o| o.new_authored.len()).sum();
    eprintln!(
        "{} orgs, {} decisions compared, {} differ, {} people authored in the new model, {} role_permissions cells differ from the spec, {} stored role grants differ from the spec",
        report.orgs.len(),
        cells,
        report.diff_count(),
        authored,
        report.role_defaults.len(),
        report.orgs.iter().map(|o| o.spec_drift.len()).sum::<usize>()
    );
    if report.diff_count() > 0 {
        std::process::exit(1);
    }
}
