//! The compat layer's rules (DB_ARCHITECTURE_AUDIT_2026-10-16 §7.3, Phase 3)
//! and the old shapes it still serves.

use madar_rust::compat::REGISTRY;
use std::path::{Path, PathBuf};

const LEGACY_VIEWS: &[&str] = &[
    "addon_item_ingredients",
    "addon_items",
    "branch_addon_overrides",
    "branch_channel_addon_overrides",
    "branch_channel_menu_overrides",
    "branch_menu_overrides",
    "branch_menu_size_overrides",
    "item_sizes",
    "menu_item_addon_slots",
    "menu_item_allowed_addons",
    "menu_item_optional_fields",
    "menu_item_recipes",
];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

fn src_files() -> Vec<(String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    files
        .into_iter()
        .map(|p| {
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            (rel, std::fs::read_to_string(&p).unwrap())
        })
        .collect()
}

/// `FROM|JOIN <view>` with no schema (the public view), in a SQL-looking spot.
fn reads_public_view(text: &str, view: &str) -> bool {
    for kw in ["FROM", "JOIN"] {
        let mut rest = text;
        while let Some(i) = rest.find(kw) {
            let after = &rest[i + kw.len()..];
            let trimmed = after.trim_start();
            let ws = after.len() - trimmed.len();
            if ws > 0
                && trimmed.starts_with(view)
                && !trimmed[view.len()..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.')
            {
                return true;
            }
            rest = &rest[i + kw.len()..];
        }
    }
    false
}

/// Phase 3.1: the server's own code reads the unified tables. Only the
/// one-time menu-unification backfill (which read the legacy TABLES it
/// replaced) still names the old relations unqualified.
#[test]
fn no_server_code_reads_the_public_legacy_views() {
    let mut offenders = Vec::new();
    for (path, text) in src_files() {
        if path == "src/menu_unification/backfill.rs" {
            continue;
        }
        for v in LEGACY_VIEWS {
            if reads_public_view(&text, v) {
                offenders.push(format!("{path}: {v}"));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "read the unified tables (or compat.<view> from a registered compat site): {offenders:?}"
    );
}

/// §7.3: the compat schema is named only where the registry says.
#[test]
fn the_compat_schema_is_named_only_at_registered_sites() {
    let registered: Vec<&str> = REGISTRY.iter().flat_map(|s| s.sites.iter().copied()).collect();
    let mut offenders = Vec::new();
    for (path, text) in src_files() {
        if path.starts_with("src/compat/") {
            continue;
        }
        let names_compat = LEGACY_VIEWS
            .iter()
            .chain(std::iter::once(&"loyalty_members_v"))
            .any(|v| text.contains(&format!("compat.{v}")));
        if names_compat && !registered.contains(&path.as_str()) {
            offenders.push(path);
        }
    }
    assert!(offenders.is_empty(), "register these in src/compat: {offenders:?}");
}

/// Every registered site exists, every counter is a real usage kind, and
/// every parked contract the registry points at is in migrations/parked/.
#[test]
fn the_registry_is_consistent() {
    use madar_rust::client_seen as cs;
    use madar_rust::compat_usage as cu;
    let kinds = [
        cs::KIND_SHIFTS_ROUTE,
        cs::KIND_TILLS_ENTITY,
        cs::KIND_TILLS_ENTITY_GONE,
        cs::KIND_REFUNDS_SHIFT_ROUTE,
        cs::KIND_REPORTS_SHIFTS_ROUTE,
        cs::KIND_SHIFT_ID_BODY,
        cs::KIND_SHIFT_ID_QUERY,
        cs::KIND_PERM_PAYLOAD_OLD,
        cs::KIND_REPLAY_LEGACY_OP,
        cs::KIND_REPLAY_SHIFT_ID_FIELD,
        cs::KIND_ERROR_WORDING,
        cs::KIND_ANALYTICS_ALIAS,
        cs::KIND_CATALOG_SYNC,
        cs::KIND_UPLOADS_LEGACY_PATH,
        cs::KIND_UPLOADS_LEGACY_REDIRECT,
        cs::KIND_MIRROR_LIST_POS,
        cs::KIND_PERMISSIONS_ROUTE,
        cu::KIND_COMPAT_VIEW_SERVER,
        cu::KIND_MENU_LEGACY_READ,
        cu::KIND_SHIM_WRITE_GONE,
    ];
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut ids = std::collections::HashSet::new();
    for s in REGISTRY {
        assert!(ids.insert(s.id), "duplicate shim id {}", s.id);
        assert!(kinds.contains(&s.counter), "{}: unknown counter {}", s.id, s.counter);
        for site in s.sites {
            assert!(root.join(site).exists(), "{}: no such site {site}", s.id);
        }
        if let Some(p) = s.parked {
            assert!(root.join(p).exists(), "{}: no parked migration {p}", s.id);
            assert!(p.starts_with("migrations/parked/"), "{}: {p} is not parked", s.id);
        }
        assert!(!s.sunset_condition.is_empty() && !s.replaces.is_empty());
    }
}

// ── The old shapes still served, pinned ──────────────────────────────────────
//
// One catalogue, seeded the way prod holds it (the unified tables, through the
// shared legacy_catalog helpers), served by every old-shape READ route. The
// whole body of each answer is pinned in tests/fixtures/compat/old_shapes.json
// (timestamps blanked). The pin was recorded from the code BEFORE Phase 3
// moved these reads (Phase 2: the public views), so this test is the
// old-versus-new contract check: `MADAR_WRITE_COMPAT_SHAPES=1` rewrites it,
// and only a deliberate contract change may do that.

#[allow(dead_code)]
mod common;

use actix_web::{App, test as atest, web};
use common::legacy_catalog as lc;
use sqlx::PgPool;
use uuid::Uuid;

const PIN: &str = "tests/fixtures/compat/old_shapes.json";

const fn fid(n: u128) -> Uuid {
    Uuid::from_u128(0xc0de_0000_0000_4000_8000_0000_0000_0000 | n)
}
const ORG: Uuid = fid(1);
const BRANCH: Uuid = fid(2);
const ADMIN: Uuid = fid(3);
const CAT: Uuid = fid(4);
const LATTE: Uuid = fid(5);
const TEA: Uuid = fid(6);
const S_SMALL: Uuid = fid(7);
const S_LARGE: Uuid = fid(8);
const MILK: Uuid = fid(9);
const BEAN: Uuid = fid(10);
const SYRUP: Uuid = fid(11);
const OAT: Uuid = fid(12);
const SKIM: Uuid = fid(13);
const VANILLA: Uuid = fid(14);
const CREAM_OPT: Uuid = fid(15);
const CREAM_ING: Uuid = fid(16);

async fn seed_catalog(pool: &PgPool) {
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations (id, name, slug) VALUES ('{ORG}', 'Compat', 'compat-shapes');
         INSERT INTO branches (id, org_id, name, code) VALUES ('{BRANCH}', '{ORG}', 'Main', 'MAIN');
         INSERT INTO users (id, org_id, name, email, password_hash, role)
              VALUES ('{ADMIN}', '{ORG}', 'Admin', 'admin@compat.test', 'x', 'org_admin');
         INSERT INTO categories (id, org_id, name) VALUES ('{CAT}', '{ORG}', 'Coffee');
         INSERT INTO menu_items (id, org_id, category_id, name, base_price)
              VALUES ('{LATTE}', '{ORG}', '{CAT}', 'Latte', 4000), ('{TEA}', '{ORG}', '{CAT}', 'Tea', 2000);
         INSERT INTO menu_item_sizes (id, menu_item_id, label, price, sort)
              VALUES ('{S_SMALL}', '{LATTE}', 'Small', 4000, 0), ('{S_LARGE}', '{LATTE}', 'Large', 5000, 1);
         INSERT INTO org_ingredients (id, org_id, name, unit, cost_per_unit) VALUES
              ('{MILK}', '{ORG}', 'Whole milk', 'ml', 0.05), ('{BEAN}', '{ORG}', 'Espresso bean', 'g', 0.9),
              ('{SYRUP}', '{ORG}', 'Vanilla syrup', 'ml', 0.2), ('{CREAM_ING}', '{ORG}', 'Cream', 'ml', 0.3);"
    ))
    .execute(pool)
    .await
    .unwrap();
    lc::item_recipe(pool, LATTE, "Small", MILK, 200, "ml").await;
    lc::item_recipe(pool, LATTE, "Small", BEAN, 18, "g").await;
    lc::item_recipe(pool, LATTE, "Large", MILK, 300, "ml").await;
    lc::item_recipe(pool, LATTE, "Large", BEAN, 18, "g").await;
    lc::addon(pool, lc::Addon::new(ORG, "Oat", "milk_type", 800).id(OAT)).await;
    lc::addon(pool, lc::Addon::new(ORG, "Skim", "milk_type", 0).id(SKIM)).await;
    lc::addon(pool, lc::Addon::new(ORG, "Vanilla", "extra", 500).id(VANILLA)).await;
    lc::addon_ingredient(pool, VANILLA, SYRUP, 15, "ml").await;
    lc::optional_field(pool, lc::Optional::new(LATTE, "Whipped cream", 300).id(CREAM_OPT).recipe(CREAM_ING, 40, "ml")).await;
    lc::addon_slot(pool, ORG, LATTE, "milk_type", true, 1, Some(1)).await;
    lc::allowed_addons(pool, LATTE, &[OAT, VANILLA]).await;
    lc::branch_menu_override(pool, BRANCH, TEA, Some(2200), true).await;
    lc::branch_size_override(pool, BRANCH, LATTE, "Large", 5300).await;
    lc::branch_addon_override(pool, BRANCH, OAT, Some(900), true).await;
    lc::channel_menu_override(pool, BRANCH, TEA, "outside", Some(2500), Some(true)).await;
    lc::channel_addon_override(pool, BRANCH, VANILLA, "outside", Some(600), None).await;
}

/// Timestamps change every run, and so do ids the database or a seed helper
/// generates (a new org's ingredient categories, an attachment row): those are
/// blanked. The fixture's own `c0de…` ids and every other value are compared
/// exactly.
fn blank_times(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(m) => {
            for (k, x) in m.iter_mut() {
                if k.ends_with("_at") || k == "created" || k == "updated" {
                    *x = serde_json::Value::String("<time>".into());
                } else if x.as_str().is_some_and(|s| {
                    !s.starts_with("c0de0000") && uuid::Uuid::parse_str(s).is_ok()
                }) {
                    *x = serde_json::Value::String("<generated id>".into());
                } else {
                    blank_times(x);
                }
            }
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(blank_times),
        _ => {}
    }
}

#[sqlx::test]
async fn the_old_shapes_are_served_exactly_as_before(pool: PgPool) {
    seed_catalog(&pool).await;
    let secret = madar_rust::auth::jwt::JwtSecret("secret".to_string());
    let token = madar_rust::auth::jwt::create_token(
        &secret,
        ADMIN,
        Some(ORG),
        madar_rust::models::UserRole::OrgAdmin,
        None,
        24,
    )
    .unwrap();
    let app = atest::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret))
            .app_data(web::Data::new(madar_rust::realtime::hub::BranchEventHub::new()))
            .configure(madar_rust::menu::routes::configure)
            .configure(madar_rust::recipes::routes::configure)
            .configure(madar_rust::delivery::routes::configure)
            .configure(madar_rust::bundles::configure),
    )
    .await;
    let routes = [
        format!("/menu-items?org_id={ORG}&full=true"),
        format!("/menu-items?org_id={ORG}&full=true&branch_id={BRANCH}"),
        format!("/menu-items/{LATTE}"),
        format!("/menu-items/{LATTE}/addon-slots"),
        format!("/menu-items/{LATTE}/optionals"),
        format!("/addon-items?org_id={ORG}"),
        format!("/addon-items?org_id={ORG}&branch_id={BRANCH}"),
        format!("/addon-items/catalog?org_id={ORG}"),
        format!("/branch-menu-overrides?branch_id={BRANCH}"),
        format!("/branch-addon-overrides?branch_id={BRANCH}"),
        format!("/delivery/channel-overrides?branch_id={BRANCH}&channel=outside"),
        format!("/delivery/channel-addon-overrides?branch_id={BRANCH}&channel=outside"),
        format!("/recipes/drinks/{LATTE}"),
        format!("/recipes/addons/{VANILLA}"),
        format!("/public/branches/{BRANCH}/menu?channel=dine_in&preview=true"),
        "/bundles".to_string(),
    ];
    let mut now = serde_json::Map::new();
    for r in routes {
        let req = atest::TestRequest::get()
            .uri(&r)
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request();
        let resp = atest::call_service(&app, req).await;
        let status = resp.status().as_u16();
        let body = atest::read_body(resp).await;
        let mut v: serde_json::Value =
            serde_json::from_slice(&body).unwrap_or(serde_json::Value::String(String::from_utf8_lossy(&body).into()));
        blank_times(&mut v);
        now.insert(r, serde_json::json!({ "status": status, "body": v }));
    }
    let now = serde_json::Value::Object(now);
    if std::env::var("MADAR_WRITE_COMPAT_SHAPES").is_ok() {
        std::fs::create_dir_all("tests/fixtures/compat").unwrap();
        std::fs::write(PIN, serde_json::to_string_pretty(&now).unwrap() + "\n").unwrap();
        return;
    }
    let mut pinned: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(PIN).expect("pin recorded")).unwrap();
    blank_times(&mut pinned);
    for (route, want) in pinned.as_object().unwrap() {
        assert_eq!(&now[route], want, "{route}");
    }
    assert_eq!(now, pinned);
}
