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
    /// The shop's ceiling on reward ITEMS per order, if it set one. The till
    /// enforces it before Charge so the server's refusal is never the first
    /// the teller hears of it.
    pub max_rewards_per_order: Option<i32>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct LookupRequest {
    pub branch_id: Uuid,
    /// The token from the scanned pass barcode. Preferred.
    pub token: Option<String>,
    /// Manual fallback for a customer whose phone is dead.
    pub phone: Option<String>,
    /// A member the till already identified, re-read before a charge so the
    /// balance and catalogue it prices against are the server's current ones.
    #[serde(default)]
    pub customer_id: Option<Uuid>,
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
        max_rewards_per_order: settings.max_rewards_per_order,
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

/// Provision this member's Google card and report every word of it.
/// **Super admin only.**
///
/// Reading the object back says what Google HOLDS. It does not say why, and by
/// the time you are reading it the write that mattered is over — a refused
/// class refresh is deliberately only a warning, because a customer must keep
/// the card they have, so the reason goes to a log rather than to the person
/// asking the question.
///
/// This runs the real provisioning through the real code path, keeping a
/// transcript: every request, its status, and Google's answer verbatim. Then it
/// reads both resources back, so the transcript and the outcome sit together.
///
/// It WRITES, which is why it is a POST and why it is not part of any page
/// load. Everything it does, opening a customer's card page does too.
#[utoipa::path(post, path = "/loyalty/members/{id}/google-refresh", tag = "loyalty",
    operation_id = "refresh_loyalty_google_pass",
    params(("id" = Uuid, Path, description = "Loyalty member id")),
    responses((status = 200, body = GoogleRefreshReport), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn google_refresh(
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

    let settings = super::settings::load_scope(pool.get_ref(), member.org_id, None)
        .await?
        .unwrap_or_else(|| LoyaltySettings::defaults(member.org_id, None));
    let brand = crate::orgs::branding::load(pool.get_ref(), member.org_id).await?;
    let locations = wallet::locations_for_member(pool.get_ref(), &member).await?;
    let copy = wallet::card_copy(pool.get_ref(), member.org_id, &settings).await;
    let headline = wallet::reward_headline(pool.get_ref(), member.org_id, &settings).await;

    let mut steps = Vec::new();
    let outcome = wallet::google::save_url_recorded(
        pool.get_ref(),
        &member,
        &settings,
        &brand,
        &locations,
        &copy,
        &headline,
        &mut steps,
    )
    .await;

    // Read both back AFTER the write, so what is reported is what Google kept —
    // which is not always what it was sent, and that difference is the whole
    // reason this endpoint exists.
    let class = wallet::google::read_class(member.org_id).await;
    let object = wallet::google::read_object(&member).await;
    let count = |v: &Result<serde_json::Value, String>| {
        v.as_ref()
            .ok()
            .and_then(|v| v["locations"].as_array().map(|a| a.len()))
            .unwrap_or(0)
    };

    Ok(HttpResponse::Ok().json(GoogleRefreshReport {
        sent_locations: locations.len(),
        class_locations: count(&class),
        object_locations: count(&object),
        steps,
        class: class.clone().ok(),
        object: object.clone().ok(),
        error: outcome
            .err()
            .map(|e| e.to_string())
            .or_else(|| class.err())
            .or_else(|| object.err()),
    }))
}

/// A provisioning run, in full.
#[derive(Debug, Serialize, ToSchema)]
pub struct GoogleRefreshReport {
    /// Branches this member's card was sent, from our side.
    pub sent_locations: usize,
    /// Branches Google kept on the shop's class.
    pub class_locations: usize,
    /// Branches Google kept on this member's object.
    pub object_locations: usize,
    /// Every request and Google's answer, in order.
    pub steps: Vec<wallet::google::WalletStep>,
    /// The class as Google holds it now.
    #[schema(value_type = Option<Object>)]
    pub class: Option<serde_json::Value>,
    /// The object as Google holds it now.
    #[schema(value_type = Option<Object>)]
    pub object: Option<serde_json::Value>,
    /// The first thing that went wrong, if anything did.
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
    if let Some(id) = body.customer_id {
        return match model::find_by_id(pool, id).await? {
            Some(m) if m.org_id == org_id => Ok(m),
            _ => Err(AppError::NotFound("No member for that card".into())),
        };
    }
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
    crate::authz::require::require(
        pool.get_ref(),
        &claims,
        crate::authz::Cap::LoyaltyPointsAdjust,
        None,
    )
    .await?;
    // A number typed by a person with no reason beside it is the one ledger row
    // nobody can later explain. Refused before anything moves.
    let note = body
        .note
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            AppError::BadRequest("Say why the points are being adjusted (note)".into())
        })?;
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
        model::Source::Manual,
        Some(note),
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
    // The same resolution the settings and rewards handlers use, from the same
    // place — this list used to answer a super admin with a 400 while the two
    // tabs beside it answered with defaults, for one cause.
    let (org_id, claims) =
        super::settings::scope_org(pool.get_ref(), &req, query.branch_id).await?;
    check_permission(pool.get_ref(), &claims, "loyalty", "read").await?;
    // `loyalty:read` is granted to TELLERS, because a till needs it to scan a
    // card — and that is a different act from reading out the whole customer
    // list, names and phone numbers included, which is what this endpoint is
    // and what the dashboard turns into a spreadsheet. The till never calls
    // this one: it calls `lookup`, which answers about the person in front of
    // it and nobody else.
    crate::authz::require::require(
        pool.get_ref(),
        &claims,
        crate::authz::Cap::LoyaltyMembersList,
        None,
    )
    .await?;

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

/// Forget a member. **Admin only.**
///
/// A void corrects a sale; this corrects a membership — someone asked the shop
/// to stop holding their details, or an admin is clearing a test signup. The
/// person is scrubbed and the books are kept: see [`model::forget`] for exactly
/// what goes and what stays, and why the ledger is not the member's data.
///
/// 204 twice in a row: forgetting someone already forgotten is not a failure,
/// and telling the caller "no such member" would confirm that a phone number
/// used to be one.
#[utoipa::path(delete, path = "/loyalty/members/{id}", tag = "loyalty",
    operation_id = "delete_loyalty_member",
    params(("id" = Uuid, Path, description = "Member ID")),
    responses((status = 204, description = "Forgotten"), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn delete_member(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "loyalty", "update").await?;
    // Above the till, like `adjust`: a teller identifies the person in front of
    // them, and does not erase people.
    crate::authz::require::require(
        pool.get_ref(),
        &claims,
        crate::authz::Cap::LoyaltyMembersDelete,
        None,
    )
    .await?;
    let Some(member) = model::find_by_id(pool.get_ref(), *id).await? else {
        return Ok(HttpResponse::NoContent().finish());
    };
    if let Some(org) = claims.org_id()
        && member.org_id != org
    {
        return Err(AppError::NotFound("Member not found".into()));
    }

    let Some(before) = model::forget(pool.get_ref(), member.id).await? else {
        return Ok(HttpResponse::NoContent().finish());
    };

    // After the commit, never inside it — the same rule every wallet call here
    // follows. The card is already dead on our side (token rotated, devices
    // dropped); this tells Google to stop rendering it. A failure is reported,
    // not surfaced: the forget has happened, and nothing the admin could do
    // with a 503 would make it more so.
    tokio::spawn(async move {
        if let Err(e) = wallet::google::expire_object(&before).await {
            use crate::observability::report::{Failure, report};
            report(Failure::new("loyalty", "expire_google_object"), &e);
        }
    });
    Ok(HttpResponse::NoContent().finish())
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

// ── Analytics ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, IntoParams)]
pub struct AnalyticsQuery {
    /// Omit for the whole organisation; supply a branch to narrow the
    /// redemption figures to it (the liability is org-wide either way — a
    /// balance can be spent at any branch).
    pub branch_id: Option<Uuid>,
    /// Inclusive start of the range. Defaults to 30 days before `to`.
    pub from: Option<chrono::DateTime<chrono::Utc>>,
    /// Exclusive end of the range. Defaults to now.
    pub to: Option<chrono::DateTime<chrono::Utc>>,
}

/// One reward, by how often it was claimed in the range.
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct TopReward {
    pub menu_item_id: Uuid,
    pub name: String,
    /// Redemption rows (one per covered order line).
    pub redemptions: i64,
    /// Units handed over.
    pub units: i64,
    /// Balance spent on it, net of anything given back by a void or refund.
    pub points: i64,
    /// Minor units of goods given away (what the covered lines were charged).
    pub value_minor: i64,
}

/// What the programme owes its members.
#[derive(Debug, Serialize, ToSchema)]
pub struct PointsLiability {
    /// Live members with a positive balance.
    pub members_with_balance: i64,
    pub outstanding_points: i64,
    pub outstanding_visits: i64,
    /// Minor units one unit of the live currency has bought, on average, over
    /// every redemption this org has recorded (value given ÷ balance spent).
    /// `None` until the first redemption with a recorded value.
    pub value_per_unit_minor: Option<f64>,
    /// The outstanding balance in the live currency × `value_per_unit_minor`,
    /// rounded. An estimate — a balance is worth what it will be spent on.
    pub valued_minor: Option<i64>,
    /// `"points"` or `"visits"` — the currency the valuation is in.
    pub currency: String,
}

/// The redemption report, computed in the database so a dashboard never loads
/// every member to draw it.
#[derive(Debug, Serialize, ToSchema)]
pub struct LoyaltyAnalytics {
    pub from: chrono::DateTime<chrono::Utc>,
    pub to: chrono::DateTime<chrono::Utc>,
    /// Redemption rows in the range (one per covered order line).
    pub redemptions: i64,
    /// Units handed over as rewards.
    pub redeemed_units: i64,
    /// Balance spent, net of reversals written in the range.
    pub redeemed_points: i64,
    /// Minor units of goods given away as rewards, on sales not voided.
    pub redeemed_value_minor: i64,
    /// Balance earned, net of clawbacks written in the range.
    pub earned_points: i64,
    /// Replayed sales whose rewards the points could not pay for.
    pub refused_redemptions: i64,
    pub top_rewards: Vec<TopReward>,
    pub liability: PointsLiability,
}

#[utoipa::path(get, path = "/loyalty/analytics", tag = "loyalty",
    operation_id = "get_loyalty_analytics", params(AnalyticsQuery),
    responses((status = 200, body = LoyaltyAnalytics), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn analytics(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<AnalyticsQuery>,
) -> Result<HttpResponse, AppError> {
    let (org_id, claims) =
        super::settings::scope_org(pool.get_ref(), &req, query.branch_id).await?;
    check_permission(pool.get_ref(), &claims, "loyalty", "read").await?;
    // Same line as the member list: a till reads a card, not the books.
    crate::authz::require::require(
        pool.get_ref(),
        &claims,
        crate::authz::Cap::LoyaltyMembersList,
        None,
    )
    .await?;
    if let Some(b) = query.branch_id {
        require_branch_access(pool.get_ref(), &claims, b).await?;
    }
    let to = query.to.unwrap_or_else(chrono::Utc::now);
    let from = query.from.unwrap_or(to - chrono::Duration::days(30));
    if from >= to {
        return Err(AppError::BadRequest("`from` must be before `to`".into()));
    }
    let pool = pool.get_ref();

    let (redemptions, redeemed_units, redeemed_points, earned_points): (i64, i64, i64, i64) =
        sqlx::query_as(
            "SELECT \
                COUNT(*) FILTER (WHERE t.kind = 'redeem'), \
                COALESCE(SUM(CASE WHEN t.kind = 'redeem' \
                    THEN COALESCE(NULLIF(oi.reward_units, 0), 1) END), 0)::bigint, \
                COALESCE(-SUM(t.points) FILTER (WHERE t.kind IN ('redeem','reverse_redeem')), 0)::bigint, \
                COALESCE(SUM(t.points) FILTER (WHERE t.kind IN ('earn','reverse_earn')), 0)::bigint \
               FROM loyalty_transactions t \
               LEFT JOIN order_items oi ON oi.id = t.order_item_id \
              WHERE t.org_id = $1 AND ($2::uuid IS NULL OR t.branch_id = $2) \
                AND t.created_at >= $3 AND t.created_at < $4",
        )
        .bind(org_id)
        .bind(query.branch_id)
        .bind(from)
        .bind(to)
        .fetch_one(pool)
        .await?;

    let (redeemed_value_minor, refused_redemptions): (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(SUM(oi.reward_covered), 0)::bigint, \
                COUNT(DISTINCT o.id) FILTER (WHERE o.loyalty_redemption_refused IS NOT NULL) \
           FROM orders o \
           JOIN branches b ON b.id = o.branch_id \
           LEFT JOIN order_items oi ON oi.order_id = o.id AND oi.is_reward \
          WHERE b.org_id = $1 AND ($2::uuid IS NULL OR o.branch_id = $2) \
            AND o.status <> 'voided' AND o.created_at >= $3 AND o.created_at < $4",
    )
    .bind(org_id)
    .bind(query.branch_id)
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await?;

    let top_rewards: Vec<TopReward> = sqlx::query_as(
        "SELECT m.id AS menu_item_id, m.name, \
                COUNT(*) FILTER (WHERE t.kind = 'redeem') AS redemptions, \
                COALESCE(SUM(CASE WHEN t.kind = 'redeem' \
                    THEN COALESCE(NULLIF(oi.reward_units, 0), 1) END), 0)::bigint AS units, \
                COALESCE(-SUM(t.points), 0)::bigint AS points, \
                COALESCE(SUM(oi.reward_covered) FILTER (WHERE t.kind = 'redeem'), 0)::bigint \
                    AS value_minor \
           FROM loyalty_transactions t \
           JOIN loyalty_transactions r ON r.id = COALESCE(t.reverses_id, t.id) \
           JOIN menu_items m ON m.id = r.reward_menu_item_id \
           LEFT JOIN order_items oi ON oi.id = t.order_item_id \
          WHERE t.org_id = $1 AND ($2::uuid IS NULL OR t.branch_id = $2) \
            AND t.kind IN ('redeem','reverse_redeem') \
            AND t.created_at >= $3 AND t.created_at < $4 \
          GROUP BY m.id, m.name \
          ORDER BY redemptions DESC, units DESC, m.name LIMIT 10",
    )
    .bind(org_id)
    .bind(query.branch_id)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;

    let settings = load_effective(pool, org_id, query.branch_id.unwrap_or(Uuid::nil())).await?;
    let currency = settings.mode().as_str().to_string();
    let (members_with_balance, outstanding_points, outstanding_visits): (i64, i64, i64) =
        sqlx::query_as(
            "SELECT COUNT(*) FILTER (WHERE (CASE WHEN $2 = 'visits' THEN visits_balance \
                                             ELSE points_balance END) > 0), \
                    COALESCE(SUM(GREATEST(points_balance, 0)), 0)::bigint, \
                    COALESCE(SUM(GREATEST(visits_balance, 0)), 0)::bigint \
               FROM loyalty_customers WHERE org_id = $1 AND deleted_at IS NULL",
        )
        .bind(org_id)
        .bind(&currency)
        .fetch_one(pool)
        .await?;
    let (value, spent): (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(SUM(oi.reward_covered), 0)::bigint, COALESCE(-SUM(t.points), 0)::bigint \
           FROM loyalty_transactions t JOIN order_items oi ON oi.id = t.order_item_id \
          WHERE t.org_id = $1 AND t.kind = 'redeem' AND t.currency = $2 AND oi.reward_covered > 0",
    )
    .bind(org_id)
    .bind(&currency)
    .fetch_one(pool)
    .await?;
    let value_per_unit_minor = (spent > 0).then(|| value as f64 / spent as f64);
    let outstanding = if currency == "visits" {
        outstanding_visits
    } else {
        outstanding_points
    };
    let valued_minor = value_per_unit_minor.map(|v| (v * outstanding as f64).round() as i64);

    Ok(HttpResponse::Ok().json(LoyaltyAnalytics {
        from,
        to,
        redemptions,
        redeemed_units,
        redeemed_points,
        redeemed_value_minor,
        earned_points,
        refused_redemptions,
        top_rewards,
        liability: PointsLiability {
            members_with_balance,
            outstanding_points,
            outstanding_visits,
            value_per_unit_minor,
            valued_minor,
            currency,
        },
    }))
}

// ── Behavior report (percentages) ───────────────────────────────

/// Behavioral rates over the range — how much of the member base actually
/// uses the programme, not just what it's worth. `total_members` and
/// `members_ever_redeemed` are org-wide and lifetime (a balance/history isn't
/// branch-scoped); every other figure narrows to `branch_id` and the range,
/// same as [`LoyaltyAnalytics`].
#[derive(Debug, Serialize, ToSchema)]
pub struct LoyaltyBehavior {
    pub from: chrono::DateTime<chrono::Utc>,
    pub to: chrono::DateTime<chrono::Utc>,
    /// Enrolled, not deleted, as of now. Org-wide.
    pub total_members: i64,
    /// Distinct (not deleted) members with any loyalty transaction in the
    /// range — deleted members are left out of every count so no rate over
    /// `total_members` can exceed 1.
    pub active_members: i64,
    /// `active_members / total_members`. `0.0` when there are no members.
    pub active_member_rate: f64,
    /// Distinct (not deleted) members who have ever redeemed a reward.
    /// Org-wide, lifetime.
    pub members_ever_redeemed: i64,
    /// `members_ever_redeemed / total_members`.
    pub redemption_rate: f64,
    /// `redeemed_points_period / earned_points_period` — the share of what
    /// was earned in the range that got spent in it. Points earned before the
    /// range and redeemed inside it are not the numerator's earn, so this can
    /// exceed 1.0 on a range with heavy redemption of an older balance.
    pub redemption_ratio: f64,
    /// Members with 2+ earning visits in the range.
    pub repeat_members: i64,
    /// Members with exactly 1 earning visit in the range.
    pub one_time_members: i64,
    /// `repeat_members / (repeat_members + one_time_members)`. `0.0` when
    /// nobody earned in the range.
    pub repeat_visit_rate: f64,
    /// Active members who enrolled during the range.
    pub new_members_active: i64,
    /// Active members who enrolled before the range started.
    pub returning_members_active: i64,
    /// `new_members_active / active_members`.
    pub new_member_share: f64,
}

fn safe_ratio(num: i64, den: i64) -> f64 {
    if den == 0 {
        0.0
    } else {
        num as f64 / den as f64
    }
}

#[utoipa::path(get, path = "/loyalty/behavior", tag = "loyalty",
    operation_id = "get_loyalty_behavior", params(AnalyticsQuery),
    responses((status = 200, body = LoyaltyBehavior), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn behavior(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<AnalyticsQuery>,
) -> Result<HttpResponse, AppError> {
    let (org_id, claims) =
        super::settings::scope_org(pool.get_ref(), &req, query.branch_id).await?;
    // The behaviour report sits with the member list (it was a manager role
    // check before architecture E).
    crate::authz::require::require(
        pool.get_ref(),
        &claims,
        crate::authz::Cap::LoyaltyMembersList,
        query.branch_id,
    )
    .await?;
    // Activity is counted only at the branches the caller works at: a branch
    // manager no longer sees the whole org's behaviour. Membership totals stay
    // programme-wide (a member belongs to the org, not a branch), exactly as a
    // single-branch filter has always reported them.
    let branches =
        crate::authz::scope::org_read_branches(pool.get_ref(), &claims, org_id, query.branch_id)
            .await?;
    let to = query.to.unwrap_or_else(chrono::Utc::now);
    let from = query.from.unwrap_or(to - chrono::Duration::days(30));
    if from >= to {
        return Err(AppError::BadRequest("`from` must be before `to`".into()));
    }
    let pool = pool.get_ref();

    let total_members: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM loyalty_customers WHERE org_id = $1 AND deleted_at IS NULL",
    )
    .bind(org_id)
    .fetch_one(pool)
    .await?;

    let members_ever_redeemed: i64 = sqlx::query_scalar(
        "SELECT COUNT(DISTINCT t.customer_id) FROM loyalty_transactions t \
           JOIN loyalty_customers c ON c.id = t.customer_id AND c.deleted_at IS NULL \
          WHERE t.org_id = $1 AND t.kind = 'redeem'",
    )
    .bind(org_id)
    .fetch_one(pool)
    .await?;

    let (earned_points_period, redeemed_points_period): (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(SUM(points) FILTER (WHERE kind IN ('earn','reverse_earn')), 0)::bigint, \
                COALESCE(-SUM(points) FILTER (WHERE kind IN ('redeem','reverse_redeem')), 0)::bigint \
           FROM loyalty_transactions \
          WHERE org_id = $1 AND ($2::uuid[] IS NULL OR branch_id = ANY($2)) \
            AND created_at >= $3 AND created_at < $4",
    )
    .bind(org_id)
    .bind(&branches)
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await?;

    let (
        active_members,
        repeat_members,
        one_time_members,
        new_members_active,
        returning_members_active,
    ): (i64, i64, i64, i64, i64) = sqlx::query_as(
        "WITH activity AS ( \
            SELECT t.customer_id, c.enrolled_at, \
                   COUNT(*) FILTER (WHERE t.kind = 'earn') AS earn_visits \
              FROM loyalty_transactions t \
              JOIN loyalty_customers c ON c.id = t.customer_id \
             WHERE t.org_id = $1 AND ($2::uuid[] IS NULL OR t.branch_id = ANY($2)) \
               AND c.deleted_at IS NULL \
               AND t.created_at >= $3 AND t.created_at < $4 \
             GROUP BY t.customer_id, c.enrolled_at \
         ) \
         SELECT COUNT(*)::bigint, \
                COUNT(*) FILTER (WHERE earn_visits >= 2)::bigint, \
                COUNT(*) FILTER (WHERE earn_visits = 1)::bigint, \
                COUNT(*) FILTER (WHERE enrolled_at >= $3)::bigint, \
                COUNT(*) FILTER (WHERE enrolled_at < $3)::bigint \
           FROM activity",
    )
    .bind(org_id)
    .bind(&branches)
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await?;

    Ok(HttpResponse::Ok().json(LoyaltyBehavior {
        from,
        to,
        total_members,
        active_members,
        active_member_rate: safe_ratio(active_members, total_members),
        members_ever_redeemed,
        redemption_rate: safe_ratio(members_ever_redeemed, total_members),
        redemption_ratio: safe_ratio(redeemed_points_period, earned_points_period),
        repeat_members,
        one_time_members,
        repeat_visit_rate: safe_ratio(repeat_members, repeat_members + one_time_members),
        new_members_active,
        returning_members_active,
        new_member_share: safe_ratio(new_members_active, active_members),
    }))
}

// ── Campaign effectiveness (win-back + birthday) ──────────────────────────────

/// One outreach campaign's return-on-nudge: did the member earn again within
/// 30 days of the message.
#[derive(Debug, Serialize, ToSchema)]
pub struct CampaignEffectivenessRow {
    /// `"winback"` or `"birthday"`.
    pub campaign: String,
    pub sent: i64,
    pub returned_within_30d: i64,
    /// `returned_within_30d / sent`. `0.0` when nothing was sent.
    pub return_rate: f64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CampaignEffectiveness {
    pub from: chrono::DateTime<chrono::Utc>,
    pub to: chrono::DateTime<chrono::Utc>,
    pub campaigns: Vec<CampaignEffectivenessRow>,
}

#[utoipa::path(get, path = "/loyalty/campaign-effectiveness", tag = "loyalty",
    operation_id = "get_loyalty_campaign_effectiveness", params(AnalyticsQuery),
    responses((status = 200, body = CampaignEffectiveness), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn campaign_effectiveness(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<AnalyticsQuery>,
) -> Result<HttpResponse, AppError> {
    let (org_id, claims) =
        super::settings::scope_org(pool.get_ref(), &req, query.branch_id).await?;
    // Sits with the member list and the behaviour report (architecture E: a
    // capability, never a role name). The branch, when named, must be one the
    // caller may read.
    crate::authz::require::require(
        pool.get_ref(),
        &claims,
        crate::authz::Cap::LoyaltyMembersList,
        query.branch_id,
    )
    .await?;
    crate::authz::scope::org_read_branches(pool.get_ref(), &claims, org_id, query.branch_id)
        .await?;
    let to = query.to.unwrap_or_else(chrono::Utc::now);
    let from = query.from.unwrap_or(to - chrono::Duration::days(30));
    if from >= to {
        return Err(AppError::BadRequest("`from` must be before `to`".into()));
    }
    let pool = pool.get_ref();

    // Loyalty is org-wide (a balance can be spent at any branch), so these two
    // nudge tables are not filtered by `branch_id` even when one is supplied —
    // same note `PointsLiability` makes.
    let (winback_sent, winback_returned): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*)::bigint, \
                COUNT(*) FILTER (WHERE EXISTS ( \
                    SELECT 1 FROM loyalty_transactions lt \
                    WHERE lt.customer_id = w.customer_id AND lt.kind = 'earn' \
                      AND lt.created_at > w.sent_at \
                      AND lt.created_at <= w.sent_at + interval '30 days' \
                ))::bigint \
           FROM loyalty_winbacks w \
           JOIN loyalty_customers c ON c.id = w.customer_id AND c.deleted_at IS NULL \
          WHERE w.org_id = $1 AND w.sent_at >= $2 AND w.sent_at < $3",
    )
    .bind(org_id)
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await?;

    let (birthday_sent, birthday_returned): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*)::bigint, \
                COUNT(*) FILTER (WHERE EXISTS ( \
                    SELECT 1 FROM loyalty_transactions lt \
                    WHERE lt.customer_id = g.customer_id AND lt.kind = 'earn' \
                      AND lt.created_at > g.sent_at \
                      AND lt.created_at <= g.sent_at + interval '30 days' \
                ))::bigint \
           FROM loyalty_birthday_greetings g \
           JOIN loyalty_customers c ON c.id = g.customer_id AND c.deleted_at IS NULL \
          WHERE g.org_id = $1 AND g.sent_at >= $2 AND g.sent_at < $3",
    )
    .bind(org_id)
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await?;

    Ok(HttpResponse::Ok().json(CampaignEffectiveness {
        from,
        to,
        campaigns: vec![
            CampaignEffectivenessRow {
                campaign: "winback".into(),
                sent: winback_sent,
                returned_within_30d: winback_returned,
                return_rate: safe_ratio(winback_returned, winback_sent),
            },
            CampaignEffectivenessRow {
                campaign: "birthday".into(),
                sent: birthday_sent,
                returned_within_30d: birthday_returned,
                return_rate: safe_ratio(birthday_returned, birthday_sent),
            },
        ],
    }))
}

// ── Points-liability trend ──────────────────────────────────────────────────

#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct LiabilityTrendPoint {
    pub week: chrono::DateTime<chrono::Utc>,
    /// Net points/visits change in that week (earn − redeem, reversals
    /// netted in) — not a running balance. See [`LiabilityTrend`].
    pub outstanding: i64,
}

/// A weekly trend of the programme's liability, in the org's live currency
/// (points or visits — never both; see [`PointsLiability`]).
///
/// `loyalty_customers.points_balance`/`visits_balance` are CURRENT balances
/// with no history table, so this is not a snapshot of the outstanding
/// balance at each week — it is each week's *net change* (earned minus
/// redeemed, reversals netted in), read straight off the ledger. Summing
/// `outstanding` across every week since the programme started would
/// reconstruct the current balance; a single week says whether that week
/// grew or shrank the liability.
#[derive(Debug, Serialize, ToSchema)]
pub struct LiabilityTrend {
    pub from: chrono::DateTime<chrono::Utc>,
    pub to: chrono::DateTime<chrono::Utc>,
    pub currency: String,
    pub points: Vec<LiabilityTrendPoint>,
}

#[utoipa::path(get, path = "/loyalty/liability-trend", tag = "loyalty",
    operation_id = "get_loyalty_liability_trend", params(AnalyticsQuery),
    responses((status = 200, body = LiabilityTrend), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn liability_trend(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<AnalyticsQuery>,
) -> Result<HttpResponse, AppError> {
    let (org_id, claims) =
        super::settings::scope_org(pool.get_ref(), &req, query.branch_id).await?;
    // Sits with the member list and the behaviour report (architecture E: a
    // capability, never a role name). The branch, when named, must be one the
    // caller may read.
    crate::authz::require::require(
        pool.get_ref(),
        &claims,
        crate::authz::Cap::LoyaltyMembersList,
        query.branch_id,
    )
    .await?;
    crate::authz::scope::org_read_branches(pool.get_ref(), &claims, org_id, query.branch_id)
        .await?;
    let to = query.to.unwrap_or_else(chrono::Utc::now);
    let from = query.from.unwrap_or(to - chrono::Duration::days(30));
    if from >= to {
        return Err(AppError::BadRequest("`from` must be before `to`".into()));
    }
    let pool = pool.get_ref();

    let settings = load_effective(pool, org_id, query.branch_id.unwrap_or(Uuid::nil())).await?;
    let currency = settings.mode().as_str().to_string();

    // Weeks are cut on the scope's wall clock (owner rule): the branch's zone
    // for a branch-scoped request, the org's for an org-wide one. A redemption
    // at 00:30 Saturday in Cairo belongs to that Saturday's week, not the UTC
    // Friday before. Weeks start Saturday (`tz::WEEK_START`). `week` is the instant that local week starts.
    let tz = crate::tz::scope_tz_name(pool, query.branch_id.unwrap_or(Uuid::nil()), org_id).await?;

    let week = crate::tz::week_start_sql("t.created_at AT TIME ZONE $5");
    let points: Vec<LiabilityTrendPoint> = sqlx::query_as(&format!(
        // Every signed ledger row moves the balance: earns, redemptions, manual
        // and birthday/win-back adjustments, and all their reversals. Leaving the
        // adjustments out would make the weeks no longer sum to the balance.
        // `$5` (the zone) is bound, never interpolated: it is free text on the branch.
        "SELECT {week} AT TIME ZONE $5 AS week, \
                COALESCE(SUM(t.points), 0)::bigint AS outstanding \
           FROM loyalty_transactions t \
           JOIN loyalty_customers c ON c.id = t.customer_id AND c.deleted_at IS NULL \
          WHERE t.org_id = $1 AND t.currency = $2 \
            AND t.created_at >= $3 AND t.created_at < $4 \
          GROUP BY week \
          ORDER BY week",
    ))
    .bind(org_id)
    .bind(&currency)
    .bind(from)
    .bind(to)
    .bind(&tz)
    .fetch_all(pool)
    .await?;

    Ok(HttpResponse::Ok().json(LiabilityTrend {
        from,
        to,
        currency,
        points,
    }))
}
