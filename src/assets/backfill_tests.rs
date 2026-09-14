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

// ── Shared content-addressed files and per-profile groups ───────────────────

/// Re-encode the same pixels with other PNG settings: different file
/// bytes (different source_hash), identical decoded pixels.
fn same_pixels_other_bytes(png: &[u8]) -> Vec<u8> {
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    let img = image::load_from_memory(png).unwrap().to_rgba8();
    let mut out = Vec::new();
    image::ImageEncoder::write_image(PngEncoder::new_with_quality(&mut out, CompressionType::Best, FilterType::Sub),
        img.as_raw(), img.width(), img.height(), image::ExtendedColorType::Rgba8).unwrap();
    out
}

fn put(store: &AssetStore, rel: &str, bytes: &[u8]) {
    let p = store.uploads_dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, bytes).unwrap();
}

async fn set_org_images(pool: &PgPool, org: Uuid, logo: Option<&str>, card: Option<&str>) {
    sqlx::query("UPDATE organizations SET logo_url = $2, brand_card_image = $3 WHERE id = $1")
        .bind(org).bind(logo).bind(card).execute(pool).await.unwrap();
}

/// Files under `<org>/` on disk vs the distinct (hash, ext) the rows reference.
async fn files_vs_rows(pool: &PgPool, store: &AssetStore, org: Uuid) -> (Vec<String>, Vec<String>) {
    let mut disk: Vec<String> = std::fs::read_dir(store.assets_dir.join(org.to_string()))
        .map(|d| d.filter_map(|e| e.ok()).filter(|e| e.path().is_file()).map(|e| e.file_name().to_string_lossy().to_string()).collect())
        .unwrap_or_default();
    disk.sort();
    let rows: Vec<String> = sqlx::query_scalar("SELECT DISTINCT hash || '.' || ext FROM assets WHERE org_id = $1 ORDER BY 1")
        .bind(org).fetch_all(pool).await.unwrap();
    (disk, rows)
}

async fn group_of(pool: &PgPool, sql: &str, id: Uuid) -> Uuid {
    sqlx::query_scalar::<_, Option<Uuid>>(sql).bind(id).fetch_one(pool).await.unwrap().expect("group attached")
}

async fn variants_of(pool: &PgPool, g: Uuid) -> Vec<String> {
    sqlx::query_scalar("SELECT variant FROM assets WHERE group_id = $1 ORDER BY variant").bind(g).fetch_all(pool).await.unwrap()
}

fn opts_org(store: &AssetStore, org: Uuid) -> BackfillOptions {
    BackfillOptions { org: Some(org), dry_run: false, limit: None, run_id: Uuid::new_v4(), verify_only: false, store: store.clone(), step_animations_dir: None }
}

#[sqlx::test]
async fn backfill_logo_with_same_pixels_as_menu_photo_but_other_bytes(pool: PgPool) {
    let (_d, store) = tmp_store();
    let org = seed_org(&pool).await;
    let photo = photo_png(600, 400, 21);
    let logo = same_pixels_other_bytes(&photo);
    assert_ne!(photo, logo);
    assert_eq!(image::load_from_memory(&photo).unwrap().to_rgba8(), image::load_from_memory(&logo).unwrap().to_rgba8());
    put(&store, &format!("{org}/menu-items/p.png"), &photo);
    put(&store, "logos/l.png", &logo);
    put(&store, "card/c.png", &logo);
    let base = "https://api.example/uploads";
    let item = seed_item(&pool, org, Some(&format!("{base}/{org}/menu-items/p.png"))).await;
    set_org_images(&pool, org, Some(&format!("{base}/logos/l.png")), Some(&format!("{base}/card/c.png"))).await;

    let rep = run(&pool, &opts_org(&store, org)).await.unwrap();
    assert_eq!((rep.totals.verified, rep.totals.failed, rep.totals.broken, rep.totals.skipped), (3, 0, 0, 0), "{:?}", rep.items_failed);

    let pg = group_of(&pool, "SELECT image_group_id FROM menu_items WHERE id=$1", item).await;
    let lg = group_of(&pool, "SELECT logo_group_id FROM organizations WHERE id=$1", org).await;
    let cg = group_of(&pool, "SELECT brand_card_image_group_id FROM organizations WHERE id=$1", org).await;
    assert_ne!(pg, lg, "logo never reuses the photo group");
    assert_eq!(lg, cg, "logo and card share the keeps_original group");
    assert!(variants_of(&pool, lg).await.contains(&"original".to_string()));
    assert!(!variants_of(&pool, pg).await.contains(&"original".to_string()));
    // The lossy `full` of both groups is one shared file.
    let shared: i64 = sqlx::query_scalar("SELECT count(*) FROM (SELECT hash FROM assets WHERE org_id=$1 GROUP BY hash HAVING count(DISTINCT group_id) > 1) s")
        .bind(org).fetch_one(&pool).await.unwrap();
    assert!(shared >= 1, "the colliding variant is stored once and referenced twice");
    let (disk, rows) = files_vs_rows(&pool, &store, org).await;
    assert_eq!(disk, rows, "no orphan files, no missing files");
    // Stored bytes count each shared file once.
    let on_disk: i64 = disk.iter().map(|f| std::fs::metadata(store.assets_dir.join(org.to_string()).join(f)).unwrap().len() as i64).sum();
    assert_eq!(rep.bytes.stored, on_disk);
}

#[sqlx::test]
async fn backfill_same_file_as_menu_photo_and_logo_gets_two_profiles(pool: PgPool) {
    let (_d, store) = tmp_store();
    let org = seed_org(&pool).await;
    let bytes = photo_png(500, 500, 22);
    put(&store, &format!("{org}/menu-items/same.png"), &bytes);
    put(&store, "logos/same.png", &bytes);
    put(&store, "card/same.png", &bytes);
    let base = "https://api.example/uploads";
    let item = seed_item(&pool, org, Some(&format!("{base}/{org}/menu-items/same.png"))).await;
    set_org_images(&pool, org, Some(&format!("{base}/logos/same.png")), Some(&format!("{base}/card/same.png"))).await;

    let rep = run(&pool, &opts_org(&store, org)).await.unwrap();
    assert_eq!((rep.totals.verified, rep.totals.failed), (3, 0), "{:?}", rep.items_failed);
    let pg = group_of(&pool, "SELECT image_group_id FROM menu_items WHERE id=$1", item).await;
    let lg = group_of(&pool, "SELECT logo_group_id FROM organizations WHERE id=$1", org).await;
    let cg = group_of(&pool, "SELECT brand_card_image_group_id FROM organizations WHERE id=$1", org).await;
    assert_ne!(pg, lg);
    assert_eq!(lg, cg);
    for g in [lg, cg] {
        assert!(variants_of(&pool, g).await.contains(&"original".to_string()), "logo/card keep an original");
    }
    let profiles: Vec<(Uuid, String)> = sqlx::query_as("SELECT id, profile FROM asset_groups WHERE org_id=$1").bind(org).fetch_all(&pool).await.unwrap();
    assert_eq!(profiles.len(), 2);
    assert!(profiles.contains(&(pg, "photo".into())) && profiles.contains(&(lg, "keeps_original".into())));
    let (disk, rows) = files_vs_rows(&pool, &store, org).await;
    assert_eq!(disk, rows);
}

#[sqlx::test]
async fn ingest_different_images_with_byte_identical_thumbnails(pool: PgPool) {
    use super::ingest::{AssetPurpose, SourceKind, ingest_bytes};
    let (_d, store) = tmp_store();
    let org = seed_org(&pool).await;
    let make = |dot: bool| {
        let mut img = image::RgbaImage::from_pixel(4000, 2000, image::Rgba([200, 180, 160, 255]));
        if dot {
            for y in 1000..1002 { for x in 2000..2002 { img.put_pixel(x, y, image::Rgba([0, 0, 0, 255])); } }
        }
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img).write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    };
    let a = ingest_bytes(&pool, &store, Some(org), AssetPurpose::MenuItemPhoto, make(false), SourceKind::Upload, None, None).await.unwrap();
    let b = ingest_bytes(&pool, &store, Some(org), AssetPurpose::MenuItemPhoto, make(true), SourceKind::Upload, None, None).await.unwrap();
    assert_ne!(a.group_id, b.group_id);
    assert!(!b.deduped);
    assert_eq!(a.variant("thumb").unwrap().hash, b.variant("thumb").unwrap().hash, "precondition: identical thumbnails");
    assert_ne!(a.variant("full").unwrap().hash, b.variant("full").unwrap().hash, "precondition: different images");
    assert_eq!((a.variants.len(), b.variants.len()), (3, 3));
    let (disk, rows) = files_vs_rows(&pool, &store, org).await;
    assert_eq!(disk, rows);
    assert!(rows.len() < 6, "the shared thumbnail is one file");
}

#[sqlx::test]
async fn failed_ingest_attempt_leaves_no_orphan_and_keeps_shared_files(pool: PgPool) {
    use super::ingest::{AssetPurpose, FAIL_BEFORE_COMMIT, SourceKind, ingest_bytes};
    let (_d, store) = tmp_store();
    let org = seed_org(&pool).await;
    let photo = photo_png(600, 400, 23);
    ingest_bytes(&pool, &store, Some(org), AssetPurpose::MenuItemPhoto, photo.clone(), SourceKind::Upload, None, None).await.unwrap();
    let (before, _) = files_vs_rows(&pool, &store, org).await;

    FAIL_BEFORE_COMMIT.lock().unwrap().push(org);
    let logo = same_pixels_other_bytes(&photo);
    for _ in 0..3 {
        let r = ingest_bytes(&pool, &store, Some(org), AssetPurpose::OrgLogo, logo.clone(), SourceKind::Backfill, None, None).await;
        assert!(r.is_err());
        let (disk, rows) = files_vs_rows(&pool, &store, org).await;
        assert_eq!(disk, before, "the attempt's own files are gone, the shared ones stay");
        assert_eq!(disk, rows);
    }
    FAIL_BEFORE_COMMIT.lock().unwrap().retain(|o| *o != org);
    let ok = ingest_bytes(&pool, &store, Some(org), AssetPurpose::OrgLogo, logo, SourceKind::Backfill, None, None).await.unwrap();
    assert!(ok.variant("original").is_some());
    let (disk, rows) = files_vs_rows(&pool, &store, org).await;
    assert_eq!(disk, rows);
    assert!(disk.len() > before.len());
}

#[sqlx::test]
async fn backfill_with_shared_files_rerun_is_noop(pool: PgPool) {
    let (_d, store) = tmp_store();
    let org = seed_org(&pool).await;
    let photo = photo_png(600, 400, 24);
    let other = same_pixels_other_bytes(&photo);
    put(&store, &format!("{org}/menu-items/p.png"), &photo);
    put(&store, &format!("{org}/menu-items/q.png"), &other); // same pixels → pixel dedup
    put(&store, "logos/l.png", &photo); // same file as a photo → own profile
    put(&store, "card/c.png", &other);
    let base = "https://api.example/uploads";
    seed_item(&pool, org, Some(&format!("{base}/{org}/menu-items/p.png"))).await;
    seed_item(&pool, org, Some(&format!("{base}/{org}/menu-items/q.png"))).await;
    set_org_images(&pool, org, Some(&format!("{base}/logos/l.png")), Some(&format!("{base}/card/c.png"))).await;

    let rep = run(&pool, &opts_org(&store, org)).await.unwrap();
    assert_eq!((rep.totals.verified, rep.totals.failed, rep.totals.deduped), (4, 0, 2), "{:?}", rep.items_failed);
    let groups: i64 = sqlx::query_scalar("SELECT count(*) FROM asset_groups").fetch_one(&pool).await.unwrap();
    assert_eq!(groups, 2);
    let snapshot = |pool: PgPool| async move {
        let rows: Vec<(String, String, Option<Uuid>, bool, Option<i64>)> = sqlx::query_as(
            "SELECT source_id, status, group_id, deduped, stored_bytes FROM asset_backfill_items ORDER BY source_table, source_id, source_field")
            .fetch_all(&pool).await.unwrap();
        let assets: Vec<(Uuid, String, String)> = sqlx::query_as("SELECT group_id, variant, hash FROM assets ORDER BY 1, 2").fetch_all(&pool).await.unwrap();
        (rows, assets)
    };
    let before = snapshot(pool.clone()).await;
    let files_before = files_vs_rows(&pool, &store, org).await;
    let second = opts_org(&store, org);
    let rep2 = run(&pool, &second).await.unwrap();
    assert_eq!(rep2.totals, rep.totals);
    assert_eq!(rep2.bytes, rep.bytes);
    assert_eq!(snapshot(pool.clone()).await, before);
    assert_eq!(files_vs_rows(&pool, &store, org).await, files_before);
    // The no-op run verified nothing; the first run is the one to prune with.
    let runs = super::backfill::verified_runs(&pool, Some(org)).await.unwrap();
    let first_id = sqlx::query_scalar::<_, Uuid>("SELECT DISTINCT run_id FROM asset_backfill_items WHERE status='verified'").fetch_one(&pool).await.unwrap();
    assert_eq!(runs, vec![(first_id, 4)]);
    assert_ne!(first_id, second.run_id);
}

#[sqlx::test]
async fn prune_keeps_a_legacy_file_another_unverified_item_uses(pool: PgPool) {
    let (_d, store) = tmp_store();
    let org = seed_org(&pool).await;
    let bytes = photo_png(300, 300, 25);
    put(&store, &format!("{org}/menu-items/s.png"), &bytes);
    let url = format!("https://api.example/uploads/{org}/menu-items/s.png");
    seed_item(&pool, org, Some(&url)).await;
    set_org_images(&pool, org, Some(&url), None).await;
    let o = opts_org(&store, org);
    run(&pool, &o).await.unwrap();
    sqlx::query("UPDATE asset_backfill_items SET status='failed' WHERE source_table='organizations'").execute(&pool).await.unwrap();
    let rep = prune(&pool, &store, Some(org), o.run_id, true).await.unwrap();
    assert_eq!(rep.deleted, 0);
    assert!(store.uploads_dir.join(format!("{org}/menu-items/s.png")).exists());
}

#[sqlx::test]
async fn small_transparent_logo_keeps_an_original_row_sharing_the_full_file(pool: PgPool) {
    use super::ingest::{AssetPurpose, SourceKind, ingest_bytes};
    let (_d, store) = tmp_store();
    let org = seed_org(&pool).await;
    let mut img = image::RgbaImage::from_pixel(200, 100, image::Rgba([0, 0, 0, 0]));
    for x in 50..150 { for y in 25..75 { img.put_pixel(x, y, image::Rgba([10, 120, 60, 255])); } }
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img).write_to(&mut buf, image::ImageFormat::Png).unwrap();
    let o = ingest_bytes(&pool, &store, Some(org), AssetPurpose::OrgLogo, buf.into_inner(), SourceKind::Backfill, None, None).await.unwrap();
    let (full, orig) = (o.variant("full").unwrap(), o.variant("original").expect("original row"));
    assert_eq!(full.hash, orig.hash, "precondition: lossless full == original");
    assert_ne!(full.id, orig.id);
    let (disk, rows) = files_vs_rows(&pool, &store, org).await;
    assert_eq!(disk, rows);
}
