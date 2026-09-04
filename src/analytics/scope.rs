//! Who may see what, and when "now" is.
//!
//! Two questions every analytics request has to answer before it can run, and
//! neither belongs in an HTTP handler:
//!
//!   1. **Which branches may this caller see?** Derived from verified JWT claims
//!      and the user's branch assignments — never from the request body. The
//!      answer is injected as `:branch_ids` by [`super::execute`], so it fences
//!      every query regardless of what was asked for.
//!   2. **What time is it for this merchant?** Timezone is org/branch
//!      configuration, never the device or the server (see the timezone
//!      integration note in `CLAUDE.md`), and it decides what "yesterday" means.
//!
//! Every narrowing path here can only ever produce a *subset* of the accessible
//! set. A forged header, a hallucinated branch name, or a branch belonging to
//! another merchant all resolve within it or are ignored.

use chrono_tz::Tz;
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{auth::jwt::Claims, db::Db, errors::AppError, models::UserRole};

/// A branch the caller may query.
#[derive(Debug, Clone, PartialEq)]
pub struct BranchRef {
    pub id: Uuid,
    pub name: String,
}

/// Which branches an answer actually covers. Returned on every response so the
/// scope of a number is never ambiguous — "all branches" versus one of them is
/// the difference between a figure being right and being off by a factor of
/// however many branches the merchant has.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ScopeInfo {
    /// True when the answer spans every branch the caller can access.
    pub all_branches: bool,
    pub branches: Vec<String>,
    /// Human-readable label, e.g. "All branches (3)" or "Sidi Henish".
    pub label: String,
    /// Set when a branch was named but could not be matched. The answer then
    /// falls back to the full accessible set, and this flags the mismatch rather
    /// than silently answering a different question.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unmatched_branch: Option<String>,
}

/// The merchant's clock and locale settings.
#[derive(Debug, Clone)]
pub struct OrgClock {
    /// IANA timezone name, as stored (e.g. "Africa/Cairo").
    pub timezone: String,
    pub tz: Tz,
}

/// Load the organization's timezone. One round trip on the tenant pool; the org
/// row is visible through RLS without an explicit filter.
pub async fn org_clock(db: &Db) -> Result<OrgClock, AppError> {
    let row: Option<(String,)> = sqlx::query_as("SELECT timezone::text FROM organizations LIMIT 1")
        .fetch_optional(db.get_ref())
        .await?;
    let timezone = match row {
        Some((tz,)) => tz,
        None => {
            // The pool is RLS-scoped to a real org, so exactly one row must be
            // visible here. Zero means OUR data is wrong — a deleted org with
            // live tokens, or a tenant pool bound to an id that no longer
            // exists. This path answers 200 with a plausible default, so it is
            // invisible to every status-based reporting rule; it has to be
            // reported explicitly or it is never seen at all.
            crate::observability::report::report_data_fault(
                "analytics",
                "org_clock",
                &"no organization row visible on a tenant-scoped pool",
            );
            "Africa/Cairo".to_string()
        }
    };
    let tz = super::spec::parse_tz(&timezone);
    Ok(OrgClock { timezone, tz })
}

/// The set of branches this caller may see analytics for — not every branch in
/// the org, and not just one:
///
///   * `org_admin` → every branch in the org;
///   * `branch_manager` / `waiter` / `kitchen` → their assignments;
///   * `teller` → the branch their token is bound to, falling back to
///     assignments.
///
/// Runs on the RLS-scoped tenant pool, so it is already fenced to the caller's
/// organization before this function adds the branch dimension.
pub async fn accessible_branches(db: &Db, claims: &Claims) -> Result<Vec<BranchRef>, AppError> {
    let rows: Vec<(Uuid, String)> = match claims.role {
        UserRole::OrgAdmin | UserRole::SuperAdmin => {
            sqlx::query_as("SELECT id, name FROM branches WHERE deleted_at IS NULL ORDER BY name")
                .fetch_all(db.get_ref())
                .await?
        }
        UserRole::Teller => match claims.branch_id() {
            Some(b) => {
                sqlx::query_as("SELECT id, name FROM branches WHERE id = $1 AND deleted_at IS NULL")
                    .bind(b)
                    .fetch_all(db.get_ref())
                    .await?
            }
            None => assigned(db, claims.user_id()).await?,
        },
        UserRole::BranchManager | UserRole::Waiter | UserRole::Kitchen => {
            assigned(db, claims.user_id()).await?
        }
    };
    Ok(rows
        .into_iter()
        .map(|(id, name)| BranchRef { id, name })
        .collect())
}

async fn assigned(db: &Db, user_id: Uuid) -> Result<Vec<(Uuid, String)>, AppError> {
    Ok(sqlx::query_as(
        "SELECT b.id, b.name FROM user_branch_assignments uba \
         JOIN branches b ON b.id = uba.branch_id AND b.deleted_at IS NULL \
         WHERE uba.user_id = $1 ORDER BY b.name",
    )
    .bind(user_id)
    .fetch_all(db.get_ref())
    .await?)
}

/// Resolve the branch set a request should cover, in priority order:
///
///   1. a branch named in the question or spec (fuzzy-matched within the
///      accessible set);
///   2. otherwise an explicitly selected branch, when the caller can access it —
///      this is the dashboard's global branch selector, read from `X-Branch-Id`;
///   3. otherwise every accessible branch.
///
/// Note what is *not* here: no path consults the request for a branch id it then
/// trusts. `selected` is intersected with `accessible`, so a forged header
/// narrows to nothing it did not already have, and `requested` is matched only
/// against names the caller can already see.
pub fn resolve(
    accessible: &[BranchRef],
    requested: Option<&str>,
    selected: Option<Uuid>,
) -> (Vec<Uuid>, ScopeInfo) {
    if let Some(name) = requested.map(str::trim).filter(|s| !s.is_empty()) {
        let matches = fuzzy_match(accessible, name);
        if !matches.is_empty() {
            return narrowed(&matches, None);
        }
        // Named but unmatched: fall back, and say so.
        return default_scope(accessible, selected, Some(name.to_string()));
    }
    default_scope(accessible, selected, None)
}

fn default_scope(
    accessible: &[BranchRef],
    selected: Option<Uuid>,
    unmatched: Option<String>,
) -> (Vec<Uuid>, ScopeInfo) {
    if let Some(sel) = selected
        && let Some(hit) = accessible.iter().find(|b| b.id == sel)
    {
        return narrowed(std::slice::from_ref(hit), unmatched);
    }
    let names: Vec<String> = accessible.iter().map(|b| b.name.clone()).collect();
    let label = match names.len() {
        0 => "No branches".to_string(),
        1 => names[0].clone(),
        n => format!("All branches ({n})"),
    };
    (
        accessible.iter().map(|b| b.id).collect(),
        ScopeInfo {
            all_branches: true,
            branches: names,
            label,
            unmatched_branch: unmatched,
        },
    )
}

fn narrowed(subset: &[BranchRef], unmatched: Option<String>) -> (Vec<Uuid>, ScopeInfo) {
    let names: Vec<String> = subset.iter().map(|b| b.name.clone()).collect();
    (
        subset.iter().map(|b| b.id).collect(),
        ScopeInfo {
            all_branches: false,
            label: names.join(", "),
            branches: names,
            unmatched_branch: unmatched,
        },
    )
}

/// Branch-name match *within the accessible set*, tolerant of how people
/// actually type a name into a chat box.
///
/// The tiers, tried in order and stopping at the first that hits:
///
///   1. exact, after normalization;
///   2. substring either way — handles "maadi" for "Maadi Branch";
///   3. every word of the query appears in the name — handles word order and
///      dropped words ("heneish sidi", "sidi");
///   4. a small edit distance — handles a genuine misspelling.
///
/// Tier 4 exists because of a real failure: a merchant asked about
/// "sidi henish" when the branch is "SIDI HENEISH". One missing letter, no
/// substring relationship, so the branch went unmatched and the question came
/// back unanswered. A one-character typo is the single most likely thing a
/// human does to a proper noun, and it should not be a dead end.
///
/// The distance budget scales with length and stays tight (1 edit for a short
/// name, at most 3 for a long one), so it fixes typos without inventing
/// matches: "Alexandria" still does not match "Maadi", which is what keeps
/// `unmatched_branch` meaningful.
fn fuzzy_match(accessible: &[BranchRef], query: &str) -> Vec<BranchRef> {
    let q = normalize_name(query);
    if q.is_empty() {
        return Vec::new();
    }

    let pick = |f: &dyn Fn(&str) -> bool| -> Vec<BranchRef> {
        accessible
            .iter()
            .filter(|b| f(&normalize_name(&b.name)))
            .cloned()
            .collect()
    };

    let exact = pick(&|n| n == q);
    if !exact.is_empty() {
        return exact;
    }
    let substring = pick(&|n| n.contains(&q) || q.contains(n));
    if !substring.is_empty() {
        return substring;
    }
    let words: Vec<&str> = q.split(' ').filter(|w| !w.is_empty()).collect();
    let by_word = pick(&|n| {
        if discriminators(&q) != discriminators(n) {
            return false;
        }
        let name_words: Vec<&str> = n.split(' ').collect();
        words.iter().all(|w| {
            name_words.iter().any(|nw| {
                // A short token must match exactly. Loose containment here is
                // how "Branch b" matched "Branch a" — "branch" contains "b".
                nw == w || (w.chars().count() >= 3 && nw.starts_with(w))
            })
        })
    });
    if !by_word.is_empty() {
        return by_word;
    }

    // Typo tier: keep only the closest candidates within budget, so a near-miss
    // never drags along a distant one.
    let mut best: Option<usize> = None;
    let mut hits: Vec<BranchRef> = Vec::new();
    for br in accessible {
        let n = normalize_name(&br.name);
        // Names that differ only in their discriminator are DIFFERENT branches,
        // however close the strings are. "Branch a"/"Branch b" and
        // "Maadi 1"/"Maadi 2" are one edit apart, and quietly answering about
        // the wrong one is the exact failure this tier must not introduce.
        if discriminators(&q) != discriminators(&n) {
            continue;
        }
        let budget = (n.chars().count().max(q.chars().count()) / 5).clamp(1, 3);
        let d = edit_distance(&q, &n);
        if d > budget {
            continue;
        }
        match best {
            Some(b) if d > b => continue,
            Some(b) if d < b => {
                hits.clear();
                best = Some(d);
            }
            None => best = Some(d),
            _ => {}
        }
        hits.push(br.clone());
    }
    hits
}

/// The tokens that distinguish one branch from a sibling: numbers, and very
/// short words. These are never typos of each other, so the typo tier must
/// treat them as exact.
fn discriminators(name: &str) -> std::collections::BTreeSet<&str> {
    name.split(' ')
        .filter(|t| !t.is_empty())
        .filter(|t| t.chars().count() <= 2 || t.chars().all(|c| c.is_numeric()))
        .collect()
}

/// Fold a name to a comparable form: lowercase, single-spaced, punctuation
/// dropped, and Arabic orthography unified.
///
/// The Arabic part matters as much as the case folding. A merchant typing
/// "المعادى" for a branch stored as "المعادي" differs only in the final letter
/// (ى vs ي), and the same is true of the alef family (أ إ آ → ا) and ة/ه.
/// Those are spelling conventions, not different names, and treating them as
/// different names fails the question for a bilingual merchant.
fn normalize_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let mapped = match ch {
            'أ' | 'إ' | 'آ' | 'ٱ' => Some('ا'),
            'ى' => Some('ي'),
            'ة' => Some('ه'),
            'ؤ' => Some('و'),
            'ئ' => Some('ي'),
            // Tashkeel (harakat) and tatweel carry no lexical weight here.
            '\u{0640}' | '\u{064B}'..='\u{0652}' | '\u{0670}' => None,
            c if c.is_alphanumeric() => Some(c.to_lowercase().next().unwrap_or(c)),
            _ => Some(' '),
        };
        if let Some(c) = mapped {
            out.push(c);
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Levenshtein distance over chars, two-row. Inputs here are branch names, so
/// the quadratic cost is irrelevant.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// The branch the dashboard's global selector is on, from the `X-Branch-Id`
/// header the frontend already sends. `None` means "all branches" (absent, or
/// the all-zeros sentinel). Used only as a default, and always intersected with
/// the accessible set.
pub fn header_branch_id(req: &actix_web::HttpRequest) -> Option<Uuid> {
    req.headers()
        .get("X-Branch-Id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| Uuid::parse_str(s).ok())
        .filter(|id| !id.is_nil())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(name: &str) -> BranchRef {
        BranchRef {
            id: Uuid::new_v4(),
            name: name.into(),
        }
    }

    #[test]
    fn no_narrowing_covers_every_accessible_branch() {
        let acc = vec![b("Sidi Henish"), b("Marina"), b("Downtown")];
        let (ids, scope) = resolve(&acc, None, None);
        assert_eq!(ids.len(), 3);
        assert!(scope.all_branches);
        assert_eq!(scope.label, "All branches (3)");
    }

    #[test]
    fn a_named_branch_narrows_case_and_space_insensitively() {
        let acc = vec![b("Sidi Henish"), b("Marina")];
        let (ids, scope) = resolve(&acc, Some("  sidi   henish "), None);
        assert_eq!(ids, vec![acc[0].id]);
        assert!(!scope.all_branches);
        assert_eq!(scope.label, "Sidi Henish");
    }

    #[test]
    fn a_partial_name_matches() {
        let acc = vec![b("Sidi Henish"), b("Marina")];
        let (ids, _) = resolve(&acc, Some("marina"), None);
        assert_eq!(ids, vec![acc[1].id]);
    }

    /// The exact question that came back unanswered: the merchant typed
    /// "sidi henish"; the branch is "SIDI HENEISH". One letter.
    #[test]
    fn a_one_letter_misspelling_still_finds_the_branch() {
        let acc = vec![b("SIDI HENEISH"), b("Maadi"), b("Centrada")];
        let (ids, scope) = resolve(&acc, Some("sidi henish"), None);
        assert_eq!(ids, vec![acc[0].id]);
        assert_eq!(scope.label, "SIDI HENEISH");
        assert!(scope.unmatched_branch.is_none());
    }

    #[test]
    fn dropped_and_reordered_words_still_match() {
        let acc = vec![b("SIDI HENEISH"), b("Maadi Branch")];
        for typed in ["heneish sidi", "heneish", "sidi"] {
            let (ids, _) = resolve(&acc, Some(typed), None);
            assert_eq!(ids, vec![acc[0].id], "{typed} should match SIDI HENEISH");
        }
    }

    #[test]
    fn arabic_spelling_variants_are_the_same_name() {
        // ى/ي and أ/ا are conventions, not different branches.
        let acc = vec![b("المعادي"), b("وسط البلد")];
        let (ids, _) = resolve(&acc, Some("المعادى"), None);
        assert_eq!(ids, vec![acc[0].id]);
    }

    /// The tolerance has to stay tight, or `unmatched_branch` stops meaning
    /// anything and a wrong branch gets answered confidently.
    #[test]
    fn a_genuinely_different_name_is_still_unmatched() {
        let acc = vec![b("SIDI HENEISH"), b("Maadi"), b("Arkan")];
        for typed in ["Alexandria", "Zamalek", "Hurghada"] {
            let (_, scope) = resolve(&acc, Some(typed), None);
            assert_eq!(
                scope.unmatched_branch.as_deref(),
                Some(typed),
                "{typed} must not be fuzzed into an existing branch"
            );
        }
    }

    /// Regression guard for a bug the tenant-isolation test caught: "Branch b"
    /// is one edit from "Branch a", so typo tolerance matched the wrong branch
    /// and reported nothing amiss. Numbered branches are the common real case.
    #[test]
    fn branches_differing_only_in_their_discriminator_are_never_confused() {
        for (names, typed) in [
            (vec!["Branch a", "Branch c"], "Branch b"),
            (vec!["Maadi 1", "Maadi 3"], "Maadi 2"),
            (vec!["Zone 5"], "Zone 6"),
        ] {
            let acc: Vec<BranchRef> = names.iter().map(|n| b(n)).collect();
            let (_, scope) = resolve(&acc, Some(typed), None);
            assert_eq!(
                scope.unmatched_branch.as_deref(),
                Some(typed),
                "{typed} must not be fuzzed into {names:?}"
            );
        }
    }

    #[test]
    fn a_short_name_does_not_collapse_into_its_neighbour() {
        // "Maadi" and "Arkan" are close in length; neither may absorb the other.
        let acc = vec![b("Maadi"), b("Arkan")];
        let (ids, _) = resolve(&acc, Some("Maadi"), None);
        assert_eq!(ids, vec![acc[0].id]);
    }

    #[test]
    fn an_unmatched_name_falls_back_and_is_flagged() {
        // Silently answering for all branches when the user asked about one
        // that does not exist would be a wrong answer presented as right.
        let acc = vec![b("Sidi Henish")];
        let (ids, scope) = resolve(&acc, Some("Alexandria"), None);
        assert_eq!(ids.len(), 1);
        assert!(scope.all_branches);
        assert_eq!(scope.unmatched_branch.as_deref(), Some("Alexandria"));
    }

    #[test]
    fn a_selected_branch_the_caller_cannot_access_is_ignored() {
        let acc = vec![b("Sidi Henish")];
        let foreign = Uuid::new_v4();
        let (ids, scope) = resolve(&acc, None, Some(foreign));
        assert_eq!(ids, vec![acc[0].id]);
        assert!(!ids.contains(&foreign));
        assert!(scope.all_branches);
    }

    #[test]
    fn a_named_branch_beats_the_selector() {
        let acc = vec![b("Sidi Henish"), b("Marina")];
        let (ids, _) = resolve(&acc, Some("Marina"), Some(acc[0].id));
        assert_eq!(ids, vec![acc[1].id]);
    }

    #[test]
    fn narrowing_can_never_widen_beyond_the_accessible_set() {
        // The core tenancy property, stated as a test: whatever the inputs, the
        // resolved ids are always a subset of what the caller may access.
        let acc = vec![b("A"), b("B")];
        let allowed: Vec<Uuid> = acc.iter().map(|x| x.id).collect();
        for requested in [None, Some("A"), Some("Z"), Some(""), Some("'; DROP--")] {
            for selected in [None, Some(Uuid::new_v4()), Some(acc[1].id)] {
                let (ids, _) = resolve(&acc, requested, selected);
                assert!(
                    ids.iter().all(|id| allowed.contains(id)),
                    "scope escaped for {requested:?}/{selected:?}"
                );
            }
        }
    }

    #[test]
    fn an_empty_accessible_set_produces_an_empty_scope_not_a_panic() {
        let (ids, scope) = resolve(&[], Some("Anything"), Some(Uuid::new_v4()));
        assert!(ids.is_empty());
        assert_eq!(scope.label, "No branches");
    }
}
