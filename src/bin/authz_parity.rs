//! Compare today's permission model with architecture E for every active person
//! and every legacy cell (PERMISSIONS_ARCHITECTURE §6 Phase 2 verification).
//!
//! ```text
//! DATABASE_URL=postgres://.../madar_prodcopy cargo run --bin authz-parity
//! ```
//! Prints one line per mismatch and exits non-zero on any UNEXPLAINED one.
//! Read-only.

#[tokio::main]
async fn main() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let pool = sqlx::PgPool::connect(&url).await.expect("connect");
    let rows = madar_rust::authz::shadow::compare_all(&pool)
        .await
        .expect("compare");
    let mut unexplained = 0;
    for m in &rows {
        println!(
            "{:<24} {:<15} {}:{} legacy={} new={} {}",
            m.user_name,
            m.role,
            m.resource,
            m.action,
            m.legacy,
            m.new,
            m.explained.unwrap_or("UNEXPLAINED")
        );
        if m.explained.is_none() {
            unexplained += 1;
        }
    }
    println!("{} mismatches, {} unexplained", rows.len(), unexplained);
    if unexplained > 0 {
        std::process::exit(1);
    }
}
