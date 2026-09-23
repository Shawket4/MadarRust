use std::collections::BTreeMap;

use proptest::prelude::*;

use super::guard::*;
use super::*;

const NOW: i64 = 1_800_000_000;

fn role(kind: RoleKind, caps: &[Cap]) -> RoleDef {
    RoleDef {
        id: format!("{kind:?}"),
        kind,
        grants: caps.iter().copied().collect(),
        limits: BTreeMap::new(),
    }
}

fn assigned(r: RoleDef, branches: &[&str]) -> AssignmentDef {
    AssignmentDef {
        role: r,
        all_branches: branches.is_empty(),
        branches: branches.iter().map(|b| b.to_string()).collect(),
        valid_from: None,
        valid_to: None,
    }
}

fn person(assignments: Vec<AssignmentDef>, overrides: Vec<OverrideDef>) -> Principal {
    Principal {
        user_id: "u".into(),
        active: true,
        is_owner: false,
        assignments,
        overrides,
    }
}

fn ov(cap: Cap, allow: bool, branch: Option<&str>) -> OverrideDef {
    OverrideDef {
        cap: cap.id(),
        allow,
        branch: branch.map(str::to_string),
        limits: None,
        valid_to: None,
    }
}

// ── the registry ────────────────────────────────────────────────────────────

#[test]
fn caps_are_sorted_by_id_and_fit_the_bitset() {
    assert!(CAPS.windows(2).all(|w| w[0].cap.id() < w[1].cap.id()));
    assert!(CAPS.iter().all(|m| (m.cap.id() as usize) < WORDS * 64));
    for m in CAPS {
        assert_eq!(Cap::from_key(m.key), Some(m.cap));
        assert_eq!(Cap::from_id(m.cap.id()), Some(m.cap));
    }
}

/// Every cell the backend reports in GET /auth/permissions has a capability.
/// This list is the backend's `permission_cells()`; a new resource there fails
/// here until the spec answers for it.
#[test]
fn every_legacy_cell_has_exactly_one_capability() {
    const RESOURCES: &[&str] = &[
        "orgs",
        "branches",
        "users",
        "categories",
        "menu_items",
        "addon_groups",
        "addon_items",
        "recipes",
        "inventory",
        "inventory_adjustments",
        "inventory_transfers",
        "stocktakes",
        "inventory_waste",
        "suppliers",
        "purchase_orders",
        "orders",
        "order_items",
        "refunds",
        "payments",
        "payment_methods",
        "tills",
        "soft_serve_batches",
        "discounts",
        "reports",
        "permissions",
        "kitchen_stations",
        "kitchen_orders",
        "open_tickets",
        "floor_plan",
        "table_transfers",
        "bookings",
        "loyalty",
        "delivery_orders",
        "delivery_settings",
        "staff",
        "work_shifts",
        "attendance",
        "leave",
        "payroll",
    ];
    let mut n = 0;
    for r in RESOURCES {
        for a in ["create", "read", "update", "delete"] {
            let hits = CAPS.iter().filter(|m| m.legacy == Some((r, a))).count();
            assert_eq!(hits, 1, "{r}:{a}");
            n += 1;
        }
    }
    assert!(Cap::from_legacy("orders", "waive_service").is_some());
    assert_eq!(CAPS.iter().filter(|m| m.legacy.is_some()).count(), n + 1);
}

#[test]
fn core_is_within_defaults_and_templates_never_give_floor_roles_admin() {
    for m in CAPS {
        assert_eq!(m.core.0 & !m.defaults.0, 0, "{}", m.key);
    }
    for t in TEMPLATES {
        for k in [RoleKind::Teller, RoleKind::Waiter, RoleKind::Kitchen] {
            let g = template_grants(t.key, k).unwrap();
            for c in g.iter() {
                assert_ne!(
                    c.meta().risk,
                    Risk::Admin,
                    "{} gives {k:?} {}",
                    t.key,
                    c.key()
                );
            }
        }
    }
}

#[test]
fn labels_exist_in_both_languages() {
    for m in CAPS {
        assert!(
            !m.en.is_empty() && !m.ar.is_empty() && m.en != m.ar,
            "{}",
            m.key
        );
    }
}

// ── resolve ─────────────────────────────────────────────────────────────────

#[test]
fn a_role_gives_its_grants_plus_core() {
    let p = person(
        vec![assigned(role(RoleKind::Teller, &[Cap::OrdersVoid]), &[])],
        vec![],
    );
    let e = resolve(&p, Scope::Anywhere, NOW, &OrgPolicy::default());
    assert!(e.can(Cap::OrdersVoid));
    assert!(e.can(Cap::MenuItemsRead), "core for a teller");
    assert!(!e.can(Cap::OrdersCreate), "selling is a grant, not core");
    assert!(!e.can(Cap::TillForceClose));
}

#[test]
fn manager_at_one_branch_cashier_at_another() {
    let p = person(
        vec![
            assigned(
                role(RoleKind::BranchManager, &[Cap::TillForceClose]),
                &["b1"],
            ),
            assigned(role(RoleKind::Teller, &[]), &["b2"]),
        ],
        vec![],
    );
    let pol = OrgPolicy::default();
    assert!(resolve(&p, Scope::Branch("b1"), NOW, &pol).can(Cap::TillForceClose));
    assert!(!resolve(&p, Scope::Branch("b2"), NOW, &pol).can(Cap::TillForceClose));
    assert!(!resolve(&p, Scope::Branch("b3"), NOW, &pol).can(Cap::MenuItemsRead));
}

#[test]
fn deny_removes_but_never_core() {
    let p = person(
        vec![assigned(role(RoleKind::Teller, &[Cap::OrdersVoid]), &[])],
        vec![
            ov(Cap::OrdersVoid, false, None),
            ov(Cap::OrdersCreate, false, None),
        ],
    );
    let e = resolve(&p, Scope::Anywhere, NOW, &OrgPolicy::default());
    assert!(!e.can(Cap::OrdersVoid));
    assert!(e.can(Cap::MenuItemsRead), "core survives a deny");
}

#[test]
fn a_branch_override_beats_an_everywhere_one() {
    let p = person(
        vec![assigned(role(RoleKind::Teller, &[]), &["b1", "b2"])],
        vec![
            ov(Cap::RefundsCreate, false, None),
            ov(Cap::RefundsCreate, true, Some("b1")),
        ],
    );
    let pol = OrgPolicy::default();
    assert!(resolve(&p, Scope::Branch("b1"), NOW, &pol).can(Cap::RefundsCreate));
    assert!(!resolve(&p, Scope::Branch("b2"), NOW, &pol).can(Cap::RefundsCreate));
}

#[test]
fn expired_overrides_and_assignments_do_nothing() {
    let mut a = assigned(role(RoleKind::BranchManager, &[Cap::TillForceClose]), &[]);
    a.valid_to = Some(NOW - 1);
    let mut o = ov(Cap::RefundsCreate, true, None);
    o.valid_to = Some(NOW);
    let e = resolve(
        &person(vec![a], vec![o]),
        Scope::Anywhere,
        NOW,
        &OrgPolicy::default(),
    );
    assert!(e.caps.is_empty());
}

#[test]
fn owners_hold_everything_and_protected_caps_cannot_be_denied() {
    let mut p = person(
        vec![],
        vec![
            ov(Cap::StaffPermissionsEdit, false, None),
            ov(Cap::OrdersVoid, false, None),
        ],
    );
    p.is_owner = true;
    let e = resolve(&p, Scope::Anywhere, NOW, &OrgPolicy::default());
    assert!(e.can(Cap::StaffPermissionsEdit));
    assert!(!e.can(Cap::OrdersVoid));
    assert_eq!(
        e.caps,
        owner_set().minus(&[Cap::OrdersVoid].into_iter().collect())
    );
}

#[test]
fn inactive_people_hold_nothing() {
    let mut p = person(vec![assigned(role(RoleKind::Teller, &[]), &[])], vec![]);
    p.active = false;
    assert!(resolve(&p, Scope::Anywhere, NOW, &OrgPolicy::default())
        .caps
        .is_empty());
}

#[test]
fn limits_take_the_most_generous_role_and_an_override_replaces_them() {
    let mut r1 = role(RoleKind::Teller, &[Cap::RefundsCreate]);
    r1.limits.insert(
        Cap::RefundsCreate.id(),
        Limits {
            max_amount: Some(200_00),
            ..Default::default()
        },
    );
    let mut r2 = role(RoleKind::Teller, &[Cap::RefundsCreate]);
    r2.limits.insert(
        Cap::RefundsCreate.id(),
        Limits {
            max_amount: Some(500_00),
            ..Default::default()
        },
    );
    let pol = OrgPolicy::default();
    let e = resolve(
        &person(vec![assigned(r1.clone(), &[]), assigned(r2, &[])], vec![]),
        Scope::Anywhere,
        NOW,
        &pol,
    );
    assert_eq!(e.limits_of(Cap::RefundsCreate).max_amount, Some(500_00));

    let r3 = role(RoleKind::Teller, &[Cap::RefundsCreate]);
    let e = resolve(
        &person(vec![assigned(r1.clone(), &[]), assigned(r3, &[])], vec![]),
        Scope::Anywhere,
        NOW,
        &pol,
    );
    assert!(
        e.limits_of(Cap::RefundsCreate).is_unlimited(),
        "an uncapped role wins"
    );

    let mut o = ov(Cap::RefundsCreate, true, None);
    o.limits = Some(Limits {
        max_amount: Some(50_00),
        ..Default::default()
    });
    let e = resolve(
        &person(vec![assigned(r1, &[])], vec![o]),
        Scope::Anywhere,
        NOW,
        &pol,
    );
    assert_eq!(e.limits_of(Cap::RefundsCreate).max_amount, Some(50_00));
}

// ── decide ──────────────────────────────────────────────────────────────────

#[test]
fn not_held_is_hidden_unless_the_owner_chose_ask_a_manager() {
    let p = person(vec![assigned(role(RoleKind::Teller, &[]), &[])], vec![]);
    let req = Request::of(Cap::TillCashSpotCheck);
    let hidden = resolve(&p, Scope::Anywhere, NOW, &OrgPolicy::default());
    assert_eq!(decide(&hidden, &req), Decision::Deny(Why::NotHeld));
    let ask = OrgPolicy {
        ask_manager: [Cap::TillCashSpotCheck, Cap::TillForceClose]
            .into_iter()
            .collect(),
    };
    let e = resolve(&p, Scope::Anywhere, NOW, &ask);
    assert_eq!(decide(&e, &req), Decision::NeedsApproval(Why::NotHeld));
    // force_close does not allow approval in the spec: the policy cannot turn it on.
    assert_eq!(
        decide(&e, &Request::of(Cap::TillForceClose)),
        Decision::Deny(Why::NotHeld)
    );
}

#[test]
fn over_a_limit_needs_approval() {
    let mut r = role(RoleKind::Teller, &[Cap::OrdersDiscountManualPercent]);
    r.limits.insert(
        Cap::OrdersDiscountManualPercent.id(),
        Limits {
            max_percent: Some(1000),
            ..Default::default()
        },
    );
    let e = resolve(
        &person(vec![assigned(r, &[])], vec![]),
        Scope::Anywhere,
        NOW,
        &OrgPolicy::default(),
    );
    let within = Request::of(Cap::OrdersDiscountManualPercent).percent(1000);
    let over = Request::of(Cap::OrdersDiscountManualPercent).percent(1500);
    assert_eq!(decide(&e, &within), Decision::Allow);
    assert_eq!(
        decide(&e, &over),
        Decision::NeedsApproval(Why::OverLimit {
            key: LimitKey::MaxPercent,
            limit: 1000,
            asked: 1500
        })
    );
}

/// The locked decision, as the crate sees it: a teller voids their own sale
/// within ten minutes, and everything else is a manager's call rather than a
/// flat refusal — which is what `approval = true` on the capability buys.
#[test]
fn own_and_max_age_route_a_void_to_a_manager() {
    let mut r = role(RoleKind::Teller, &[Cap::OrdersVoid]);
    r.limits.insert(
        Cap::OrdersVoid.id(),
        Limits {
            own: true,
            max_age_minutes: Some(10),
            ..Default::default()
        },
    );
    let e = resolve(
        &person(vec![assigned(r, &[])], vec![]),
        Scope::Anywhere,
        NOW,
        &OrgPolicy::default(),
    );
    let mine = |min: i64| Request::of(Cap::OrdersVoid).own(true).age_minutes(min);
    assert_eq!(decide(&e, &mine(9)), Decision::Allow);
    assert_eq!(decide(&e, &mine(10)), Decision::Allow, "the boundary is in");
    assert_eq!(
        decide(&e, &mine(11)),
        Decision::NeedsApproval(Why::OverLimit {
            key: LimitKey::MaxAgeMinutes,
            limit: 10,
            asked: 11
        })
    );
    // Someone else's sale, however fresh. "Not yours" is the answer even when
    // the age is over too — it is the one the manager is actually asked about.
    assert_eq!(
        decide(&e, &Request::of(Cap::OrdersVoid).own(false).age_minutes(1)),
        Decision::NeedsApproval(Why::NotYours)
    );
    assert_eq!(
        decide(&e, &Request::of(Cap::OrdersVoid).own(false).age_minutes(99)),
        Decision::NeedsApproval(Why::NotYours)
    );
    // A caller that never learned to say whose sale it is does not get a free
    // pass: unknown authorship is not "mine".
    assert_eq!(
        decide(&e, &Request::of(Cap::OrdersVoid)),
        Decision::NeedsApproval(Why::NotYours)
    );
}

/// A manager holds the same capability unlimited, so they can approve.
#[test]
fn an_unlimited_holder_may_approve_a_capped_void() {
    let mgr = resolve(
        &person(
            vec![assigned(
                role(RoleKind::BranchManager, &[Cap::OrdersVoid]),
                &[],
            )],
            vec![],
        ),
        Scope::Anywhere,
        NOW,
        &OrgPolicy::default(),
    );
    let req = Request::of(Cap::OrdersVoid).own(false).age_minutes(120);
    assert_eq!(decide(&mgr, &req), Decision::Allow);
    assert_eq!(can_approve(&mgr, "mgr", "teller", &req), Ok(()));
    assert_eq!(
        can_approve(&mgr, "mgr", "mgr", &req),
        Err(Why::SamePerson),
        "nobody approves their own"
    );
}

/// `own` is a scope, so it combines and compares the opposite way round to a
/// ceiling: unrestricted is the generous side.
#[test]
fn own_combines_as_the_generous_side_being_unrestricted() {
    let restricted = Limits {
        own: true,
        max_age_minutes: Some(10),
        ..Default::default()
    };
    let free = Limits::UNLIMITED;
    assert_eq!(Limits::most_generous(&restricted, &free), free);
    assert!(restricted.within(&free));
    assert!(!free.within(&restricted));
    // And a capability that does not accept the key never carries it.
    assert!(!restricted.restricted_to(&[LimitKey::MaxAmount]).own);
}

#[test]
fn an_approver_must_be_someone_else_allowed_outright() {
    let mgr = resolve(
        &person(
            vec![assigned(
                role(RoleKind::BranchManager, &[Cap::TillCashSpotCheck]),
                &[],
            )],
            vec![],
        ),
        Scope::Anywhere,
        NOW,
        &OrgPolicy::default(),
    );
    let req = Request::of(Cap::TillCashSpotCheck);
    assert_eq!(can_approve(&mgr, "m", "t", &req), Ok(()));
    assert_eq!(can_approve(&mgr, "m", "m", &req), Err(Why::SamePerson));
    let teller = resolve(
        &person(vec![assigned(role(RoleKind::Teller, &[]), &[])], vec![]),
        Scope::Anywhere,
        NOW,
        &OrgPolicy::default(),
    );
    assert!(can_approve(&teller, "t2", "t", &req).is_err());
}

// ── guard ───────────────────────────────────────────────────────────────────

fn eff_of(kind: RoleKind, caps: &[Cap]) -> EffectiveSet {
    resolve(
        &person(vec![assigned(role(kind, caps), &[])], vec![]),
        Scope::Anywhere,
        NOW,
        &OrgPolicy::default(),
    )
}

#[test]
fn guard_rules() {
    let mut mgr_caps: Vec<Cap> = core_set(RoleKind::Teller).iter().collect();
    mgr_caps.extend([
        Cap::StaffPermissionsEdit,
        Cap::StaffUsersEdit,
        Cap::RefundsCreate,
        Cap::OrdersVoid,
    ]);
    let mgr = eff_of(RoleKind::BranchManager, &mgr_caps);
    let teller = eff_of(RoleKind::Teller, &[Cap::OrdersVoid]);
    let kinds = teller.kinds;
    assert_eq!(
        may_set_override(
            &mgr,
            "m",
            &teller,
            "t",
            kinds,
            Cap::RefundsCreate,
            true,
            None
        ),
        Ok(())
    );
    assert_eq!(
        may_set_override(
            &mgr,
            "m",
            &mgr,
            "m",
            mgr.kinds,
            Cap::RefundsCreate,
            true,
            None
        ),
        Err(GuardError::SelfEdit)
    );
    assert!(matches!(
        may_set_override(
            &mgr,
            "m",
            &teller,
            "t",
            kinds,
            Cap::TillForceClose,
            true,
            None
        ),
        Err(GuardError::NotHeld { .. })
    ));
    assert!(matches!(
        may_set_override(
            &mgr,
            "m",
            &teller,
            "t",
            kinds,
            Cap::BranchesRead,
            false,
            None
        ),
        Err(GuardError::CoreRemoval { .. })
    ));
    // The teller holds something the manager lacks: not dominant.
    let odd = eff_of(RoleKind::Teller, &[Cap::HrPayrollRead]);
    assert!(matches!(
        may_touch(&mgr, "m", &odd, "t",),
        Err(GuardError::NotDominant { .. })
    ));
    // Owners.
    let mut owner = EffectiveSet {
        caps: CapSet::all(),
        owner: true,
        ..Default::default()
    };
    owner.kinds.insert(RoleKind::OrgAdmin);
    assert_eq!(
        may_touch(&mgr, "m", &owner, "o"),
        Err(GuardError::OwnerProtected)
    );
    assert_eq!(
        may_set_override(
            &owner,
            "o1",
            &owner,
            "o2",
            owner.kinds,
            Cap::StaffPermissionsEdit,
            false,
            None
        ),
        Err(GuardError::OwnerProtected)
    );
}

// ── properties ──────────────────────────────────────────────────────────────

fn any_cap() -> impl Strategy<Value = Cap> {
    (0..CAPS.len()).prop_map(|i| CAPS[i].cap)
}

fn any_kind() -> impl Strategy<Value = RoleKind> {
    prop::sample::select(RoleKind::ALL.to_vec())
}

fn any_principal() -> impl Strategy<Value = Principal> {
    let asg = (
        any_kind(),
        prop::collection::vec(any_cap(), 0..12),
        prop::bool::ANY,
        prop::sample::subsequence(vec!["b1", "b2", "b3"], 0..3),
    )
        .prop_map(|(k, caps, all, br)| assigned(role(k, &caps), if all { &[] } else { &br[..] }));
    let ovr = (
        any_cap(),
        prop::bool::ANY,
        prop::option::of(prop::sample::select(vec!["b1", "b2"])),
    )
        .prop_map(|(c, allow, b)| ov(c, allow, b));
    (
        prop::collection::vec(asg, 0..3),
        prop::collection::vec(ovr, 0..6),
        prop::bool::weighted(0.1),
    )
        .prop_map(|(a, o, owner)| Principal {
            user_id: "p".into(),
            active: true,
            is_owner: owner,
            assignments: a,
            overrides: o,
        })
}

proptest! {
    /// P1: adding an allow never removes a capability.
    #[test]
    fn p1_an_allow_is_monotone(p in any_principal(), c in any_cap(), branch in prop::sample::select(vec!["b1", "b2", "b3"])) {
        let pol = OrgPolicy::default();
        let before = resolve(&p, Scope::Branch(branch), NOW, &pol);
        let mut q = p.clone();
        q.overrides.retain(|o| o.cap != c.id());
        let base = resolve(&q, Scope::Branch(branch), NOW, &pol);
        q.overrides.push(ov(c, true, None));
        let after = resolve(&q, Scope::Branch(branch), NOW, &pol);
        prop_assert!(base.caps.is_subset(&after.caps));
        prop_assert!(after.caps.contains(c));
        let _ = before;
    }

    /// P3: an owner always holds every protected capability.
    #[test]
    fn p3_owners_keep_protected(mut p in any_principal()) {
        p.is_owner = true;
        let e = resolve(&p, Scope::Anywhere, NOW, &OrgPolicy::default());
        for m in CAPS.iter().filter(|m| m.protected) {
            prop_assert!(e.can(m.cap));
        }
    }

    /// P4: resolution does not depend on the order grants arrive in.
    #[test]
    fn p4_order_independent(p in any_principal()) {
        let pol = OrgPolicy::default();
        let mut q = p.clone();
        q.assignments.reverse();
        q.overrides.reverse();
        for s in [Scope::Anywhere, Scope::Branch("b1"), Scope::Branch("b2")] {
            let a = resolve(&p, s, NOW, &pol);
            let b = resolve(&q, s, NOW, &pol);
            prop_assert_eq!(a.caps, b.caps);
        }
    }

    /// Core grants survive any overrides.
    #[test]
    fn core_survives_any_overrides(p in any_principal()) {
        let e = resolve(&p, Scope::Anywhere, NOW, &OrgPolicy::default());
        for k in e.kinds.iter() {
            prop_assert!(core_set(k).is_subset(&e.caps));
        }
    }

    /// Anti-escalation closure (I1): whatever override writes a non-owner actor
    /// gets accepted against a target, the target never ends up holding a
    /// capability the actor lacks.
    #[test]
    fn i1_accepted_overrides_never_lift_a_target_above_the_actor(
        actor in any_principal(),
        target in any_principal(),
        writes in prop::collection::vec((any_cap(), prop::bool::ANY), 1..10),
    ) {
        let pol = OrgPolicy::default();
        let mut actor = actor;
        actor.is_owner = false;
        let a = resolve(&actor, Scope::Anywhere, NOW, &pol);
        let mut t = target;
        for (cap, allow) in writes {
            let te = resolve(&t, Scope::Anywhere, NOW, &pol);
            if may_set_override(&a, "actor", &te, "target", te.kinds, cap, allow, None).is_ok() {
                t.overrides.push(ov(cap, allow, None));
                let after = resolve(&t, Scope::Anywhere, NOW, &pol);
                prop_assert!(after.caps.is_subset(&a.caps), "target rose above actor");
            }
        }
    }

    /// G3: a role edit accepted from a non-owner never adds what they lack.
    #[test]
    fn g3_role_edits_stay_within_the_actor(actor in any_principal(), before in prop::collection::vec(any_cap(), 0..10), after in prop::collection::vec(any_cap(), 0..10), kind in any_kind()) {
        let mut actor = actor;
        actor.is_owner = false;
        let a = resolve(&actor, Scope::Anywhere, NOW, &OrgPolicy::default());
        let b: CapSet = before.into_iter().collect();
        let f: CapSet = after.into_iter().collect();
        if may_edit_role(&a, Kinds(kind.bit()), &b, &f).is_ok() {
            prop_assert!(f.minus(&b).is_subset(&a.caps));
        }
    }
}

// ── the vendored copy is not edited by hand ─────────────────────────────────

#[test]
fn crate_hash_matches_its_files() {
    const FILES: &[&[u8]] = &[
        include_bytes!("../Cargo.toml"),
        include_bytes!("lib.rs"),
        include_bytes!("generated.rs"),
        include_bytes!("guard.rs"),
        include_bytes!("snapshot.rs"),
        include_bytes!("tests.rs"),
    ];
    let mut h: u64 = 0xcbf29ce484222325;
    for p in FILES {
        for b in *p {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x100000001b3);
    }
    let recorded = include_str!("../crate.hash").trim();
    assert_eq!(
        format!("{h:016x}"),
        recorded,
        "madar-authz was edited without regenerating: edit MadarRust/authz, run authz-gen"
    );
}

#[test]
fn template_limits_sit_on_grants_the_role_holds() {
    for t in TEMPLATES {
        for (k, c, l) in t.limits {
            let g = template_grants(t.key, *k).unwrap();
            assert!(g.contains(*c), "{}: {k:?} limit on {} it does not hold", t.key, c.key());
            assert_ne!(*l, Limits::UNLIMITED, "{}: empty limit on {}", t.key, c.key());
        }
        // The locked decision: a teller voids their own sale within ten minutes,
        // and any refund goes to a manager.
        let teller = template_limits(t.key, RoleKind::Teller);
        let void = teller.iter().find(|(c, _)| *c == Cap::OrdersVoid).unwrap().1;
        assert!(void.own);
        assert_eq!(void.max_age_minutes, Some(10));
        let refund = teller.iter().find(|(c, _)| *c == Cap::RefundsCreate).unwrap().1;
        assert_eq!(refund.max_amount, Some(0));
        assert!(!template_grants(t.key, RoleKind::Waiter).unwrap().contains(Cap::RefundsCreate));
    }
}

/// Owner decisions 2026-09-16: no peer writes for non-owners, and a
/// permissions editor revokes only what they hold.
#[test]
fn a_non_owner_touches_only_people_strictly_below_them() {
    let mut mgr_caps: Vec<Cap> = core_set(RoleKind::Teller).iter().collect();
    mgr_caps.extend([Cap::StaffPermissionsEdit, Cap::StaffUsersEdit, Cap::RefundsCreate]);
    let mgr = eff_of(RoleKind::BranchManager, &mgr_caps);
    // A peer holding LESS than the actor is still refused: rank, not caps.
    let peer = eff_of(RoleKind::BranchManager, &[]);
    assert_eq!(may_touch(&mgr, "m1", &peer, "m2"), Err(GuardError::NotAbove));
    assert_eq!(
        may_set_override(&mgr, "m1", &peer, "m2", peer.kinds, Cap::RefundsCreate, true, None),
        Err(GuardError::NotAbove)
    );
    assert_eq!(
        may_assign(&mgr, "m1", &peer, "m2", &CapSet::default()),
        Err(GuardError::NotAbove)
    );
    assert_eq!(may_give_kind(&mgr, RoleKind::BranchManager), Err(GuardError::NotAbove));
    assert_eq!(may_give_kind(&mgr, RoleKind::OrgAdmin), Err(GuardError::OwnerProtected));
    assert_eq!(may_give_kind(&mgr, RoleKind::Teller), Ok(()));

    // Revoking needs the capability too.
    let teller = eff_of(RoleKind::Teller, &[Cap::HrPayrollRead]);
    let t_kinds = teller.kinds;
    let mut odd_mgr = mgr.clone();
    odd_mgr.caps = odd_mgr.caps.union(&[Cap::HrPayrollRead].into_iter().collect());
    assert_eq!(
        may_set_override(&odd_mgr, "m", &teller, "t", t_kinds, Cap::TillForceClose, false, None),
        Err(GuardError::NotHeld { cap: Cap::TillForceClose.key().into() })
    );
    // ...and granting the editing power onward needs holding it (it does here),
    // while granting role management it lacks is refused.
    assert_eq!(
        may_set_override(&odd_mgr, "m", &teller, "t", t_kinds, Cap::StaffRolesManage, true, None),
        Err(GuardError::NotHeld { cap: Cap::StaffRolesManage.key().into() })
    );
    assert_eq!(
        may_set_override(&odd_mgr, "m", &teller, "t", t_kinds, Cap::StaffOwnersManage, true, None),
        Err(GuardError::NotHeld { cap: Cap::StaffOwnersManage.key().into() })
    );

    // Owners are unchanged: an owner touches another owner's non-protected access.
    let mut owner = EffectiveSet { caps: CapSet::all(), owner: true, ..Default::default() };
    owner.kinds.insert(RoleKind::OrgAdmin);
    assert_eq!(may_touch(&owner, "o1", &owner.clone(), "o2"), Ok(()));
    assert_eq!(may_give_kind(&owner, RoleKind::OrgAdmin), Ok(()));
}

// ── remote approvals (PM-1) ─────────────────────────────────────────────────

#[test]
fn an_act_over_the_limit_waits_for_someone_without_that_limit() {
    let capped = |max: Option<i64>| {
        let mut r = role(RoleKind::BranchManager, &[Cap::HrAdjustmentsCreate]);
        if let Some(m) = max {
            r.limits.insert(
                Cap::HrAdjustmentsCreate.id(),
                Limits {
                    max_amount: Some(m),
                    ..Default::default()
                },
            );
        }
        resolve(
            &person(vec![assigned(r, &[])], vec![]),
            Scope::Anywhere,
            NOW,
            &OrgPolicy::default(),
        )
    };
    let manager = capped(Some(1000_00));
    let bigger = capped(Some(5000_00));
    let owner = capped(None);
    let req = Request::of(Cap::HrAdjustmentsCreate).amount(2000_00);

    assert_eq!(park(decide(&manager, &Request::of(Cap::HrAdjustmentsCreate).amount(5_00)), req, "emp", "mgr"), None);
    let p = park(decide(&manager, &req), req, "emp", "mgr").expect("waits");
    assert!(matches!(p.why, Why::OverLimit { limit: 1000_00, asked: 2000_00, .. }));

    assert_eq!(can_settle(&bigger, "other", &p), Ok(()));
    assert_eq!(can_settle(&owner, "owner", &p), Ok(()));
    assert!(matches!(can_settle(&manager, "peer", &p), Err(Why::OverLimit { .. })), "same limit can't");
    assert_eq!(can_settle(&owner, "mgr", &p), Err(Why::SamePerson), "not who asked");
    assert_eq!(can_settle(&owner, "emp", &p), Err(Why::SamePerson), "not who it's for");
    let json = serde_json::to_string(&p).unwrap();
    assert_eq!(serde_json::from_str::<Pending>(&json).unwrap(), p, "stored as it was parked");
}
