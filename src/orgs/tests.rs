use actix_web::{App, test, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;
use crate::orgs::handlers::Org;
use crate::orgs::routes;

fn get_secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn generate_token(user_id: Uuid, org_id: Option<Uuid>, role: UserRole) -> String {
    crate::auth::jwt::create_token(&get_secret(), user_id, org_id, role, None, 24).unwrap()
}

fn generate_super_admin_token() -> String {
    generate_token(Uuid::new_v4(), None, UserRole::SuperAdmin)
}

fn generate_org_admin_token(org_id: Uuid) -> String {
    generate_token(Uuid::new_v4(), Some(org_id), UserRole::OrgAdmin)
}

fn multipart_body(fields: &[(&str, &str)]) -> String {
    let mut body = String::new();
    for (name, val) in fields {
        body.push_str("--boundary\r\n");
        body.push_str(&format!(
            "Content-Disposition: form-data; name=\"{}\"\r\n\r\n",
            name
        ));
        body.push_str(val);
        body.push_str("\r\n");
    }
    body.push_str("--boundary--\r\n");
    body
}

#[sqlx::test]
async fn test_create_org_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let token = generate_super_admin_token();
    let body = multipart_body(&[
        ("name", "Test Organization"),
        ("slug", "test-org"),
        ("currency_code", "USD"),
        ("tax_rate", "0.05"),
    ]);

    let req = test::TestRequest::post()
        .uri("/orgs")
        .insert_header(("Content-Type", "multipart/form-data; boundary=boundary"))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(
        resp.status().is_success(),
        "Response was {:?}",
        resp.status()
    );

    let org: Org = test::read_body_json(resp).await;
    assert_eq!(org.name, "Test Organization");
    assert_eq!(org.slug.as_deref(), Some("test-org"));
    assert_eq!(org.currency_code, "USD");
    // Depending on DB mapping, tax_rate could be parsed differently, but it should succeed.
}

#[sqlx::test]
async fn test_create_org_conflict(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let token = generate_super_admin_token();

    // First org
    let body = multipart_body(&[("name", "Org 1"), ("slug", "shared-slug")]);
    let req = test::TestRequest::post()
        .uri("/orgs")
        .insert_header(("Content-Type", "multipart/form-data; boundary=boundary"))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    // Second org with same slug
    let body2 = multipart_body(&[("name", "Org 2"), ("slug", "shared-slug")]);
    let req2 = test::TestRequest::post()
        .uri("/orgs")
        .insert_header(("Content-Type", "multipart/form-data; boundary=boundary"))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(body2)
        .to_request();
    let resp2 = test::call_service(&app, req2).await;
    assert_eq!(resp2.status(), actix_web::http::StatusCode::CONFLICT);
}

#[sqlx::test]
async fn test_create_org_unauthorized(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let body = multipart_body(&[("name", "Org"), ("slug", "slug")]);
    let req = test::TestRequest::post()
        .uri("/orgs")
        .insert_header(("Content-Type", "multipart/form-data; boundary=boundary"))
        .set_payload(body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn test_list_orgs(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    // Seed orgs
    sqlx::query!("INSERT INTO organizations (name, slug) VALUES ('A', 'a'), ('B', 'b')")
        .execute(&pool)
        .await
        .unwrap();

    let token = generate_super_admin_token();
    let req = test::TestRequest::get()
        .uri("/orgs")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let orgs: Vec<Org> = test::read_body_json(resp).await;
    assert_eq!(orgs.len(), 2);
    // Ordered by name
    assert_eq!(orgs[0].name, "A");
    assert_eq!(orgs[1].name, "B");
}

#[sqlx::test]
async fn test_get_org(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = Uuid::new_v4();
    sqlx::query!(
        "INSERT INTO organizations (id, name, slug) VALUES ($1, 'Test Org', 'test')",
        org_id
    )
    .execute(&pool)
    .await
    .unwrap();

    // SuperAdmin
    let token = generate_super_admin_token();
    let req = test::TestRequest::get()
        .uri(&format!("/orgs/{}", org_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let org: Org = test::read_body_json(resp).await;
    assert_eq!(org.id, org_id);

    sqlx::query!("INSERT INTO role_permissions (role, resource, action, granted) VALUES ('org_admin'::user_role, 'orgs'::permission_resource, 'read'::permission_action, true)")
        .execute(&pool)
        .await
        .unwrap();

    // Same Org Admin
    let admin_token = generate_org_admin_token(org_id);
    let req2 = test::TestRequest::get()
        .uri(&format!("/orgs/{}", org_id))
        .insert_header(("Authorization", format!("Bearer {}", admin_token)))
        .to_request();
    let resp2 = test::call_service(&app, req2).await;
    assert!(resp2.status().is_success());

    // Different Org Admin
    let other_admin_token = generate_org_admin_token(Uuid::new_v4());
    let req3 = test::TestRequest::get()
        .uri(&format!("/orgs/{}", org_id))
        .insert_header(("Authorization", format!("Bearer {}", other_admin_token)))
        .to_request();
    let resp3 = test::call_service(&app, req3).await;
    assert_eq!(resp3.status(), actix_web::http::StatusCode::FORBIDDEN);
}

// Offline-auth bundle: returns argon2id PIN verifiers for the org's PIN-login
// roles (teller, waiter, kitchen — with null for those who never logged in
// online). Email/password roles (org_admin, etc.) are excluded.
#[sqlx::test]
async fn test_offline_auth_bundle_returns_org_tellers(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org', $2)")
        .bind(org_id)
        .bind(format!("org-{org_id}"))
        .execute(&pool)
        .await
        .unwrap();

    // Teller WITH an offline hash (has logged in online before).
    let off_hash = crate::auth::offline::hash_offline_pin("1234").unwrap();
    sqlx::query("INSERT INTO users (id, org_id, name, role, pin_hash, offline_pin_hash) VALUES ($1,$2,'Alice','teller'::user_role,'h',$3)")
        .bind(Uuid::new_v4()).bind(org_id).bind(&off_hash).execute(&pool).await.unwrap();
    // Teller WITHOUT (never logged in online) → null hash.
    sqlx::query("INSERT INTO users (id, org_id, name, role, pin_hash) VALUES ($1,$2,'Bob','teller'::user_role,'h')")
        .bind(Uuid::new_v4()).bind(org_id).execute(&pool).await.unwrap();
    // A WAITER with an offline hash MUST appear (offline fire-now-pay-later).
    let waiter_hash = crate::auth::offline::hash_offline_pin("2345").unwrap();
    sqlx::query("INSERT INTO users (id, org_id, name, role, pin_hash, offline_pin_hash) VALUES ($1,$2,'Wendy','waiter'::user_role,'h',$3)")
        .bind(Uuid::new_v4()).bind(org_id).bind(&waiter_hash).execute(&pool).await.unwrap();
    // A KITCHEN device user with an offline hash MUST appear (offline KDS unlock).
    let kitchen_hash = crate::auth::offline::hash_offline_pin("3456").unwrap();
    sqlx::query("INSERT INTO users (id, org_id, name, role, pin_hash, offline_pin_hash) VALUES ($1,$2,'Kds1','kitchen'::user_role,'h',$3)")
        .bind(Uuid::new_v4()).bind(org_id).bind(&kitchen_hash).execute(&pool).await.unwrap();
    // A non-PIN role in the org must NOT appear in the bundle.
    sqlx::query("INSERT INTO users (id, org_id, name, email, password_hash, role) VALUES ($1,$2,'Mgr',$3,'h','org_admin'::user_role)")
        .bind(Uuid::new_v4()).bind(org_id).bind(format!("m-{org_id}@t.com")).execute(&pool).await.unwrap();

    let token = generate_org_admin_token(org_id);
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/orgs/{org_id}/offline-auth-bundle"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success(), "got {:?}", resp.status());
    let body: serde_json::Value = test::read_body_json(resp).await;
    let tellers = body["tellers"].as_array().unwrap();
    assert_eq!(tellers.len(), 4, "teller + waiter + kitchen, no org_admin");
    let alice = tellers.iter().find(|t| t["name"] == "Alice").unwrap();
    assert_eq!(alice["offline_pin_hash"].as_str().unwrap(), off_hash);
    let bob = tellers.iter().find(|t| t["name"] == "Bob").unwrap();
    assert!(
        bob["offline_pin_hash"].is_null(),
        "Bob never logged in online → null"
    );
    let wendy = tellers.iter().find(|t| t["name"] == "Wendy").unwrap();
    assert_eq!(wendy["role"], "waiter");
    assert_eq!(wendy["offline_pin_hash"].as_str().unwrap(), waiter_hash);
    let kds = tellers.iter().find(|t| t["name"] == "Kds1").unwrap();
    assert_eq!(kds["role"], "kitchen");
    assert_eq!(kds["offline_pin_hash"].as_str().unwrap(), kitchen_hash);
    assert!(
        tellers.iter().all(|t| t["name"] != "Mgr"),
        "org_admin excluded"
    );

    // The bundle ships the org's stable LAN secret (32 bytes → 64 hex chars).
    let lan_secret = body["lan_secret"].as_str().expect("lan_secret present");
    assert_eq!(lan_secret.len(), 64, "32-byte secret hex-encoded");
    assert!(lan_secret.chars().all(|c| c.is_ascii_hexdigit()), "hex");
}

// Authorization: a token from a DIFFERENT org cannot fetch this org's bundle.
#[sqlx::test]
async fn test_offline_auth_bundle_cross_org_forbidden(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org', $2)")
        .bind(org_id)
        .bind(format!("org-{org_id}"))
        .execute(&pool)
        .await
        .unwrap();

    let other_token = generate_org_admin_token(Uuid::new_v4());
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/orgs/{org_id}/offline-auth-bundle"))
            .insert_header(("Authorization", format!("Bearer {other_token}")))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::FORBIDDEN);
}

#[sqlx::test]
async fn test_update_org(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = Uuid::new_v4();
    sqlx::query!(
        "INSERT INTO organizations (id, name, slug) VALUES ($1, 'Original Name', 'orig-slug')",
        org_id
    )
    .execute(&pool)
    .await
    .unwrap();

    let token = generate_super_admin_token();
    let req = test::TestRequest::patch()
        .uri(&format!("/orgs/{}", org_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "name": "Updated Name",
            "slug": "updated-slug"
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let org: Org = test::read_body_json(resp).await;
    assert_eq!(org.name, "Updated Name");
    assert_eq!(org.slug.as_deref(), Some("updated-slug"));
}

#[sqlx::test]
async fn test_update_org_conflict(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org1_id = Uuid::new_v4();
    let org2_id = Uuid::new_v4();
    sqlx::query!(
        "INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org 1', 'slug-1')",
        org1_id
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query!(
        "INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org 2', 'slug-2')",
        org2_id
    )
    .execute(&pool)
    .await
    .unwrap();

    let token = generate_super_admin_token();
    let req = test::TestRequest::patch()
        .uri(&format!("/orgs/{}", org1_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "slug": "slug-2"
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::CONFLICT);
}

#[sqlx::test]
async fn test_delete_org(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = Uuid::new_v4();
    sqlx::query!(
        "INSERT INTO organizations (id, name, slug) VALUES ($1, 'To Delete', 'to-del')",
        org_id
    )
    .execute(&pool)
    .await
    .unwrap();

    let token = generate_super_admin_token();
    let req = test::TestRequest::delete()
        .uri(&format!("/orgs/{}", org_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    // Verify it's deleted (fetch again)
    let req2 = test::TestRequest::get()
        .uri(&format!("/orgs/{}", org_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp2 = test::call_service(&app, req2).await;
    assert_eq!(resp2.status(), actix_web::http::StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn test_delete_org_not_found(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let token = generate_super_admin_token();
    let req = test::TestRequest::delete()
        .uri(&format!("/orgs/{}", Uuid::new_v4()))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn test_upload_org_logo(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = Uuid::new_v4();
    sqlx::query!(
        "INSERT INTO organizations (id, name, slug) VALUES ($1, 'Logo Org', 'logo-org')",
        org_id
    )
    .execute(&pool)
    .await
    .unwrap();

    let token = generate_super_admin_token();

    // A real PNG, because the handler decodes what it is given now: it has to
    // look at the alpha channel to decide the stored format and the
    // `brand_logo_is_mark` flag, so bytes that are not an image cannot get
    // through here any more.
    let req = test::TestRequest::put()
        .uri(&format!("/orgs/{}/logo", org_id))
        .insert_header(("Content-Type", "multipart/form-data; boundary=boundary"))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(logo_multipart(&tiny_transparent_png()))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success(), "got {}", resp.status());

    let org: Org = test::read_body_json(resp).await;
    let logo_url = org.logo_url.expect("a logo url");
    // Transparency survives, so the file is a PNG and the card may still
    // repaint the mark.
    assert!(
        logo_url.ends_with(".png"),
        "a transparent logo must stay a PNG, got {logo_url}"
    );

    // And the same route now refuses a file that is not an image at all,
    // rather than writing it to disk and serving it as a logo.
    let req = test::TestRequest::put()
        .uri(&format!("/orgs/{}/logo", org_id))
        .insert_header(("Content-Type", "multipart/form-data; boundary=boundary"))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(logo_multipart(b"fake-image-bytes"))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status().as_u16(), 400);
}

/// A 64x64 PNG that is mostly clear — the shape of a real uploaded mark.
fn tiny_transparent_png() -> Vec<u8> {
    let mut img = image::RgbaImage::from_pixel(64, 64, image::Rgba([0, 0, 0, 0]));
    for y in 24..40 {
        for x in 24..40 {
            img.put_pixel(x, y, image::Rgba([17, 17, 17, 255]));
        }
    }
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut buf, image::ImageFormat::Png)
        .unwrap();
    buf.into_inner()
}

/// Wrap file bytes as the `logo` field of a multipart body. Binary, so it is
/// built as bytes rather than as a `String`.
fn logo_multipart(file: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(b"--boundary\r\n");
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"logo\"; filename=\"test.png\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: image/png\r\n\r\n");
    body.extend_from_slice(file);
    body.extend_from_slice(b"\r\n--boundary--\r\n");
    body
}

// ═══════════════════════════════════════════════════════════════════
// Onboarding — derived checklist + completion flag
// ═══════════════════════════════════════════════════════════════════

async fn seed_org_row(pool: &PgPool) -> Uuid {
    let org_id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Onb', $2)")
        .bind(org_id)
        .bind(format!("onb-{org_id}"))
        .execute(pool)
        .await
        .unwrap();
    org_id
}

async fn grant_org_permission(pool: &PgPool, action: &str) {
    sqlx::query(&format!(
        "INSERT INTO role_permissions (role, resource, action, granted) \
         VALUES ('org_admin'::user_role, 'orgs'::permission_resource, '{action}'::permission_action, true) \
         ON CONFLICT DO NOTHING"
    ))
    .execute(pool)
    .await
    .unwrap();
}

#[sqlx::test]
async fn test_onboarding_checklist_and_complete(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org_row(&pool).await;
    grant_org_permission(&pool, "read").await;
    grant_org_permission(&pool, "update").await;
    let token = generate_org_admin_token(org_id);

    // Fresh org: nothing set up → not completable, not completed.
    let req = test::TestRequest::get()
        .uri(&format!("/orgs/{org_id}/onboarding"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success(), "got {:?}", resp.status());
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["completed"], false);
    assert_eq!(body["can_complete"], false);
    let steps = body["steps"].as_array().unwrap();
    assert!(steps.iter().all(|s| s["done"] == false));

    // Satisfy the required steps: branch + payment method + category + item.
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'B')")
        .bind(Uuid::new_v4())
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO org_payment_methods (org_id, name, label_translations, color, icon, is_active) VALUES ($1, 'cash', '{}'::jsonb, '#000', 'cash', true)")
        .bind(org_id).execute(&pool).await.unwrap();
    let cat = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1, $2, 'C')")
        .bind(cat)
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO menu_items (id, org_id, category_id, name, base_price, is_active) VALUES ($1, $2, $3, 'Latte', 7000, true)")
        .bind(Uuid::new_v4()).bind(org_id).bind(cat).execute(&pool).await.unwrap();

    let req = test::TestRequest::get()
        .uri(&format!("/orgs/{org_id}/onboarding"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let body: serde_json::Value = test::read_body_json(test::call_service(&app, req).await).await;
    assert_eq!(body["can_complete"], true);
    assert_eq!(body["completed"], false);

    // Complete — idempotent, persists, returns the fresh status.
    for _ in 0..2 {
        let req = test::TestRequest::post()
            .uri(&format!("/orgs/{org_id}/onboarding/complete"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["completed"], true);
        assert!(body["completed_at"].is_string());
    }
}

/// The optional `org_profile` step flips to done once the org has a logo
/// (currency/tax carry NOT-NULL defaults, so a logo is the only honest
/// "they personalized it" signal). It is never required, so it must not
/// gate `can_complete`.
#[sqlx::test]
async fn test_onboarding_org_profile_step(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org_row(&pool).await;
    grant_org_permission(&pool, "read").await;
    let token = generate_org_admin_token(org_id);

    let fetch = |token: String| {
        let app = &app;
        async move {
            let req = test::TestRequest::get()
                .uri(&format!("/orgs/{org_id}/onboarding"))
                .insert_header(("Authorization", format!("Bearer {token}")))
                .to_request();
            test::read_body_json::<serde_json::Value, _>(test::call_service(app, req).await).await
        }
    };

    let body = fetch(token.clone()).await;
    let profile = body["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["key"] == "org_profile")
        .expect("org_profile step present");
    assert_eq!(profile["done"], false);
    assert_eq!(profile["required"], false);

    sqlx::query("UPDATE organizations SET logo_url = 'logo.png' WHERE id = $1")
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();

    let body = fetch(token).await;
    let profile = body["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["key"] == "org_profile")
        .unwrap();
    assert_eq!(profile["done"], true);
}

/// A rate has to come BACK as a number, not a string.
///
/// "I set it to 14, it says saved, and nothing changes" is what a string on
/// this field looks like from a dashboard: the write lands, and the form that
/// reads it back runs `Number.isFinite("0.14")`, gets false, and shows zero —
/// so the setting appears to do nothing while the database has exactly what
/// was asked for. The column is `numeric` and the field is a `BigDecimal`, and
/// nothing until now pinned which of the two shapes serde picks.
#[sqlx::test]
async fn a_rate_comes_back_as_a_json_number(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, tax_rate) VALUES ($1,'O','o-json',0.05)",
    )
    .bind(org_id)
    .execute(&pool)
    .await
    .unwrap();
    let token = generate_super_admin_token();

    let resp = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/orgs/{}", org_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&serde_json::json!({
                "tax_rate": 0.14,
                "service_charge_rate": 0.12,
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = test::read_body_json(resp).await;

    for field in ["tax_rate", "service_charge_rate"] {
        assert!(
            body[field].is_number(),
            "{field} came back as {:?} — a dashboard reading this with \
             Number.isFinite gets false and shows zero",
            body[field]
        );
    }
    assert_eq!(body["tax_rate"].as_f64().unwrap(), 0.14);
    assert_eq!(body["service_charge_rate"].as_f64().unwrap(), 0.12);
}

/// V16: tax_rate outside [0, 1] must be rejected (negative or >100%).
#[sqlx::test]
async fn test_update_org_rejects_out_of_range_tax_rate(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, tax_rate) VALUES ($1,'O','o-tax-16',0.14)",
    )
    .bind(org_id)
    .execute(&pool)
    .await
    .unwrap();
    let token = generate_super_admin_token();

    for bad in [-0.5_f64, 5.0_f64] {
        let resp = test::call_service(
            &app,
            test::TestRequest::patch()
                .uri(&format!("/orgs/{}", org_id))
                .insert_header(("Authorization", format!("Bearer {}", token)))
                .set_json(&serde_json::json!({"tax_rate": bad}))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 400, "tax_rate {bad} must be rejected");
    }

    // The persisted value is unchanged by the rejected calls.
    let tr: sqlx::types::BigDecimal =
        sqlx::query_scalar("SELECT tax_rate FROM organizations WHERE id=$1")
            .bind(org_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(tr.to_string().parse::<f64>().unwrap(), 0.14);

    // A valid rate is accepted.
    let resp = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/orgs/{}", org_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&serde_json::json!({"tax_rate": 0.2}))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success());
}

// ── The public brand lookup ──────────────────────────────────────────────────
//
// The first request a customer's browser makes, before there is any notion of a
// session, and the only thing standing between a wildcard subdomain and a list
// of our customers.

fn brand_app(
    pool: &PgPool,
) -> impl std::future::Future<
    Output = impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
> {
    let pool = pool.clone();
    async move {
        test::init_service(
            App::new()
                .app_data(web::Data::new(pool))
                .app_data(web::Data::new(get_secret()))
                .configure(routes::configure),
        )
        .await
    }
}

async fn seed_shop(pool: &PgPool, name: &str, slug: Option<&str>) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug, is_active) VALUES ($1, $2, $3, true)")
        .bind(id)
        .bind(name)
        .bind(slug)
        .execute(pool)
        .await
        .unwrap();
    id
}

#[sqlx::test]
async fn a_shop_is_found_by_its_short_name(pool: PgPool) {
    let id = seed_shop(&pool, "Drops", Some("drops")).await;
    let app = brand_app(&pool).await;

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/public/orgs/brand?slug=drops")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["org_id"].as_str().unwrap(), id.to_string());
    assert_eq!(body["slug"], "drops");
}

/// A blank `?slug=` is not a shop that might exist — it is no name at all.
///
/// It used to MATCH, back when a shop with no address carried `''`: the org
/// specifically meant to have no public identity was handed back whole — name,
/// branding, logo, org id. The column holds NULL for that now, so the match is
/// gone at the source too; this pins the QUERY side, which is the half a stored
/// value cannot fix. No hostname can reach it either way (a first label is
/// never empty), but a public endpoint should not answer a question nobody
/// asked.
#[sqlx::test]
async fn a_blank_short_name_does_not_resolve_the_shop_that_has_none(pool: PgPool) {
    seed_shop(&pool, "Rue", None).await;
    let app = brand_app(&pool).await;

    for uri in [
        "/public/orgs/brand?slug=",
        "/public/orgs/brand?slug=%20",
        "/public/orgs/brand",
    ] {
        let resp = test::call_service(&app, test::TestRequest::get().uri(uri).to_request()).await;
        assert_eq!(
            resp.status().as_u16(),
            400,
            "{uri} must name no shop at all, not the one with no name"
        );
    }
}

/// A switched-off shop and a shop that never existed answer identically.
///
/// A wildcard subdomain answers for every name anyone types, so a lookup that
/// distinguished the two would be an enumeration tool for the whole customer
/// list — type names, keep the ones that 403 instead of 404.
#[sqlx::test]
async fn an_inactive_shop_is_indistinguishable_from_no_shop(pool: PgPool) {
    let id = seed_shop(&pool, "Closed", Some("closed")).await;
    sqlx::query("UPDATE organizations SET is_active = false WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let app = brand_app(&pool).await;

    let mut bodies = Vec::new();
    for uri in [
        "/public/orgs/brand?slug=closed",
        "/public/orgs/brand?slug=never-existed-xyz",
    ] {
        let resp = test::call_service(&app, test::TestRequest::get().uri(uri).to_request()).await;
        assert_eq!(resp.status().as_u16(), 404, "{uri}");
        bodies.push(test::read_body(resp).await);
    }
    assert_eq!(
        bodies[0], bodies[1],
        "the two answers must be byte-identical"
    );
}

// ── A shop without an address has no slug ────────────────────────────────────

/// The database holds the invariant, not just the handler.
///
/// `slug` was NOT NULL, so a shop from before slugs existed carried `''` — "no
/// slug" wearing the costume of a slug. It broke three things: the branding
/// freeze read it as a name worth protecting and made the org uneditable, the
/// public brand lookup matched it, and — quietly — `uq_organizations_slug` is
/// UNIQUE, so `''` occupied a slot and a SECOND such org would have collided.
#[sqlx::test]
async fn a_shop_without_an_address_has_no_slug_rather_than_a_blank_one(pool: PgPool) {
    let blank = sqlx::query("INSERT INTO organizations (name, slug) VALUES ('Blank', '')")
        .execute(&pool)
        .await;
    assert!(blank.is_err(), "an empty slug is no longer storable");

    let spaces = sqlx::query("INSERT INTO organizations (name, slug) VALUES ('Spaces', '   ')")
        .execute(&pool)
        .await;
    assert!(spaces.is_err(), "nor a blank one");

    // NULL is the state `''` was pretending to be, and a btree unique index
    // treats NULLs as distinct — so ANY number of shops may have no address.
    for name in ["No Address One", "No Address Two"] {
        sqlx::query("INSERT INTO organizations (name, slug) VALUES ($1, NULL)")
            .bind(name)
            .execute(&pool)
            .await
            .expect("two shops with no address must coexist");
    }

    // A real slug is still unique among the living.
    sqlx::query("INSERT INTO organizations (name, slug) VALUES ('Drops', 'drops')")
        .execute(&pool)
        .await
        .unwrap();
    let dupe = sqlx::query("INSERT INTO organizations (name, slug) VALUES ('Drops Two', 'drops')")
        .execute(&pool)
        .await;
    assert!(dupe.is_err(), "a name that IS an address is still taken");
}

/// The deadlock that made an organisation uneditable, end to end.
///
/// Branded + no slug: the freeze must not fire, because nothing is printed on
/// a name that does not exist. Then, once it has one, it stops moving.
#[sqlx::test]
async fn a_branded_shop_with_no_address_can_still_be_given_one(pool: PgPool) {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, custom_branding) VALUES ($1, 'Rue', NULL, true)",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let token = generate_super_admin_token();
    let patch = |slug: &str| {
        test::TestRequest::patch()
            .uri(&format!("/orgs/{id}"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(serde_json::json!({ "slug": slug }))
            .to_request()
    };

    let resp = test::call_service(&app, patch("rue")).await;
    assert_eq!(resp.status().as_u16(), 200, "a blank name freezes nothing");
    let org: Org = test::read_body_json(resp).await;
    assert_eq!(org.slug.as_deref(), Some("rue"));

    // And now it is a hostname on a printed card.
    let resp = test::call_service(&app, patch("rue-coffee")).await;
    assert_eq!(resp.status().as_u16(), 409, "a real name is load-bearing");
}
