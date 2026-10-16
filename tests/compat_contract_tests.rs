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
