//! The compat layer (DB_ARCHITECTURE_AUDIT_2026-10-16 §7.3, Phase 3.2).
//!
//! Old tills are never forced to update, so some old request and response
//! shapes are served for as long as a device out there still uses them. This
//! module is the ONE list of every such shim: what it serves, since when, what
//! replaced it, the counter that says whether anyone still uses it, and the
//! condition under which it may go. Removing a shim = its teardown PR deletes
//! its entry here, its code at the listed sites, and (when it has one) moves
//! its parked contract migration into `migrations/`.
//!
//! Rules the tests hold (`tests/compat_contract_tests.rs`):
//! * the `compat.*` schema is named only at the sites registered here;
//! * every entry's counter is a real legacy kind, so its usage is visible in
//!   `client_seen` / `compat_usage`;
//! * the response shapes the registered read routes serve are pinned.
//!
//! The sunset condition is always at least: 0 hits of the entry's counter for
//! 30 consecutive days across all orgs (`compat_usage`), AND every active
//! device's `client_seen.app_version` at or above the entry's `fixed_in`
//! build, AND an owner sign-off line in the teardown PR.

use crate::client_seen as cs;
use crate::compat_usage as cu;

/// One compatibility shim.
#[derive(Debug, Clone, Copy)]
pub struct Shim {
    /// Stable id of the shim (also the teardown PR's subject).
    pub id: &'static str,
    /// The usage counter (a `client_seen` / `compat_usage` kind).
    pub counter: &'static str,
    /// The release that introduced the shim (what still calls it).
    pub introduced_in: &'static str,
    /// What replaced the old shape.
    pub replaces: &'static str,
    /// The sunset condition beyond the standard one (see the module doc).
    pub sunset_condition: &'static str,
    /// Files that implement it. A file that names `compat.*` must be listed
    /// by some entry.
    pub sites: &'static [&'static str],
    /// The parked contract migration that finishes it, if any.
    pub parked: Option<&'static str>,
}

/// Every shim the server keeps for old clients.
pub const REGISTRY: &[Shim] = &[
    Shim {
        id: "legacy_catalog_reads",
        counter: cu::KIND_MENU_LEGACY_READ,
        introduced_in: "menu unification (2026-09), POS < 0.9",
        replaces: "the unified catalog: modifier groups/options, recipe_lines, menu_price_overrides \
                   (/menu-items/{id}/studio, /modifier-groups, /catalog/sync)",
        sunset_condition: "no POS build below the first one that reads modifier groups only \
                           (the legacy addon/recipe/override lists and /menu-items' legacy fields \
                           are what older builds price from)",
        sites: &[
            "src/menu/handlers.rs",
            "src/menu/full_feed.rs",
            "src/recipes/handlers.rs",
            "src/delivery/settings.rs",
        ],
        parked: Some("migrations/parked/p3_drop_public_compat_views.sql"),
    },
    Shim {
        id: "legacy_catalog_writes_gone",
        counter: cu::KIND_SHIM_WRITE_GONE,
        introduced_in: "menu unification (2026-09)",
        replaces: "the unified catalog write routes",
        sunset_condition: "the 25 tombstones (410 SHIM_WRITE_REMOVED) may be deleted outright: \
                           these writes have failed in prod since the unification",
        sites: &[
            "src/menu/handlers.rs",
            "src/recipes/handlers.rs",
            "src/delivery/settings.rs",
            "src/compat_usage.rs",
        ],
        parked: None,
    },
    Shim {
        id: "legacy_option_columns",
        counter: cu::KIND_MENU_LEGACY_READ,
        introduced_in: "menu unification (2026-09)",
        replaces: "modifier_groups.effect / swap_category_id and ingredient category slugs",
        sunset_condition: "the milk/coffee swap logic keys on effect/swap_category_id only \
                           (no read of legacy_addon_type / legacy_source / legacy_origin left), \
                           and legacy_catalog_reads is gone",
        sites: &["src/menu/handlers.rs"],
        parked: Some("migrations/parked/p3_drop_legacy_option_columns.sql"),
    },
    Shim {
        id: "included_option_ids_array",
        counter: cu::KIND_MENU_LEGACY_READ,
        introduced_in: "menu unification (2026-09)",
        replaces: "menu_item_modifier_group_options (DB Phase 3.4)",
        sunset_condition: "scripts/db_phase3_check.sql clean on live prod for a release, and \
                           every writer writes the join table directly",
        sites: &["src/menu/studio.rs", "src/menu/modifiers.rs"],
        parked: Some("migrations/parked/p3_mimg_option_allowlist_contract.sql"),
    },
    Shim {
        id: "shifts_adapters",
        counter: cs::KIND_SHIFTS_ROUTE,
        introduced_in: "POS < 0.7",
        replaces: "/tills/* (the till is the shift)",
        sunset_condition: "also 0 hits of legacy_tills_entity, legacy_tills_entity_gone, \
                           legacy_refunds_shift_route, legacy_reports_shifts_route, shift_id_body, \
                           shift_id_query, replay_open_close_shift_op, replay_shift_id_field, \
                           perm_payload_old, legacy_error_wording, analytics_shifts_alias",
        sites: &[
            "src/tills/legacy_routes.rs",
            "src/tills/legacy.rs",
            "src/sync/handlers.rs",
            "src/auth/handlers.rs",
        ],
        parked: None,
    },
    Shim {
        id: "mirror_lists",
        counter: cs::KIND_MIRROR_LIST_POS,
        introduced_in: "POS/KDS before /sync/pull",
        replaces: "the /sync/pull changefeed",
        sunset_condition: "-",
        sites: &[],
        parked: None,
    },
    Shim {
        id: "uploads_legacy_paths",
        counter: cs::KIND_UPLOADS_LEGACY_REDIRECT,
        introduced_in: "before signed asset URLs",
        replaces: "signed org-scoped asset URLs",
        sunset_condition: "never: printed QR codes and issued wallet passes hold these URLs \
                           for years; keep the redirect",
        sites: &["src/uploads/handlers.rs"],
        parked: None,
    },
    Shim {
        id: "bundles_stub",
        counter: cu::KIND_MENU_LEGACY_READ,
        introduced_in: "combos v1 (removed 2026-09-25)",
        replaces: "deals / combos",
        sunset_condition: "no POS build that calls GET /bundles",
        sites: &["src/bundles/mod.rs"],
        parked: None,
    },
    Shim {
        id: "loyalty_members_view",
        counter: cu::KIND_COMPAT_VIEW_SERVER,
        introduced_in: "customers unification (2026-09)",
        replaces: "customers + loyalty memberships",
        sunset_condition: "the loyalty module reads its base tables (Phase 5.5 rename) and \
                           nothing names loyalty_members_v",
        sites: &[],
        parked: None,
    },
    Shim {
        id: "order_addon_id_translation",
        counter: cu::KIND_MENU_LEGACY_READ,
        introduced_in: "menu unification (2026-09)",
        replaces: "-",
        sunset_condition: "never: a sale queued offline under an old build must always \
                           replay (stable-id invariant: an addon_item id is a modifier_options id)",
        sites: &[],
        parked: None,
    },
];

/// The registered shim with this id.
pub fn shim(id: &str) -> Option<&'static Shim> {
    REGISTRY.iter().find(|s| s.id == id)
}
