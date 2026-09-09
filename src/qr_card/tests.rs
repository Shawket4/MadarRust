//! Tests for the QR card module.
//!
//! `render` — pure rendering tests (9 original, no DB, no Shlink).
//! `http`   — HTTP-layer tests with `#[sqlx::test]` and a fake ShortLinkProvider.

// ── Pure render tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod render {
    use image::GenericImageView;

    use crate::qr_card::{
        PAPER, QrCardOptions, TEAL, TEAL_LIGHT, render, render_qr_card_png, render_qr_card_svg,
        render_qr_receipt_png,
    };

    const SHORT: &str = "https://sfx.link/Ab3xK";

    fn opts(short_url: &str) -> QrCardOptions {
        QrCardOptions {
            short_url: short_url.to_string(),
            ..Default::default()
        }
    }

    fn decode_png(png: &[u8]) -> String {
        let img = image::load_from_memory(png).expect("valid png").to_luma8();
        let mut prepared = rqrr::PreparedImage::prepare(img);
        let grids = prepared.detect_grids();
        assert!(!grids.is_empty(), "no QR grid detected");
        let (_meta, content) = grids[0].decode().expect("QR decodes");
        content
    }

    fn dims(png: &[u8]) -> (u32, u32) {
        image::load_from_memory(png)
            .expect("valid png")
            .dimensions()
    }

    #[test]
    fn card_scans_at_300_and_600_dpi() {
        for dpi in [300u32, 600] {
            let png = render_qr_card_png(&QrCardOptions { dpi, ..opts(SHORT) }).expect("render");
            assert_eq!(decode_png(&png), SHORT, "dpi {dpi}");
        }
    }

    #[test]
    fn card_scans_with_caption_latin_and_arabic() {
        for caption in ["Table 5", "امسح للقائمة"] {
            let png = render_qr_card_png(&QrCardOptions {
                caption: Some(caption.to_string()),
                ..opts(SHORT)
            })
            .expect("render");
            assert_eq!(decode_png(&png), SHORT, "caption {caption}");
        }
    }

    #[test]
    fn a6_dimensions_are_exact() {
        let png = render_qr_card_png(&QrCardOptions {
            dpi: 300,
            ..opts(SHORT)
        })
        .expect("render");
        assert_eq!(dims(&png), (1240, 1748), "A6 trim @ 300 DPI");

        let png600 = render_qr_card_png(&QrCardOptions {
            dpi: 600,
            ..opts(SHORT)
        })
        .expect("render");
        assert_eq!(
            dims(&png600),
            (render::px(105.0, 600), render::px(148.0, 600))
        );
        assert_eq!(dims(&png600), (2480, 3496));
    }

    #[test]
    fn a6_dimensions_with_bleed() {
        let png = render_qr_card_png(&QrCardOptions {
            bleed_mm: 3.0,
            crop_marks: true,
            dpi: 600,
            ..opts(SHORT)
        })
        .expect("render");
        assert_eq!(dims(&png), (render::px(111.0, 600), render::px(154.0, 600)));
        assert_eq!(decode_png(&png), SHORT);
    }

    #[test]
    fn render_is_deterministic() {
        let a = render_qr_card_png(&opts(SHORT)).expect("render");
        let b = render_qr_card_png(&opts(SHORT)).expect("render");
        assert_eq!(a, b, "same options must produce identical bytes");

        let svg_a = render_qr_card_svg(&opts(SHORT)).expect("svg");
        let svg_b = render_qr_card_svg(&opts(SHORT)).expect("svg");
        assert_eq!(svg_a, svg_b);
    }

    #[test]
    fn pathological_input_errors_without_panic() {
        let long = "a".repeat(10_000);
        let cases = ["", long.as_str(), "héllo–ünïcодé™"];
        for c in cases {
            assert!(
                render_qr_card_png(&opts(c)).is_err(),
                "expected Err for {c:?}"
            );
            assert!(
                render_qr_card_svg(&opts(c)).is_err(),
                "expected Err for {c:?}"
            );
            assert!(
                render_qr_receipt_png(c, 8).is_err(),
                "expected Err for receipt {c:?}"
            );
        }
    }

    #[test]
    fn svg_contains_brand_tokens() {
        let svg = render_qr_card_svg(&opts(SHORT)).expect("svg");
        assert!(svg.contains(TEAL));
        assert!(svg.contains(PAPER));
        assert!(svg.contains(TEAL_LIGHT));
        assert!(svg.starts_with("<svg"));
    }

    #[test]
    fn receipt_qr_scans_and_is_square() {
        let png = render_qr_receipt_png(SHORT, 8).expect("render");
        let (w, h) = dims(&png);
        assert_eq!(w, h, "receipt QR is square");
        assert_eq!(decode_png(&png), SHORT);
    }

    #[test]
    fn emit_golden_render() {
        let png = render_qr_card_png(&QrCardOptions {
            caption: Some("Table 5".to_string()),
            ..opts(SHORT)
        })
        .expect("render");
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/target/qr_card_golden.png");
        std::fs::write(path, &png).expect("write golden");
    }

    // ── Marketing path validation (pure, no DB) ───────────────────────────────

    #[test]
    fn marketing_path_validation() {
        use crate::qr_card::handlers::validate_marketing_path_pub;
        assert!(
            validate_marketing_path_pub("http://evil.com").is_err(),
            "absolute URL rejected"
        );
        assert!(
            validate_marketing_path_pub("//evil.com/x").is_err(),
            "protocol-relative rejected"
        );
        assert!(
            validate_marketing_path_pub("/menu?promo=dec").is_ok(),
            "clean relative path accepted"
        );
        assert!(
            validate_marketing_path_pub("no-leading-slash").is_err(),
            "missing leading slash rejected"
        );
    }
}

// ── HTTP layer tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod http {
    use std::sync::Arc;

    use actix_web::{App, test, web};
    use sqlx::PgPool;
    use uuid::Uuid;

    use crate::auth::jwt::JwtSecret;
    use crate::models::UserRole;
    use crate::qr_card::db::BranchTable;
    use crate::qr_card::handlers::QrResponse;
    use crate::qr_card::shlink::ShortLinkProvider;
    use crate::qr_card::shlink::fake::FakeShortLinkProvider;

    fn get_secret() -> JwtSecret {
        JwtSecret("secret".to_string())
    }

    fn token(user_id: Uuid, org_id: Uuid, role: UserRole) -> String {
        crate::auth::jwt::create_token(&get_secret(), user_id, Some(org_id), role, None, 24)
            .unwrap()
    }

    fn org_admin_token(user_id: Uuid, org_id: Uuid) -> String {
        token(user_id, org_id, UserRole::OrgAdmin)
    }

    async fn seed_org(pool: &PgPool) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query!(
            "INSERT INTO organizations (id, name, slug) VALUES ($1, 'QR Test Org', $2)",
            id,
            format!("qr-{id}")
        )
        .execute(pool)
        .await
        .unwrap();
        id
    }

    async fn seed_branch(pool: &PgPool, org_id: Uuid) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query!(
            "INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'QR Test Branch')",
            id,
            org_id
        )
        .execute(pool)
        .await
        .unwrap();
        id
    }

    async fn grant(pool: &PgPool, role: &str, resource: &str, action: &str) {
        sqlx::query(
            "INSERT INTO role_permissions (role, resource, action, granted)
             VALUES ($1::user_role, $2::permission_resource, $3::permission_action, true)
             ON CONFLICT DO NOTHING",
        )
        .bind(role)
        .bind(resource)
        .bind(action)
        .execute(pool)
        .await
        .unwrap();
    }

    fn make_app(
        pool: PgPool,
        fake: Arc<dyn ShortLinkProvider>,
    ) -> actix_web::App<
        impl actix_web::dev::ServiceFactory<
            actix_web::dev::ServiceRequest,
            Config = (),
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
            InitError = (),
        >,
    > {
        App::new()
            .app_data(web::Data::new(pool))
            .app_data(web::Data::new(get_secret()))
            .app_data(web::Data::new(fake))
            .configure(crate::qr_card::routes::configure)
            .configure(crate::branches::routes::configure)
            .configure(crate::orgs::routes::configure)
    }

    // ── Table CRUD ────────────────────────────────────────────────────────────

    #[sqlx::test]
    async fn test_create_and_list_tables(pool: PgPool) {
        let org_id = seed_org(&pool).await;
        let branch_id = seed_branch(&pool, org_id).await;
        grant(&pool, "org_admin", "branches", "update").await;
        grant(&pool, "org_admin", "branches", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let req = test::TestRequest::post()
            .uri(&format!("/branches/{branch_id}/tables"))
            .insert_header(("Authorization", format!("Bearer {tok}")))
            .set_json(&serde_json::json!({ "label": "Table 1" }))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 201, "create table");
        let tbl: BranchTable = test::read_body_json(resp).await;
        assert_eq!(tbl.label, "Table 1");
        assert_eq!(tbl.branch_id, branch_id);

        let req = test::TestRequest::get()
            .uri(&format!("/branches/{branch_id}/tables"))
            .insert_header(("Authorization", format!("Bearer {tok}")))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
        let tables: Vec<BranchTable> = test::read_body_json(resp).await;
        assert_eq!(tables.len(), 1);
    }

    #[sqlx::test]
    async fn test_table_label_uniqueness(pool: PgPool) {
        let org_id = seed_org(&pool).await;
        let branch_id = seed_branch(&pool, org_id).await;
        grant(&pool, "org_admin", "branches", "update").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let r1 = test::call_service(
            &app,
            test::TestRequest::post()
                .uri(&format!("/branches/{branch_id}/tables"))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .set_json(&serde_json::json!({ "label": "Table 2" }))
                .to_request(),
        )
        .await;
        assert_eq!(r1.status(), 201);

        let r2 = test::call_service(
            &app,
            test::TestRequest::post()
                .uri(&format!("/branches/{branch_id}/tables"))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .set_json(&serde_json::json!({ "label": "Table 2" }))
                .to_request(),
        )
        .await;
        assert_eq!(r2.status(), 409, "duplicate label must be 409");
    }

    #[sqlx::test]
    async fn test_delete_table(pool: PgPool) {
        let org_id = seed_org(&pool).await;
        let branch_id = seed_branch(&pool, org_id).await;
        grant(&pool, "org_admin", "branches", "update").await;
        grant(&pool, "org_admin", "branches", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri(&format!("/branches/{branch_id}/tables"))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .set_json(&serde_json::json!({ "label": "To Delete" }))
                .to_request(),
        )
        .await;
        let tbl: BranchTable = test::read_body_json(resp).await;

        let resp = test::call_service(
            &app,
            test::TestRequest::delete()
                .uri(&format!("/branches/{branch_id}/tables/{}", tbl.id))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 204);

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/branches/{branch_id}/tables"))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .to_request(),
        )
        .await;
        let tables: Vec<BranchTable> = test::read_body_json(resp).await;
        assert!(tables.is_empty());
    }

    // ── Short-link dedup ──────────────────────────────────────────────────────

    #[sqlx::test]
    async fn test_branch_qr_dedup(pool: PgPool) {
        // Safety: test process is single-threaded at this point.
        unsafe { std::env::set_var("PUBLIC_ORDER_BASE_URL", "https://example.com") };
        let org_id = seed_org(&pool).await;
        let branch_id = seed_branch(&pool, org_id).await;
        grant(&pool, "org_admin", "branches", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let call = || {
            test::TestRequest::get()
                .uri(&format!("/branches/{branch_id}/qr?card=false"))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .to_request()
        };

        let r1: QrResponse = test::read_body_json(test::call_service(&app, call()).await).await;
        let r2: QrResponse = test::read_body_json(test::call_service(&app, call()).await).await;
        assert_eq!(
            r1.short_code, r2.short_code,
            "dedup must return same short_code"
        );
    }

    // ── Auth gating ───────────────────────────────────────────────────────────

    #[sqlx::test]
    async fn test_tables_require_auth(pool: PgPool) {
        let org_id = seed_org(&pool).await;
        let branch_id = seed_branch(&pool, org_id).await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;

        let req = test::TestRequest::get()
            .uri(&format!("/branches/{branch_id}/tables"))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);
    }

    #[sqlx::test]
    async fn test_tables_require_permission(pool: PgPool) {
        let org_id = seed_org(&pool).await;
        let branch_id = seed_branch(&pool, org_id).await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let req = test::TestRequest::get()
            .uri(&format!("/branches/{branch_id}/tables"))
            .insert_header(("Authorization", format!("Bearer {tok}")))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 403);
    }

    // ── Marketing path validation ─────────────────────────────────────────────

    #[sqlx::test]
    async fn test_marketing_link_rejects_bad_path(pool: PgPool) {
        // Safety: test process is single-threaded at this point.
        unsafe { std::env::set_var("PUBLIC_ORDER_BASE_URL", "https://example.com") };
        let org_id = seed_org(&pool).await;
        grant(&pool, "org_admin", "branches", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        for bad in ["http://evil.com/x", "//evil.com/x", "no-slash"] {
            let resp = test::call_service(
                &app,
                test::TestRequest::post()
                    .uri("/qr/links")
                    .insert_header(("Authorization", format!("Bearer {tok}")))
                    .set_json(&serde_json::json!({ "label": "bad", "path": bad }))
                    .to_request(),
            )
            .await;
            assert_eq!(resp.status(), 400, "bad path {bad:?} must be 400");
        }

        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/qr/links")
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .set_json(&serde_json::json!({ "label": "good", "path": "/menu?p=1" }))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 201, "valid path must succeed");
    }

    // ── org QR ────────────────────────────────────────────────────────────────

    #[sqlx::test]
    async fn test_org_qr_happy_path(pool: PgPool) {
        unsafe { std::env::set_var("PUBLIC_ORDER_BASE_URL", "https://example.com") };
        let org_id = seed_org(&pool).await;
        grant(&pool, "org_admin", "branches", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/orgs/{org_id}/qr?card=false"))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200, "org QR should succeed");
        let qr: QrResponse = test::read_body_json(resp).await;
        assert_eq!(qr.kind, "org_order");
        // org_id must be in the path segment, not a query param
        assert!(
            qr.long_url.contains(&format!("/order/{}", org_id)),
            "long_url must use /order/<org_id> path"
        );
        assert!(!qr.short_url.is_empty());
        assert!(qr.qr_data_url.starts_with("data:image/"));
    }

    #[sqlx::test]
    async fn test_org_qr_dedup(pool: PgPool) {
        unsafe { std::env::set_var("PUBLIC_ORDER_BASE_URL", "https://example.com") };
        let org_id = seed_org(&pool).await;
        grant(&pool, "org_admin", "branches", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let call = || {
            test::TestRequest::get()
                .uri(&format!("/orgs/{org_id}/qr?card=false"))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .to_request()
        };
        let r1: QrResponse = test::read_body_json(test::call_service(&app, call()).await).await;
        let r2: QrResponse = test::read_body_json(test::call_service(&app, call()).await).await;
        assert_eq!(
            r1.short_code, r2.short_code,
            "org QR must be deduplicated across calls"
        );
    }

    #[sqlx::test]
    async fn test_org_qr_requires_auth(pool: PgPool) {
        let org_id = seed_org(&pool).await;
        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/orgs/{org_id}/qr?card=false"))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 401);
    }

    #[sqlx::test]
    async fn test_org_qr_requires_permission(pool: PgPool) {
        let org_id = seed_org(&pool).await;
        // No branches:read grant — permission check must block.
        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/orgs/{org_id}/qr?card=false"))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 403, "missing branches:read must yield 403");
    }

    #[sqlx::test]
    async fn test_org_qr_rejects_cross_org(pool: PgPool) {
        unsafe { std::env::set_var("PUBLIC_ORDER_BASE_URL", "https://example.com") };
        let org_a = seed_org(&pool).await;
        let org_b = seed_org(&pool).await;
        grant(&pool, "org_admin", "branches", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        // Token is for org_a but the path targets org_b.
        let tok = org_admin_token(Uuid::new_v4(), org_a);

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/orgs/{org_b}/qr?card=false"))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 403, "cross-org org QR must be 403");
    }

    // ── in-mall branch QR ─────────────────────────────────────────────────────

    #[sqlx::test]
    async fn test_branch_qr_in_mall_generates_in_mall_kind(pool: PgPool) {
        unsafe { std::env::set_var("PUBLIC_ORDER_BASE_URL", "https://example.com") };
        let org_id = seed_org(&pool).await;
        let branch_id = seed_branch(&pool, org_id).await;
        grant(&pool, "org_admin", "branches", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!(
                    "/branches/{branch_id}/qr?card=false\
                     &place_name=Shop+12&floor=Ground&unit_number=Unit+4"
                ))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let qr: QrResponse = test::read_body_json(resp).await;
        assert_eq!(
            qr.kind, "branch_order_in_mall",
            "all three in-mall params must produce branch_order_in_mall kind"
        );
    }

    #[sqlx::test]
    async fn test_branch_qr_in_mall_long_url_carries_prefill_params(pool: PgPool) {
        unsafe { std::env::set_var("PUBLIC_ORDER_BASE_URL", "https://example.com") };
        let org_id = seed_org(&pool).await;
        let branch_id = seed_branch(&pool, org_id).await;
        grant(&pool, "org_admin", "branches", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!(
                    "/branches/{branch_id}/qr?card=false\
                     &place_name=Kiosk+5&floor=First+Floor&unit_number=K5"
                ))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .to_request(),
        )
        .await;
        let qr: QrResponse = test::read_body_json(resp).await;
        assert!(
            qr.long_url.contains("channel=in_mall"),
            "long_url must lock channel to in_mall; got: {}",
            qr.long_url
        );
        assert!(
            qr.long_url.contains("place_name="),
            "long_url must carry place_name"
        );
        assert!(qr.long_url.contains("floor="), "long_url must carry floor");
        assert!(
            qr.long_url.contains("unit_number="),
            "long_url must carry unit_number"
        );
        assert!(
            qr.long_url.contains(&branch_id.to_string()),
            "long_url must include branch_id"
        );
        assert!(
            qr.long_url.contains(&org_id.to_string()),
            "long_url must include org_id in path"
        );
    }

    /// Providing only place_name (missing floor and unit_number) must silently
    /// fall back to a standard branch_order URL, not return an error.
    #[sqlx::test]
    async fn test_branch_qr_in_mall_partial_params_fall_back_to_standard(pool: PgPool) {
        unsafe { std::env::set_var("PUBLIC_ORDER_BASE_URL", "https://example.com") };
        let org_id = seed_org(&pool).await;
        let branch_id = seed_branch(&pool, org_id).await;
        grant(&pool, "org_admin", "branches", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                // only place_name, no floor or unit_number
                .uri(&format!(
                    "/branches/{branch_id}/qr?card=false&place_name=Shop+5"
                ))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let qr: QrResponse = test::read_body_json(resp).await;
        assert_eq!(
            qr.kind, "branch_order",
            "incomplete in-mall params must fall back to standard branch_order kind"
        );
        assert!(
            !qr.long_url.contains("channel=in_mall"),
            "standard URL must not contain channel=in_mall"
        );
    }

    #[sqlx::test]
    async fn test_branch_qr_in_mall_different_locations_get_distinct_codes(pool: PgPool) {
        unsafe { std::env::set_var("PUBLIC_ORDER_BASE_URL", "https://example.com") };
        let org_id = seed_org(&pool).await;
        let branch_id = seed_branch(&pool, org_id).await;
        grant(&pool, "org_admin", "branches", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let r1: QrResponse = test::read_body_json(
            test::call_service(
                &app,
                test::TestRequest::get()
                    .uri(&format!(
                        "/branches/{branch_id}/qr?card=false\
                         &place_name=Location+A&floor=1&unit_number=U1"
                    ))
                    .insert_header(("Authorization", format!("Bearer {tok}")))
                    .to_request(),
            )
            .await,
        )
        .await;

        let r2: QrResponse = test::read_body_json(
            test::call_service(
                &app,
                test::TestRequest::get()
                    .uri(&format!(
                        "/branches/{branch_id}/qr?card=false\
                         &place_name=Location+B&floor=2&unit_number=U2"
                    ))
                    .insert_header(("Authorization", format!("Bearer {tok}")))
                    .to_request(),
            )
            .await,
        )
        .await;

        assert_ne!(
            r1.short_code, r2.short_code,
            "different in-mall locations must produce distinct short codes"
        );
    }

    #[sqlx::test]
    async fn test_branch_qr_in_mall_same_location_deduped(pool: PgPool) {
        unsafe { std::env::set_var("PUBLIC_ORDER_BASE_URL", "https://example.com") };
        let org_id = seed_org(&pool).await;
        let branch_id = seed_branch(&pool, org_id).await;
        grant(&pool, "org_admin", "branches", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let call = || {
            test::TestRequest::get()
                .uri(&format!(
                    "/branches/{branch_id}/qr?card=false\
                     &place_name=Kiosk&floor=Ground&unit_number=K1"
                ))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .to_request()
        };
        let r1: QrResponse = test::read_body_json(test::call_service(&app, call()).await).await;
        let r2: QrResponse = test::read_body_json(test::call_service(&app, call()).await).await;
        assert_eq!(
            r1.short_code, r2.short_code,
            "same in-mall location must return the same short code"
        );
    }

    /// Standard branch QR and an in-mall QR for the same branch must have
    /// completely different short codes — they are separate dedup entries.
    #[sqlx::test]
    async fn test_branch_qr_standard_and_in_mall_are_separate(pool: PgPool) {
        unsafe { std::env::set_var("PUBLIC_ORDER_BASE_URL", "https://example.com") };
        let org_id = seed_org(&pool).await;
        let branch_id = seed_branch(&pool, org_id).await;
        grant(&pool, "org_admin", "branches", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let standard: QrResponse = test::read_body_json(
            test::call_service(
                &app,
                test::TestRequest::get()
                    .uri(&format!("/branches/{branch_id}/qr?card=false"))
                    .insert_header(("Authorization", format!("Bearer {tok}")))
                    .to_request(),
            )
            .await,
        )
        .await;

        let in_mall: QrResponse = test::read_body_json(
            test::call_service(
                &app,
                test::TestRequest::get()
                    .uri(&format!(
                        "/branches/{branch_id}/qr?card=false\
                         &place_name=Shop&floor=1&unit_number=S1"
                    ))
                    .insert_header(("Authorization", format!("Bearer {tok}")))
                    .to_request(),
            )
            .await,
        )
        .await;

        assert_ne!(standard.short_code, in_mall.short_code);
        assert_eq!(standard.kind, "branch_order");
        assert_eq!(in_mall.kind, "branch_order_in_mall");
    }

    // ── Booking QR (the reservations app, a different host entirely) ──────────

    /// Turn bookings on for a branch, the way the settings dialog does.
    async fn enable_bookings(pool: &PgPool, org_id: Uuid, branch_id: Uuid) {
        sqlx::query(
            "INSERT INTO branch_booking_settings (org_id, branch_id, enabled) \
             VALUES ($1, $2, true) \
             ON CONFLICT (branch_id) DO UPDATE SET enabled = true",
        )
        .bind(org_id)
        .bind(branch_id)
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test]
    async fn booking_qr_points_at_the_reservations_app(pool: PgPool) {
        // Safety: test process is single-threaded at this point.
        unsafe {
            std::env::set_var(
                "PUBLIC_RESERVATIONS_BASE_URL",
                "https://reservations.example.com",
            )
        };
        let org_id = seed_org(&pool).await;
        let branch_id = seed_branch(&pool, org_id).await;
        enable_bookings(&pool, org_id, branch_id).await;
        grant(&pool, "org_admin", "bookings", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/branches/{branch_id}/booking-qr?card=false"))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let qr: QrResponse = test::read_body_json(resp).await;
        assert_eq!(qr.kind, "branch_booking");
        // The reservations bundle routes on `/{org}/{branch}` — NOT `/order/...`,
        // and not the ordering host.
        assert_eq!(
            qr.long_url,
            format!("https://reservations.example.com/{org_id}/{branch_id}")
        );

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/orgs/{org_id}/booking-qr?card=false"))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let qr: QrResponse = test::read_body_json(resp).await;
        assert_eq!(qr.kind, "org_booking");
        assert_eq!(
            qr.long_url,
            format!("https://reservations.example.com/{org_id}")
        );
    }

    /// A card leading to "we don't take bookings" is worse than no card, and a
    /// branch's settings row defaults `enabled` to false — so this is the state
    /// of every branch until someone turns bookings on.
    #[sqlx::test]
    async fn booking_qr_refuses_a_branch_with_bookings_off(pool: PgPool) {
        // Safety: test process is single-threaded at this point.
        unsafe {
            std::env::set_var(
                "PUBLIC_RESERVATIONS_BASE_URL",
                "https://reservations.example.com",
            )
        };
        let org_id = seed_org(&pool).await;
        let branch_id = seed_branch(&pool, org_id).await;
        grant(&pool, "org_admin", "bookings", "read").await;

        let fake = Arc::new(FakeShortLinkProvider::new()) as Arc<dyn ShortLinkProvider>;
        let app = test::init_service(make_app(pool.clone(), fake)).await;
        let tok = org_admin_token(Uuid::new_v4(), org_id);

        for uri in [
            format!("/branches/{branch_id}/booking-qr"),
            format!("/orgs/{org_id}/booking-qr"),
        ] {
            let resp = test::call_service(
                &app,
                test::TestRequest::get()
                    .uri(&uri)
                    .insert_header(("Authorization", format!("Bearer {tok}")))
                    .to_request(),
            )
            .await;
            assert_eq!(resp.status(), 409, "{uri} with bookings off");
        }
    }
}

// ── Branded cards (the branding tier) ─────────────────────────────────────────

#[cfg(test)]
mod branded {
    use image::{DynamicImage, GenericImageView, Rgba, RgbaImage};

    use crate::orgs::branding::{self, OrgBrand, Palette};
    use crate::qr_card::brand::{CardBrand, MAX_LOGO_PX, MIN_LOGO_PX, card_brand, prepare_logo};
    use crate::qr_card::{
        PAPER, QrCardOptions, TEAL, TEAL_LIGHT, render, render_qr_card_png, render_qr_card_svg,
    };

    const SHORT: &str = "https://sfx.link/Ab3xK";
    /// A light brand: the shop's colour is the ground, near-black ink on it.
    const GOLD: (&str, &str, &str) = ("#FFD400", "#12222A", "#8A6D00");

    fn opts(brand: Option<CardBrand>) -> QrCardOptions {
        QrCardOptions {
            short_url: SHORT.into(),
            brand,
            ..Default::default()
        }
    }

    fn org(p: (&str, &str, &str)) -> OrgBrand {
        OrgBrand {
            name: "Qahwa & Co".into(),
            custom_branding: true,
            palette: Palette {
                background: p.0.into(),
                foreground: p.1.into(),
                accent: p.2.into(),
            },
            ..Default::default()
        }
    }

    fn lum(hex: &str) -> f64 {
        let (r, g, b) = branding::parse_hex(hex).expect("hex");
        branding::luminance(r, g, b)
    }

    fn solid(w: u32, h: u32, px: [u8; 4]) -> DynamicImage {
        DynamicImage::ImageRgba8(RgbaImage::from_pixel(w, h, Rgba(px)))
    }

    fn two_tone(w: u32, h: u32) -> DynamicImage {
        let mut img = RgbaImage::new(w, h);
        for (x, _y, p) in img.enumerate_pixels_mut() {
            *p = if x < w / 2 {
                Rgba([220, 30, 30, 255])
            } else {
                Rgba([30, 60, 220, 255])
            };
        }
        DynamicImage::ImageRgba8(img)
    }

    fn decode_qr(png: &[u8]) -> String {
        let img = image::load_from_memory(png).expect("valid png").to_luma8();
        let mut prepared = rqrr::PreparedImage::prepare(img);
        let grids = prepared.detect_grids();
        assert!(!grids.is_empty(), "no QR grid detected");
        grids[0].decode().expect("QR decodes").1
    }

    /// The whole promise of the unbranded card: a shop that has not bought the
    /// branding tier gets the card Madar has always printed, to the byte.
    ///
    /// A golden file rather than a spot-check of a few substrings, because the
    /// ways this could regress are not ones a substring test would notice — a
    /// colour resolved through the wrong variable, an attribute reordered, a
    /// footer line emitted when it should not have been. The SVG is composed
    /// by string formatting alone, with no font or rasteriser in the path, so
    /// it is stable enough to pin exactly.
    #[test]
    fn the_unbranded_card_is_byte_for_byte_the_card_that_shipped_before() {
        let svg = render_qr_card_svg(&QrCardOptions {
            short_url: SHORT.into(),
            caption: Some("Table 5".into()),
            ..Default::default()
        })
        .expect("svg");
        assert_eq!(
            svg,
            include_str!("golden_unbranded_card.svg"),
            "the unbranded card changed; if that was deliberate, the golden file \
             has to be regenerated and the change justified"
        );
    }

    /// The tier gate is the loader's job, and this is the whole of this
    /// module's part in it: no flag check, just the `None` that routes back to
    /// the card above.
    #[test]
    fn an_org_off_the_tier_yields_no_brand_at_all() {
        let mut o = org(GOLD);
        o.custom_branding = false;
        assert!(card_brand(&o).is_none());
    }

    /// The QR is the one part of the card that is not a matter of taste.
    ///
    /// Two properties, and both matter: the pair has to clear AA, and the
    /// modules have to be the DARKER half — a light-on-dark code is legal SVG
    /// and unreadable to most of the scanners these cards are pointed at.
    #[test]
    fn the_qr_pair_is_always_dark_on_light_and_clears_aa() {
        for p in [
            GOLD,
            ("#0D6273", "#EFF3F4", "#2E94A6"), // a dark brand — the pair inverts
            ("#1A1A2E", "#EFF3F4", "#4A4A7E"),
            ("#7F7F7F", "#888888", "#909090"), // readable against nothing
        ] {
            let b = card_brand(&org(p)).expect("on the tier");
            let (g, i) = (lum(&b.ground), lum(&b.ink));
            assert!(i < g, "modules must be the darker half for {}", p.0);
            assert!(
                branding::contrast(g, i) >= 4.5,
                "{} vs {} is {:.2}:1",
                b.ground,
                b.ink,
                branding::contrast(g, i)
            );
        }
    }

    /// The defensive path: a palette that cannot be made to scan gives the card
    /// up rather than the code. The accent goes with it, because a hairline
    /// that fails 3:1 against the ground it is drawn on is not a frame.
    #[test]
    fn an_unreadable_palette_falls_back_to_ink_on_paper() {
        let b = card_brand(&org(("#7F7F7F", "#888888", "#909090"))).expect("on the tier");
        assert_eq!(b.ground, PAPER);
        assert_eq!(b.ink, TEAL);
        assert_eq!(b.accent, TEAL, "an unreadable accent is not drawn");
    }

    /// A branded card scans, which is the only thing about it that is not
    /// negotiable.
    #[test]
    fn a_branded_card_still_scans() {
        for p in [GOLD, ("#0D6273", "#EFF3F4", "#2E94A6")] {
            let b = card_brand(&org(p)).expect("on the tier");
            let png = render_qr_card_png(&QrCardOptions {
                dpi: 300,
                ..opts(Some(b))
            })
            .expect("render");
            assert_eq!(decode_qr(&png), SHORT, "palette {}", p.0);
        }
    }

    /// What the shop actually bought: its colours on the card, its name where
    /// Madar's wordmark was, and none of Madar's own tokens left anywhere —
    /// including inside the fallback mark, which ships hardcoded in teal.
    #[test]
    fn a_branded_card_wears_the_shops_colours_and_name() {
        let b = card_brand(&org(GOLD)).expect("on the tier");
        let svg = render_qr_card_svg(&opts(Some(b))).expect("svg");
        for c in [GOLD.0, GOLD.1, GOLD.2] {
            assert!(svg.contains(c), "{c} missing from the card");
        }
        assert!(
            !svg.contains(TEAL),
            "Madar's teal survived on a branded card"
        );
        assert!(!svg.contains(TEAL_LIGHT));
        assert!(!svg.contains(PAPER));
        assert!(svg.contains("Qahwa &amp; Co"), "the shop's name, escaped");
    }

    /// Madar's credit is on both cards. It is the lockup on an unbranded one
    /// and a line of type on a branded one, and the point of the test is that
    /// there is no third state where it is absent.
    #[test]
    fn powered_by_madar_is_on_the_branded_card_and_the_lockup_on_the_other() {
        let branded = render_qr_card_svg(&opts(Some(card_brand(&org(GOLD)).expect("on the tier"))))
            .expect("svg");
        assert!(branded.contains("Powered by Madar"));

        let plain = render_qr_card_svg(&opts(None)).expect("svg");
        assert!(
            !plain.contains("Powered by Madar"),
            "the unbranded card credits Madar with the wordmark, not a line"
        );
        assert!(
            plain.contains(TEAL_LIGHT),
            "the wordmark lockup is still there"
        );
    }

    /// The print floor, and the reason it exists.
    ///
    /// The slot is 15 mm and the card rasterises at 600 DPI, so one image pixel
    /// per device pixel would be `15 / 25.4 * 600 = 354`. The bar is set at the
    /// 300 DPI commercial-print floor instead — `15 / 25.4 * 300 = 177` — below
    /// which the logo is being blown up more than twofold and prints soft. On a
    /// run of several hundred cards that is discovered by the shop, not by us.
    #[test]
    fn a_logo_below_the_print_floor_is_refused() {
        assert_eq!(MIN_LOGO_PX, 177, "15 mm at 300 DPI");
        assert!(prepare_logo(&solid(176, 176, [20, 20, 20, 255]), true, TEAL).is_none());
        assert!(
            prepare_logo(&solid(177, 40, [20, 20, 20, 255]), true, TEAL).is_some(),
            "the long edge is what gets fitted to the slot, so it is what counts"
        );
    }

    /// …and a refused logo leaves a card with Madar's vector mark on it, not a
    /// hole where the mark should be.
    #[test]
    fn a_refused_logo_falls_back_to_madars_mark() {
        let mut b = card_brand(&org(GOLD)).expect("on the tier");
        let ink = b.ink.clone();
        b.logo = prepare_logo(&solid(120, 120, [20, 20, 20, 255]), true, &ink);
        assert!(b.logo.is_none(), "too small to print");

        let svg = render_qr_card_svg(&opts(Some(b))).expect("svg");
        assert!(!svg.contains("<image"), "nothing raster was drawn");
        assert!(
            svg.contains(r#"<circle cx="74.04" cy="25.96""#),
            "Madar's mark is in the slot"
        );
    }

    /// A logo that passes is embedded as a `data:` URI, fitted to its own
    /// aspect — and, because resvg is built here without raster-image support,
    /// it has to arrive in the PNG by another route. This is the test that
    /// catches that route being broken: it reads the pixel at the card's
    /// centre, which is the middle of the mark slot.
    #[test]
    fn an_accepted_logo_reaches_both_the_svg_and_the_raster() {
        let logo = prepare_logo(&solid(400, 400, [255, 0, 255, 255]), false, "#12222A")
            .expect("400 px clears the floor");
        let mut b = card_brand(&org(GOLD)).expect("on the tier");
        b.logo = Some(logo);

        let svg = render_qr_card_svg(&opts(Some(b.clone()))).expect("svg");
        assert!(svg.contains(r#"<image x="45" y="51.5" width="15" height="15""#));
        assert!(svg.contains(r#"href="data:image/png;base64,"#));

        let png = render_qr_card_png(&QrCardOptions {
            dpi: 150,
            ..opts(Some(b))
        })
        .expect("render");
        let img = image::load_from_memory(&png).expect("valid png");
        let (cx, cy) = (render::px(52.5, 150), render::px(59.0, 150));
        assert_eq!(
            img.get_pixel(cx, cy).0,
            [255, 0, 255, 255],
            "the shop's logo is on the printed card"
        );
    }

    /// A wide logo keeps its shape: the long edge fills the slot and the short
    /// one is left short, rather than the mark being squared up into something
    /// nobody drew.
    #[test]
    fn a_wide_logo_is_fitted_by_its_long_edge() {
        let logo =
            prepare_logo(&solid(400, 200, [255, 0, 255, 255]), false, "#12222A").expect("accepted");
        let mut b = card_brand(&org(GOLD)).expect("on the tier");
        b.logo = Some(logo);
        let svg = render_qr_card_svg(&opts(Some(b))).expect("svg");
        assert!(svg.contains(r#"<image x="45" y="55.25" width="15" height="7.5""#));
    }

    /// Tinting is for a single-colour mark and nothing else. Repainting every
    /// pixel of a two-colour logo does not recolour it, it flattens it into a
    /// silhouette — so `logo_is_mark` decides, and this is what it decides
    /// between.
    #[test]
    fn a_multi_colour_logo_is_drawn_as_uploaded() {
        let img = two_tone(400, 400);
        let as_is = prepare_logo(&img, false, "#12222A").expect("accepted");
        let tinted = prepare_logo(&img, true, "#12222A").expect("accepted");

        let a = image::load_from_memory(&as_is.png).expect("png");
        assert_eq!(a.get_pixel(10, 10).0, [220, 30, 30, 255]);
        assert_eq!(a.get_pixel(390, 10).0, [30, 60, 220, 255]);

        let t = image::load_from_memory(&tinted.png).expect("png");
        assert_eq!(t.get_pixel(10, 10).0, [0x12, 0x22, 0x2A, 255]);
        assert_eq!(
            t.get_pixel(10, 10).0,
            t.get_pixel(390, 10).0,
            "a tint collapses the two halves, which is why it is gated"
        );
    }

    /// Nothing above the slot at the highest DPI this module will ever raster
    /// can be seen, and an uncapped upload is megabytes of base64 in every
    /// card SVG — which is returned inline in a JSON response.
    #[test]
    fn an_oversized_logo_is_capped() {
        let big =
            prepare_logo(&solid(3000, 1500, [20, 20, 20, 255]), false, TEAL).expect("accepted");
        assert_eq!(big.width, MAX_LOGO_PX, "15 mm at 2400 DPI");
        assert!(big.height < MAX_LOGO_PX, "aspect preserved");
    }

    /// SVG text does not wrap, so a long name is a name that runs off the card
    /// unless something stops it. Something stops it.
    #[test]
    fn a_long_shop_name_is_cut_rather_than_left_to_overflow() {
        let mut o = org(GOLD);
        o.name = "The Extremely Long Coffee House Of Heliopolis".into();
        let svg =
            render_qr_card_svg(&opts(Some(card_brand(&o).expect("on the tier")))).expect("svg");
        assert!(svg.contains('…'), "elided");
        assert!(!svg.contains("Heliopolis"));
    }

    /// An org row that has gone missing has no name, and a card with a gap
    /// where the name goes reads as a rendering fault. It simply has no name
    /// line, which is a finished card.
    #[test]
    fn a_nameless_org_gets_a_card_without_a_name_line() {
        let mut o = org(GOLD);
        o.name = "   ".into();
        let svg =
            render_qr_card_svg(&opts(Some(card_brand(&o).expect("on the tier")))).expect("svg");
        assert!(svg.contains("Powered by Madar"));
        assert_eq!(svg.matches("<text").count(), 1, "only the Madar line");
    }
}
