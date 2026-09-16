//! Device activation codes (POS_SIGNIN_OVERHAUL.md §4): an owner issues a code,
//! a tablet binds itself with it, and nothing about a person is involved.

use actix_web::{App, http::StatusCode, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;

fn secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn token(user: Uuid, org: Uuid, role: UserRole) -> String {
    crate::auth::jwt::create_token(&secret(), user, Some(org), role, None, 24).unwrap()
}

async fn org(pool: &PgPool, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(name)
        .bind(format!("o-{id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn branch(pool: &PgPool, org: Uuid, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(org)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn user(pool: &PgPool, org: Uuid, role: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, role, email, password_hash)
         VALUES ($1, $2, $3, $4::user_role, $5, 'h')",
    )
    .bind(id)
    .bind(org)
    .bind(format!("U {id}"))
    .bind(role)
    .bind(format!("{id}@t.com"))
    .execute(pool)
    .await
    .unwrap();
    id
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .configure(crate::devices::routes::configure)
                .configure(crate::auth::routes::configure),
        )
        .await
    };
}

async fn call(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    req: test::TestRequest,
    bearer: Option<&str>,
) -> (StatusCode, Value) {
    let req = match bearer {
        Some(b) => req.insert_header(("Authorization", format!("Bearer {b}"))),
        None => req,
    };
    let resp = test::call_service(app, req.to_request()).await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

async fn seed(pool: &PgPool) {
    crate::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
}

#[sqlx::test]
async fn an_owner_issues_a_code_and_a_tablet_binds_itself_once(pool: PgPool) {
    seed(&pool).await;
    let app = app!(pool);
    let o = org(&pool, "Rue").await;
    let b = branch(&pool, o, "Maadi").await;
    let owner = user(&pool, o, "org_admin").await;
    let bearer = token(owner, o, UserRole::OrgAdmin);

    let (st, code) = call(
        &app,
        test::TestRequest::post()
            .uri("/devices/activation-codes")
            .set_json(json!({"branch_id": b, "label": "Front counter"})),
        Some(&bearer),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{code}");
    assert_eq!(code["state"], "free");
    let digits = code["code"].as_str().unwrap().to_string();
    assert_eq!(digits.len(), 8);

    // The tablet: no bearer, no person.
    let device = Uuid::new_v4();
    let (st, act) = call(
        &app,
        test::TestRequest::post()
            .uri("/auth/activate-device")
            .set_json(json!({
                "code": digits, "device_id": device, "device_code": "t1", "platform": "android"
            })),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{act}");
    assert_eq!(act["org_id"], o.to_string());
    assert_eq!(act["branch_id"], b.to_string());
    assert_eq!(act["branch_name"], "Maadi");
    assert_eq!(act["org_name"], "Rue");
    assert_eq!(act["device"]["code"], "T1");
    assert_eq!(act["device"]["label"], "Front counter");
    let dev_token = act["device_token"].as_str().unwrap().to_string();
    assert!(
        crate::devices::activation::verify_credential(&pool, device, &dev_token)
            .await
            .unwrap()
    );
    let stored: String = sqlx::query_scalar("SELECT credential_hash FROM devices WHERE id = $1")
        .bind(device)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_ne!(stored, dev_token, "only a hash is kept");

    // Single use: a second tablet gets the same answer as a made-up code.
    let (st, again) = call(
        &app,
        test::TestRequest::post()
            .uri("/auth/activate-device")
            .set_json(json!({
                "code": digits, "device_id": Uuid::new_v4()
            })),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(again["code"], "ACTIVATION_CODE_INVALID");
    let (st, _) = call(
        &app,
        test::TestRequest::post()
            .uri("/auth/activate-device")
            .set_json(json!({
                "code": "00000000", "device_id": Uuid::new_v4()
            })),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // The list shows it used, by this device.
    let (st, list) = call(
        &app,
        test::TestRequest::get().uri(&format!("/devices/activation-codes?branch_id={b}")),
        Some(&bearer),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(list[0]["state"], "used");
    assert_eq!(list[0]["used_by_device"], device.to_string());

    // Deactivating the device kills its credential.
    let (st, _) = call(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/devices/{device}"))
            .set_json(json!({"retired": true})),
        Some(&bearer),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        !crate::devices::activation::verify_credential(&pool, device, &dev_token)
            .await
            .unwrap()
    );
}

#[sqlx::test]
async fn revoked_and_expired_codes_do_not_bind(pool: PgPool) {
    seed(&pool).await;
    let app = app!(pool);
    let o = org(&pool, "Rue").await;
    let b = branch(&pool, o, "Arkan").await;
    let owner = user(&pool, o, "org_admin").await;
    let bearer = token(owner, o, UserRole::OrgAdmin);

    let issue = || {
        test::TestRequest::post()
            .uri("/devices/activation-codes")
            .set_json(json!({"branch_id": b}))
    };
    let (_, revoked) = call(&app, issue(), Some(&bearer)).await;
    let (st, r) = call(
        &app,
        test::TestRequest::post().uri(&format!(
            "/devices/activation-codes/{}/revoke",
            revoked["id"].as_str().unwrap()
        )),
        Some(&bearer),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(r["state"], "revoked");

    let (_, expired) = call(&app, issue(), Some(&bearer)).await;
    sqlx::query(
        "UPDATE device_activation_codes SET expires_at = now() - interval '1 minute' WHERE id = $1",
    )
    .bind(Uuid::parse_str(expired["id"].as_str().unwrap()).unwrap())
    .execute(&pool)
    .await
    .unwrap();

    for c in [&revoked, &expired] {
        let (st, _) = call(
            &app,
            test::TestRequest::post()
                .uri("/auth/activate-device")
                .set_json(json!({
                    "code": c["code"], "device_id": Uuid::new_v4()
                })),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }
    let (_, list) = call(
        &app,
        test::TestRequest::get().uri(&format!("/devices/activation-codes?branch_id={b}")),
        Some(&bearer),
    )
    .await;
    let states: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["state"].as_str().unwrap())
        .collect();
    assert!(
        states.contains(&"revoked") && states.contains(&"expired"),
        "{states:?}"
    );
}

#[sqlx::test]
async fn a_teller_cannot_issue_codes_and_a_code_never_rehomes_another_orgs_device(pool: PgPool) {
    seed(&pool).await;
    let app = app!(pool);
    let o = org(&pool, "Rue").await;
    let b = branch(&pool, o, "Arkan").await;
    let teller = user(&pool, o, "teller").await;
    let (st, _) = call(
        &app,
        test::TestRequest::post()
            .uri("/devices/activation-codes")
            .set_json(json!({"branch_id": b})),
        Some(&token(teller, o, UserRole::Teller)),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    // A device already registered to ANOTHER org is not taken over by a code.
    let other = org(&pool, "Other").await;
    let ob = branch(&pool, other, "Elsewhere").await;
    let device = Uuid::new_v4();
    sqlx::query("INSERT INTO devices (id, org_id, branch_id, code) VALUES ($1, $2, $3, 'X1')")
        .bind(device)
        .bind(other)
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    let owner = user(&pool, o, "org_admin").await;
    let (_, code) = call(
        &app,
        test::TestRequest::post()
            .uri("/devices/activation-codes")
            .set_json(json!({"branch_id": b})),
        Some(&token(owner, o, UserRole::OrgAdmin)),
    )
    .await;
    let (st, _) = call(
        &app,
        test::TestRequest::post()
            .uri("/auth/activate-device")
            .set_json(json!({
                "code": code["code"], "device_id": device
            })),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let still_free: bool =
        sqlx::query_scalar("SELECT used_at IS NULL FROM device_activation_codes WHERE code = $1")
            .bind(code["code"].as_str().unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        still_free,
        "the refused claim rolled back; the code stays free"
    );
}

/// Phase 4 (PERMISSIONS_ARCHITECTURE §4.4): an activated device fetches a
/// snapshot of its branch signed by a key the server publishes; it lists the
/// people who may sign in there, and nothing without the device credential.
#[sqlx::test]
async fn an_activated_device_gets_a_signed_snapshot_of_its_branch(pool: PgPool) {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    seed(&pool).await;
    let app = app!(pool);
    let o = org(&pool, "Rue").await;
    let b = branch(&pool, o, "Maadi").await;
    let owner = user(&pool, o, "org_admin").await;
    let teller = user(&pool, o, "teller").await;
    sqlx::query("UPDATE users SET pin_hash = 'x' WHERE id = $1")
        .bind(teller)
        .execute(&pool)
        .await
        .unwrap();
    let bearer = token(owner, o, UserRole::OrgAdmin);
    let (_, code) = call(
        &app,
        test::TestRequest::post()
            .uri("/devices/activation-codes")
            .set_json(json!({"branch_id": b})),
        Some(&bearer),
    )
    .await;
    let device = Uuid::new_v4();
    let (_, act) = call(
        &app,
        test::TestRequest::post()
            .uri("/auth/activate-device")
            .set_json(json!({"code": code["code"], "device_id": device})),
        None,
    )
    .await;
    let dev_token = act["device_token"].as_str().unwrap().to_string();

    let (st, _) = call(
        &app,
        test::TestRequest::get()
            .uri("/devices/me/authz-snapshot")
            .insert_header(("X-Madar-Device", device.to_string())),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "no credential, no snapshot");

    let (st, snap) = call(
        &app,
        test::TestRequest::get()
            .uri("/devices/me/authz-snapshot")
            .insert_header(("X-Madar-Device", device.to_string()))
            .insert_header(("X-Madar-Device-Token", dev_token)),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{snap}");
    let signed: madar_authz::snapshot::SignedSnapshot = serde_json::from_value(snap).unwrap();
    assert_eq!(signed.body.branch_id, b.to_string());
    assert_eq!(signed.body.device_id, device.to_string());
    assert_eq!(
        signed.body.expires_at,
        i64::MAX,
        "no expiry (locked decision)"
    );
    assert!(
        signed.body.user(&teller.to_string()).is_some(),
        "the teller signs in here"
    );

    let (_, keys) = call(&app, test::TestRequest::get().uri("/auth/authz-keys"), None).await;
    let key = keys
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["kid"] == signed.kid.as_str())
        .expect("the signing key is published");
    let unhex = |s: &str| -> Vec<u8> {
        s.as_bytes()
            .chunks(2)
            .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap())
            .collect()
    };
    let pk: [u8; 32] = unhex(key["public_key"].as_str().unwrap())
        .try_into()
        .unwrap();
    let sig: [u8; 64] = unhex(&signed.sig).try_into().unwrap();
    VerifyingKey::from_bytes(&pk)
        .unwrap()
        .verify(&signed.body.signing_bytes(), &Signature::from_bytes(&sig))
        .expect("the signature verifies");
}
