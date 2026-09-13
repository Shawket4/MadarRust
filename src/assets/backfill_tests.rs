//! Backfill tests on a fixture uploads dir (contract §11.8).

use sqlx::PgPool;
use uuid::Uuid;

use super::AssetStore;
use super::backfill::{BackfillOptions, prune, run};
use super::tests::{photo_png, schema, seed_item, seed_org, tmp_store};

struct Fx {
    _d: tempfile::TempDir,
    store: AssetStore,
    org: Uuid,
    good: Uuid,
    dup: Uuid,
    missing: Uuid,
    broken: Uuid,
}

async fn fixture(pool: &PgPool) -> Fx {
    schema(pool).await;
    let (d, store) = tmp_store();
    let org = seed_org(pool).await;
    let put = |rel: &str, bytes: &[u8]| {
        let p = store.uploads_dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    };
    let photo = photo_png(300, 200, 1);
    put(&format!("{org}/menu-items/a.png"), &photo);
    put(&format!("{org}/menu-items/b.png"), &photo); // identical bytes → deduped
    put(&format!("{org}/menu-items/bad.png"), b"\x89PNG\r\n\x1a\nnot really");
    let base = "https://api.example/uploads";
    let good = seed_item(pool, org, Some(&format!("{base}/{org}/menu-items/a.png"))).await;
    let dup = seed_item(pool, org, Some(&format!("{base}/{org}/menu-items/b.png"))).await;
    let missing = seed_item(pool, org, Some(&format!("{base}/{org}/menu-items/gone.png"))).await;
    let broken = seed_item(pool, org, Some(&format!("{base}/{org}/menu-items/bad.png"))).await;
    Fx { _d: d, store, org, good, dup, missing, broken }
}

fn opts(fx: &Fx, dry: bool) -> BackfillOptions {
    BackfillOptions { org: Some(fx.org), dry_run: dry, limit: None, run_id: Uuid::new_v4(), verify_only: false, store: fx.store.clone(), step_animations_dir: None }
}

async fn counts(pool: &PgPool) -> (i64, i64, i64) {
    let a: i64 = sqlx::query_scalar("SELECT count(*) FROM assets").fetch_one(pool).await.unwrap();
    let b: i64 = sqlx::query_scalar("SELECT count(*) FROM asset_backfill_items").fetch_one(pool).await.unwrap();
    let c: i64 = sqlx::query_scalar("SELECT count(*) FROM menu_items WHERE image_group_id IS NOT NULL").fetch_one(pool).await.unwrap();
    (a, b, c)
}

async fn status(pool: &PgPool, id: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM asset_backfill_items WHERE source_id=$1").bind(id.to_string()).fetch_one(pool).await.unwrap()
}

#[sqlx::test]
async fn backfill_dry_run_writes_nothing(pool: PgPool) {
    let fx = fixture(&pool).await;
    let rep = run(&pool, &opts(&fx, true)).await.unwrap();
    assert!(rep.dry_run);
    assert_eq!(rep.totals.discovered, 4);
    assert_eq!((rep.totals.ingested, rep.totals.missing, rep.totals.broken, rep.totals.deduped), (2, 1, 1, 1));
    assert!(rep.bytes.source > 0 && rep.bytes.stored > 0);
    assert_eq!(counts(&pool).await, (0, 0, 0));
    assert!(!fx.store.assets_dir.exists(), "no files written");
}

#[sqlx::test]
async fn backfill_marks_missing_and_broken_and_rerun_is_noop(pool: PgPool) {
    let fx = fixture(&pool).await;
    let rep = run(&pool, &opts(&fx, false)).await.unwrap();
    assert_eq!((rep.totals.verified, rep.totals.missing, rep.totals.broken, rep.totals.deduped), (2, 1, 1, 1));
    assert_eq!(status(&pool, fx.missing).await, "missing");
    assert_eq!(status(&pool, fx.broken).await, "broken");
    assert_eq!(rep.items_failed.len(), 2);
    let g: Vec<Option<Uuid>> = sqlx::query_scalar("SELECT image_group_id FROM menu_items WHERE id = ANY($1) ORDER BY id")
        .bind(vec![fx.good, fx.dup]).fetch_all(&pool).await.unwrap();
    assert!(g[0].is_some() && g[0] == g[1], "deduped to one group");
    // legacy URLs untouched, legacy paths mapped
    let url: String = sqlx::query_scalar("SELECT image_url FROM menu_items WHERE id=$1").bind(fx.good).fetch_one(&pool).await.unwrap();
    assert!(url.ends_with("/a.png"));
    let mapped: i64 = sqlx::query_scalar("SELECT count(*) FROM asset_legacy_paths").fetch_one(&pool).await.unwrap();
    assert_eq!(mapped, 2);

    let before = counts(&pool).await;
    let updated: Vec<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar("SELECT updated_at FROM asset_backfill_items ORDER BY source_id").fetch_all(&pool).await.unwrap();
    let rep2 = run(&pool, &opts(&fx, false)).await.unwrap();
    assert_eq!(counts(&pool).await, before, "backfill_rerun_is_noop");
    assert_eq!(rep2.totals, rep.totals);
    let updated2: Vec<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar("SELECT updated_at FROM asset_backfill_items WHERE status='verified' ORDER BY source_id").fetch_all(&pool).await.unwrap();
    assert!(updated2.iter().all(|u| updated.contains(u)), "verified items not touched again");
}

#[sqlx::test]
async fn backfill_resumes_after_kill(pool: PgPool) {
    let fx = fixture(&pool).await;
    // "kill" after one item: limit 1 processes a single pending item.
    let mut o = opts(&fx, false);
    o.limit = Some(1);
    run(&pool, &o).await.unwrap();
    let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM asset_backfill_items WHERE status='pending'").fetch_one(&pool).await.unwrap();
    assert_eq!(pending, 3);
    // simulate a crash between INGEST and VERIFY
    sqlx::query("UPDATE asset_backfill_items SET status='ingested' WHERE status='verified'").execute(&pool).await.unwrap();
    let rep = run(&pool, &opts(&fx, false)).await.unwrap();
    assert_eq!((rep.totals.verified, rep.totals.missing, rep.totals.broken), (2, 1, 1));
    let groups: i64 = sqlx::query_scalar("SELECT count(*) FROM asset_groups").fetch_one(&pool).await.unwrap();
    assert_eq!(groups, 1);
}

#[sqlx::test]
async fn backfill_never_clobbers_newer_upload(pool: PgPool) {
    let fx = fixture(&pool).await;
    let mut o = opts(&fx, false);
    o.limit = Some(0); // discover only
    run(&pool, &o).await.unwrap();
    // A new upload lands through the live pipeline before the backfill runs.
    let newer = super::ingest::ingest_bytes(&pool, &fx.store, Some(fx.org), super::ingest::AssetPurpose::MenuItemPhoto,
        photo_png(50, 50, 99), super::ingest::SourceKind::Upload, None, None).await.unwrap();
    sqlx::query("UPDATE menu_items SET image_group_id=$1 WHERE id=$2").bind(newer.group_id).bind(fx.good).execute(&pool).await.unwrap();
    run(&pool, &opts(&fx, false)).await.unwrap();
    let g: Option<Uuid> = sqlx::query_scalar("SELECT image_group_id FROM menu_items WHERE id=$1").bind(fx.good).fetch_one(&pool).await.unwrap();
    assert_eq!(g, Some(newer.group_id));
    assert_eq!(status(&pool, fx.good).await, "skipped");
}

#[sqlx::test]
async fn backfill_prune_requires_verified_run(pool: PgPool) {
    let fx = fixture(&pool).await;
    let unknown = Uuid::new_v4();
    assert!(prune(&pool, &fx.store, Some(fx.org), unknown, false).await.is_err());
    let o = opts(&fx, false);
    run(&pool, &o).await.unwrap();
    // broken item in scope → refused without --allow-partial
    assert!(prune(&pool, &fx.store, Some(fx.org), o.run_id, false).await.is_err());
    let a = fx.store.uploads_dir.join(format!("{}/menu-items/a.png", fx.org));
    assert!(a.exists());
    let rep = prune(&pool, &fx.store, Some(fx.org), o.run_id, true).await.unwrap();
    assert_eq!(rep.deleted, 2);
    assert!(!a.exists());
    assert!(fx.store.uploads_dir.join(format!("{}/menu-items/bad.png", fx.org)).exists(), "unverified originals kept");
}

#[sqlx::test]
async fn encoder_version_bump_creates_new_group_on_backfill(pool: PgPool) {
    let fx = fixture(&pool).await;
    run(&pool, &opts(&fx, false)).await.unwrap();
    // Pretend the existing group was made by an older encoder.
    sqlx::query("UPDATE asset_groups SET encoder = 'madar-ingest/0 old'").execute(&pool).await.unwrap();
    sqlx::query("UPDATE assets SET encoder = 'madar-ingest/0 old', hash = md5(hash) || md5(hash || 'x')").execute(&pool).await.unwrap();
    sqlx::query("UPDATE asset_backfill_items SET status='pending' WHERE status='verified'").execute(&pool).await.unwrap();
    sqlx::query("UPDATE menu_items SET image_group_id = NULL").execute(&pool).await.unwrap();
    run(&pool, &opts(&fx, false)).await.unwrap();
    let groups: i64 = sqlx::query_scalar("SELECT count(*) FROM asset_groups").fetch_one(&pool).await.unwrap();
    assert_eq!(groups, 2, "new encoder → new group; old rows remain valid");
}
