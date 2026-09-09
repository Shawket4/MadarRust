//! Apple Wallet / Google Wallet passes.
//!
//! **Degrade-safe behind config, exactly like `WHATSAPP_SERVICE_URL`.** Madar
//! does not hold the credentials yet: with the env vars unset, issuing a pass is
//! skipped and logged, signup still succeeds, and the customer simply gets no
//! "add to wallet" button. Nothing else in the program depends on a pass
//! existing — the member's token is minted at signup either way, so the barcode
//! can be printed on a receipt or read from the site in the meantime.
//!
//! One Madar-owned issuer serves every tenant (one Apple Pass Type ID, one
//! Google issuer), with the tenant's identity coming from `loyalty_settings`
//! branding. The credential lookup is per-org from the start, so per-tenant
//! certificates can be dropped in later without a schema change.

pub mod apns;
pub mod apple;
pub mod google;
pub mod refresh;
pub mod web_service;

use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use super::model::MemberRow;
use super::settings::LoyaltySettings;
use crate::errors::AppError;

/// Both wallets cap the locations they will act on at ten; more are ignored, so
/// sending more only costs bytes on every device holding the pass.
const MAX_LOCATIONS: usize = 10;

/// A branch as a pass surfaces it: Apple puts it on the lock screen when the
/// customer is nearby, Google geofences the object the same way.
///
/// Shared by both wallets, which is why it lives here rather than in either.
#[derive(Debug, Clone)]
pub struct PassLocation {
    pub latitude: f64,
    pub longitude: f64,
    pub name: String,
}

/// Every branch of the org that has coordinates.
///
/// These are the columns the staff-geofencing work already added and the branch
/// dialog already edits — the program needed no new location UI, only a reason
/// to read them.
/// The branches to put on ONE member's card, nearest first.
///
/// Apple allows ten locations per pass and Google is similar, so a chain with
/// more branches than that has to choose — and choosing ALPHABETICALLY, which
/// is what taking the first ten by name did, hands a customer in Alexandria ten
/// Cairo branches because they sort earlier. The card then never surfaces at
/// the shop they actually use.
///
/// Nearest to where they joined is the best guess available: we do not know
/// where a customer is, and the counter they signed up at is the one they were
/// standing in. Sorted here rather than in SQL so the distance is the same
/// `haversine_meters` the geofence uses.
pub async fn locations_for_member(
    pool: &PgPool,
    member: &MemberRow,
) -> Result<Vec<PassLocation>, AppError> {
    let mut all = all_located_branches(pool, member.org_id).await?;
    match anchor_for(pool, member).await? {
        Some(from) => all.sort_by(|a, b| {
            let d = |l: &PassLocation| {
                crate::geo::osrm::haversine_meters(
                    from,
                    crate::geo::osrm::LatLng {
                        lat: l.latitude,
                        lng: l.longitude,
                    },
                )
            };
            d(a).partial_cmp(&d(b)).unwrap_or(std::cmp::Ordering::Equal)
        }),
        // Nothing to measure from: a brand-new member who joined through the
        // shop's own code has told us nothing about where they are. The
        // busiest branches are the best prior available — they are where most
        // people are — and this corrects itself the moment they buy something.
        None => {
            let busiest = branch_popularity(pool, member.org_id).await?;
            all.sort_by(|a, b| {
                let rank = |l: &PassLocation| {
                    busiest
                        .iter()
                        .position(|n| n == &l.name)
                        .unwrap_or(usize::MAX)
                };
                rank(a).cmp(&rank(b)).then_with(|| a.name.cmp(&b.name))
            });
        }
    }
    all.truncate(MAX_LOCATIONS);
    Ok(all)
}

/// Where to measure "nearest" from, for one member.
///
/// Where they actually SHOP, before where they signed up. A member who joined
/// through the shop's org-wide code named no branch at all, and one who joined
/// at a counter may have been passing through — but the branch they keep buying
/// at is the one whose card should surface. Most frequent, then most recent,
/// because a person who moves should not be anchored to last year forever.
///
/// Recomputed on every pass refresh, so a card that started on a poor guess
/// quietly corrects itself after the first visit.
async fn anchor_for(
    pool: &PgPool,
    member: &MemberRow,
) -> Result<Option<crate::geo::osrm::LatLng>, AppError> {
    let shopped: Option<(f64, f64)> = sqlx::query_as(
        "SELECT b.latitude, b.longitude \
           FROM loyalty_transactions t \
           JOIN branches b ON b.id = t.branch_id \
          WHERE t.customer_id = $1 \
            AND b.latitude IS NOT NULL AND b.longitude IS NOT NULL \
            AND b.deleted_at IS NULL \
          GROUP BY b.id, b.latitude, b.longitude \
          ORDER BY count(*) DESC, max(t.created_at) DESC \
          LIMIT 1",
    )
    .bind(member.id)
    .fetch_optional(pool)
    .await?;
    if let Some((lat, lng)) = shopped {
        return Ok(Some(crate::geo::osrm::LatLng { lat, lng }));
    }

    // Never bought anything yet: the counter they signed up at, if it has
    // coordinates. A branch nobody has located cannot anchor anything.
    let Some(b) = member.joined_branch_id else {
        return Ok(None);
    };
    let joined: Option<(f64, f64)> = sqlx::query_as(
        "SELECT latitude, longitude FROM branches \
          WHERE id = $1 AND latitude IS NOT NULL AND longitude IS NOT NULL",
    )
    .bind(b)
    .fetch_optional(pool)
    .await?;
    Ok(joined.map(|(lat, lng)| crate::geo::osrm::LatLng { lat, lng }))
}

/// Branch names, busiest first, for a member we know nothing about yet.
async fn branch_popularity(pool: &PgPool, org_id: Uuid) -> Result<Vec<String>, AppError> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT b.name FROM branches b \
           LEFT JOIN loyalty_transactions t ON t.branch_id = b.id \
          WHERE b.org_id = $1 AND b.is_active AND b.deleted_at IS NULL \
          GROUP BY b.id, b.name \
          ORDER BY count(t.id) DESC, b.name",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(n,)| n).collect())
}

/// Every branch with coordinates, unsorted and uncapped.
async fn all_located_branches(pool: &PgPool, org_id: Uuid) -> Result<Vec<PassLocation>, AppError> {
    let rows: Vec<(f64, f64, String)> = sqlx::query_as(
        "SELECT latitude, longitude, name FROM branches \
          WHERE org_id = $1 AND is_active AND deleted_at IS NULL \
            AND latitude IS NOT NULL AND longitude IS NOT NULL \
          ORDER BY name",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(latitude, longitude, name)| PassLocation {
            latitude,
            longitude,
            name,
        })
        .collect())
}

pub async fn locations_for_org(pool: &PgPool, org_id: Uuid) -> Result<Vec<PassLocation>, AppError> {
    let rows: Vec<(f64, f64, String)> = sqlx::query_as(
        "SELECT latitude, longitude, name FROM branches \
          WHERE org_id = $1 AND is_active AND deleted_at IS NULL \
            AND latitude IS NOT NULL AND longitude IS NOT NULL \
          ORDER BY name LIMIT $2",
    )
    .bind(org_id)
    .bind(MAX_LOCATIONS as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(latitude, longitude, name)| PassLocation {
            latitude,
            longitude,
            name,
        })
        .collect())
}

/// What signup hands the customer. Either side may be absent: a tenant with only
/// Google credentials configured shows one button, not a broken one.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PassLinks {
    /// Downloads the signed `.pkpass`. Site-relative, because the signup page
    /// is served from the same origin as the API — so a pass needs a
    /// CERTIFICATE, not a configured base URL.
    pub apple_url: Option<String>,
    /// `https://pay.google.com/gp/v/save/<jwt>`.
    pub google_url: Option<String>,
    /// False when neither wallet is configured — the site shows the member's
    /// QR on the page instead of dead buttons.
    pub any: bool,
}

/// Read PEM/key material from `KEY_FILE` (a path) or `KEY` (inline).
///
/// Shared by both wallets on purpose. Apple had the file form and Google did
/// not, which meant a documented `LOYALTY_GOOGLE_SA_KEY_FILE` was silently
/// ignored and the wallet read as unconfigured — no button, no error, nothing
/// in a log. One helper, so the two cannot drift again.
///
/// The file form is what production should use: a private key in an env var has
/// to have its newlines escaped, shows up in `docker inspect`, and lands in any
/// process listing that dumps the environment. The inline form stays for local
/// development and tests.
pub(crate) fn key_material(key: &str) -> Option<Vec<u8>> {
    if let Some(path) = std::env::var(format!("{key}_FILE"))
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        return match std::fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                // Configured but unreadable is an operator error worth shouting
                // about: silently falling back to "not configured" looks
                // identical to never having set it up.
                tracing::error!(path = %path, error = %e, "cannot read {}_FILE", key);
                None
            }
        };
    }
    std::env::var(key)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.replace("\\n", "\n").into_bytes())
}

/// The customer-facing base URL (`loyalty.madar-pos.cloud`), trailing slash
/// trimmed. Unset degrades softly: the Apple link is omitted rather than the
/// signup failing.
pub fn loyalty_base() -> Option<String> {
    std::env::var("PUBLIC_LOYALTY_BASE_URL")
        .ok()
        .map(|s| s.trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
}

/// The scheme and host of a URL, with no path. `None` if it is not absolute.
///
/// Pure so the parsing is testable: getting this wrong points every device on
/// the estate at an address that does not answer.
pub fn origin_of(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host = rest.split('/').next().filter(|h| !h.is_empty())?;
    let scheme = if url.starts_with("https://") {
        "https"
    } else {
        "http"
    };
    Some(format!("{scheme}://{host}"))
}

/// Where Apple's devices should call to refresh a pass.
///
/// Absolute by necessity — it is baked into a file on someone's phone, so it
/// cannot be relative like the download link is.
///
/// Falls back to the API's own origin, because that is where these endpoints
/// actually live and because tying pass UPDATES to `PUBLIC_LOYALTY_BASE_URL`
/// made one unset variable mean "no pass ever updates again" — silently, and
/// unfixably for every pass already issued under it.
pub fn web_service_url() -> Option<String> {
    absolute_api_url("/wallet")
}

/// An absolute, publicly reachable URL for one of this backend's paths.
///
/// Needed wherever a URL leaves our own pages: baked into a file on someone's
/// phone, or handed to Google to fetch from its servers. Site-relative works for
/// the customer's own browser and nowhere else.
///
/// Prefers the customer-facing host, whose `/api/` nginx strips before proxying
/// — the same route the `.pkpass` download already takes, so if one works both
/// do. Falls back to the API's own origin, which is where these paths actually
/// live, so a single unset variable cannot silently disable pass updates for
/// every pass already issued.
pub fn absolute_api_url(path: &str) -> Option<String> {
    if let Some(base) = loyalty_base() {
        return Some(format!("{base}/api{path}"));
    }
    let uploads = std::env::var("UPLOADS_BASE_URL").ok()?;
    Some(format!("{}{path}", origin_of(&uploads)?))
}

/// The org-level lines both wallets print on the back of the card.
///
/// Gathered once and carried as one value rather than as a widening row of
/// `&[String]` parameters — the two wallets must print the same words, and the
/// surest way to keep them printing the same words is to hand them the same
/// thing.
#[derive(Debug, Default, Clone)]
pub struct CardCopy {
    /// Rewards a member can claim, already phrased.
    pub rewards: Vec<String>,
    /// EVERY branch the card works at.
    ///
    /// Deliberately not the geofence list. That one is capped at ten, sorted by
    /// distance, and holds only branches somebody has put coordinates on — all
    /// correct for deciding where a phone should wake the card up, and all
    /// wrong for a heading that says "Where it works". Reusing it meant a shop
    /// with six branches and two sets of coordinates advertised two branches.
    pub branches: Vec<String>,
}

/// Everything the back of the card says about the shop, in one round trip each.
pub async fn card_copy(pool: &PgPool, org_id: Uuid) -> CardCopy {
    CardCopy {
        rewards: reward_lines(pool, org_id).await,
        branches: branch_names(pool, org_id).await,
    }
}

/// Every branch that is open, whether or not anyone has located it.
async fn branch_names(pool: &PgPool, org_id: Uuid) -> Vec<String> {
    sqlx::query_as::<_, (String,)>(
        "SELECT name FROM branches \
          WHERE org_id = $1 AND is_active AND deleted_at IS NULL \
          ORDER BY name",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(n,)| n)
    .collect()
}

/// Build both "add to wallet" links for a member.
/// The org's rewards, phrased once for both wallets.
///
/// Shared so an espresso does not read "Espresso — 5 visits" on one card and
/// something else on the other; the two are meant to be the same card.
pub async fn reward_lines(pool: &PgPool, org_id: Uuid) -> Vec<String> {
    crate::loyalty::settings::load_effective_rewards_org(pool, org_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| format!("{} — {} {}", r.name, r.cost_amount, r.cost_currency))
        .collect()
}

/// What the customer is working towards, in their own shop's words.
///
/// "Get a free drink" tells someone what the card is FOR in a way a stepper and
/// a ratio never do — those say how far, not what for. The cheapest reward is
/// the honest headline: a customer who can afford the espresso HAS earned
/// something, whatever the cake costs.
///
/// Falls back to the bare price when a shop has curated nothing yet, which at
/// least names the target.
pub async fn reward_headline(pool: &PgPool, org_id: Uuid, settings: &LoyaltySettings) -> String {
    let cheapest = crate::loyalty::settings::load_effective_rewards_org(pool, org_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .min_by_key(|r| r.cost_amount);
    match cheapest {
        Some(r) => r.name,
        None => format!(
            "{} {}",
            settings.default_reward_cost,
            google::balance_label(settings.mode()).to_lowercase()
        ),
    }
}

/// One line on the back of the card: a heading and what it says.
///
/// Apple calls these back fields and Google calls them text modules, and both
/// are the same thing — what you see after turning the card over. Built once,
/// here, so the two wallets cannot drift into telling a customer different
/// things about the same programme, which is exactly what happened while each
/// assembled its own.
pub struct BackLine {
    pub key: &'static str,
    pub label: String,
    pub value: String,
}

/// How many branches the back of the card lists before it stops.
///
/// A back field is read, not scanned, and forty names is not a list anyone
/// reads. Past this it says how many more there are, which is honest about
/// being partial in a way a silently truncated list is not.
const BRANCHES_SHOWN: usize = 12;

pub fn back_of_card(
    member: &MemberRow,
    settings: &LoyaltySettings,
    copy: &CardCopy,
) -> Vec<BackLine> {
    let threshold = settings.default_reward_cost;
    let mut out = vec![
        BackLine {
            key: "howitworks",
            label: "How it works".into(),
            // The two programs are explained in their own terms — a stamp card
            // that talked about EGP per point would be a card nobody could
            // follow at the counter.
            value: match settings.mode() {
                crate::loyalty::earn::Mode::Points => format!(
                    "Show this card when you pay. You earn a point for every {} EGP you spend, \
                     and a reward costs {threshold} points.",
                    settings.earn_piastres_per_point / 100,
                ),
                crate::loyalty::earn::Mode::Visits => format!(
                    "Show this card when you pay. Every order earns a stamp, \
                     and a reward costs {threshold} of them."
                ),
            },
        },
        BackLine {
            key: "member",
            label: "Member".into(),
            value: format!("{} · {}", member.name, member.phone),
        },
    ];
    if !copy.rewards.is_empty() {
        out.push(BackLine {
            key: "rewards",
            label: "Rewards you can claim".into(),
            value: copy.rewards.join("\n"),
        });
    }
    if !copy.branches.is_empty() {
        let mut value = copy
            .branches
            .iter()
            .take(BRANCHES_SHOWN)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(rest) = copy.branches.len().checked_sub(BRANCHES_SHOWN)
            && rest > 0
        {
            value.push_str(&format!("\nand {rest} more"));
        }
        out.push(BackLine {
            key: "branches",
            label: "Where it works".into(),
            value,
        });
    }
    if let Some(terms) = settings.terms.as_deref().filter(|t| !t.trim().is_empty()) {
        out.push(BackLine {
            key: "terms",
            label: "Terms".into(),
            value: terms.to_string(),
        });
    }
    out
}

/// Where a member's `.pkpass` is downloaded, when Apple is configured.
///
/// Relative to the site root, and under `/api/` — which is what nginx proxies
/// to the backend. Two things this deliberately is NOT:
///
///   * not a PAGE path (`/pass/…`), which falls through to the SPA fallback
///     and hands the customer an HTML document iOS refuses without a word;
///   * not absolute, so it does NOT depend on `PUBLIC_LOYALTY_BASE_URL`. The
///     page is served from this same origin, so signing a pass is all it takes
///     to offer one. The base URL is for the COUNTER QR's target and the pass's
///     self-update address — not for handing over the file.
pub fn apple_link(member: &MemberRow) -> Option<String> {
    apple::is_configured().then(|| {
        format!(
            "/api/public/loyalty/pass/{}/apple.pkpass",
            member.member_token
        )
    })
}

pub async fn links_for(
    pool: &PgPool,
    member: &MemberRow,
    settings: &LoyaltySettings,
    brand: &crate::orgs::branding::OrgBrand,
    locations: &[PassLocation],
) -> PassLinks {
    let apple_url = apple_link(member);
    // The same lines Apple prints on the back of its pass.
    let copy = card_copy(pool, member.org_id).await;
    let headline = reward_headline(pool, member.org_id, settings).await;
    // Provisioning talks to Google, so it can fail in ways a signup must
    // survive: an unlinked service account, a refused class, a network blip.
    // The customer gets the Apple badge and the code on their card either way,
    // and the reason lands in the log rather than in their face.
    let google_url =
        match google::save_url(pool, member, settings, brand, locations, &copy, &headline).await {
            Ok(url) => url,
            Err(e) => {
                tracing::warn!(error = %e, "loyalty: could not build the Google Wallet save link");
                None
            }
        };
    PassLinks {
        any: apple_url.is_some() || google_url.is_some(),
        apple_url,
        google_url,
    }
}

/// Push the member's new balance to whichever wallets hold their pass.
///
/// Fire-and-forget, copying `delivery::whatsapp::send_message`: the till is
/// never blocked on Apple's or Google's servers, and a failure is reported
/// rather than surfaced. The balance in the database is the truth; the pass is a
/// cache of it that catches up.
pub fn push_update(pool: &PgPool, customer_id: Uuid) {
    if !apple::is_configured() && !google::is_configured() {
        tracing::debug!(
            customer_id = %customer_id,
            "loyalty: no wallet configured — skipping pass update"
        );
        return;
    }
    let pool = pool.clone();
    tokio::spawn(async move {
        if let Err(e) = push_update_inner(&pool, customer_id).await {
            use crate::observability::report::{Failure, report};
            report(Failure::new("loyalty", "push_pass_update"), &e);
        }
    });
}

async fn push_update_inner(pool: &PgPool, customer_id: Uuid) -> Result<(), AppError> {
    let Some(member) = super::model::find_by_id(pool, customer_id).await? else {
        return Ok(());
    };
    if member.google_object_id.is_some() {
        google::push_balance(pool, &member).await?;
    }
    if member.apple_serial.is_some() {
        apple::notify_devices(pool, &member).await?;
    }
    sqlx::query("UPDATE loyalty_customers SET pass_updated_at = now() WHERE id = $1")
        .bind(customer_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Bring one member's pass up to date and wait for it.
///
/// `push_update` is fire-and-forget, which is right at a till and wrong in a
/// sweep: a loop that does not wait would launch a task per member and hand
/// APNs the entire estate at once.
pub async fn refresh_pass(pool: &PgPool, customer_id: Uuid) -> Result<(), AppError> {
    push_update_inner(pool, customer_id).await
}

/// Serialises the tests that read and write the wallet environment.
///
/// Env is process-global: two tests toggling `LOYALTY_APPLE_*` race, and the
/// failure looks like a signing bug rather than a test-harness one. Every test
/// in this module tree that touches those variables takes this first.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loyalty::model::MemberRow;
    use uuid::Uuid;

    fn member() -> MemberRow {
        MemberRow {
            id: Uuid::nil(),
            org_id: Uuid::nil(),
            name: "Ali".into(),
            phone: "201000000001".into(),
            member_token: "Mabcdefghijklmnopqrstuv".into(),
            points_balance: 0,
            visits_balance: 0,
            lifetime_points: 0,
            lifetime_visits: 0,
            locale: "en".into(),
            apple_serial: None,
            apple_auth_token: None,
            google_object_id: None,
            pass_updated_at: None,
            joined_branch_id: None,
            enrolled_at: chrono::Utc::now(),
            marketing_opt_out: false,
        }
    }

    #[test]
    fn the_update_address_survives_an_unset_loyalty_base() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: the lock makes this the only thread touching the environment.
        unsafe {
            std::env::remove_var("PUBLIC_LOYALTY_BASE_URL");
            std::env::set_var(
                "UPLOADS_BASE_URL",
                "https://api.madar-pos.cloud/api/uploads",
            );
        }
        // Falls back to the API origin, where the endpoints actually are.
        assert_eq!(
            web_service_url().as_deref(),
            Some("https://api.madar-pos.cloud/wallet")
        );

        unsafe {
            std::env::set_var("PUBLIC_LOYALTY_BASE_URL", "https://loyalty.madar-pos.cloud");
        }
        assert_eq!(
            web_service_url().as_deref(),
            Some("https://loyalty.madar-pos.cloud/api/wallet")
        );
        unsafe {
            std::env::remove_var("PUBLIC_LOYALTY_BASE_URL");
            std::env::remove_var("UPLOADS_BASE_URL");
        }
        // Nothing configured: the pass still issues, it simply never self-updates.
        assert!(web_service_url().is_none());
    }

    /// "Where it works" is the shop's branches, not the ones we can geofence.
    ///
    /// It used to be built from the same array as the geofence — which is
    /// capped at ten, sorted by distance, and holds only branches somebody had
    /// put coordinates on. A shop with six branches and two sets of
    /// coordinates therefore told its customers it had two.
    #[test]
    fn the_back_of_the_card_lists_branches_nobody_has_located() {
        let s = LoyaltySettings::defaults(Uuid::nil(), None);
        let copy = CardCopy {
            rewards: vec![],
            branches: vec!["Maadi".into(), "Zamalek".into(), "Alexandria".into()],
        };
        // One located branch, three open ones.
        let lines = back_of_card(&apple::tests::member(), &s, &copy);
        let branches = lines.iter().find(|l| l.key == "branches").unwrap();
        assert_eq!(branches.value, "Maadi\nZamalek\nAlexandria");
    }

    /// A back field is read, not scanned; past a dozen it says how many more.
    #[test]
    fn a_long_list_of_branches_says_how_many_it_left_out() {
        let s = LoyaltySettings::defaults(Uuid::nil(), None);
        let copy = CardCopy {
            rewards: vec![],
            branches: (1..=15).map(|i| format!("Branch {i}")).collect(),
        };
        let lines = back_of_card(&apple::tests::member(), &s, &copy);
        let branches = lines.iter().find(|l| l.key == "branches").unwrap();
        assert_eq!(branches.value.lines().count(), BRANCHES_SHOWN + 1);
        assert!(
            branches.value.ends_with("and 3 more"),
            "a truncated list must admit it is truncated: {}",
            branches.value
        );
    }

    /// The two cards are meant to be one card. This is what stops them drifting.
    #[test]
    fn both_wallets_are_given_the_same_card() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: the lock makes this the only thread touching the environment.
        unsafe {
            std::env::set_var("LOYALTY_APPLE_PASS_TYPE_ID", "pass.example");
            std::env::set_var("LOYALTY_APPLE_TEAM_ID", "TEAM123456");
        }
        let mut s = LoyaltySettings::defaults(Uuid::nil(), None);
        s.mode = "visits".into();
        s.default_reward_cost = 5;
        s.terms = Some("One per visit".into());
        let m = apple::tests::member();
        let locs = [PassLocation {
            name: "Maadi".into(),
            latitude: 30.0,
            longitude: 31.0,
        }];
        let copy = CardCopy {
            rewards: vec!["Espresso — 5 visits".to_string()],
            branches: vec!["Maadi".into(), "Zamalek".into()],
        };

        let apple = apple::pass_json(
            &m,
            &s,
            &locs,
            &copy,
            "Free espresso",
            &apple::PassBrand::default(),
        )
        .unwrap();
        let google = google::loyalty_object("338", &m, &s, &locs, &copy, "Free espresso");

        // Google gives a card TWO face slots where Apple gives four, so parity
        // is about which words land where, not about a field-for-field copy.
        //
        // How far along, in each wallet's largest slot, with the same label.
        assert_eq!(
            apple["storeCard"]["secondaryFields"][0]["value"],
            google["loyaltyPoints"]["balance"]["string"]
        );
        // The balance's own label. Apple puts it on the big number and Google
        // on its largest slot; the words must match, or one phone calls them
        // orders while the other calls them points.
        assert_eq!(
            apple["storeCard"]["primaryFields"][0]["label"],
            google["loyaltyPoints"]["label"]
        );

        // And what it is FOR, in the other — the same words on both, or a
        // customer comparing two phones sees two different promises.
        assert_eq!(
            apple["storeCard"]["secondaryFields"][1]["value"],
            google["secondaryLoyaltyPoints"]["balance"]["string"]
        );
        assert_eq!(
            apple["storeCard"]["secondaryFields"][1]["label"],
            google["secondaryLoyaltyPoints"]["label"]
        );

        // The barcode carries the same thing.
        assert_eq!(apple["barcodes"][0]["message"], google["barcode"]["value"]);

        // And the back of the card is the same list, in the same order, saying
        // the same things — Apple behind it, Google beneath it.
        let apple_back: Vec<(String, String)> = apple["storeCard"]["backFields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                (
                    f["label"].as_str().unwrap().to_string(),
                    f["value"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        // Google renders `accountName` and `accountId` as rows of its own, so
        // the Member line is dropped there rather than printed a third time —
        // it carries a phone number.
        let apple_back: Vec<(String, String)> = apple_back
            .into_iter()
            .filter(|(l, _)| l != "Member")
            .collect();
        let google_back: Vec<(String, String)> = google["textModulesData"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                (
                    m["header"].as_str().unwrap().to_string(),
                    m["body"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(apple_back, google_back);
        assert!(
            apple_back.iter().any(|(l, _)| l == "Terms"),
            "the shop's terms reach both: {apple_back:?}"
        );
        assert!(
            google["accountName"].as_str() == Some(m.name.as_str()),
            "Google shows the member itself, which is why we do not repeat it"
        );

        unsafe {
            std::env::remove_var("LOYALTY_APPLE_PASS_TYPE_ID");
            std::env::remove_var("LOYALTY_APPLE_TEAM_ID");
        }
    }

    #[test]
    fn origin_parsing_keeps_the_scheme_and_drops_the_path() {
        assert_eq!(
            origin_of("https://api.madar-pos.cloud/api/uploads").as_deref(),
            Some("https://api.madar-pos.cloud")
        );
        assert_eq!(
            origin_of("http://localhost:8081/x").as_deref(),
            Some("http://localhost:8081")
        );
        assert_eq!(
            origin_of("https://api.example.com").as_deref(),
            Some("https://api.example.com")
        );
        // Not absolute, so there is no origin to take.
        assert_eq!(origin_of("/api/uploads"), None);
        assert_eq!(origin_of("https://"), None);
    }

    /// The Apple link is the API path, and a CERTIFICATE is all it takes.
    ///
    /// Two things this pins. It must not be a page path — that falls through to
    /// the SPA fallback and hands the customer an HTML document iOS refuses
    /// with no message, which looks exactly like a certificate problem. And it
    /// must not depend on `PUBLIC_LOYALTY_BASE_URL`: the page is same-origin,
    /// so requiring an absolute URL made a working set of certificates look
    /// broken for no reason.
    #[test]
    fn the_apple_link_points_at_the_api_and_needs_no_base_url() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: the lock above makes this the only thread touching the wallet
        // environment for the duration.
        unsafe {
            // Deliberately NOT set: a pass needs a certificate, not this.
            std::env::remove_var("PUBLIC_LOYALTY_BASE_URL");
            std::env::set_var("LOYALTY_APPLE_PASS_TYPE_ID", "pass.example");
            std::env::set_var("LOYALTY_APPLE_TEAM_ID", "TEAM123456");
            std::env::set_var("LOYALTY_APPLE_CERT_PEM", "x");
            std::env::set_var("LOYALTY_APPLE_KEY_PEM", "x");
            std::env::set_var("LOYALTY_APPLE_WWDR_PEM", "x");
        }
        assert_eq!(
            apple_link(&member()).as_deref(),
            Some("/api/public/loyalty/pass/Mabcdefghijklmnopqrstuv/apple.pkpass"),
            "certificates alone must be enough to offer the pass"
        );

        // With nothing configured at all, no buttons: the page shows the
        // member's QR instead, which still works at the till.
        unsafe {
            std::env::remove_var("LOYALTY_APPLE_PASS_TYPE_ID");
            std::env::remove_var("LOYALTY_APPLE_TEAM_ID");
            std::env::remove_var("LOYALTY_APPLE_CERT_PEM");
            std::env::remove_var("LOYALTY_APPLE_KEY_PEM");
            std::env::remove_var("LOYALTY_APPLE_WWDR_PEM");
        }
        assert!(
            apple_link(&member()).is_none(),
            "no certificate means no Apple link — the card still carries its code"
        );
    }

    // Deliberately ONE test, not two: these read process-global environment,
    // and two tests mutating it race in the same process.
}
