//! The Phase 4 reconciliation (DB audit 2026-10-16 §8 Phase 4, §10 "authz"):
//! for every person x capability x scope, the decision the LEGACY tables imply
//! next to the decision the architecture E tables give. The cut-over to the
//! single model is safe exactly when the two agree everywhere.
//!
//! This replaces "30 days of shadow agreement" with an exhaustive check: the
//! shadow comparison only ever saw the cells somebody happened to hit, and only
//! at `Scope::Anywhere`; this walks all of them, at every live branch too.
//!
//! **Old side** (`old_effective`) is computed from the legacy tables only —
//! `users.role`, `user_branch_assignments`, the global `role_permissions` and
//! the per-user `permissions` — through THE MAPPING, the rules the mirror
//! triggers (`20260918100000_authz_org_model`, `20260918140000_authz_branch_truth`,
//! `20260918210000_provisioned_roles_keep_their_template`) and the shared crate
//! apply. Every rule is listed here, because a rule not written down is a
//! difference nobody can review:
//!
//! 1. A person's role grants: the org's system role of their `users.role`.
//!    For a role still on the legacy template, a capability WITH a legacy cell
//!    is granted exactly when the global `role_permissions` grants that cell
//!    (the mirror trigger's rule). A capability with NO legacy cell was never
//!    decided by the legacy model: its only record is the role's stored grant,
//!    so that is what both sides use (`spec_drift` lists where the stored grant
//!    differs from today's spec default — an owner decision, not a diff). A
//!    role provisioned from a template (`orgs::provision`) is outside the
//!    legacy tables altogether: its stored grants are used, and `spec_drift`
//!    compares them with `template_grants`.
//! 2. Branches: a branch manager works at the branches listed for them; anyone
//!    else works everywhere until a branch is listed for them, then only there.
//!    `Scope::Anywhere` is covered by any assignment (as `resolve` does).
//! 3. Core capabilities are always held by their role kinds.
//! 4. `org_admin` is the owner, and an owner holds every non-legacy capability.
//! 5. A `permissions` row is an org-wide override: allow adds the capability;
//!    deny removes it unless it is core for a kind the person holds there, or
//!    protected and the person is the owner.
//! 6. The plan's ceiling (`madar_plan::apply`) cuts both sides alike.
//! 7. "Works here" (`owner || a role kind covers the scope`) gates both sides:
//!    a capability at a branch the person does not work at is no decision.
//!
//! **Authored in the new model.** Access an owner set in the access editor
//! (a hand-made assignment, a role grant that is not the template's, an
//! override that is not a mirrored legacy one, or a branch-specific override)
//! has no legacy representation at all; the person is counted and listed as
//! `new_authored` and not compared. Limits are not compared (the legacy model
//! has none).
//!
//! Read-only. The output is one TSV, no names and no customer data.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use madar_plan::Plan;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use super::{Cap, CapSet, EffectiveSet, RoleKind, Scope};
use crate::errors::AppError;

/// Where a decision is asked: anywhere (the unscoped `check_permission`) or at
/// one live branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum At {
    Anywhere,
    Branch(Uuid),
}

impl std::fmt::Display for At {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            At::Anywhere => f.write_str("*"),
            At::Branch(b) => write!(f, "{b}"),
        }
    }
}

/// One person x capability x scope where the two sides disagree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diff {
    pub org_id: Uuid,
    pub user_id: Uuid,
    pub role: String,
    pub at: At,
    pub cap: &'static str,
    pub old: bool,
    pub new: bool,
}

/// One org's result.
#[derive(Clone, Debug, Default)]
pub struct OrgResult {
    pub org_id: Uuid,
    pub users: usize,
    pub compared: usize,
    pub new_authored: Vec<(Uuid, &'static str)>,
    /// Decisions compared (people x capabilities x scopes).
    pub cells: usize,
    pub old_hash: String,
    pub new_hash: String,
    pub diffs: Vec<Diff>,
    /// (role key, capability, stored, spec default) for the org's system roles.
    pub spec_drift: Vec<(String, &'static str, bool, bool)>,
}

/// A `role_permissions` cell whose global default differs from the spec's
/// default for the role kind (rule 1 uses `role_permissions`, so these are
/// not decision diffs; they are what a role seeded from the spec would change).
#[derive(Clone, Debug)]
pub struct RoleDefaultDiff {
    pub role: String,
    pub resource: &'static str,
    pub action: &'static str,
    pub role_permissions: Option<bool>,
    pub spec_default: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub orgs: Vec<OrgResult>,
    pub role_defaults: Vec<RoleDefaultDiff>,
}

impl Report {
    pub fn diff_count(&self) -> usize {
        self.orgs.iter().map(|o| o.diffs.len()).sum()
    }

    /// The TSV: `org` rows (one per org), `diff` rows, `new_authored` rows and
    /// `role_default` rows. Ids only.
    pub fn to_tsv(&self) -> String {
        let mut out = String::from(
            "kind\torg_id\tusers\tcompared\tnew_authored\tcells\told_hash\tnew_hash\tdiffs\n",
        );
        for o in &self.orgs {
            out.push_str(&format!(
                "org\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                o.org_id,
                o.users,
                o.compared,
                o.new_authored.len(),
                o.cells,
                o.old_hash,
                o.new_hash,
                o.diffs.len()
            ));
        }
        for o in &self.orgs {
            for d in &o.diffs {
                out.push_str(&format!(
                    "diff\t{}\t{}\t{}\t{}\t{}\told={}\tnew={}\n",
                    d.org_id, d.user_id, d.role, d.at, d.cap, d.old, d.new
                ));
            }
            for (role, cap, stored, spec) in &o.spec_drift {
                out.push_str(&format!(
                    "spec_drift\t{}\t{}\t{}\tstored={}\tspec={}\n",
                    o.org_id, role, cap, stored, spec
                ));
            }
            for (u, why) in &o.new_authored {
                out.push_str(&format!("new_authored\t{}\t{}\t{}\n", o.org_id, u, why));
            }
        }
        for r in &self.role_defaults {
            out.push_str(&format!(
                "role_default\t{}\t{}:{}\trole_permissions={}\tspec={}\n",
                r.role,
                r.resource,
                r.action,
                r.role_permissions
                    .map_or("none".to_string(), |g| g.to_string()),
                r.spec_default
            ));
        }
        out
    }
}

/// Everything the old side reads, for one person.
#[derive(Clone, Debug)]
pub struct LegacyPerson {
    pub kind: RoleKind,
    pub active: bool,
    /// Live branches of the person's org listed in `user_branch_assignments`.
    pub listed: BTreeSet<Uuid>,
    /// `permissions` rows: (resource, action) -> granted.
    pub overrides: Vec<(String, String, bool)>,
}

/// The grants rule 1 gives a role kind under its template, from the legacy
/// table (legacy cells of a legacy-template role) and the role's stored grants
/// (everything else).
pub fn role_grants(
    kind: RoleKind,
    template: Option<&str>,
    stored: &CapSet,
    role_permissions: &HashMap<(String, String, String), bool>,
) -> CapSet {
    if template.is_some_and(|t| t != "legacy") {
        return *stored;
    }
    let mut s = CapSet::EMPTY;
    for m in super::CAPS {
        let held = match m.legacy {
            Some((r, a)) => role_permissions
                .get(&(kind.as_str().to_string(), r.to_string(), a.to_string()))
                .copied()
                .unwrap_or(false),
            None => stored.contains(m.cap),
        };
        if held {
            s.insert(m.cap);
        }
    }
    s
}

/// Where a system role's stored grants differ from what the spec would give
/// that role today (rule 1's note). Core capabilities are implied, never
/// stored, so they are not compared.
pub fn spec_drift_of(kind: RoleKind, template: Option<&str>, stored: &CapSet) -> Vec<(Cap, bool, bool)> {
    let core = super::core_set(kind);
    let (expected, legacy_template) = match template {
        Some(t) if t != "legacy" => (super::template_grants(t, kind).unwrap_or_default(), false),
        _ => (
            super::CAPS
                .iter()
                .filter(|m| m.defaults.contains(kind))
                .map(|m| m.cap)
                .collect(),
            true,
        ),
    };
    super::CAPS
        .iter()
        .filter(|m| !core.contains(m.cap))
        // A legacy-template role's legacy cells follow role_permissions.
        .filter(|m| !(legacy_template && m.legacy.is_some()))
        .filter_map(|m| {
            let (st, sp) = (stored.contains(m.cap), expected.contains(m.cap));
            (st != sp).then_some((m.cap, st, sp))
        })
        .collect()
}

/// The old side's effective set for one person at one scope (rules 1-6).
pub fn old_effective(
    p: &LegacyPerson,
    grants: &CapSet,
    plan: Plan,
    at: At,
) -> EffectiveSet {
    let mut eff = EffectiveSet::default();
    if !p.active {
        return eff;
    }
    let owner = p.kind == RoleKind::OrgAdmin;
    let all_branches = p.kind != RoleKind::BranchManager && p.listed.is_empty();
    let covers = match at {
        At::Anywhere => true,
        At::Branch(b) => all_branches || p.listed.contains(&b),
    };
    if owner {
        eff.owner = true;
        eff.kinds.insert(RoleKind::OrgAdmin);
        eff.caps = super::owner_set();
    }
    if covers {
        eff.kinds.insert(p.kind);
        eff.caps = eff.caps.union(grants).union(&super::core_set(p.kind));
    }
    for (r, a, granted) in &p.overrides {
        let Some(c) = Cap::from_legacy(r, a) else {
            continue;
        };
        if *granted {
            eff.caps.insert(c);
        } else if !((eff.owner && c.meta().protected) || super::is_core_for(c, eff.kinds)) {
            eff.caps.remove(c);
        }
    }
    madar_plan::apply(plan, eff)
}

fn works(eff: &EffectiveSet) -> bool {
    eff.owner || eff.kinds.0 != 0
}

/// The decision a set gives for `c` (rule 7).
fn decides(eff: &EffectiveSet, c: Cap) -> bool {
    works(eff) && eff.can(c)
}

fn hash(lines: &BTreeSet<String>) -> String {
    let mut h = Sha256::new();
    for l in lines {
        h.update(l.as_bytes());
        h.update(b"\n");
    }
    h.finalize()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Why a person's access cannot be compared with the legacy tables, if it
/// cannot.
async fn new_authored(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Option<&'static str>, AppError> {
    let r = sqlx::query!(
        r#"SELECT
             EXISTS (SELECT 1 FROM role_assignments
                      WHERE user_id = $1 AND revoked_at IS NULL AND NOT managed) AS "hand_assignment!",
             EXISTS (SELECT 1 FROM user_overrides
                      WHERE user_id = $1 AND revoked_at IS NULL
                        AND (branch_id IS NOT NULL
                             OR reason IS DISTINCT FROM 'legacy override'
                                AND reason IS DISTINCT FROM 'migrated from legacy override')) AS "editor_override!",
             EXISTS (SELECT 1 FROM role_assignments ra
                       JOIN org_role_grants g ON g.org_role_id = ra.org_role_id
                      WHERE ra.user_id = $1 AND ra.revoked_at IS NULL
                        AND g.source IS DISTINCT FROM 'template') AS "edited_role!""#,
        user_id
    )
    .fetch_one(pool)
    .await?;
    Ok(if r.hand_assignment {
        Some("assignment set in the access editor")
    } else if r.editor_override {
        Some("override set in the access editor")
    } else if r.edited_role {
        Some("role grants edited in the access editor")
    } else {
        None
    })
}

/// Run the reconciliation over every org (or one).
pub async fn run(pool: &PgPool, only_org: Option<Uuid>) -> Result<Report, AppError> {
    let rp_rows = sqlx::query!(
        r#"SELECT role::text AS "role!", resource::text AS "resource!", action::text AS "action!", granted
             FROM role_permissions"#
    )
    .fetch_all(pool)
    .await?;
    let role_permissions: HashMap<(String, String, String), bool> = rp_rows
        .into_iter()
        .map(|r| ((r.role, r.resource, r.action), r.granted))
        .collect();

    let mut report = Report::default();
    for kind in RoleKind::ALL {
        for (r, a) in crate::permissions::permission_cells() {
            let Some(c) = Cap::from_legacy(r, a) else {
                continue;
            };
            let spec = c.meta().defaults.contains(kind) || c.meta().core.contains(kind);
            let rp = role_permissions
                .get(&(kind.as_str().to_string(), r.to_string(), a.to_string()))
                .copied();
            if rp.unwrap_or(false) != spec {
                report.role_defaults.push(RoleDefaultDiff {
                    role: kind.as_str().to_string(),
                    resource: r,
                    action: a,
                    role_permissions: rp,
                    spec_default: spec,
                });
            }
        }
    }

    let orgs = sqlx::query!(
        r#"SELECT id, plan FROM organizations
            WHERE ($1::uuid IS NULL OR id = $1) ORDER BY id"#,
        only_org
    )
    .fetch_all(pool)
    .await?;
    let mut conn = pool.acquire().await?;
    let now = chrono::Utc::now().timestamp();
    for org in orgs {
        let plan = Plan::parse(&org.plan);
        let branches: Vec<Uuid> = sqlx::query_scalar!(
            "SELECT id FROM branches WHERE org_id = $1 AND deleted_at IS NULL ORDER BY id",
            org.id
        )
        .fetch_all(pool)
        .await?;
        let roles = sqlx::query!(
            r#"SELECT r.key, r.template_key,
                      COALESCE(ARRAY(SELECT g.capability_id FROM org_role_grants g
                                      WHERE g.org_role_id = r.id), '{}') AS "grants!: Vec<i16>"
                 FROM org_roles r
                WHERE r.org_id = $1 AND r.is_system AND r.deleted_at IS NULL"#,
            org.id
        )
        .fetch_all(pool)
        .await?;
        let mut templates: HashMap<String, (Option<String>, CapSet)> = HashMap::new();
        let mut spec_drift = Vec::new();
        for r in roles {
            let stored: CapSet = r
                .grants
                .iter()
                .filter_map(|c| Cap::from_id(*c as u16))
                .collect();
            if let Some(kind) = RoleKind::parse(&r.key) {
                for (c, st, sp) in spec_drift_of(kind, r.template_key.as_deref(), &stored) {
                    spec_drift.push((r.key.clone(), c.key(), st, sp));
                }
            }
            templates.insert(r.key, (r.template_key, stored));
        }
        let people = sqlx::query!(
            r#"SELECT u.id, u.role::text AS "role!", (u.is_active AND u.deleted_at IS NULL) AS "active!",
                      COALESCE(ARRAY(SELECT a.branch_id FROM user_branch_assignments a
                                       JOIN branches b ON b.id = a.branch_id
                                                      AND b.org_id = u.org_id AND b.deleted_at IS NULL
                                      WHERE a.user_id = u.id), '{}') AS "listed!: Vec<Uuid>"
                 FROM users u
                WHERE u.org_id = $1 AND u.deleted_at IS NULL AND u.role <> 'super_admin'
                  AND NOT u.is_guest_principal
                ORDER BY u.id"#,
            org.id
        )
        .fetch_all(pool)
        .await?;
        let overrides = sqlx::query!(
            r#"SELECT p.user_id, p.resource::text AS "resource!", p.action::text AS "action!", p.granted
                 FROM permissions p JOIN users u ON u.id = p.user_id
                WHERE u.org_id = $1"#,
            org.id
        )
        .fetch_all(pool)
        .await?;
        let mut by_user: HashMap<Uuid, Vec<(String, String, bool)>> = HashMap::new();
        for o in overrides {
            by_user
                .entry(o.user_id)
                .or_default()
                .push((o.resource, o.action, o.granted));
        }

        let mut res = OrgResult {
            org_id: org.id,
            users: people.len(),
            spec_drift,
            ..Default::default()
        };
        let mut old_lines = BTreeSet::new();
        let mut new_lines = BTreeSet::new();
        let mut grants_cache: BTreeMap<String, CapSet> = BTreeMap::new();
        let scopes: Vec<At> = std::iter::once(At::Anywhere)
            .chain(branches.iter().map(|b| At::Branch(*b)))
            .collect();
        for person in people {
            let Some(kind) = RoleKind::parse(&person.role) else {
                continue;
            };
            if let Some(why) = new_authored(pool, person.id).await? {
                res.new_authored.push((person.id, why));
                continue;
            }
            res.compared += 1;
            let legacy = LegacyPerson {
                kind,
                active: person.active,
                listed: person.listed.into_iter().collect(),
                overrides: by_user.remove(&person.id).unwrap_or_default(),
            };
            let grants = grants_cache
                .entry(person.role.clone())
                .or_insert_with(|| {
                    let (template, stored) = templates
                        .get(&person.role)
                        .cloned()
                        .unwrap_or((None, CapSet::EMPTY));
                    role_grants(kind, template.as_deref(), &stored, &role_permissions)
                })
                .clone();
            let loaded = super::load::load(&mut conn, person.id).await?;
            for at in &scopes {
                let old = old_effective(&legacy, &grants, plan, *at);
                let b = match at {
                    At::Branch(b) => Some(b.to_string()),
                    At::Anywhere => None,
                };
                let new = match &loaded {
                    Some(l) => l.resolve_at(
                        match &b {
                            Some(b) => Scope::Branch(b),
                            None => Scope::Anywhere,
                        },
                        now,
                    ),
                    None => EffectiveSet::default(),
                };
                for c in Cap::all() {
                    // Legacy-tier capabilities (dead cells kept for old
                    // tablets) are compared like any other.
                    res.cells += 1;
                    let (o, n) = (decides(&old, c), decides(&new, c));
                    if o {
                        old_lines.insert(format!("{}:{at}:{}", person.id, c.id()));
                    }
                    if n {
                        new_lines.insert(format!("{}:{at}:{}", person.id, c.id()));
                    }
                    if o != n {
                        res.diffs.push(Diff {
                            org_id: org.id,
                            user_id: person.id,
                            role: person.role.clone(),
                            at: *at,
                            cap: c.key(),
                            old: o,
                            new: n,
                        });
                    }
                }
            }
        }
        res.old_hash = hash(&old_lines);
        res.new_hash = hash(&new_lines);
        report.orgs.push(res);
    }
    Ok(report)
}
