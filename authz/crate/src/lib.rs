//! # madar-authz
//!
//! The permission decision library shared by the backend (MadarRust) and the POS
//! core (madar-core). The same code answers "may this person do this" online on
//! the server and offline on a till, so the two can never disagree.
//!
//! - [`generated`]: the capability registry, generated from
//!   `authz/spec/capabilities.toml`. Never edit by hand.
//! - [`CapSet`]: a fixed-size bitset of capabilities.
//! - [`resolve`]: a person's effective capabilities (roles per branch, core
//!   grants, per-user allow/deny, owner rule).
//! - [`decide`]: allow, needs a manager's approval, or deny, with limits.
//! - [`guard`]: anti-escalation rules for every write that changes access.
//! - [`legacy`]: the old `resource:action` cells for pre-0.8 tablets.
//!
//! Nothing here does I/O. Callers load grants and pass them in.

#[rustfmt::skip]
pub mod generated;
pub mod guard;
pub mod snapshot;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use generated::{Cap, CAPS, GROUPS, ROLE_LABELS, SPEC_HASH, SPEC_VERSION, TEMPLATES, WORDS};

// ── Role kinds ──────────────────────────────────────────────────────────────

/// What a role "behaves like". Every custom role has one; it decides the core
/// grants a role can never lose, and the role name older tablets see.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleKind {
    OrgAdmin,
    BranchManager,
    Teller,
    Waiter,
    Kitchen,
}

impl RoleKind {
    pub const ALL: [RoleKind; 5] = [
        RoleKind::OrgAdmin,
        RoleKind::BranchManager,
        RoleKind::Teller,
        RoleKind::Waiter,
        RoleKind::Kitchen,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            RoleKind::OrgAdmin => "org_admin",
            RoleKind::BranchManager => "branch_manager",
            RoleKind::Teller => "teller",
            RoleKind::Waiter => "waiter",
            RoleKind::Kitchen => "kitchen",
        }
    }

    pub fn parse(s: &str) -> Option<RoleKind> {
        RoleKind::ALL.into_iter().find(|k| k.as_str() == s)
    }

    pub const fn bit(self) -> u8 {
        match self {
            RoleKind::OrgAdmin => 1,
            RoleKind::BranchManager => 2,
            RoleKind::Teller => 4,
            RoleKind::Waiter => 8,
            RoleKind::Kitchen => 16,
        }
    }
}

/// A set of role kinds.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Kinds(pub u8);

impl Kinds {
    pub const fn contains(self, k: RoleKind) -> bool {
        self.0 & k.bit() != 0
    }
    pub fn insert(&mut self, k: RoleKind) {
        self.0 |= k.bit();
    }
    pub fn iter(self) -> impl Iterator<Item = RoleKind> {
        RoleKind::ALL.into_iter().filter(move |k| self.contains(*k))
    }
}

// ── Capability metadata ─────────────────────────────────────────────────────

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Always on for its core role kinds; never a toggle.
    Core,
    /// Shown with a plain name in its group.
    Configurable,
    /// Behind the "Advanced" section.
    Advanced,
    /// Never shown; kept for older tablets' permission payload.
    Legacy,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Normal,
    Money,
    Pii,
    Admin,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitKey {
    /// Money in minor units (piastres).
    MaxAmount,
    /// Basis points (1000 = 10%).
    MaxPercent,
    /// Stock value in minor units.
    MaxValue,
    /// How old the thing acted on may be, in minutes. "A teller may void their
    /// own sale within 10 minutes" is this plus [`LimitKey::Own`].
    MaxAgeMinutes,
    /// Only what this person did themselves. Not a ceiling but a scope, so it
    /// is a flag rather than a number: see [`Limits::own`].
    Own,
}

#[derive(Debug)]
pub struct CapMeta {
    pub cap: Cap,
    pub key: &'static str,
    pub legacy: Option<(&'static str, &'static str)>,
    pub group: &'static str,
    pub tier: Tier,
    pub risk: Risk,
    pub defaults: Kinds,
    pub core: Kinds,
    pub approval: bool,
    pub limits: &'static [LimitKey],
    pub pos: bool,
    pub protected: bool,
    pub en: &'static str,
    pub ar: &'static str,
    pub hint_en: Option<&'static str>,
    pub hint_ar: Option<&'static str>,
}

pub struct GroupMeta {
    pub key: &'static str,
    pub en: &'static str,
    pub ar: &'static str,
}

pub struct TemplateMeta {
    pub key: &'static str,
    pub version: u32,
    pub en: &'static str,
    pub ar: &'static str,
    pub roles: &'static [RoleKind],
    pub add: &'static [(RoleKind, Cap)],
    pub remove: &'static [(RoleKind, Cap)],
    /// Default limits on a provisioned org's grants, per role kind.
    pub limits: &'static [(RoleKind, Cap, Limits)],
}

impl Cap {
    pub fn id(self) -> u16 {
        self as u16
    }

    pub fn meta(self) -> &'static CapMeta {
        let i = CAPS
            .binary_search_by_key(&self.id(), |m| m.cap.id())
            .expect("every Cap has metadata");
        &CAPS[i]
    }

    pub fn key(self) -> &'static str {
        self.meta().key
    }

    pub fn from_id(id: u16) -> Option<Cap> {
        CAPS.binary_search_by_key(&id, |m| m.cap.id())
            .ok()
            .map(|i| CAPS[i].cap)
    }

    pub fn from_key(key: &str) -> Option<Cap> {
        CAPS.iter().find(|m| m.key == key).map(|m| m.cap)
    }

    /// The capability that answers for an old `resource:action` cell.
    pub fn from_legacy(resource: &str, action: &str) -> Option<Cap> {
        CAPS.iter()
            .find(|m| m.legacy == Some((resource, action)))
            .map(|m| m.cap)
    }

    pub fn all() -> impl Iterator<Item = Cap> {
        CAPS.iter().map(|m| m.cap)
    }
}

// ── CapSet ──────────────────────────────────────────────────────────────────

/// A set of capabilities as a bitset over capability ids. O(1) membership.
#[derive(Copy, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CapSet(pub [u64; WORDS]);

impl std::fmt::Debug for CapSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter().map(Cap::key)).finish()
    }
}

impl CapSet {
    pub const EMPTY: CapSet = CapSet([0; WORDS]);

    pub fn all() -> CapSet {
        Cap::all().collect()
    }

    pub fn contains(&self, c: Cap) -> bool {
        let id = c.id() as usize;
        self.0[id / 64] & (1u64 << (id % 64)) != 0
    }

    pub fn insert(&mut self, c: Cap) {
        let id = c.id() as usize;
        self.0[id / 64] |= 1u64 << (id % 64);
    }

    pub fn remove(&mut self, c: Cap) {
        let id = c.id() as usize;
        self.0[id / 64] &= !(1u64 << (id % 64));
    }

    pub fn union(&self, o: &CapSet) -> CapSet {
        let mut r = *self;
        for (a, b) in r.0.iter_mut().zip(o.0) {
            *a |= b;
        }
        r
    }

    pub fn intersect(&self, o: &CapSet) -> CapSet {
        let mut r = *self;
        for (a, b) in r.0.iter_mut().zip(o.0) {
            *a &= b;
        }
        r
    }

    pub fn minus(&self, o: &CapSet) -> CapSet {
        let mut r = *self;
        for (a, b) in r.0.iter_mut().zip(o.0) {
            *a &= !b;
        }
        r
    }

    pub fn is_subset(&self, o: &CapSet) -> bool {
        self.0.iter().zip(o.0).all(|(a, b)| a & !b == 0)
    }

    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|w| *w == 0)
    }

    pub fn len(&self) -> usize {
        self.0.iter().map(|w| w.count_ones() as usize).sum()
    }

    pub fn iter(&self) -> impl Iterator<Item = Cap> + '_ {
        Cap::all().filter(move |c| self.contains(*c))
    }

    pub fn keys(&self) -> Vec<&'static str> {
        self.iter().map(Cap::key).collect()
    }

    pub fn from_keys<'a>(keys: impl IntoIterator<Item = &'a str>) -> CapSet {
        keys.into_iter().filter_map(Cap::from_key).collect()
    }
}

impl FromIterator<Cap> for CapSet {
    fn from_iter<I: IntoIterator<Item = Cap>>(iter: I) -> Self {
        let mut s = CapSet::EMPTY;
        for c in iter {
            s.insert(c);
        }
        s
    }
}

/// The grants a role kind can never lose.
pub fn core_set(kind: RoleKind) -> CapSet {
    CAPS.iter()
        .filter(|m| m.core.contains(kind))
        .map(|m| m.cap)
        .collect()
}

/// What an owner holds regardless of role grants: every non-legacy capability.
pub fn owner_set() -> CapSet {
    CAPS.iter()
        .filter(|m| m.tier != Tier::Legacy)
        .map(|m| m.cap)
        .collect()
}

/// Is `cap` core for any of `kinds`?
pub fn is_core_for(cap: Cap, kinds: Kinds) -> bool {
    kinds.0 & cap.meta().core.0 != 0
}

/// A template's grants for one role kind: the spec defaults, plus the template's
/// additions, minus its removals, plus core.
pub fn template_grants(template: &str, kind: RoleKind) -> Option<CapSet> {
    let t = TEMPLATES.iter().find(|t| t.key == template)?;
    let mut s: CapSet = CAPS
        .iter()
        .filter(|m| m.defaults.contains(kind))
        .map(|m| m.cap)
        .collect();
    for (k, c) in t.add {
        if *k == kind {
            s.insert(*c);
        }
    }
    for (k, c) in t.remove {
        if *k == kind {
            s.remove(*c);
        }
    }
    Some(s.union(&core_set(kind)))
}

/// A template's default limits for one role kind, on grants the kind holds.
pub fn template_limits(template: &str, kind: RoleKind) -> Vec<(Cap, Limits)> {
    let Some(t) = TEMPLATES.iter().find(|t| t.key == template) else {
        return Vec::new();
    };
    t.limits
        .iter()
        .filter(|(k, _, _)| *k == kind)
        .map(|(_, c, l)| (*c, *l))
        .collect()
}

// ── Limits ──────────────────────────────────────────────────────────────────

/// Caps on a capability. `None` is unlimited.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Limits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_amount: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_percent: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_value: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age_minutes: Option<i64>,
    /// Only the person's own work. `false` (the default, and what every stored
    /// row without the field means) is unrestricted, so an old snapshot or an
    /// old grant row keeps meaning exactly what it meant before.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub own: bool,
}

impl Limits {
    pub const UNLIMITED: Limits = Limits {
        max_amount: None,
        max_percent: None,
        max_value: None,
        max_age_minutes: None,
        own: false,
    };

    pub fn is_unlimited(&self) -> bool {
        *self == Limits::UNLIMITED
    }

    pub fn get(&self, k: LimitKey) -> Option<i64> {
        match k {
            LimitKey::MaxAmount => self.max_amount,
            LimitKey::MaxPercent => self.max_percent,
            LimitKey::MaxValue => self.max_value,
            LimitKey::MaxAgeMinutes => self.max_age_minutes,
            // Not a number. Read it with `.own`; `decide` checks it separately.
            LimitKey::Own => None,
        }
    }

    /// Roles combine by the most generous value; unlimited wins.
    pub fn most_generous(a: &Limits, b: &Limits) -> Limits {
        fn m(x: Option<i64>, y: Option<i64>) -> Option<i64> {
            match (x, y) {
                (Some(x), Some(y)) => Some(x.max(y)),
                _ => None,
            }
        }
        Limits {
            max_amount: m(a.max_amount, b.max_amount),
            max_percent: m(a.max_percent, b.max_percent),
            max_value: m(a.max_value, b.max_value),
            max_age_minutes: m(a.max_age_minutes, b.max_age_minutes),
            // One role that may act on anyone's work is the generous one.
            own: a.own && b.own,
        }
    }

    /// Is every value here within `other` (no more generous than it)?
    pub fn within(&self, other: &Limits) -> bool {
        fn w(x: Option<i64>, y: Option<i64>) -> bool {
            match (x, y) {
                (_, None) => true,
                (None, Some(_)) => false,
                (Some(x), Some(y)) => x <= y,
            }
        }
        w(self.max_amount, other.max_amount)
            && w(self.max_percent, other.max_percent)
            && w(self.max_value, other.max_value)
            && w(self.max_age_minutes, other.max_age_minutes)
            // Restricted to your own is within unrestricted, never the reverse.
            && (self.own || !other.own)
    }

    /// Only the keys the capability accepts.
    pub fn restricted_to(&self, keys: &[LimitKey]) -> Limits {
        let keep = |k: LimitKey, v: Option<i64>| if keys.contains(&k) { v } else { None };
        Limits {
            max_amount: keep(LimitKey::MaxAmount, self.max_amount),
            max_percent: keep(LimitKey::MaxPercent, self.max_percent),
            max_value: keep(LimitKey::MaxValue, self.max_value),
            max_age_minutes: keep(LimitKey::MaxAgeMinutes, self.max_age_minutes),
            own: keys.contains(&LimitKey::Own) && self.own,
        }
    }
}

// ── Grants in, effective set out ────────────────────────────────────────────

/// One role as its grants are stored for an org.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoleDef {
    pub id: String,
    pub kind: RoleKind,
    pub grants: CapSet,
    /// Only capped capabilities appear; a missing entry is unlimited.
    #[serde(default)]
    pub limits: BTreeMap<u16, Limits>,
}

/// A person holds a role at a set of branches (or everywhere), for a while.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssignmentDef {
    pub role: RoleDef,
    pub all_branches: bool,
    #[serde(default)]
    pub branches: Vec<String>,
    #[serde(default)]
    pub valid_from: Option<i64>,
    #[serde(default)]
    pub valid_to: Option<i64>,
}

impl AssignmentDef {
    fn live_at(&self, now: i64) -> bool {
        self.valid_from.is_none_or(|f| f <= now) && self.valid_to.is_none_or(|t| now < t)
    }
}

/// A per-person exception: allow or deny one capability, everywhere or at one
/// branch, optionally with its own limits and an expiry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OverrideDef {
    pub cap: u16,
    pub allow: bool,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub limits: Option<Limits>,
    #[serde(default)]
    pub valid_to: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Principal {
    pub user_id: String,
    pub active: bool,
    pub is_owner: bool,
    #[serde(default)]
    pub assignments: Vec<AssignmentDef>,
    #[serde(default)]
    pub overrides: Vec<OverrideDef>,
}

/// Where the question is asked.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Scope<'a> {
    /// Anywhere the person works (the union over their branches).
    Anywhere,
    Branch(&'a str),
}

/// Org-wide policy the owner sets per capability.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OrgPolicy {
    /// Capabilities where a person without the grant sees "ask a manager"
    /// instead of nothing. Only capabilities whose spec allows approval count.
    #[serde(default)]
    pub ask_manager: CapSet,
}

/// What a person may do at a scope, fully resolved.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EffectiveSet {
    pub caps: CapSet,
    /// Capped capabilities only.
    #[serde(default)]
    pub limits: BTreeMap<u16, Limits>,
    pub kinds: Kinds,
    pub owner: bool,
    #[serde(default)]
    pub ask_manager: CapSet,
}

impl EffectiveSet {
    pub fn can(&self, c: Cap) -> bool {
        self.caps.contains(c)
    }
    pub fn limits_of(&self, c: Cap) -> Limits {
        self.limits.get(&c.id()).copied().unwrap_or_default()
    }
}

/// Resolve a person's effective capabilities.
///
/// - Inactive: nothing.
/// - Owner: every capability; only non-protected capabilities can be denied.
/// - Otherwise: the union of the grants of every live assignment covering the
///   scope, plus the core grants of each role's kind; limits combine by the most
///   generous. Then per-person overrides: an override for one branch beats an
///   everywhere override for the same capability; deny beats allow at equal
///   specificity; an allow's limits replace the role's; a deny never removes a
///   core grant of a held kind.
pub fn resolve(p: &Principal, scope: Scope<'_>, now: i64, policy: &OrgPolicy) -> EffectiveSet {
    let mut eff = EffectiveSet {
        ask_manager: policy
            .ask_manager
            .iter()
            .filter(|c| c.meta().approval)
            .collect(),
        ..Default::default()
    };
    if !p.active {
        eff.ask_manager = CapSet::EMPTY;
        return eff;
    }

    // Unlimited-by-some-role, per capability, while combining.
    let mut unlimited = CapSet::EMPTY;
    if p.is_owner {
        // Every capability that means something today; legacy-tier cells (dead
        // `resource:action` pairs kept for old tablets) come only from the
        // owner's role, so the grid an old tablet reads is unchanged.
        eff.owner = true;
        eff.kinds.insert(RoleKind::OrgAdmin);
        eff.caps = owner_set();
        unlimited = CapSet::all();
    }
    for a in &p.assignments {
        if !a.live_at(now) {
            continue;
        }
        let covers = match scope {
            Scope::Anywhere => true,
            Scope::Branch(b) => a.all_branches || a.branches.iter().any(|x| x == b),
        };
        if !covers {
            continue;
        }
        eff.kinds.insert(a.role.kind);
        let granted = a.role.grants.union(&core_set(a.role.kind));
        for c in granted.iter() {
            match a.role.limits.get(&c.id()).filter(|l| !l.is_unlimited()) {
                Some(l) if !unlimited.contains(c) => {
                    let merged = eff
                        .limits
                        .get(&c.id())
                        .map_or(*l, |prev| Limits::most_generous(prev, l));
                    eff.limits
                        .insert(c.id(), merged.restricted_to(c.meta().limits));
                }
                Some(_) => {}
                None => {
                    unlimited.insert(c);
                    eff.limits.remove(&c.id());
                }
            }
            eff.caps.insert(c);
        }
    }

    // Overrides: pick, per capability, the most specific live one; deny wins ties.
    let mut chosen: BTreeMap<u16, (&OverrideDef, bool)> = BTreeMap::new(); // (override, is_branch_specific)
    for o in &p.overrides {
        if o.valid_to.is_some_and(|t| now >= t) || Cap::from_id(o.cap).is_none() {
            continue;
        }
        let specific = match (&o.branch, scope) {
            (None, _) => false,
            (Some(b), Scope::Branch(s)) if b == s => true,
            // Anywhere: a branch allow still counts (the person can do it
            // somewhere); a branch deny does not remove it everywhere.
            (Some(_), Scope::Anywhere) if o.allow => false,
            _ => continue,
        };
        match chosen.get(&o.cap) {
            Some((prev, prev_specific)) => {
                let replace = (specific && !prev_specific)
                    || (specific == *prev_specific && !o.allow && prev.allow);
                if replace {
                    chosen.insert(o.cap, (o, specific));
                }
            }
            None => {
                chosen.insert(o.cap, (o, specific));
            }
        }
    }
    for (id, (o, _)) in chosen {
        let c = Cap::from_id(id).expect("checked above");
        if o.allow {
            eff.caps.insert(c);
            match o.limits {
                Some(l) if !l.is_unlimited() => {
                    eff.limits.insert(id, l.restricted_to(c.meta().limits));
                }
                Some(_) => {
                    eff.limits.remove(&id);
                }
                None => {}
            }
        } else {
            let protected_for_owner = eff.owner && c.meta().protected;
            if protected_for_owner || is_core_for(c, eff.kinds) {
                continue;
            }
            eff.caps.remove(c);
            eff.limits.remove(&id);
        }
    }
    eff
}

// ── Decisions ───────────────────────────────────────────────────────────────

/// What is being attempted, with the figures a limit is checked against.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub cap: u16,
    #[serde(default)]
    pub amount: Option<i64>,
    #[serde(default)]
    pub percent: Option<i64>,
    #[serde(default)]
    pub value: Option<i64>,
    /// How old the thing acted on is, in minutes.
    #[serde(default)]
    pub age_minutes: Option<i64>,
    /// Did this person do the thing they are acting on? `None` means the caller
    /// did not say, and an `own` limit then can't be satisfied — a caller that
    /// never learned to answer must not silently pass the check.
    #[serde(default)]
    pub own: Option<bool>,
}

impl Request {
    pub fn of(cap: Cap) -> Request {
        Request {
            cap: cap.id(),
            ..Default::default()
        }
    }
    pub fn amount(mut self, minor: i64) -> Request {
        self.amount = Some(minor);
        self
    }
    pub fn percent(mut self, bps: i64) -> Request {
        self.percent = Some(bps);
        self
    }
    pub fn value(mut self, minor: i64) -> Request {
        self.value = Some(minor);
        self
    }
    pub fn age_minutes(mut self, minutes: i64) -> Request {
        self.age_minutes = Some(minutes);
        self
    }
    /// Whether the actor is the author of the thing being acted on.
    pub fn own(mut self, own: bool) -> Request {
        self.own = Some(own);
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "why", rename_all = "snake_case")]
pub enum Why {
    NotHeld,
    OverLimit {
        key: LimitKey,
        limit: i64,
        asked: i64,
    },
    UnknownCapability,
    SamePerson,
    /// Held, but only over the person's own work, and this isn't theirs (or the
    /// caller did not say whose it is).
    NotYours,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Decision {
    Allow,
    /// A person holding the act (within the figures) may approve it on this
    /// device with their PIN.
    NeedsApproval(Why),
    Deny(Why),
}

impl Decision {
    pub fn is_allow(&self) -> bool {
        matches!(self, Decision::Allow)
    }
}

/// Decide a request against an effective set.
///
/// 1. Not held: "ask a manager" when the owner turned that on for this
///    capability (and the spec allows approval), else deny.
/// 2. Held but over a limit: needs approval when the spec allows approval for
///    the capability, else deny. A limit exists to route the rest to a manager.
/// 3. Otherwise allow.
pub fn decide(eff: &EffectiveSet, req: &Request) -> Decision {
    let Some(cap) = Cap::from_id(req.cap) else {
        return Decision::Deny(Why::UnknownCapability);
    };
    let meta = cap.meta();
    if !eff.caps.contains(cap) {
        return if meta.approval && eff.ask_manager.contains(cap) {
            Decision::NeedsApproval(Why::NotHeld)
        } else {
            Decision::Deny(Why::NotHeld)
        };
    }
    let limits = eff.limits_of(cap);
    // Scope before ceilings: "not your sale" is the truer answer than "over the
    // amount" when both are true, and it is the one a manager is asked about.
    if limits.own && req.own != Some(true) {
        return if meta.approval {
            Decision::NeedsApproval(Why::NotYours)
        } else {
            Decision::Deny(Why::NotYours)
        };
    }
    for (key, asked) in [
        (LimitKey::MaxAmount, req.amount),
        (LimitKey::MaxPercent, req.percent),
        (LimitKey::MaxValue, req.value),
        (LimitKey::MaxAgeMinutes, req.age_minutes),
    ] {
        if let (Some(limit), Some(asked)) = (limits.get(key), asked) {
            if asked > limit {
                let why = Why::OverLimit { key, limit, asked };
                return if meta.approval {
                    Decision::NeedsApproval(why)
                } else {
                    Decision::Deny(why)
                };
            }
        }
    }
    Decision::Allow
}

/// May `approver` approve `req` for `subject`? The approver must be someone else
/// and must be allowed the request outright.
pub fn can_approve(
    approver: &EffectiveSet,
    approver_id: &str,
    subject_id: &str,
    req: &Request,
) -> Result<(), Why> {
    if approver_id == subject_id {
        return Err(Why::SamePerson);
    }
    match decide(approver, req) {
        Decision::Allow => Ok(()),
        Decision::NeedsApproval(w) | Decision::Deny(w) => Err(w),
    }
}

// ── Legacy projection ───────────────────────────────────────────────────────

pub mod legacy {
    use super::*;

    /// Is the old `resource:action` cell granted, for a pre-0.8 tablet?
    pub fn granted(eff: &EffectiveSet, resource: &str, action: &str) -> bool {
        Cap::from_legacy(resource, action).is_some_and(|c| eff.caps.contains(c))
    }

    /// The grants a legacy role (global `role_permissions` rows) maps to.
    pub fn caps_from_cells<'a>(cells: impl IntoIterator<Item = (&'a str, &'a str)>) -> CapSet {
        cells
            .into_iter()
            .filter_map(|(r, a)| Cap::from_legacy(r, a))
            .collect()
    }
}

#[cfg(test)]
mod tests;
