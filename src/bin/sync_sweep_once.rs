//! Run ONE changefeed sweep pass now (`sync::pull::sweeper::sweep_once`) and
//! print its report: what the server's ten-minute task does, on demand. For
//! measuring the Phase 6 ledger retention on a prod copy before it ships; on
//! prod the running server does this by itself.
//!
//! ```text
//! DATABASE_URL=postgres://.../<prod copy> cargo run --bin sync-sweep-once
//! ```

#[tokio::main]
async fn main() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let pool = sqlx::PgPool::connect(&url).await.expect("connect");
    let t = std::time::Instant::now();
    let report = madar_rust::sync::pull::sweeper::sweep_once(&pool)
        .await
        .expect("sweep");
    println!("{report:?} in {:?}", t.elapsed());
}
