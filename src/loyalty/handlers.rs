//! Staff-facing surfaces: the teller's scan and redeem, and the admin's member
//! list, history and corrections.
//!
//! The teller never types a number of points. Earning is the server's business
//! (see [`super::model::award_for_order`], called from `create_order_inner`);
//! what a teller does here is identify the member in front of them and, when the
//! balance allows, hand over a reward.

use actix_web::{HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::model::{self, LedgerEntry, MemberRow, MemberView};
use super::settings::{
    LoyaltySettings, RewardItem, ScopeQuery, load_effective, load_effective_rewards,
};
use super::{resolve_branch_org, wallet};
use crate::auth::guards::require_super_admin;
use crate::delivery::{normalize_phone, require_branch_access};
use crate::errors::{AppError, AppErrorResponse};
use crate::models::UserRole;
use crate::orgs::handlers::extract_claims;
use crate::permissions::checker::check_permission;

/// What the teller's scan screen shows.
#[derive(Debug, Serialize, ToSchema)]
pub struct ScanResult {
    pub member: MemberView,
    /// What this member could claim at this branch right now. Empty until the
    /// balance reaches the threshold, so the screen cannot tempt a teller into
    /// handing over a reward that has not been earned.
    pub rewards: Vec<RewardItem>,
    /// Recent history, so a teller can answer "where did my points go?".
    pub recent: Vec<LedgerEntry>,
    /// The whole menu is claimable, not just `rewards`.
    ///
    /// When on, `rewards` stops being the list of what MAY be claimed — it is
    /// only what happens to be curated — and the till offers every line at
    /// `any_item_cost`. Sent rather than inferred, because a till cannot tell
    /// "no catalogue" apart from "any item" without being told.
    pub any_item: bool,
    /// What one line costs when `any_item` is on, in the branch's currency.
    pub any_item_cost: i32,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct LookupRequest {
    pub branch_id: Uuid,
    /// The token from the scanned pass barcode. Preferred.
    pub token: Option<String>,
    /// Manual fallback for a customer whose phone is dead.
    pub phone: Option<String>,
}

/// Identify the member in front of the till.
///
/// A POST rather than a GET because the member token is a bearer-ish secret: in
/// a query string it would land in access logs, browser history and any proxy
/// in between.
#[utoipa::path(post, path = "/loyalty/lookup", tag = "loyalty", operation_id = "loyalty_lookup",
    request_body = LookupRequest, responses((status = 200, body = ScanResult), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn lookup(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<LookupRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "loyalty", "read").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;
    let org_id = resolve_branch_org(pool.get_ref(), body.branch_id).await?;

    let member = resolve_member(pool.get_ref(), org_id, &body).await?;
    let settings = load_effective(pool.get_ref(), org_id, body.branch_id).await?;
    let (all_rewards, _) = load_effective_rewards(pool.get_ref(), org_id, body.branch_id).await?;
    let recent = model::ledger(pool.get_ref(), member.id, 10).await?;
    let mode = settings.mode();
    let target = model::reward_target(&settings, &all_rewards);
    let view = member.clone().view(mode, target);
    // Only what this balance can actually buy, priced in the live currency. A
    // screen that lists a reward the customer cannot afford is a screen that
    // tempts a teller into handing it over anyway.
    let balance = member.balance_in(mode);
    // Affordability only. The catalogue is already priced in this branch's
    // currency, so a second check against the row's stored one could only drop
    // rewards the shop had configured — a till that offered nothing while the
    // dashboard listed a full catalogue, with nothing anywhere saying why.
    let rewards: Vec<_> = all_rewards
        .into_iter()
        .filter(|r| r.cost_amount <= balance)
        .collect();
    Ok(HttpResponse::Ok().json(ScanResult {
        member: view,
        rewards,
        recent,
        any_item: settings.reward_any_item,
        any_item_cost: settings.default_reward_cost,
    }))
}

/// The birthday greeting as it would actually be sent, in both languages.
#[derive(Debug, Serialize, ToSchema)]
pub struct BirthdayPreview {
    pub en: String,
    pub ar: String,
}

/// Render the greeting for settings that have NOT been saved yet.
///
/// Rendered by the server, from the same `message_for` the sweep uses, because
/// a preview reimplemented in the dashboard is a preview that drifts — and the
/// thing it would drift from is a message sent once a year to a customer, where
/// nobody would ever catch it.
///
/// Takes the settings being edited rather than reading the stored ones: the
/// point is to see what you are about to save.
#[utoipa::path(post, path = "/loyalty/birthday-preview", tag = "loyalty",
    operation_id = "preview_loyalty_birthday_message", request_body = LoyaltySettings,
    responses((status = 200, body = BirthdayPreview), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn birthday_preview(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<LoyaltySettings>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "loyalty", "update").await?;
    let settings = body.into_inner();
    // A stand-in name, so the placeholder is visibly a placeholder.
    let sample = "Sara";
    Ok(HttpResponse::Ok().json(BirthdayPreview {
        en: crate::loyalty::birthdays::message_for(&settings, sample, "en"),
        ar: crate::loyalty::birthdays::message_for(&settings, sample, "ar"),
    }))
}

/// What a wallet needs before it will offer a button, and whether it has it.
#[derive(Debug, Serialize, ToSchema)]
pub struct WalletProvider {
    /// Everything present. False means the button is not offered at all.
    pub configured: bool,
    /// The settings still missing, by name. Empty when `configured`.
    pub missing: Vec<String>,
    /// Google only: what Google itself said when asked. `None` for Apple, which
    /// signs locally and has nobody to ask.
    pub reachable: Option<bool>,
    pub detail: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct WalletStatus {
    pub apple: WalletProvider,
    pub google: WalletProvider,
    /// Apple's push channel, which is SEPARATE from pass signing.
    ///
    /// A pass can be issued perfectly and never change on anyone's phone,
    /// because the two are configured independently — and this panel used to
    /// report Apple as fine while every balance update went nowhere.
    pub apns: WalletProvider,
}

/// Why there is no "Add to Wallet" button. **Super admin only.**
///
/// Every failure in this feature has looked the same from the outside — a
/// missing button, or a save that says "something went wrong" — while the cause
/// was a variable nobody set, a key file the code never read, a service account
/// Google had not been told about, or a link over a size limit. None of those
/// reach a customer's screen, and only some reach a log.
///
/// This asks, on demand, and reports what it finds. It makes live calls to
/// Google, so it is deliberately not part of any page load.
#[utoipa::path(get, path = "/loyalty/wallet-status", tag = "loyalty",
    operation_id = "get_loyalty_wallet_status", params(ScopeQuery),
    responses((status = 200, body = WalletStatus), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn wallet_status(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<ScopeQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "loyalty", "update").await?;
    // Super admin ONLY, and not because it is dangerous — because it is not the
    // shop's business. It names Madar's environment variables and reports what
    // Apple and Google said about our service accounts. Handing an org manager
    // "LOYALTY_GOOGLE_SA_KEY_FILE is not set" tells them nothing they can act
    // on and everything about plumbing they never asked to know.
    require_super_admin(&claims)?;
    // Optional, and that is the point. Three of the four things reported here
    // are pure environment — Apple's signing keys, the APNs keys, Google's
    // service account — and are the same answer for every shop on the box.
    // Only the card class belongs to one org. Requiring one up front gated the
    // whole panel on its one optional line, which is how a super admin with no
    // org pinned got a 400 instead of an answer.
    //
    // A super admin's token carries no org of its own; the dashboard pins one
    // with `X-Org-Id`, which is what `scope_org` reads.
    let org_id = match query.branch_id {
        Some(b) => {
            require_branch_access(pool.get_ref(), &claims, b).await?;
            Some(resolve_branch_org(pool.get_ref(), b).await?)
        }
        None => claims.scope_org(crate::auth::middleware::header_org_id(&req)),
    };

    let apple_missing = wallet::apple::missing_env();
    let apns_missing = wallet::apns::missing_env();
    let google_missing = wallet::google::missing_env();
    // Only worth asking Google when there is something to ask with.
    let (reachable, detail) = if google_missing.is_empty() {
        match wallet::google::check(org_id).await {
            Ok(ok) => (Some(true), Some(ok)),
            Err(e) => (Some(false), Some(e)),
        }
    } else {
        (None, None)
    };

    Ok(HttpResponse::Ok().json(WalletStatus {
        apple: WalletProvider {
            configured: apple_missing.is_empty(),
            missing: apple_missing,
            reachable: None,
            detail: None,
        },
        apns: WalletProvider {
            configured: apns_missing.is_empty(),
            missing: apns_missing,
            reachable: None,
            detail: None,
        },
        google: WalletProvider {
            configured: google_missing.is_empty(),
            missing: google_missing,
            reachable,
            detail,
        },
    }))
}

/// What Google is actually holding for one member's card. **Super admin only.**
///
/// The nearby-notification question has been answered three times by reasoning
/// and never by looking: either the `locations` are on the object Google holds
/// and the gap is in what Google does with them, or they never arrived and the
/// gap is ours. Both stories fit every symptom from the outside; only the object
/// separates them. This returns it verbatim, unsummarised, because a summary
/// would be one more layer of my guessing between the evidence and the reader.
///
/// Super admin for the same reason as `wallet_status`: it is Madar's plumbing,
/// named in Google's vocabulary, and there is nothing an org manager could do
/// with it.
#[utoipa::path(get, path = "/loyalty/members/{id}/google-object", tag = "loyalty",
    operation_id = "get_loyalty_google_object",
    params(("id" = Uuid, Path, description = "Loyalty member id")),
    responses((status = 200, body = GoogleObjectDump), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn google_object(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "loyalty", "update").await?;
    require_super_admin(&claims)?;
    let member = model::find_by_id(pool.get_ref(), *id)
        .await?
        .ok_or_else(|| AppError::NotFound("Member not found".into()))?;

    // What we BELIEVE we sent, computed the same way the writer computes it. If
    // this is empty the object was always going to be location-less and Google
    // is not the suspect.
    let expected = wallet::locations_for_member(pool.get_ref(), &member)
        .await
        .unwrap_or_default()
        .len();

    match wallet::google::read_object(&member).await {
        Ok(object) => {
            let stored = object
                .get("locations")
                .and_then(|l| l.as_array())
                .map_or(0, |a| a.len());
            Ok(HttpResponse::Ok().json(GoogleObjectDump {
                expected_locations: expected,
                stored_locations: stored,
                object: Some(object),
                error: None,
            }))
        }
        Err(e) => Ok(HttpResponse::Ok().json(GoogleObjectDump {
            expected_locations: expected,
            stored_locations: 0,
            object: None,
            error: Some(e),
        })),
    }
}

/// The card Google holds, plus the two counts that make it readable at a glance.
#[derive(Debug, Serialize, ToSchema)]
pub struct GoogleObjectDump {
    /// Branches this member's card should be pinned to, from our own side.
    pub expected_locations: usize,
    /// Branches Google says are on it. A gap between the two is the answer.
    pub stored_locations: usize,
    /// Google's object, untouched. `None` when the read itself failed.
    #[schema(value_type = Option<Object>)]
    pub object: Option<serde_json::Value>,
    /// Why there is no object, in Google's words.
    pub error: Option<String>,
}

/// The member a lookup names, checked against the branch's org.
///
/// A token is globally unique and carries no org of its own, so a member from
/// another tenant would otherwise resolve here. Reporting that as "not found"
/// rather than "wrong org" keeps one tenant from probing another's membership.
async fn resolve_member(
    pool: &PgPool,
    org_id: Uuid,
    body: &LookupRequest,
) -> Result<MemberRow, AppError> {
    let found = match (&body.token, &body.phone) {
        (Some(token), _) if !token.trim().is_empty() => {
            model::find_by_token(pool, token.trim()).await?
        }
        (_, Some(phone)) if !phone.trim().is_empty() => {
            model::find_by_phone(pool, org_id, &normalize_phone(phone)?).await?
        }
        _ => {
            return Err(AppError::BadRequest(
                "Supply the scanned token or a phone number".into(),
            ));
        }
    };
    match found {
        Some(m) if m.org_id == org_id => Ok(m),
        _ => Err(AppError::NotFound("No member for that card".into())),
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AdjustRequest {
    pub branch_id: Uuid,
    pub customer_id: Uuid,
    /// Signed. Negative takes points away.
    pub points: i32,
    pub note: Option<String>,
}

#[utoipa::path(post, path = "/loyalty/adjust", tag = "loyalty", operation_id = "loyalty_adjust",
    request_body = AdjustRequest, responses((status = 200, body = MemberView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn adjust(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<AdjustRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    // Handing out points by hand is the one action here that creates value from
    // nothing, so it sits above the till: the permission table is consulted as
    // usual AND the actor must be an admin. `permission_action` has no rung
    // above `update`, so the role check is what separates this from a redeem.
    check_permission(pool.get_ref(), &claims, "loyalty", "update").await?;
    if !matches!(claims.role, UserRole::OrgAdmin | UserRole::SuperAdmin) {
        return Err(AppError::Forbidden(
            "Only an admin may adjust a member's points by hand".into(),
        ));
    }
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;
    let org_id = resolve_branch_org(pool.get_ref(), body.branch_id).await?;

    let member = model::find_by_id(pool.get_ref(), body.customer_id)
        .await?
        .filter(|m| m.org_id == org_id)
        .ok_or_else(|| AppError::NotFound("No member for that card".into()))?;
    let settings = load_effective(pool.get_ref(), org_id, body.branch_id).await?;
    let (rewards, _) = load_effective_rewards(pool.get_ref(), org_id, body.branch_id).await?;
    let mode = settings.mode();
    let target = model::reward_target(&settings, &rewards);

    let view = model::adjust(
        pool.get_ref(),
        &member,
        body.branch_id,
        mode,
        body.points,
        body.note.clone(),
        claims.user_id_safe().ok(),
    )
    .await?;
    wallet::push_update(pool.get_ref(), member.id);
    Ok(HttpResponse::Ok().json(MemberView {
        next_reward_cost: target,
        points_to_next_reward: (target - view.balance).max(0),
        can_redeem: view.balance >= target,
        ..view
    }))
}

// ── Admin: the member list and one member's history ──────────────────────────

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct MembersQuery {
    /// Scopes the thresholds shown. Omit to use the org default.
    pub branch_id: Option<Uuid>,
    /// Name or phone fragment.
    pub q: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MembersPage {
    pub members: Vec<MemberView>,
    pub total: i64,
}

#[utoipa::path(get, path = "/loyalty/members", tag = "loyalty", operation_id = "list_loyalty_members",
    params(MembersQuery), responses((status = 200, body = MembersPage), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn list_members(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<MembersQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "loyalty", "read").await?;
    let org_id = match (claims.org_id(), query.branch_id) {
        (Some(o), _) => o,
        (None, Some(b)) => resolve_branch_org(pool.get_ref(), b).await?,
        (None, None) => {
            return Err(AppError::BadRequest(
                "branch_id is required for a super admin".into(),
            ));
        }
    };

    let scope = match query.branch_id {
        Some(b) => {
            require_branch_access(pool.get_ref(), &claims, b).await?;
            load_effective(pool.get_ref(), org_id, b).await?
        }
        None => super::settings::load_scope(pool.get_ref(), org_id, None)
            .await?
            .unwrap_or_else(|| super::settings::LoyaltySettings::defaults(org_id, None)),
    };
    let rewards = match query.branch_id {
        Some(b) => load_effective_rewards(pool.get_ref(), org_id, b).await?.0,
        None => super::settings::load_effective_rewards_org(pool.get_ref(), org_id).await?,
    };
    let mode = scope.mode();
    let target = model::reward_target(&scope, &rewards);

    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let offset = query.offset.unwrap_or(0).max(0);
    // `%` and `_` in a search box are literal characters to the person typing
    // them, not wildcards that quietly match everything.
    let needle = query
        .q
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            format!(
                "%{}%",
                s.replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            )
        });

    let rows: Vec<MemberRow> = sqlx::query_as(&format!(
        "SELECT {} FROM loyalty_customers \
          WHERE org_id = $1 AND deleted_at IS NULL \
            AND ($2::text IS NULL OR name ILIKE $2 OR phone ILIKE $2) \
          ORDER BY enrolled_at DESC LIMIT $3 OFFSET $4",
        model::MEMBER_COLS
    ))
    .bind(org_id)
    .bind(&needle)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool.get_ref())
    .await?;

    let total: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM loyalty_customers \
          WHERE org_id = $1 AND deleted_at IS NULL \
            AND ($2::text IS NULL OR name ILIKE $2 OR phone ILIKE $2)",
    )
    .bind(org_id)
    .bind(&needle)
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(MembersPage {
        members: rows.into_iter().map(|r| r.view(mode, target)).collect(),
        total,
    }))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MemberDetail {
    pub member: MemberView,
    pub ledger: Vec<LedgerEntry>,
}

#[utoipa::path(get, path = "/loyalty/members/{id}", tag = "loyalty", operation_id = "get_loyalty_member",
    params(("id" = Uuid, Path, description = "Member ID"), MembersQuery),
    responses((status = 200, body = MemberDetail), AppErrorResponse), security(("bearer_jwt" = [])))]
pub async fn get_member(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
    query: web::Query<MembersQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "loyalty", "read").await?;
    let member = model::find_by_id(pool.get_ref(), *id)
        .await?
        .ok_or_else(|| AppError::NotFound("Member not found".into()))?;
    if let Some(org) = claims.org_id()
        && member.org_id != org
    {
        return Err(AppError::NotFound("Member not found".into()));
    }
    let scope = match query.branch_id {
        Some(b) => load_effective(pool.get_ref(), member.org_id, b).await?,
        None => super::settings::load_scope(pool.get_ref(), member.org_id, None)
            .await?
            .unwrap_or_else(|| super::settings::LoyaltySettings::defaults(member.org_id, None)),
    };
    let rewards =
        super::settings::load_effective_rewards_org(pool.get_ref(), member.org_id).await?;
    let mode = scope.mode();
    let target = model::reward_target(&scope, &rewards);
    let ledger = model::ledger(pool.get_ref(), member.id, 200).await?;
    Ok(HttpResponse::Ok().json(MemberDetail {
        member: member.view(mode, target),
        ledger,
    }))
}
