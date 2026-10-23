//! Read fast paths (OPT-A): answer "nothing changed" and the permission
//! inputs from memory, never stale.
//!
//! The POS asks the same questions over and over: an idle `/sync/pull` at the
//! head of its branch's feed, `/catalog/sync?since=<current revision>`, and
//! on every request, "may this person do this" (the account check plus the
//! org's authz epoch). Each answer is a function of a few database facts that
//! change rarely. This module keeps those facts in memory and drops them the
//! moment they may have changed:
//!
//! * **Notifications** ([`listener`]): `sync_changes` (payload = branch, sent
//!   by `sync_emit` at commit), `sync_feed_purged` (the sweeper raised a
//!   branch's purge watermark), `authz_epoch` / `authz_user` and
//!   `catalog_revision` (migration `20261021310000_fastpath_notify`). Postgres
//!   delivers them at commit, in commit order, to every instance listening, so
//!   another instance's write reaches this one's cache too.
//! * **This instance's own writes** ([`fence`]): a mutating request that
//!   finished here bumps its org's fence before its response leaves, so the
//!   writer's next read never races the notification of its own write.
//! * **A lost listener** is "anything may have changed": the session changes,
//!   every entry dies, and nothing is cached until the listener is back.
//!   With no listener at all (tests that never spawn it, `MADAR_FASTPATH=0`)
//!   every read goes to the database exactly as before.
//!
//! Every entry also carries a hard age limit (`MADAR_FASTPATH_MAX_AGE_SECS`,
//! default 300 s) as a backstop, and an idle-pull entry a deadline from the
//! clock-based live gates (`sync_idle_valid_until`).
//!
//! ## The fill rule (why a cached value is never older than its event)
//! A reader takes a [`Capture`] BEFORE it reads the database and stores what it
//! read only if nothing relevant moved in between ([`State::unchanged`]); the
//! entry is then sealed with the counters as they are. An event that lands
//! later moves a counter and the seal no longer matches. So an entry is only
//! ever served while no event has arrived since the data it holds was read.
use std::collections::{BTreeMap, HashMap};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::sync::pull::TypeChecksum;

pub mod listener;

/// What the counters were when a read began (see the module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capture {
    session: u64,
    noorg: u64,
    authz: u64,
    fence_total: u64,
    branch: u64,
    catalog: u64,
}

/// The counters an entry was stored under; compared on every use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Seal {
    session: u64,
    noorg: u64,
    authz: u64,
    org_fence: u64,
    branch: u64,
    catalog: u64,
}

#[derive(Clone, Debug)]
struct Entry<T> {
    value: T,
    seal: Seal,
    filled: Instant,
    /// Hard deadline beyond the age limit (the idle pull's clock gates).
    until: Option<Instant>,
}

/// A user's account check (`permissions::checker`), cached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Account {
    pub ok: bool,
    pub valid_after: Option<i64>,
}

/// A cached idle `/sync/pull` answer: the branch has nothing after `since`.
#[derive(Clone, Debug)]
pub struct Idle {
    pub since: i64,
    pub checksums: BTreeMap<String, TypeChecksum>,
}

/// Fast-path counters (exported for tests and observability).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub idle_hits: u64,
    pub catalog_hits: u64,
    pub epoch_hits: u64,
    pub account_hits: u64,
    pub branch_hits: u64,
    pub session_hits: u64,
}

#[derive(Default)]
struct State {
    /// 0 = not listening (nothing is served or stored).
    session: u64,
    sessions: u64,
    /// Local writes with no org (a super admin's): every entry depends on it.
    noorg: u64,
    /// Any authz notification: every user-keyed entry and epoch depends on it.
    authz: u64,
    /// Every local write, any org (the fill rule's cross-org check).
    fence_total: u64,
    org_fence: HashMap<Uuid, u64>,
    branch_gen: HashMap<Uuid, u64>,
    catalog_gen: HashMap<Uuid, u64>,

    branch_org: HashMap<Uuid, Entry<Uuid>>,
    idle: HashMap<Uuid, Entry<Idle>>,
    /// user → (org, that org's authz epoch).
    user_epoch: HashMap<Uuid, Entry<(Option<Uuid>, i64)>>,
    accounts: HashMap<Uuid, Entry<(Option<Uuid>, Account)>>,
    /// session key (`auth::sessions::key`) → (org, still live?).
    auth_sessions: HashMap<Uuid, Entry<(Option<Uuid>, bool)>>,
    catalog_rev: HashMap<Uuid, Entry<i64>>,
    stats: Stats,
}

/// Entries per map before it is simply cleared (a bound, never a policy).
const MAX_ENTRIES: usize = 50_000;

impl State {
    fn org_fence(&self, org: Option<Uuid>) -> u64 {
        org.and_then(|o| self.org_fence.get(&o).copied())
            .unwrap_or(0)
    }

    fn capture(&self, branch: Option<Uuid>, catalog: Option<Uuid>) -> Capture {
        Capture {
            session: self.session,
            noorg: self.noorg,
            authz: self.authz,
            fence_total: self.fence_total,
            branch: branch
                .and_then(|b| self.branch_gen.get(&b).copied())
                .unwrap_or(0),
            catalog: catalog
                .and_then(|o| self.catalog_gen.get(&o).copied())
                .unwrap_or(0),
        }
    }

    /// Nothing relevant moved since `cap`, and the listener is up.
    fn unchanged(&self, cap: &Capture, branch: Option<Uuid>, catalog: Option<Uuid>) -> bool {
        cap.session != 0 && self.capture(branch, catalog) == *cap
    }

    fn seal(&self, org: Option<Uuid>, branch: Option<Uuid>, catalog: Option<Uuid>) -> Seal {
        let c = self.capture(branch, catalog);
        Seal {
            session: c.session,
            noorg: c.noorg,
            authz: c.authz,
            org_fence: self.org_fence(org),
            branch: c.branch,
            catalog: c.catalog,
        }
    }

    fn valid<T>(
        &self,
        e: &Entry<T>,
        org: Option<Uuid>,
        branch: Option<Uuid>,
        catalog: Option<Uuid>,
    ) -> bool {
        self.session != 0
            && e.seal == self.seal(org, branch, catalog)
            && e.filled.elapsed() < max_age()
            && e.until.is_none_or(|u| Instant::now() < u)
    }
}

fn state() -> MutexGuard<'static, State> {
    static S: LazyLock<Mutex<State>> = LazyLock::new(Default::default);
    S.lock().unwrap_or_else(|e| e.into_inner())
}

fn insert_bounded<T>(map: &mut HashMap<Uuid, T>, k: Uuid, v: T) {
    if map.len() >= MAX_ENTRIES && !map.contains_key(&k) {
        map.clear();
    }
    map.insert(k, v);
}

/// `MADAR_FASTPATH=0` turns every fast path off (rollback switch).
pub fn enabled() -> bool {
    static V: LazyLock<bool> = LazyLock::new(|| {
        !matches!(
            std::env::var("MADAR_FASTPATH").ok().as_deref(),
            Some("0" | "false" | "off")
        )
    });
    *V
}

fn max_age() -> Duration {
    static V: LazyLock<Duration> = LazyLock::new(|| {
        Duration::from_secs(
            std::env::var("MADAR_FASTPATH_MAX_AGE_SECS")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(300),
        )
    });
    *V
}

/// Is anything served from memory right now?
pub fn live() -> bool {
    state().session != 0
}

pub fn stats() -> Stats {
    state().stats
}

// ── Events ──────────────────────────────────────────────────────────────

/// The listener is (re)connected and LISTENing: a new session. Everything
/// cached before is dropped (notifications may have been missed).
pub(crate) fn session_started() {
    let mut s = state();
    s.sessions += 1;
    let id = s.sessions;
    *s = State {
        session: id,
        sessions: id,
        stats: s.stats,
        ..Default::default()
    };
}

/// The listener lost its connection: serve nothing until it is back.
pub(crate) fn session_lost() {
    let mut s = state();
    let sessions = s.sessions;
    *s = State {
        sessions,
        stats: s.stats,
        ..Default::default()
    };
}

/// `sync_changes` / `sync_feed_purged` for `branch`.
pub fn on_branch_changed(branch: Uuid) {
    let mut s = state();
    *s.branch_gen.entry(branch).or_default() += 1;
    s.idle.remove(&branch);
}

/// `authz_epoch` or `authz_user`: grants, an account, a session cutoff or a
/// session's revocation changed somewhere. Rare, so every user-keyed entry
/// goes.
pub(crate) fn on_authz() {
    let mut s = state();
    s.authz += 1;
    s.accounts.clear();
    s.user_epoch.clear();
    s.auth_sessions.clear();
}

/// `catalog_revision` for `org`.
pub(crate) fn on_catalog(org: Uuid) {
    let mut s = state();
    *s.catalog_gen.entry(org).or_default() += 1;
    s.catalog_rev.remove(&org);
}

/// A mutating request finished on this instance (see [`fence`]).
pub fn on_local_write(org: Option<Uuid>) {
    let mut s = state();
    s.fence_total += 1;
    match org {
        Some(o) => *s.org_fence.entry(o).or_default() += 1,
        None => s.noorg += 1,
    }
}

// ── Reads ───────────────────────────────────────────────────────────────

/// Take BEFORE reading the database; `None` when nothing may be cached.
pub fn capture(branch: Option<Uuid>, catalog: Option<Uuid>) -> Option<Capture> {
    if !enabled() {
        return None;
    }
    let s = state();
    (s.session != 0).then(|| s.capture(branch, catalog))
}

/// The org a live branch belongs to.
pub fn branch_org(branch: Uuid) -> Option<Uuid> {
    let mut s = state();
    let e = s.branch_org.get(&branch)?;
    let org = e.value;
    if !s.valid(e, Some(org), Some(branch), None) {
        return None;
    }
    s.stats.branch_hits += 1;
    Some(org)
}

pub fn put_branch_org(cap: Capture, branch: Uuid, org: Uuid) {
    let mut s = state();
    if !s.unchanged(&cap, Some(branch), None) {
        return;
    }
    let seal = s.seal(Some(org), Some(branch), None);
    insert_bounded(
        &mut s.branch_org,
        branch,
        Entry {
            value: org,
            seal,
            filled: Instant::now(),
            until: None,
        },
    );
}

/// A user's (org, authz epoch).
pub fn user_epoch(user: Uuid) -> Option<(Option<Uuid>, i64)> {
    let mut s = state();
    let e = s.user_epoch.get(&user)?;
    let v = e.value;
    if !s.valid(e, v.0, None, None) {
        return None;
    }
    s.stats.epoch_hits += 1;
    Some(v)
}

pub fn put_user_epoch(cap: Capture, user: Uuid, org: Option<Uuid>, epoch: i64) {
    let mut s = state();
    if !s.unchanged(&cap, None, None) {
        return;
    }
    let seal = s.seal(org, None, None);
    insert_bounded(
        &mut s.user_epoch,
        user,
        Entry {
            value: (org, epoch),
            seal,
            filled: Instant::now(),
            until: None,
        },
    );
}

/// A user's account check. The org comes from the epoch entry, so an account
/// is only served while the user's org is known too.
pub fn account(user: Uuid) -> Option<Account> {
    let mut s = state();
    let e = s.accounts.get(&user)?;
    let (org, acc) = e.value;
    if !s.valid(e, org, None, None) {
        return None;
    }
    s.stats.account_hits += 1;
    Some(acc)
}

pub fn put_account(cap: Capture, user: Uuid, org: Option<Uuid>, acc: Account) {
    let mut s = state();
    if !s.unchanged(&cap, None, None) {
        return;
    }
    let seal = s.seal(org, None, None);
    insert_bounded(
        &mut s.accounts,
        user,
        Entry {
            value: (org, acc),
            seal,
            filled: Instant::now(),
            until: None,
        },
    );
}

/// Whether a session (`auth::sessions`) is live: not revoked, not expired.
pub fn session_live(key: Uuid) -> Option<bool> {
    let mut s = state();
    let e = s.auth_sessions.get(&key)?;
    let (org, live) = e.value;
    if !s.valid(e, org, None, None) {
        return None;
    }
    s.stats.session_hits += 1;
    Some(live)
}

/// Store a session's answer. A live one is kept no later than `until` (its
/// expiry), so a cached "live" never outlasts the session itself.
pub fn put_session_live(
    cap: Capture,
    key: Uuid,
    org: Option<Uuid>,
    live: bool,
    until: Option<Instant>,
) {
    let mut s = state();
    if !s.unchanged(&cap, None, None) {
        return;
    }
    let seal = s.seal(org, None, None);
    insert_bounded(
        &mut s.auth_sessions,
        key,
        Entry {
            value: (org, live),
            seal,
            filled: Instant::now(),
            until,
        },
    );
}

/// The org's current catalog revision.
pub fn catalog_revision(org: Uuid) -> Option<i64> {
    let mut s = state();
    let e = s.catalog_rev.get(&org)?;
    let v = e.value;
    if !s.valid(e, Some(org), None, Some(org)) {
        return None;
    }
    s.stats.catalog_hits += 1;
    Some(v)
}

pub fn put_catalog_revision(cap: Capture, org: Uuid, rev: i64) {
    let mut s = state();
    if !s.unchanged(&cap, None, Some(org)) {
        return;
    }
    let seal = s.seal(Some(org), None, Some(org));
    insert_bounded(
        &mut s.catalog_rev,
        org,
        Entry {
            value: rev,
            seal,
            filled: Instant::now(),
            until: None,
        },
    );
}

/// The cached idle answer for a till at `since` on `branch` (of `org`).
pub fn idle(org: Uuid, branch: Uuid, since: i64) -> Option<Idle> {
    let mut s = state();
    let e = s.idle.get(&branch)?;
    if e.value.since != since || !s.valid(e, Some(org), Some(branch), None) {
        return None;
    }
    let v = e.value.clone();
    s.stats.idle_hits += 1;
    Some(v)
}

/// Store an idle answer read under `cap`. `until`: the clock deadline from
/// `sync_idle_valid_until` (None = nothing on the clock).
pub fn put_idle(
    cap: Capture,
    org: Uuid,
    branch: Uuid,
    idle: Idle,
    until: Option<chrono::DateTime<chrono::Utc>>,
) {
    let until = match until {
        Some(t) => {
            let left = (t - chrono::Utc::now()).to_std().unwrap_or(Duration::ZERO);
            if left.is_zero() {
                return;
            }
            Some(Instant::now() + left)
        }
        None => None,
    };
    let mut s = state();
    if !s.unchanged(&cap, Some(branch), None) {
        return;
    }
    let seal = s.seal(Some(org), Some(branch), None);
    insert_bounded(
        &mut s.idle,
        branch,
        Entry {
            value: idle,
            seal,
            filled: Instant::now(),
            until,
        },
    );
}

// ── The local write fence ───────────────────────────────────────────────

/// Paths that are POSTed but only read (a mutating method is otherwise
/// assumed to have written).
fn read_only_post(path: &str) -> bool {
    path.ends_with("/sync/pull")
}

/// Middleware: a mutating request that finished on this instance bumps its
/// org's fence BEFORE its response leaves, so the writer's next read never
/// races the notification of its own write. Outermost-but-one (it reads the
/// claims the scope's JwtMiddleware attached).
pub async fn fence(
    req: actix_web::dev::ServiceRequest,
    next: actix_web::middleware::Next<impl actix_web::body::MessageBody>,
) -> Result<actix_web::dev::ServiceResponse<impl actix_web::body::MessageBody>, actix_web::Error> {
    use actix_web::HttpMessage;
    let mutating = !matches!(
        *req.method(),
        actix_web::http::Method::GET
            | actix_web::http::Method::HEAD
            | actix_web::http::Method::OPTIONS
    ) && !read_only_post(req.path());
    let res = next.call(req).await;
    if mutating {
        let org = match &res {
            Ok(r) => {
                let ext = r.request().extensions();
                ext.get::<crate::auth::jwt::Claims>()
                    .and_then(|c| c.org_id())
                    .or_else(|| {
                        ext.get::<crate::staff::principal::StaffPrincipal>()
                            .map(|p| p.org_id)
                    })
            }
            Err(_) => None,
        };
        on_local_write(org);
    }
    res
}

/// Test hook: wait until the listener has seen every notification committed
/// before this call (Postgres delivers them in commit order, so a barrier
/// NOTIFY sent now arrives after all of them).
#[doc(hidden)]
pub async fn barrier(pool: &sqlx::PgPool) {
    let nonce = Uuid::new_v4();
    let rx = listener::await_barrier(nonce);
    sqlx::query("SELECT pg_notify($1, $2)")
        .bind(listener::BARRIER)
        .bind(nonce.to_string())
        .execute(pool)
        .await
        .expect("barrier notify");
    let _ = tokio::time::timeout(Duration::from_secs(10), rx).await;
}
