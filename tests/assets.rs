#![allow(clippy::too_many_arguments)]
//! Track B4 tests (contract §11.8 + §11.10).

use std::io::Cursor;
use std::time::Duration;

use actix_web::{App, http::StatusCode, test, web};
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::assets::AssetStore;
use madar_rust::assets::ingest::*;
use madar_rust::auth::jwt::{JwtSecret, create_token};
use madar_rust::models::UserRole;

pub(crate) async fn schema(pool: &PgPool) {
    // Schema comes from B1's migration (20260914090400_assets.sql).
    let _ = pool;
}

pub(crate) fn tmp_store() -> (tempfile::TempDir, AssetStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = AssetStore::new(dir.path().join("assets"), dir.path().join("uploads"));
    std::fs::create_dir_all(&store.uploads_dir).unwrap();
    (dir, store)
}

pub(crate) async fn seed_org(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org', $2)")
        .bind(id)
        .bind(format!("o-{}", &id.to_string()[..8]))
        .execute(pool)
        .await
        .unwrap();
    id
}

pub(crate) async fn seed_branch(pool: &PgPool, org: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'B')")
        .bind(id)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    id
}

pub(crate) async fn seed_item(pool: &PgPool, org: Uuid, image_url: Option<&str>) -> Uuid {
    let cat = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(cat)
        .bind(org)
        .bind(format!("C-{cat}"))
        .execute(pool)
        .await
        .unwrap();
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO menu_items (id, org_id, category_id, name, base_price, is_active, image_url) VALUES ($1,$2,$3,'I',100,true,$4)")
        .bind(id)
        .bind(org)
        .bind(cat)
        .bind(image_url)
        .execute(pool)
        .await
        .unwrap();
    id
}

pub(crate) fn photo_png(w: u32, h: u32, seed: u32) -> Vec<u8> {
    let mut img = RgbaImage::new(w, h);
    for (x, y, p) in img.enumerate_pixels_mut() {
        *p = Rgba([
            ((x * 7 + y * 3 + seed) % 256) as u8,
            ((x * 3 + y * 11) % 256) as u8,
            ((x * 13 + y * 5 + seed * 3) % 256) as u8,
            255,
        ]);
    }
    let mut buf = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(img)
        .write_to(&mut buf, ImageFormat::Png)
        .unwrap();
    buf.into_inner()
}

fn transparent_png(size: u32) -> Vec<u8> {
    let mut img = RgbaImage::from_pixel(size, size, Rgba([0, 0, 0, 0]));
    for y in size / 4..size * 3 / 4 {
        for x in size / 4..size * 3 / 4 {
            img.put_pixel(x, y, Rgba([20, 40, 200, 255]));
        }
    }
    let mut buf = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(img)
        .write_to(&mut buf, ImageFormat::Png)
        .unwrap();
    buf.into_inner()
}

const LOTTIE: &[u8] = br#"{"v":"5.7.4","w":200,"h":100,"layers":[{"ty":4}]}"#;

fn secret() -> JwtSecret {
    JwtSecret("secret".into())
}
fn token(org: Option<Uuid>, role: UserRole) -> String {
    create_token(&secret(), Uuid::new_v4(), org, role, None, 1).unwrap()
}

/// A token for a REAL owner of `org`. The POS asset feed and the job route
/// refuse a person who works nowhere, which a made-up user id is.
async fn owner_token(pool: &PgPool, org: Uuid) -> String {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, role, email, password_hash)
         VALUES ($1, $2, 'Owner', 'org_admin'::user_role, $3, 'h')",
    )
    .bind(id)
    .bind(org)
    .bind(format!("{id}@assets.test"))
    .execute(pool)
    .await
    .unwrap();
    create_token(&secret(), id, Some(org), UserRole::OrgAdmin, None, 1).unwrap()
}

async fn ing(
    pool: &PgPool,
    store: &AssetStore,
    org: Option<Uuid>,
    p: AssetPurpose,
    raw: &[u8],
) -> Result<IngestOutcome, madar_rust::errors::AppError> {
    ingest_bytes(
        pool,
        store,
        org,
        p,
        raw.to_vec(),
        SourceKind::Upload,
        Some("x.png"),
        None,
    )
    .await
}

fn rel_url(u: &str) -> String {
    u[u.find("/assets/").unwrap()..].to_string()
}

// ── sniff / stage ───────────────────────────────────────────────────────────

#[sqlx::test]
async fn stage_rejects_non_image_by_magic_bytes_ignoring_client_mime(pool: PgPool) {
    schema(&pool).await;
    let (_d, store) = tmp_store();
    let org = seed_org(&pool).await;
    let item = seed_item(&pool, org, None).await;
    let target = AssetTarget::new(AssetTable::MenuItems, item, AssetField::Image);
    for bad in [
        &b"<html>not a png</html>"[..],
        &b"%PDF-1.4 ..."[..],
        &[0u8; 64][..],
    ] {
        let r = stage_with(
            &pool,
            &store,
            Some(org),
            AssetPurpose::MenuItemPhoto,
            IngestSource::Bytes(bytes::Bytes::copy_from_slice(bad)),
            Some("photo.png"),
            target,
            None,
        )
        .await;
        assert!(
            matches!(r, Err(madar_rust::errors::AppError::BadRequest(_))),
            "{r:?}"
        );
    }
    // Lottie JSON is not a menu photo either.
    let r = stage_with(
        &pool,
        &store,
        Some(org),
        AssetPurpose::MenuItemPhoto,
        IngestSource::Bytes(bytes::Bytes::from_static(LOTTIE)),
        None,
        target,
        None,
    )
    .await;
    assert!(r.is_err());
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM asset_jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0);

    // Through HTTP with a lying image/png part → 400.
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret()))
            .configure(madar_rust::uploads::routes::configure),
    )
    .await;
    let b = "XBOUNDARY";
    let mut body = format!("--{b}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n").into_bytes();
    body.extend_from_slice(b"definitely text");
    body.extend_from_slice(format!("\r\n--{b}--\r\n").as_bytes());
    let req = test::TestRequest::post()
        .uri(&format!("/uploads/menu-items/{item}"))
        .insert_header((
            "Authorization",
            format!("Bearer {}", token(None, UserRole::SuperAdmin)),
        ))
        .insert_header(("Content-Type", format!("multipart/form-data; boundary={b}")))
        .set_payload(body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test]
async fn stage_never_uses_client_filename_in_path(pool: PgPool) {
    schema(&pool).await;
    let (_d, store) = tmp_store();
    let org = seed_org(&pool).await;
    let item = seed_item(&pool, org, None).await;
    let evil = "../../etc/passwd\u{0007}.png";
    let job = stage_with(
        &pool,
        &store,
        Some(org),
        AssetPurpose::MenuItemPhoto,
        IngestSource::Bytes(photo_png(20, 20, 1).into()),
        Some(evil),
        AssetTarget::new(AssetTable::MenuItems, item, AssetField::Image),
        None,
    )
    .await
    .unwrap();
    let (path, label): (String, Option<String>) =
        sqlx::query_as("SELECT staged_path, label FROM asset_jobs WHERE id=$1")
            .bind(job)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(path.ends_with(&format!("_staging/{job}")), "{path}");
    assert!(!path.contains("passwd"));
    let label = label.unwrap();
    assert!(
        !label.contains('/') && !label.contains('\u{0007}'),
        "{label}"
    );
    madar_rust::assets::worker::run_one(&pool, &store).await.unwrap();
    let keys: Vec<String> = sqlx::query_scalar("SELECT hash || '.' || ext FROM assets")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(keys.iter().all(|k| !k.contains("passwd")));
}

// ── ingest ──────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn ingest_generates_thumb_tile_full_with_bounds(pool: PgPool) {
    schema(&pool).await;
    let (_d, store) = tmp_store();
    let org = seed_org(&pool).await;
    let o = ing(
        &pool,
        &store,
        Some(org),
        AssetPurpose::MenuItemPhoto,
        &photo_png(2400, 1200, 2),
    )
    .await
    .unwrap();
    let dims = |v: &str| {
        let a = o.variant(v).unwrap();
        (a.width.unwrap(), a.height.unwrap())
    };
    assert_eq!(dims("thumb"), (128, 64));
    assert_eq!(dims("tile"), (512, 256));
    assert_eq!(dims("full"), (1600, 800));
    assert!(o.variant("original").is_none(), "photos keep no original");
    for v in &o.variants {
        assert_eq!(v.ext, "webp");
        let bytes = std::fs::read(store.path_for_key(&AssetStore::key(Some(org), &v.hash, "webp")))
            .unwrap();
        let img = image::load_from_memory(&bytes).unwrap();
        assert_eq!(
            (img.width() as i32, img.height() as i32),
            (v.width.unwrap(), v.height.unwrap())
        );
    }
    // never upscaled
    let small = ing(
        &pool,
        &store,
        Some(org),
        AssetPurpose::MenuItemPhoto,
        &photo_png(100, 50, 3),
    )
    .await
    .unwrap();
    assert_eq!(small.variant("full").unwrap().width, Some(100));
}

#[sqlx::test]
async fn ingest_original_kept_only_for_logo_and_card(pool: PgPool) {
    schema(&pool).await;
    let (_d, store) = tmp_store();
    let org = seed_org(&pool).await;
    for (i, (p, keep)) in [
        (AssetPurpose::OrgLogo, true),
        (AssetPurpose::LoyaltyCardImage, true),
        (AssetPurpose::MenuItemPhoto, false),
        (AssetPurpose::BundlePhoto, false),
    ]
    .into_iter()
    .enumerate()
    {
        let o = ing(
            &pool,
            &store,
            Some(org),
            p,
            &photo_png(300, 300, 40 + i as u32),
        )
        .await
        .unwrap();
        assert_eq!(o.variant("original").is_some(), keep, "{p:?}");
    }
}

#[sqlx::test]
async fn ingest_transparent_png_lossless_webp(pool: PgPool) {
    schema(&pool).await;
    let (_d, store) = tmp_store();
    let org = seed_org(&pool).await;
    let o = ing(
        &pool,
        &store,
        Some(org),
        AssetPurpose::OrgLogo,
        &transparent_png(256),
    )
    .await
    .unwrap();
    let full = o.variant("full").unwrap();
    assert!(full.has_alpha);
    let bytes =
        std::fs::read(store.path_for_key(&AssetStore::key(Some(org), &full.hash, "webp"))).unwrap();
    assert!(bytes.windows(4).any(|w| w == b"VP8L"), "lossless bitstream");
    let img = image::load_from_memory(&bytes).unwrap().to_rgba8();
    assert_eq!(img.get_pixel(0, 0).0[3], 0);
    assert_eq!(img.get_pixel(128, 128).0, [20, 40, 200, 255]);
}

#[sqlx::test]
async fn ingest_strips_exif_and_icc(pool: PgPool) {
    schema(&pool).await;
    let (_d, store) = tmp_store();
    let org = seed_org(&pool).await;
    // JPEG with an APP1 EXIF segment and an APP2 ICC segment carrying marker text.
    let mut jpeg = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(
        image::load_from_memory(&photo_png(64, 64, 5))
            .unwrap()
            .to_rgba8(),
    )
    .to_rgb8()
    .write_to(&mut jpeg, ImageFormat::Jpeg)
    .unwrap();
    let jpeg = jpeg.into_inner();
    let seg = |marker: u8, payload: &[u8]| {
        let mut v = vec![0xff, marker];
        v.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        v.extend_from_slice(payload);
        v
    };
    let mut raw = vec![0xff, 0xd8];
    raw.extend(seg(0xe1, b"Exif\0\0II*\0\x08\0\0\0GPS-SECRET-LOCATION"));
    raw.extend(seg(0xe2, b"ICC_PROFILE\0\x01\x01ICC-SECRET-PROFILE"));
    raw.extend_from_slice(&jpeg[2..]);
    let o = ing(&pool, &store, Some(org), AssetPurpose::MenuItemPhoto, &raw)
        .await
        .unwrap();
    for v in &o.variants {
        let b = std::fs::read(store.path_for_key(&AssetStore::key(Some(org), &v.hash, "webp")))
            .unwrap();
        for needle in [&b"GPS-SECRET"[..], b"ICC-SECRET", b"EXIF", b"ICCP", b"XMP "] {
            assert!(
                !b.windows(needle.len()).any(|w| w == needle),
                "{} leaked",
                String::from_utf8_lossy(needle)
            );
        }
    }
}

#[sqlx::test]
async fn ingest_rejects_animated_gif(pool: PgPool) {
    schema(&pool).await;
    let (_d, store) = tmp_store();
    let org = seed_org(&pool).await;
    let mut raw = Cursor::new(Vec::new());
    {
        let mut enc = image::codecs::gif::GifEncoder::new(&mut raw);
        for s in [0u8, 255] {
            enc.encode_frame(image::Frame::new(RgbaImage::from_pixel(
                4,
                4,
                Rgba([s, s, s, 255]),
            )))
            .unwrap();
        }
    }
    let r = ing(
        &pool,
        &store,
        Some(org),
        AssetPurpose::MenuItemPhoto,
        &raw.into_inner(),
    )
    .await;
    assert!(matches!(r, Err(madar_rust::errors::AppError::BadRequest(ref m)) if m.contains("Animated")));
}

#[sqlx::test]
async fn ingest_lottie_validates_and_zstd(pool: PgPool) {
    schema(&pool).await;
    let (_d, store) = tmp_store();
    let o = ing(&pool, &store, None, AssetPurpose::StepAnimation, LOTTIE)
        .await
        .unwrap();
    assert_eq!(o.variants.len(), 1);
    let a = &o.variants[0];
    assert_eq!(
        (a.variant.as_str(), a.ext.as_str(), a.content_type.as_str()),
        ("animation", "lottie.zst", "application/zstd")
    );
    assert_eq!((a.width, a.height), (Some(200), Some(100)));
    let path = store.path_for_key(&format!("global/{}.lottie.zst", a.hash));
    assert_eq!(
        zstd::decode_all(&std::fs::read(path).unwrap()[..]).unwrap(),
        LOTTIE
    );
    assert!(
        ing(
            &pool,
            &store,
            None,
            AssetPurpose::StepAnimation,
            br#"{"hello":1}"#
        )
        .await
        .is_err()
    );
    assert!(
        ing(
            &pool,
            &store,
            None,
            AssetPurpose::StepAnimation,
            &photo_png(8, 8, 1)
        )
        .await
        .is_err()
    );
    assert!(
        ing(
            &pool,
            &store,
            None,
            AssetPurpose::MenuItemPhoto,
            &photo_png(8, 8, 1)
        )
        .await
        .is_err(),
        "images need an org"
    );
}

#[sqlx::test]
async fn ingest_hash_is_sha256_of_final_bytes(pool: PgPool) {
    schema(&pool).await;
    let (_d, store) = tmp_store();
    let org = seed_org(&pool).await;
    let raw = photo_png(700, 300, 6);
    let o = ing(&pool, &store, Some(org), AssetPurpose::MenuItemPhoto, &raw)
        .await
        .unwrap();
    let source_hash: String =
        sqlx::query_scalar("SELECT DISTINCT source_hash FROM assets WHERE group_id=$1")
            .bind(o.group_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(source_hash, madar_rust::assets::sha256_hex(&raw));
    for v in &o.variants {
        let b = std::fs::read(store.path_for_key(&AssetStore::key(Some(org), &v.hash, &v.ext)))
            .unwrap();
        assert_eq!(madar_rust::assets::sha256_hex(&b), v.hash);
        assert_eq!(b.len() as i64, v.bytes);
        assert_ne!(v.hash, source_hash);
    }
}

#[sqlx::test]
async fn encoder_settings_recorded(pool: PgPool) {
    schema(&pool).await;
    let (_d, store) = tmp_store();
    let org = seed_org(&pool).await;
    let o = ing(
        &pool,
        &store,
        Some(org),
        AssetPurpose::MenuItemPhoto,
        &photo_png(900, 900, 7),
    )
    .await
    .unwrap();
    let rows: Vec<(String, String, serde_json::Value)> =
        sqlx::query_as("SELECT variant, encoder, encoder_settings FROM assets WHERE group_id=$1")
            .bind(o.group_id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 3);
    for (variant, encoder, s) in rows {
        assert!(
            encoder.starts_with(ENCODER_VERSION) && encoder.contains("libwebp-"),
            "{encoder}"
        );
        assert_eq!(s["quality"], 80);
        assert_eq!(s["method"], 6);
        assert_eq!(s["filter"], "lanczos3");
        assert!(s["libwebp_version"].as_str().unwrap().contains('.'));
        let edge = match variant.as_str() {
            "thumb" => 128,
            "tile" => 512,
            _ => 1600,
        };
        assert_eq!(s["max_edge"], edge);
    }
}

#[sqlx::test]
async fn ingest_dedups_within_org(pool: PgPool) {
    schema(&pool).await;
    let (_d, store) = tmp_store();
    let org = seed_org(&pool).await;
    let raw = photo_png(900, 900, 8);
    let a = ing(&pool, &store, Some(org), AssetPurpose::MenuItemPhoto, &raw)
        .await
        .unwrap();
    let b = ing(&pool, &store, Some(org), AssetPurpose::CategoryPhoto, &raw)
        .await
        .unwrap();
    assert!(!a.deduped && b.deduped);
    assert_eq!(a.group_id, b.group_id);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM assets")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 3);
}

#[sqlx::test]
async fn ingest_source_hash_dedup_skips_conversion(pool: PgPool) {
    schema(&pool).await;
    let (_d, store) = tmp_store();
    let org = seed_org(&pool).await;
    let raw = photo_png(300, 300, 9);
    let a = ing(&pool, &store, Some(org), AssetPurpose::MenuItemPhoto, &raw)
        .await
        .unwrap();
    // Remove the stored files: a dedup hit must not regenerate (it never converts).
    for v in &a.variants {
        std::fs::remove_file(store.path_for_key(&AssetStore::key(Some(org), &v.hash, &v.ext)))
            .unwrap();
    }
    let b = ing(&pool, &store, Some(org), AssetPurpose::MenuItemPhoto, &raw)
        .await
        .unwrap();
    assert!(b.deduped);
    for v in &b.variants {
        assert!(
            !store
                .path_for_key(&AssetStore::key(Some(org), &v.hash, &v.ext))
                .exists()
        );
    }
}

#[sqlx::test]
async fn ingest_same_bytes_other_org_creates_separate_row_and_file(pool: PgPool) {
    schema(&pool).await;
    let (_d, store) = tmp_store();
    let (o1, o2) = (seed_org(&pool).await, seed_org(&pool).await);
    let raw = photo_png(200, 200, 10);
    let a = ing(&pool, &store, Some(o1), AssetPurpose::MenuItemPhoto, &raw)
        .await
        .unwrap();
    let b = ing(&pool, &store, Some(o2), AssetPurpose::MenuItemPhoto, &raw)
        .await
        .unwrap();
    assert!(!b.deduped, "never deduped across orgs");
    assert_ne!(a.group_id, b.group_id);
    let ta = a.variant("tile").unwrap();
    let tb = b.variant("tile").unwrap();
    assert_eq!(ta.hash, tb.hash);
    assert_ne!(ta.id, tb.id);
    assert!(
        store
            .path_for_key(&format!("{o1}/{}.webp", ta.hash))
            .exists()
    );
    assert!(
        store
            .path_for_key(&format!("{o2}/{}.webp", tb.hash))
            .exists()
    );
    // read_asset_bytes is org-scoped.
    assert!(
        read_asset_bytes_with(&pool, &store, Some(o2), ta.id)
            .await
            .is_err()
    );
    assert!(
        read_asset_bytes_with(&pool, &store, Some(o1), ta.id)
            .await
            .is_ok()
    );
}

#[::core::prelude::v1::test]
fn ingest_url_blocks_private_ips_and_http() {
    let bad = [
        "http://example.com/a.png",
        "https://127.0.0.1/a.png",
        "https://10.1.2.3/a",
        "https://192.168.0.1/a",
        "https://169.254.169.254/latest",
        "https://[::1]/a",
        "https://[fd00::1]/a",
        "https://localhost/a",
        "https://100.64.0.1/a",
        "https://u:p@example.com/a",
        "ftp://example.com/a",
    ];
    for u in bad {
        assert!(
            check_url_syntax(&url::Url::parse(u).unwrap()).is_err(),
            "{u}"
        );
    }
    assert!(check_url_syntax(&url::Url::parse("https://cdn.example.com/a.png").unwrap()).is_ok());
    assert!(!is_public_ip("::ffff:10.0.0.1".parse().unwrap()));
    assert!(is_public_ip("8.8.8.8".parse().unwrap()));
}

#[sqlx::test]
async fn attach_never_overwrites_existing_legacy_url(pool: PgPool) {
    schema(&pool).await;
    let (_d, store) = tmp_store();
    let org = seed_org(&pool).await;
    let legacy = "https://api.example/uploads/x/menu-items/old.jpg";
    let with_url = seed_item(&pool, org, Some(legacy)).await;
    let without = seed_item(&pool, org, None).await;
    let o = ing(
        &pool,
        &store,
        Some(org),
        AssetPurpose::MenuItemPhoto,
        &photo_png(50, 50, 11),
    )
    .await
    .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    for id in [with_url, without] {
        attach(
            &mut conn,
            &AssetTarget::new(AssetTable::MenuItems, id, AssetField::Image),
            &o,
        )
        .await
        .unwrap();
    }
    let (u1, g1): (Option<String>, Option<Uuid>) =
        sqlx::query_as("SELECT image_url, image_group_id FROM menu_items WHERE id=$1")
            .bind(with_url)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(u1.as_deref(), Some(legacy));
    assert_eq!(g1, Some(o.group_id));
    let (u2,): (Option<String>,) = sqlx::query_as("SELECT image_url FROM menu_items WHERE id=$1")
        .bind(without)
        .fetch_one(&pool)
        .await
        .unwrap();
    let minted = u2.unwrap();
    assert!(
        minted.ends_with(&format!("{org}/menu-items/{}.webp", o.group_id)),
        "{minted}"
    );
    // A second upload replaces a minted URL (it is ours), but still not the legacy one.
    let o2 = ing(
        &pool,
        &store,
        Some(org),
        AssetPurpose::MenuItemPhoto,
        &photo_png(50, 50, 12),
    )
    .await
    .unwrap();
    for id in [with_url, without] {
        attach(
            &mut conn,
            &AssetTarget::new(AssetTable::MenuItems, id, AssetField::Image),
            &o2,
        )
        .await
        .unwrap();
    }
    let u1: Option<String> = sqlx::query_scalar("SELECT image_url FROM menu_items WHERE id=$1")
        .bind(with_url)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(u1.as_deref(), Some(legacy));
    let u2: Option<String> = sqlx::query_scalar("SELECT image_url FROM menu_items WHERE id=$1")
        .bind(without)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(u2.unwrap().contains(&o2.group_id.to_string()));
    // Cross-org attach refused.
    let other = seed_org(&pool).await;
    let foreign = seed_item(&pool, other, None).await;
    assert!(
        attach(
            &mut conn,
            &AssetTarget::new(AssetTable::MenuItems, foreign, AssetField::Image),
            &o
        )
        .await
        .is_err()
    );
}

// ── worker ──────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn upload_route_to_worker_attaches_group(pool: PgPool) {
    schema(&pool).await;
    let (_d, store) = tmp_store();
    let org = seed_org(&pool).await;
    let item = seed_item(&pool, org, None).await;
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret()))
            .configure(madar_rust::uploads::routes::configure),
    )
    .await;
    // stage_with via env store is used by the route; use the store explicitly instead:
    let job = stage_with(
        &pool,
        &store,
        Some(org),
        AssetPurpose::MenuItemPhoto,
        IngestSource::Bytes(photo_png(600, 400, 13).into()),
        None,
        AssetTarget::new(AssetTable::MenuItems, item, AssetField::Image),
        None,
    )
    .await
    .unwrap();
    let refs = madar_rust::assets::refs::slot_refs(
        &pool,
        org,
        AssetTable::MenuItems,
        AssetField::Image,
        &[item],
    )
    .await
    .unwrap();
    assert!(
        matches!(refs.get(&item), Some(madar_rust::assets::refs::AssetGroupRef::Processing(p)) if p.job_id == job)
    );
    assert_eq!(
        madar_rust::assets::worker::run_one(&pool, &store).await.unwrap(),
        Some(madar_rust::assets::worker::JobResult::Done(job))
    );
    let g: Option<Uuid> = sqlx::query_scalar("SELECT image_group_id FROM menu_items WHERE id=$1")
        .bind(item)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(g.is_some());
    assert!(
        !store.staging_dir().join(job.to_string()).exists(),
        "staged file removed"
    );
    let refs = madar_rust::assets::refs::slot_refs(
        &pool,
        org,
        AssetTable::MenuItems,
        AssetField::Image,
        &[item],
    )
    .await
    .unwrap();
    let r = refs.get(&item).unwrap().ready().unwrap();
    assert_eq!(r.variants.tile.as_ref().unwrap().width, Some(512));
    let json = serde_json::to_string(&refs.get(&item)).unwrap();
    assert!(!json.contains("base64") && json.contains("\"thumb\""));
    // job route (org-scoped, 404 for other orgs)
    let req = test::TestRequest::get()
        .uri(&format!("/assets/jobs/{job}"))
        .insert_header((
            "Authorization",
            format!("Bearer {}", owner_token(&pool, org).await),
        ))
        .to_request();
    let app2 = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret()))
            .configure(madar_rust::assets::routes::configure),
    )
    .await;
    let v: serde_json::Value = test::call_and_read_body_json(&app2, req).await;
    assert_eq!(v["status"], "done");
    assert_eq!(v["result"]["pos"]["variant"], "tile");
    let req = test::TestRequest::get()
        .uri(&format!("/assets/jobs/{job}"))
        .insert_header((
            "Authorization",
            format!("Bearer {}", owner_token(&pool, seed_org(&pool).await).await),
        ))
        .to_request();
    assert_eq!(
        test::call_service(&app2, req).await.status(),
        StatusCode::NOT_FOUND
    );
    drop(app);
}

#[sqlx::test]
async fn worker_retries_then_fails_job(pool: PgPool) {
    schema(&pool).await;
    let (d, store) = tmp_store();
    let org = seed_org(&pool).await;
    let item = seed_item(&pool, org, None).await;
    let job = stage_with(
        &pool,
        &store,
        Some(org),
        AssetPurpose::MenuItemPhoto,
        IngestSource::Bytes(photo_png(30, 30, 14).into()),
        None,
        AssetTarget::new(AssetTable::MenuItems, item, AssetField::Image),
        None,
    )
    .await
    .unwrap();
    // An asset dir that cannot be created (a regular file in the way) = transient I/O failure.
    let blocked = d.path().join("blocker");
    std::fs::write(&blocked, b"x").unwrap();
    let broken = AssetStore::new(blocked.join("assets"), store.uploads_dir.clone());
    // The table trigger stamps updated_at = now(); disable it so the test can age the row.
    sqlx::query("ALTER TABLE asset_jobs DISABLE TRIGGER trg_asset_jobs_updated_at")
        .execute(&pool)
        .await
        .unwrap();
    for attempt in 1..=madar_rust::assets::worker::MAX_ATTEMPTS {
        sqlx::query("UPDATE asset_jobs SET updated_at = now() - interval '1 hour' WHERE id=$1")
            .bind(job)
            .execute(&pool)
            .await
            .unwrap();
        // staged file lives in the good store; point the job at it explicitly
        let r = madar_rust::assets::worker::run_one(&pool, &broken).await.unwrap();
        let (status, attempts): (String, i32) =
            sqlx::query_as("SELECT status, attempts FROM asset_jobs WHERE id=$1")
                .bind(job)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(attempts, attempt);
        if attempt < madar_rust::assets::worker::MAX_ATTEMPTS {
            assert_eq!(r, Some(madar_rust::assets::worker::JobResult::Retry(job)));
            assert_eq!(status, "queued");
            sqlx::query("UPDATE asset_jobs SET updated_at = now() WHERE id=$1")
                .bind(job)
                .execute(&pool)
                .await
                .unwrap();
            assert!(
                madar_rust::assets::worker::run_one(&pool, &broken)
                    .await
                    .unwrap()
                    .is_none(),
                "backoff respected"
            );
        } else {
            assert_eq!(r, Some(madar_rust::assets::worker::JobResult::Failed(job)));
            assert_eq!(status, "failed");
        }
    }
}

// ── routes ──────────────────────────────────────────────────────────────────

async fn asset_app(
    pool: &PgPool,
    store: &AssetStore,
) -> impl actix_web::dev::Service<
    actix_http::Request,
    Response = actix_web::dev::ServiceResponse,
    Error = actix_web::Error,
> {
    test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret()))
            .app_data(web::Data::new(store.clone()))
            .configure(madar_rust::assets::routes::configure)
            .configure(madar_rust::uploads::routes::configure),
    )
    .await
}

#[sqlx::test]
async fn asset_routes_auth_and_cache_headers(pool: PgPool) {
    schema(&pool).await;
    let (_d, store) = tmp_store();
    let (org, other) = (seed_org(&pool).await, seed_org(&pool).await);
    let o = ing(
        &pool,
        &store,
        Some(org),
        AssetPurpose::MenuItemPhoto,
        &photo_png(90, 90, 15),
    )
    .await
    .unwrap();
    let tile = o.variant("tile").unwrap();
    let app = asset_app(&pool, &store).await;
    let path = format!("/assets/{org}/{}.webp", tile.hash);

    // asset_route_jwt_same_org_ok + asset_cache_headers_immutable
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&path)
            .insert_header((
                "Authorization",
                format!("Bearer {}", token(Some(org), UserRole::OrgAdmin)),
            ))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let h = resp.headers();
    assert_eq!(
        h.get("cache-control").unwrap(),
        "private, max-age=31536000, immutable"
    );
    assert_eq!(
        h.get("etag").unwrap(),
        format!("\"{}\"", tile.hash).as_str()
    );
    assert_eq!(h.get("x-content-type-options").unwrap(), "nosniff");
    assert_eq!(h.get("accept-ranges").unwrap(), "bytes");
    assert_eq!(h.get("content-type").unwrap(), "image/webp");

    // asset_route_other_org_404 (and no token → 404, never 403)
    for auth in [Some(token(Some(other), UserRole::OrgAdmin)), None] {
        let mut r = test::TestRequest::get().uri(&path);
        if let Some(t) = auth {
            r = r.insert_header(("Authorization", format!("Bearer {t}")));
        }
        assert_eq!(
            test::call_service(&app, r.to_request()).await.status(),
            StatusCode::NOT_FOUND
        );
    }
    // same hash under the other org's prefix → 404 even with that org's JWT
    let r = test::TestRequest::get()
        .uri(&format!("/assets/{other}/{}.webp", tile.hash))
        .insert_header((
            "Authorization",
            format!("Bearer {}", token(Some(other), UserRole::OrgAdmin)),
        ))
        .to_request();
    assert_eq!(
        test::call_service(&app, r).await.status(),
        StatusCode::NOT_FOUND
    );

    // asset_route_signed_url_ok_and_expired_404
    let signed = rel_url(&signed_url(
        Some(org),
        &tile.hash,
        "webp",
        Duration::from_secs(3600),
    ));
    assert_eq!(
        test::call_service(&app, test::TestRequest::get().uri(&signed).to_request())
            .await
            .status(),
        StatusCode::OK
    );
    let key = format!("{org}/{}.webp", tile.hash);
    let past = chrono::Utc::now().timestamp() - 10;
    let expired = format!("{path}?exp={past}&sig={}", sign(&key, past));
    assert_eq!(
        test::call_service(&app, test::TestRequest::get().uri(&expired).to_request())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    // public cache for long-lived signed URLs
    let long = rel_url(&signed_url(Some(org), &tile.hash, "webp", WALLET_TTL));
    let resp = test::call_service(&app, test::TestRequest::get().uri(&long).to_request()).await;
    assert_eq!(
        resp.headers().get("cache-control").unwrap(),
        "public, max-age=31536000, immutable"
    );

    // asset_route_forged_sig_404: signature for another org's key
    let exp = chrono::Utc::now().timestamp() + 3600;
    let forged = format!(
        "{path}?exp={exp}&sig={}",
        sign(&format!("{other}/{}.webp", tile.hash), exp)
    );
    assert_eq!(
        test::call_service(&app, test::TestRequest::get().uri(&forged).to_request())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let garbage = format!("{path}?exp={exp}&sig={}", "0".repeat(64));
    assert_eq!(
        test::call_service(&app, test::TestRequest::get().uri(&garbage).to_request())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[::core::prelude::v1::test]
fn signed_url_exp_bucketed_daily() {
    let day = 86_400;
    let now = 1_700_000_123;
    let e = bucketed_exp(now, Duration::from_secs(3600));
    assert_eq!(e % day, 0);
    assert!(e >= now + 3600 && e < now + 3600 + day);
    assert_eq!(
        bucketed_exp(now + 50, Duration::from_secs(3600)),
        e,
        "stable within the day"
    );
    let k = "o/h.webp";
    assert!(verify_signature(k, e, &sign(k, e)));
    assert!(!verify_signature(k, e + day, &sign(k, e)));
    assert!(!verify_signature(k, e, "zz"));
}

#[::core::prelude::v1::test]
fn tar_is_byte_identical_for_same_inputs() {
    let d = tempfile::tempdir().unwrap();
    let mut files = Vec::new();
    for (i, body) in [b"bbbb".to_vec(), vec![7u8; 700], b"a".to_vec()]
        .into_iter()
        .enumerate()
    {
        let hash = madar_rust::assets::sha256_hex(&body);
        let p = d.path().join(format!("{i}"));
        std::fs::write(&p, &body).unwrap();
        files.push(madar_rust::assets::tarball::TarFile {
            hash,
            ext: "webp".into(),
            bytes: body.len() as u64,
            path: p,
        });
    }
    let mut rev = files.clone();
    rev.reverse();
    madar_rust::assets::tarball::sort_files(&mut files);
    madar_rust::assets::tarball::sort_files(&mut rev);
    let mut a = Vec::new();
    let mut b = Vec::new();
    madar_rust::assets::tarball::write_tar(&mut a, &files, None).unwrap();
    madar_rust::assets::tarball::write_tar(&mut b, &rev, None).unwrap();
    assert_eq!(a, b);
    let mut ar = tar::Archive::new(&a[..]);
    let names: Vec<String> = ar
        .entries()
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            assert_eq!(e.header().mtime().unwrap(), 0);
            assert_eq!(e.header().mode().unwrap(), 0o644);
            e.path().unwrap().to_string_lossy().to_string()
        })
        .collect();
    assert_eq!(names[0], "index.json");
    let mut sorted = names[1..].to_vec();
    sorted.sort();
    assert_eq!(names[1..], sorted[..]);
    let idx = madar_rust::assets::tarball::index_json(&files, None);
    assert!(!idx.contains(&b' ') && idx.starts_with(b"{\"files\":[{\"bytes\":"));
}

#[sqlx::test]
async fn bundle_debounced_and_contains_exactly_referenced_hashes(pool: PgPool) {
    schema(&pool).await;
    let (_d, store) = tmp_store();
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_item(&pool, org, None).await;
    let used = ing(
        &pool,
        &store,
        Some(org),
        AssetPurpose::MenuItemPhoto,
        &photo_png(80, 80, 16),
    )
    .await
    .unwrap();
    let _unused = ing(
        &pool,
        &store,
        Some(org),
        AssetPurpose::MenuItemPhoto,
        &photo_png(80, 80, 17),
    )
    .await
    .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    attach(
        &mut conn,
        &AssetTarget::new(AssetTable::MenuItems, item, AssetField::Image),
        &used,
    )
    .await
    .unwrap();
    drop(conn);
    // debounce: just dirtied → nothing built
    assert!(
        madar_rust::assets::bundle::run_due(&pool, &store, madar_rust::assets::bundle::DEBOUNCE)
            .await
            .unwrap()
            .is_empty()
    );
    let built = madar_rust::assets::bundle::run_due(&pool, &store, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(built.len(), 1);
    let b = &built[0];
    assert_eq!((b.branch_id, b.file_count), (branch, 1));
    let bytes = std::fs::read(store.path_for_key(&b.file_key)).unwrap();
    assert_eq!(madar_rust::assets::sha256_hex(&bytes), b.sha256);
    let names: Vec<String> = tar::Archive::new(&bytes[..])
        .entries()
        .unwrap()
        .map(|e| e.unwrap().path().unwrap().to_string_lossy().to_string())
        .collect();
    assert_eq!(
        names,
        vec![
            "index.json".to_string(),
            format!("{}.webp", used.variant("tile").unwrap().hash)
        ]
    );
    // unchanged content → no new bundle
    sqlx::query("INSERT INTO asset_bundle_dirty (branch_id, dirty_since) VALUES ($1, now())")
        .bind(branch)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        madar_rust::assets::bundle::run_due(&pool, &store, Duration::ZERO)
            .await
            .unwrap()
            .is_empty()
    );
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM asset_bundles")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

#[sqlx::test]
async fn bundle_route_range_resume_and_topup(pool: PgPool) {
    schema(&pool).await;
    let (_d, store) = tmp_store();
    let (org, other) = (seed_org(&pool).await, seed_org(&pool).await);
    let branch = seed_branch(&pool, org).await;
    let item = seed_item(&pool, org, None).await;
    let o = ing(
        &pool,
        &store,
        Some(org),
        AssetPurpose::MenuItemPhoto,
        &photo_png(900, 700, 18),
    )
    .await
    .unwrap();
    let anim = ing(&pool, &store, None, AssetPurpose::StepAnimation, LOTTIE)
        .await
        .unwrap();
    let foreign = ing(
        &pool,
        &store,
        Some(other),
        AssetPurpose::MenuItemPhoto,
        &photo_png(40, 40, 19),
    )
    .await
    .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    attach(
        &mut conn,
        &AssetTarget::new(AssetTable::MenuItems, item, AssetField::Image),
        &o,
    )
    .await
    .unwrap();
    drop(conn);
    let b = madar_rust::assets::bundle::build_branch(&pool, &store, branch)
        .await
        .unwrap()
        .unwrap();
    let app = asset_app(&pool, &store).await;
    let auth = format!("Bearer {}", owner_token(&pool, org).await);
    let uri = format!("/sync/asset-bundles/{org}/assets-{branch}-{}.tar", b.seq);
    let full = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&uri)
            .insert_header(("Authorization", auth.clone()))
            .to_request(),
    )
    .await;
    assert_eq!(madar_rust::assets::sha256_hex(&full), b.sha256);
    // resume from byte 700
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&uri)
            .insert_header(("Authorization", auth.clone()))
            .insert_header(("Range", "bytes=700-"))
            .insert_header(("If-Range", format!("\"{}\"", b.sha256)))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        resp.headers().get("content-range").unwrap(),
        format!("bytes 700-{}/{}", full.len() - 1, full.len()).as_str()
    );
    let tail = test::read_body(resp).await;
    let mut joined = full[..700].to_vec();
    joined.extend_from_slice(&tail);
    assert_eq!(joined, full.to_vec());
    // stale If-Range → full body
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&uri)
            .insert_header(("Authorization", auth.clone()))
            .insert_header(("Range", "bytes=700-"))
            .insert_header(("If-Range", "\"old\""))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    // other org → 404
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&uri)
            .insert_header((
                "Authorization",
                format!("Bearer {}", owner_token(&pool, other).await),
            ))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // topup_streams_only_org_hashes_rest_in_missing
    let tile = o.variant("tile").unwrap().hash.clone();
    let full_v = o.variant("full").unwrap().hash.clone();
    let foreign_tile = foreign.variant("tile").unwrap().hash.clone();
    let body = serde_json::json!({"branch_id": branch, "hashes": [tile, anim.variants[0].hash, foreign_tile, full_v, "f".repeat(64)]});
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/assets")
            .insert_header(("Authorization", auth.clone()))
            .set_json(&body)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let tarb = test::read_body(resp).await;
    let mut ar = tar::Archive::new(&tarb[..]);
    let mut entries = ar.entries().unwrap();
    let mut first = entries.next().unwrap().unwrap();
    let mut idx = String::new();
    std::io::Read::read_to_string(&mut first, &mut idx).unwrap();
    let idx: serde_json::Value = serde_json::from_str(&idx).unwrap();
    let got: Vec<String> = idx["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["hash"].as_str().unwrap().to_string())
        .collect();
    let mut want = vec![tile.clone(), anim.variants[0].hash.clone()];
    want.sort();
    assert_eq!(got, want);
    let missing = idx["missing"].as_array().unwrap();
    assert_eq!(
        missing.len(),
        3,
        "foreign, non-POS variant and unknown are all just missing"
    );
    let rest: Vec<(String, Vec<u8>)> = entries
        .map(|e| {
            let mut e = e.unwrap();
            let mut v = Vec::new();
            std::io::Read::read_to_end(&mut e, &mut v).unwrap();
            (e.path().unwrap().to_string_lossy().to_string(), v)
        })
        .collect();
    assert_eq!(rest.len(), 2);
    for (name, data) in rest {
        assert_eq!(name.split('.').next().unwrap(), madar_rust::assets::sha256_hex(&data));
    }
    let too_many = serde_json::json!({"branch_id": branch, "hashes": vec!["a".repeat(64); 2001]});
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/assets")
            .insert_header(("Authorization", auth))
            .set_json(&too_many)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(v["code"], "TOO_MANY_HASHES");
}

#[sqlx::test]
async fn legacy_upload_path_redirects_after_prune(pool: PgPool) {
    schema(&pool).await;
    let (_d, store) = tmp_store();
    let org = seed_org(&pool).await;
    let rel = format!("{org}/menu-items/old.png");
    let file = store.uploads_dir.join(&rel);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, photo_png(64, 64, 20)).unwrap();
    let o = ing(
        &pool,
        &store,
        Some(org),
        AssetPurpose::MenuItemPhoto,
        &std::fs::read(&file).unwrap(),
    )
    .await
    .unwrap();
    sqlx::query("INSERT INTO asset_legacy_paths (legacy_path, org_id, asset_id) VALUES ($1,$2,$3)")
        .bind(&rel)
        .bind(org)
        .bind(o.variant("full").unwrap().id)
        .execute(&pool)
        .await
        .unwrap();
    let app = asset_app(&pool, &store).await;
    let uri = format!("/uploads/{rel}");
    let (resp, hits) = madar_rust::client_seen::collect_hits(test::call_service(
        &app,
        test::TestRequest::get().uri(&uri).to_request(),
    ))
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "original still served");
    assert_eq!(
        hits.iter().map(|h| (h.kind, h.org_id)).collect::<Vec<_>>(),
        vec![(madar_rust::client_seen::KIND_UPLOADS_LEGACY_PATH, Some(org))]
    );
    std::fs::remove_file(&file).unwrap();
    let (resp, hits) = madar_rust::client_seen::collect_hits(test::call_service(
        &app,
        test::TestRequest::get().uri(&uri).to_request(),
    ))
    .await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert_eq!(
        hits.iter().map(|h| (h.kind, h.org_id)).collect::<Vec<_>>(),
        vec![(madar_rust::client_seen::KIND_UPLOADS_LEGACY_REDIRECT, Some(org))]
    );
    let loc = resp
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let follow = test::call_service(
        &app,
        test::TestRequest::get().uri(&rel_url(&loc)).to_request(),
    )
    .await;
    assert_eq!(follow.status(), StatusCode::OK);
    // unknown → 404; asset store and traversal never exposed through /uploads
    for bad in [
        "/uploads/nope/x.png".to_string(),
        "/uploads/../secret".to_string(),
        format!(
            "/uploads/assets/{org}/{}.webp",
            o.variant("full").unwrap().hash
        ),
    ] {
        assert_eq!(
            test::call_service(&app, test::TestRequest::get().uri(&bad).to_request())
                .await
                .status(),
            StatusCode::NOT_FOUND,
            "{bad}"
        );
    }
}

#[::core::prelude::v1::test]
fn no_process_upload_callers_remain() {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out)
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p)
            }
        }
    }
    let mut files = Vec::new();
    walk(std::path::Path::new("src"), &mut files);
    for f in files {
        let s = f.to_string_lossy().to_string();
        if s.starts_with("src/assets") {
            continue;
        }
        let body = std::fs::read_to_string(&f).unwrap();
        assert!(
            !body.contains("process_upload("),
            "{s} still calls process_upload"
        );
        assert!(
            !body.contains("delete_old_image("),
            "{s} still deletes upload files"
        );
        for (i, line) in body.lines().enumerate() {
            let writes = line.contains("fs::write(") || line.contains("File::create(");
            if writes
                && (body.contains("UPLOADS_DIR") || line.contains("uploads"))
                && !s.contains("test")
                && !s.starts_with("src/bin")
            {
                panic!("{s}:{} writes files next to UPLOADS_DIR", i + 1);
            }
        }
    }
}

#[sqlx::test]
async fn branding_reads_logo_via_asset(pool: PgPool) {
    schema(&pool).await;
    // AssetStore::from_env is used by branding; point it at a temp dir via the
    // default layout (ASSETS_DIR unset → UPLOADS_DIR/assets is process env) —
    // so write into whatever from_env resolves.
    let store = AssetStore::from_env();
    let org = seed_org(&pool).await;
    sqlx::query("UPDATE organizations SET custom_branding = true, logo_url = 'https://x/uploads/logos/gone.png' WHERE id=$1").bind(org).execute(&pool).await.unwrap();
    let o = ing(
        &pool,
        &store,
        Some(org),
        AssetPurpose::OrgLogo,
        &transparent_png(64),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE organizations SET logo_group_id = $1 WHERE id=$2")
        .bind(o.group_id)
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
    let brand = madar_rust::orgs::branding::load(&pool, org).await.unwrap();
    let img = madar_rust::orgs::branding::read_logo(brand.logo_url.as_deref().unwrap())
        .expect("logo from asset store, legacy file gone");
    assert_eq!(img.width(), 64);
    for v in &o.variants {
        let _ =
            std::fs::remove_file(store.path_for_key(&AssetStore::key(Some(org), &v.hash, &v.ext)));
    }
}
