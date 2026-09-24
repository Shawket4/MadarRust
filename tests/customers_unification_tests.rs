//! The customers unification (CUSTOMERS_UNIFICATION_DESIGN.md), end to end: the
//! shared phone rule, `resolve_or_create` under concurrency, the public join,
//! a merge of two members, the token alias, and the seeded data migration.
//!
//! An integration suite rather than `src/**/tests.rs`: the single lib test
//! binary is large enough that XProtect kills it on this machine (see
//! `XPROTECT_FALSE_POSITIVE.md`); a binary that links the library the way the
//! server does is not matched.

use std::borrow::Cow;

use actix_web::{App, http::StatusCode, test, web};
use madar_rust::auth::jwt::{JwtSecret, create_token};
use madar_rust::models::UserRole;
use madar_rust::realtime::hub::BranchEventHub;
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::migrate::Migrator;
use uuid::Uuid;

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

fn secret() -> JwtSecret {
    JwtSecret("secret".into())
}
fn token(uid: Uuid, org: Uuid, role: UserRole) -> String {
    create_token(&secret(), uid, Some(org), role, None, 24).unwrap()
}
fn u(s: &str) -> Uuid {
    Uuid::parse_str(s).unwrap()
}

async fn seed_org(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org', $2)")
        .bind(id)
        .bind(format!("org-{id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}
async fn seed_branch(pool: &PgPool, org: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Branch')")
        .bind(id)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    id
}
async fn seed_user(pool: &PgPool, org: Uuid, role: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, $3, $4, 'h', $5::user_role)",
    )
    .bind(id)
    .bind(org)
    .bind(format!("{role}-{id}"))
    .bind(format!("{id}@t.com"))
    .bind(role)
    .execute(pool)
    .await
    .unwrap();
    id
}
async fn enable_program(pool: &PgPool, org: Uuid) {
    sqlx::query(
        "INSERT INTO loyalty_settings \
            (org_id, branch_id, enabled, mode, earn_piastres_per_point, default_reward_cost, \
             require_otp, stamp_per_line_item) \
         VALUES ($1, NULL, true, 'points', 1000, 100, false, false)",
    )
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
}
async fn i64_of(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new(BranchEventHub::new()))
                .configure(madar_rust::customers::routes::configure)
                .configure(madar_rust::loyalty::routes::configure),
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
    bearer: &str,
) -> (StatusCode, Value) {
    let resp = test::call_service(
        app,
        req.insert_header(("Authorization", format!("Bearer {bearer}")))
            .to_request(),
    )
    .await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn subset(pred: impl Fn(i64) -> bool) -> Migrator {
    let migrations: Vec<_> = MIGRATOR
        .iter()
        .filter(|m| pred(m.version))
        .cloned()
        .collect();
    Migrator {
        migrations: Cow::Owned(migrations),
        ignore_missing: true,
        locking: true,
        no_tx: false,
    }
}

/// A loyalty member, the way the unified model stores one: a `customers` row
/// (the person) and a `loyalty_customers` row under THE SAME id (the card).
/// `phone` is kept as typed and keyed through the database's own
/// `phone_canonical`, so a deliberately bad phone seeds a member with no key.
async fn seed_loyalty_member(
    pool: &PgPool,
    org: Uuid,
    phone: &str,
    name: &str,
    token: &str,
) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO customers (org_id, name, phone, phone_key, source) \
         VALUES ($1, $2, $3, phone_canonical($3), 'loyalty') RETURNING id",
    )
    .bind(org)
    .bind(name)
    .bind(phone)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO loyalty_customers (id, org_id, member_token) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(org)
        .bind(token)
        .execute(pool)
        .await
        .unwrap();
    id
}

/// A brand-new database cloned from `template0`: the test cluster's `template1`
/// may be pre-migrated, and these tests must seed the OLD schema. The returned
/// guard drops the database when the test ends (pass or panic).
async fn fresh(pool: &PgPool) -> (PgPool, FreshDb) {
    let name = format!("_sqlx_test_t0_{}", Uuid::new_v4().simple());
    sqlx::raw_sql(&format!("CREATE DATABASE \"{name}\" TEMPLATE template0"))
        .execute(pool)
        .await
        .expect("create fresh database");
    let base = pool.connect_options().as_ref().clone();
    let fresh = sqlx::pool::PoolOptions::new()
        .max_connections(4)
        .connect_with(base.clone().database(&name))
        .await
        .expect("connect fresh database");
    (fresh, FreshDb { name, base })
}

struct FreshDb {
    name: String,
    base: sqlx::postgres::PgConnectOptions,
}

impl Drop for FreshDb {
    fn drop(&mut self) {
        let name = std::mem::take(&mut self.name);
        let opts = self.base.clone().database("postgres");
        // Drop runs outside any async context guarantee: use a private runtime.
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                use sqlx::Connection;
                if let Ok(mut conn) = sqlx::PgConnection::connect_with(&opts).await {
                    let _ =
                        sqlx::raw_sql(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
                            .execute(&mut conn)
                            .await;
                }
            });
        })
        .join();
    }
}

// ── The shared phone rule ────────────────────────────────────────────────────

/// Every shared vector, through the Rust rule AND the database's
/// `phone_canonical`, with no exception list: the two may never disagree.
#[sqlx::test]
async fn phone_vectors_pass_in_rust_and_in_sql(pool: PgPool) {
    let doc: Value = serde_json::from_str(madar_ids::vectors::PHONE).unwrap();
    let valid = doc["valid"].as_array().unwrap();
    let invalid = doc["invalid"].as_array().unwrap();
    assert!(valid.len() >= 20 && invalid.len() >= 8);
    let sql = async |raw: &str| -> Option<String> {
        sqlx::query_scalar("SELECT phone_canonical($1)")
            .bind(raw)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    for v in valid {
        let (raw, want) = (v[0].as_str().unwrap(), v[1].as_str().unwrap());
        assert_eq!(
            madar_rust::phone::canonical(raw).as_deref(),
            Some(want),
            "rust: {raw:?}"
        );
        assert_eq!(sql(raw).await.as_deref(), Some(want), "sql: {raw:?}");
    }
    for v in invalid {
        let raw = v.as_str().unwrap();
        assert_eq!(madar_rust::phone::canonical(raw), None, "rust: {raw:?}");
        assert_eq!(sql(raw).await, None, "sql: {raw:?}");
    }
    // Rule 6 by name: mobiles are exactly 12 digits, landlines are untouched.
    assert_eq!(madar_rust::phone::canonical("010012345"), None);
    assert_eq!(madar_rust::phone::canonical("0100123456789"), None);
    assert!(madar_rust::phone::canonical("0132345678").is_some());
}

// ── Unification: one person per phone, the card hangs from the person ────────

async fn adjust(pool: &PgPool, org: Uuid, member: Uuid, branch: Uuid, points: i32) {
    sqlx::query(
        "INSERT INTO loyalty_transactions (org_id, customer_id, branch_id, kind, currency, points, source) \
         VALUES ($1, $2, $3, 'adjust', 'points', $4, 'manual')",
    )
    .bind(org)
    .bind(member)
    .bind(branch)
    .bind(points)
    .execute(pool)
    .await
    .unwrap();
}

/// Eight flows meet the same new phone at the same instant, each typing it its
/// own way. The unique index on the canonical key decides; every caller gets
/// the ONE customer and nobody gets an error.
#[sqlx::test]
async fn resolve_or_create_is_safe_under_concurrency(pool: PgPool) {
    use madar_rust::customers::handlers::{CustomerSource, resolve_or_create};
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let spellings = [
        "01001234567",
        "+201001234567",
        "0100 123 4567",
        "201001234567",
        "00201001234567",
        "1001234567",
        "(010) 0123-4567",
        "٠١٠٠١٢٣٤٥٦٧",
    ];
    let mut tasks = Vec::new();
    for (i, phone) in spellings.into_iter().enumerate() {
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            let mut tx = pool.begin().await.unwrap();
            let (id, _) = resolve_or_create(
                &mut tx,
                org,
                phone,
                &format!("Guest {i}"),
                CustomerSource::Online,
                Some(branch),
                None,
                None,
            )
            .await
            .expect("a racing resolve never errors");
            tx.commit().await.unwrap();
            id
        }));
    }
    let mut ids = Vec::new();
    for t in tasks {
        ids.push(t.await.unwrap());
    }
    ids.dedup();
    assert_eq!(
        ids.len(),
        1,
        "every caller resolved to the same customer: {ids:?}"
    );
    let (n, key): (i64, Option<String>) = sqlx::query_as(
        "SELECT count(*), max(phone_key) FROM customers WHERE org_id = $1 AND merged_into IS NULL",
    )
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 1);
    assert_eq!(key.as_deref(), Some("201001234567"));

    // An invalid phone makes no customer at all.
    let mut tx = pool.begin().await.unwrap();
    let refused = resolve_or_create(
        &mut tx,
        org,
        "010012345",
        "Short",
        CustomerSource::Online,
        None,
        None,
        None,
    )
    .await;
    assert!(refused.is_err(), "a truncated mobile is refused (rule 6)");
}

/// Both customers hold a card. The loser's balance crosses to the survivor as
/// a netted pair of adjustments, its membership retires, and its barcode keeps
/// finding the survivor until the alias expires.
#[sqlx::test]
async fn merging_two_members_moves_the_balance_and_aliases_the_card(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let owner = seed_user(&pool, org, "org_admin").await;
    let bearer = token(owner, org, UserRole::OrgAdmin);

    let keep = seed_loyalty_member(&pool, org, "01001234567", "Omar", "Mkeep0001").await;
    let dupe = seed_loyalty_member(&pool, org, "01112345678", "Omar M", "Mdupe0001").await;
    adjust(&pool, org, keep, branch, 30).await;
    adjust(&pool, org, dupe, branch, 12).await;

    let (s, body) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!("/customers/{dupe}/merge"))
            .set_json(json!({ "into": keep })),
        &bearer,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["customer"]["id"], keep.to_string());
    assert_eq!(body["customer"]["is_member"], true);
    assert_eq!(body["customer"]["points_balance"], 42);

    let balances: Vec<(Uuid, i32, bool)> = sqlx::query_as(
        "SELECT id, points_balance, deleted_at IS NOT NULL FROM loyalty_customers \
          WHERE id = ANY($1) ORDER BY points_balance DESC",
    )
    .bind(&[keep, dupe][..])
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(balances, vec![(keep, 42, false), (dupe, 0, true)]);

    // The ledger was appended to, never edited, and the pair nets to zero.
    let (rows, net): (i64, i64) = sqlx::query_as(
        "SELECT count(*), COALESCE(sum(points), 0)::bigint FROM loyalty_transactions \
          WHERE org_id = $1 AND source = 'merge'",
    )
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((rows, net), (2, 0));

    // The duplicate's number is remembered, not lost.
    let remembered: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM customer_phone_history \
          WHERE customer_id = $1 AND phone_key = '201112345678' AND reason = 'merge'",
    )
    .bind(keep)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(remembered, 1);

    // The old card still scans, to the survivor; the survivor's own token wins.
    let by_old = madar_rust::loyalty::model::find_by_token(&pool, "Mdupe0001")
        .await
        .unwrap()
        .expect("the retired card resolves through its alias");
    assert_eq!(by_old.id, keep);
    let by_own = madar_rust::loyalty::model::find_by_token(&pool, "Mkeep0001")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(by_own.id, keep);

    // Ninety days on it is simply an unknown card.
    sqlx::query("UPDATE loyalty_token_aliases SET expires_at = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        madar_rust::loyalty::model::find_by_token(&pool, "Mdupe0001")
            .await
            .unwrap()
            .is_none()
    );
}

/// Only the duplicate holds a card: the merge is refused with a code rather
/// than quietly swapping which customer survives.
#[sqlx::test]
async fn merging_a_member_into_a_non_member_is_refused(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let owner = seed_user(&pool, org, "org_admin").await;
    let bearer = token(owner, org, UserRole::OrgAdmin);
    let member = seed_loyalty_member(&pool, org, "01001234567", "Omar", "Monly0001").await;
    let plain: Uuid = sqlx::query_scalar(
        "INSERT INTO customers (org_id, name, phone, phone_key, source) \
         VALUES ($1, 'Omar', '01112345678', '201112345678', 'dashboard') RETURNING id",
    )
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();

    let (s, body) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!("/customers/{member}/merge"))
            .set_json(json!({ "into": plain })),
        &bearer,
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "CUSTOMER_MERGE_MEMBER_SURVIVES");
    let still: bool = sqlx::query_scalar("SELECT merged_into IS NULL FROM customers WHERE id = $1")
        .bind(member)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(still, "nothing moved");
}

// ── Unification: the card hangs from the person ──────────────────────────────

/// A join makes ONE person and one card, under the same id.
#[sqlx::test]
async fn a_join_creates_the_customer_and_the_card_under_one_id(pool: PgPool) {
    madar_rust::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    enable_program(&pool, org).await;
    let app = app!(pool);

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/public/loyalty/join")
            .set_json(json!({"branch_id": branch, "name": "Ali", "phone": "0100 123 4567", "locale": "ar"}))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success(), "{}", resp.status());

    let rows: Vec<(
        Uuid,
        Uuid,
        String,
        Option<String>,
        Option<String>,
        String,
        Option<String>,
    )> = sqlx::query_as(
        "SELECT c.id, m.id, c.name, c.phone, c.phone_key, c.source, c.locale \
               FROM customers c JOIN loyalty_customers m ON m.id = c.id WHERE c.org_id = $1",
    )
    .bind(org)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
    let (cid, mid, name, phone, key, source, locale) = rows[0].clone();
    assert_eq!(cid, mid, "the membership IS the customer's id");
    assert_eq!(name, "Ali");
    assert_eq!(phone.as_deref(), Some("0100 123 4567"), "kept as typed");
    assert_eq!(key.as_deref(), Some("201001234567"));
    assert_eq!(source, "loyalty");
    assert_eq!(locale.as_deref(), Some("ar"));

    // Joining again, typed another way, makes nobody new.
    let again = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/public/loyalty/join")
            .set_json(json!({"branch_id": branch, "name": "Ali", "phone": "+201001234567"}))
            .to_request(),
    )
    .await;
    assert!(again.status().is_success());
    let n: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM customers WHERE org_id = $1), \
                (SELECT count(*) FROM loyalty_customers WHERE org_id = $1)",
    )
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, (1, 1));
}

/// Staff typed this customer in last month. Joining with the same number gives
/// THAT person a card: same id, their name on file untouched, their source kept.
#[sqlx::test]
async fn a_join_over_an_existing_manual_customer_gives_that_person_the_card(pool: PgPool) {
    madar_rust::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    enable_program(&pool, org).await;
    let existing: Uuid = sqlx::query_scalar(
        "INSERT INTO customers (org_id, name, phone, phone_key, notes, source) \
         VALUES ($1, 'Omar Hassan', '01001234567', '201001234567', 'no sugar', 'pos') RETURNING id",
    )
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    let app = app!(pool);

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/public/loyalty/join")
            .set_json(json!({"branch_id": branch, "name": "omar", "phone": "+20 100 123 4567"}))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success(), "{}", resp.status());

    let member: Uuid = sqlx::query_scalar("SELECT id FROM loyalty_customers WHERE org_id = $1")
        .bind(org)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(member, existing);
    let (n, name, notes, source): (i64, String, Option<String>, String) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM customers WHERE org_id = $1), name, notes, source \
           FROM customers WHERE id = $2",
    )
    .bind(org)
    .bind(existing)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 1, "no second person");
    assert_eq!(
        name, "Omar Hassan",
        "a typed name never overwrites the one on file"
    );
    assert_eq!(notes.as_deref(), Some("no sugar"));
    assert_eq!(source, "pos");
}

// ── The pre-built pass follows the person ───────────────────────────────────

async fn cache_pass(pool: &PgPool, org: Uuid, member: Uuid) {
    sqlx::query(
        "INSERT INTO loyalty_pass_cache (customer_id, org_id, bytes, fingerprint) \
         VALUES ($1, $2, '\\x00'::bytea, 'stale')",
    )
    .bind(member)
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
}
async fn cached(pool: &PgPool, member: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM loyalty_pass_cache WHERE customer_id = $1")
        .bind(member)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A stored pass has the member's name baked in. A rename drops it, a merge
/// drops both sides', and no wallet needs to be configured for that to happen.
#[sqlx::test]
async fn a_rename_and_a_merge_drop_the_stored_pass(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let owner = seed_user(&pool, org, "org_admin").await;
    let bearer = token(owner, org, UserRole::OrgAdmin);
    let keep = seed_loyalty_member(&pool, org, "01001234567", "Omar", "Mpc-keep-1").await;
    let dupe = seed_loyalty_member(&pool, org, "01112345678", "Omar M", "Mpc-dupe-1").await;
    cache_pass(&pool, org, keep).await;

    // Notes are not on the card: the stored pass stays.
    let (s, body) = call(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/customers/{keep}"))
            .set_json(json!({ "notes": "no sugar" })),
        &bearer,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(cached(&pool, keep).await, 1);

    let (s, body) = call(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/customers/{keep}"))
            .set_json(json!({ "name": "Omar Hassan" })),
        &bearer,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(cached(&pool, keep).await, 0, "the name is on the card");

    cache_pass(&pool, org, keep).await;
    cache_pass(&pool, org, dupe).await;
    let (s, body) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!("/customers/{dupe}/merge"))
            .set_json(json!({ "into": keep })),
        &bearer,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(cached(&pool, keep).await, 0);
    assert_eq!(cached(&pool, dupe).await, 0);
}

/// Erasing a customer who holds a card erases the card too: the token dies,
/// the stored pass and the phone history go, and the ledger stays.
#[sqlx::test]
async fn erasing_a_member_kills_the_card_and_its_stored_pass(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let owner = seed_user(&pool, org, "org_admin").await;
    let bearer = token(owner, org, UserRole::OrgAdmin);
    let member = seed_loyalty_member(&pool, org, "01001234567", "Omar", "Merase-0001").await;
    adjust(&pool, org, member, branch, 10).await;
    cache_pass(&pool, org, member).await;
    sqlx::query(
        "INSERT INTO customer_phone_history (org_id, customer_id, phone, phone_key) \
         VALUES ($1, $2, '01112345678', '201112345678')",
    )
    .bind(org)
    .bind(member)
    .execute(&pool)
    .await
    .unwrap();

    let (s, body) = call(
        &app,
        test::TestRequest::post().uri(&format!("/customers/{member}/erase")),
        &bearer,
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{body}");

    let (name, phone, erased): (String, Option<String>, bool) =
        sqlx::query_as("SELECT name, phone, erased_at IS NOT NULL FROM customers WHERE id = $1")
            .bind(member)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((name.as_str(), phone, erased), ("", None, true));
    let (gone, tok): (bool, String) = sqlx::query_as(
        "SELECT deleted_at IS NOT NULL, member_token FROM loyalty_customers WHERE id = $1",
    )
    .bind(member)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(gone, "the card is retired with the person");
    assert_ne!(tok, "Merase-0001", "the barcode is dead");
    assert!(
        madar_rust::loyalty::model::find_by_token(&pool, "Merase-0001")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(cached(&pool, member).await, 0);
    assert_eq!(
        i64_of(
            &pool,
            &format!("SELECT count(*) FROM customer_phone_history WHERE customer_id = '{member}'")
        )
        .await,
        0
    );
    // The books are not the member's data.
    assert_eq!(
        i64_of(
            &pool,
            &format!("SELECT count(*) FROM loyalty_transactions WHERE customer_id = '{member}'")
        )
        .await,
        1
    );
}

// ── Customers unification: the seeded data migration ────────────────────────

const PRE_CUSTOMERS_UNIFICATION: i64 = 20260925010000;

/// The old world, seeded, then the six unification migrations over it:
/// * two manual customers that are one phone typed two ways fold together;
/// * a member whose phone a manual customer holds becomes THAT person (the
///   customer's spelling and notes kept, its orders moved to the member's id);
/// * a member nobody typed in gets a customer of their own;
/// * a member whose stored phone FAILS the rule keeps the card, the typed
///   phone and no key — the migration neither drops the row nor fails;
/// * a forgotten member gets an erased customer.
#[sqlx::test]
async fn customers_unification_migrates_seeded_data(pool: PgPool) {
    let (db, _guard) = fresh(&pool).await;
    for role in ["sufrix", "madar_app"] {
        let _ = sqlx::raw_sql(&format!("CREATE ROLE {role} NOLOGIN"))
            .execute(&db)
            .await;
    }
    subset(|v| v < PRE_CUSTOMERS_UNIFICATION)
        .run(&db)
        .await
        .expect("migrations before the unification");

    let org = u("00000000-0000-4000-8000-0000000c0001");
    let branch = u("00000000-0000-4000-8000-0000000c0002");
    let teller = u("00000000-0000-4000-8000-0000000c0003");
    let m_fold = u("00000000-0000-4000-8000-0000000c0011");
    let m_alone = u("00000000-0000-4000-8000-0000000c0012");
    let m_badphone = u("00000000-0000-4000-8000-0000000c0013");
    let m_forgotten = u("00000000-0000-4000-8000-0000000c0014");
    let c_fold = u("00000000-0000-4000-8000-0000000c0021");
    let c_dup_a = u("00000000-0000-4000-8000-0000000c0022");
    let c_dup_b = u("00000000-0000-4000-8000-0000000c0023");
    let c_text = u("00000000-0000-4000-8000-0000000c0024");

    sqlx::raw_sql(&format!(
        "INSERT INTO organizations (id, name, slug) VALUES ('{org}', 'Org', 'org-cu');
         INSERT INTO branches (id, org_id, name) VALUES ('{branch}', '{org}', 'B');
         INSERT INTO users (id, org_id, name, email, password_hash, role)
         VALUES ('{teller}', '{org}', 'T', 'cu@t.com', 'h', 'teller');

         INSERT INTO loyalty_customers (id, org_id, phone, name, member_token, locale, birth_month, birth_day, marketing_opt_out)
         VALUES ('{m_fold}', '{org}', '201001234567', 'Omar', 'tok-cu-1', 'ar', 3, 17, true),
                ('{m_alone}', '{org}', '201112345678', 'Mona', 'tok-cu-2', 'en', NULL, NULL, false),
                ('{m_badphone}', '{org}', '20100123456', 'Shorty', 'tok-cu-3', 'en', NULL, NULL, false);
         INSERT INTO loyalty_customers (id, org_id, phone, name, member_token, deleted_at)
         VALUES ('{m_forgotten}', '{org}', 'deleted:{m_forgotten}', 'Deleted member', 'tok-cu-4', now());

         INSERT INTO customers (id, org_id, name, phone, phone_key, notes, created_at)
         VALUES ('{c_fold}', '{org}', 'Omar Hassan', '0100 123 4567', customers_phone_key('0100 123 4567'), 'no sugar', now() - interval '30 days'),
                ('{c_dup_a}', '{org}', 'Sara', '01212345678', customers_phone_key('01212345678'), NULL, now() - interval '20 days'),
                ('{c_dup_b}', '{org}', 'Sara A', '٠١٢١٢٣٤٥٦٧٨', customers_phone_key('٠١٢١٢٣٤٥٦٧٨'), NULL, now() - interval '10 days'),
                ('{c_text}', '{org}', 'Walk-in', 'ask the manager', customers_phone_key('ask the manager'), NULL, now());"
    ))
    .execute(&db)
    .await
    .expect("seed the old world");

    let till: Uuid = sqlx::query_scalar(
        "INSERT INTO tills (branch_id, teller_id, status, opening_cash) VALUES ($1, $2, 'open', 0) RETURNING id",
    )
    .bind(branch)
    .bind(teller)
    .fetch_one(&db)
    .await
    .unwrap();
    for (n, customer) in [(1, c_fold), (2, c_dup_b)] {
        sqlx::query(
            "INSERT INTO orders (branch_id, teller_id, till_id, subtotal, tax_amount, total_amount, status, order_number, payment_method, order_ref, customer_id) \
             VALUES ($1, $2, $3, 100, 0, 100, 'completed', $4, 'cash', $5, $6)",
        )
        .bind(branch)
        .bind(teller)
        .bind(till)
        .bind(n)
        .bind(format!("CU-{n}"))
        .bind(customer)
        .execute(&db)
        .await
        .unwrap();
    }

    subset(|_| true)
        .run(&db)
        .await
        .expect("the unification migrations run over seeded data");

    // Every membership survived, and each has a customer under its own id.
    assert_eq!(
        i64_of(&db, "SELECT count(*) FROM loyalty_customers").await,
        4
    );
    assert_eq!(
        i64_of(
            &db,
            "SELECT count(*) FROM loyalty_customers m JOIN customers c ON c.id = m.id"
        )
        .await,
        4
    );

    // The folded member: the customer's spelling, notes and typed phone; the
    // member's preferences; the old row steps aside and its order follows.
    let (name, phone, key, notes, locale, month, opt_out): (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i16>,
        bool,
    ) = sqlx::query_as(
        "SELECT name, phone, phone_key, notes, locale, birth_month, marketing_opt_out \
           FROM customers WHERE id = $1",
    )
    .bind(m_fold)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(name, "Omar Hassan");
    assert_eq!(phone.as_deref(), Some("0100 123 4567"));
    assert_eq!(key.as_deref(), Some("201001234567"));
    assert_eq!(notes.as_deref(), Some("no sugar"));
    assert_eq!(locale.as_deref(), Some("ar"));
    assert_eq!(month, Some(3));
    assert!(opt_out);
    let merged_into: Option<Uuid> =
        sqlx::query_scalar("SELECT merged_into FROM customers WHERE id = $1")
            .bind(c_fold)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(merged_into, Some(m_fold));
    let order_owner: Option<Uuid> =
        sqlx::query_scalar("SELECT customer_id FROM orders WHERE order_ref = 'CU-1'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(order_owner, Some(m_fold));

    // A member nobody typed in.
    let (source, key): (String, Option<String>) =
        sqlx::query_as("SELECT source, phone_key FROM customers WHERE id = $1")
            .bind(m_alone)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(source, "loyalty");
    assert_eq!(key.as_deref(), Some("201112345678"));

    // The member whose phone fails rule 6 (an 11-digit 2010…): kept, unkeyed.
    let (phone, key, erased): (Option<String>, Option<String>, bool) = sqlx::query_as(
        "SELECT phone, phone_key, erased_at IS NOT NULL FROM customers WHERE id = $1",
    )
    .bind(m_badphone)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(
        phone.as_deref(),
        Some("20100123456"),
        "phone as it was stored"
    );
    assert_eq!(key, None);
    assert!(!erased);
    assert_eq!(
        i64_of(
            &db,
            &format!("SELECT count(*) FROM loyalty_customers WHERE id = '{m_badphone}' AND deleted_at IS NULL")
        )
        .await,
        1,
        "the membership is kept"
    );
    let view_phone: String =
        sqlx::query_scalar("SELECT phone FROM loyalty_members_v WHERE id = $1")
            .bind(m_badphone)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(view_phone, "20100123456");

    // A forgotten member is an erased customer.
    let (name, erased): (String, bool) =
        sqlx::query_as("SELECT name, erased_at IS NOT NULL FROM customers WHERE id = $1")
            .bind(m_forgotten)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(name, "");
    assert!(erased);

    // One phone typed two ways: the row with the order survives, the other
    // merges into it, and free text keeps its row with no key.
    let survivor: Option<Uuid> =
        sqlx::query_scalar("SELECT merged_into FROM customers WHERE id = $1")
            .bind(c_dup_a)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(survivor, Some(c_dup_b), "most orders wins");
    assert_eq!(
        i64_of(
            &db,
            "SELECT count(*) FROM customers WHERE phone_key = '201212345678' AND merged_into IS NULL"
        )
        .await,
        1
    );
    let (phone, key): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT phone, phone_key FROM customers WHERE id = $1")
            .bind(c_text)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(phone.as_deref(), Some("ask the manager"));
    assert_eq!(key, None);

    // And the invariant the whole design rests on.
    assert_eq!(
        i64_of(
            &db,
            "SELECT count(*) FROM (SELECT 1 FROM customers \
              WHERE merged_into IS NULL AND erased_at IS NULL AND phone_key IS NOT NULL \
              GROUP BY org_id, phone_key HAVING count(*) > 1) x"
        )
        .await,
        0
    );
}
