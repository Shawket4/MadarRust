//! "Order now" from the wallet pass (CUSTOMERS_UNIFICATION_DESIGN.md §4).
//!
//! **The token identifies, the device authorises.** `member_token` is printed
//! as the QR on the card and seen by every cashier, so on its own it shows a
//! first name, a masked phone and a branch name — enough to say "is this you?"
//! and nothing a stranger could use. A `device_token` (the existing HMAC from
//! `/public/otp/verify`, bound to a phone) for the customer's CURRENT phone is
//! what unlocks the prefill, the saved addresses, and any change of identity.
//! After a phone change the old phone's tokens stop matching by construction.
//!
//! Nothing here is stored as a preference: last branch, channel and payment
//! hint are DERIVED from the latest order, and re-validated on every open, so
//! there is no row to go stale.

use actix_web::{HttpResponse, web};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::handlers::{self, CustomerAddress, IdentityActor, ReplaceRefusal};
use crate::auth::jwt::JwtSecret;
use crate::delivery::whatsapp::verify_device_token;
use crate::errors::{AppError, AppErrorResponse};
use crate::loyalty::model::{self, MemberRow};

/// Saved addresses checked (and returned) per open. Each outside address is a
/// road-distance lookup, so the list is bounded.
const MAX_ADDRESSES: usize = 8;
/// Identity replacements a customer may make themself in a rolling 30 days.
const MAX_REPLACEMENTS_30D: i64 = 2;

fn not_found() -> AppError {
    // The same 404 the card endpoints give: unknown, another org's, or an
    // alias that has expired are indistinguishable from outside.
    AppError::NotFound("Card not found".into())
}

/// `•••• 4567` — the last four digits, never more.
pub fn phone_hint(canonical: &str) -> String {
    let digits: Vec<char> = canonical.chars().filter(|c| c.is_ascii_digit()).collect();
    let tail: String = digits[digits.len().saturating_sub(4)..].iter().collect();
    format!("•••• {tail}")
}

fn first_name(name: &str) -> String {
    name.split_whitespace().next().unwrap_or("").to_string()
}

fn device_ok(secret: &str, phone: &str, token: Option<&str>) -> bool {
    matches!(token, Some(t) if !t.is_empty() && !phone.is_empty()
        && verify_device_token(secret, phone, t))
}

// ── the context ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct OrderNowQuery {
    /// From `/public/otp/verify`, for the customer's current phone. Absent or
    /// not valid for that phone → the masked context.
    #[serde(default)]
    pub device_token: Option<String>,
}

/// The branch the customer last ordered from, re-checked now.
#[derive(Debug, Serialize, ToSchema)]
pub struct OrderNowBranch {
    pub id: Uuid,
    pub name: String,
    /// The channel they last used there.
    pub channel: String,
    /// True when it cannot be used as-is right now; the client falls back to
    /// its branch/channel chooser and keeps the rest of the prefill.
    pub stale: bool,
    /// `branch_unavailable` | `channel_closed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_reason: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OrderNowAddress {
    #[serde(flatten)]
    pub address: CustomerAddress,
    /// True when it can no longer be delivered to from the last branch.
    pub stale: bool,
    /// `out_of_zone` | `zone_unavailable` | `branch_unavailable`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_reason: Option<String>,
}

/// Everything a verified device gets. Absent from the masked context.
#[derive(Debug, Serialize, ToSchema)]
pub struct OrderNowFull {
    pub customer_id: Uuid,
    pub name: String,
    /// Canonical (`2010…`).
    pub phone: String,
    pub locale: String,
    /// Derived from the latest order; `None` before the first one.
    pub last_branch: Option<OrderNowBranch>,
    /// `cash` | `card`, as they last said.
    pub last_payment_hint: Option<String>,
    /// Most recently used first.
    pub addresses: Vec<OrderNowAddress>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OrderNowContext {
    /// True → this is the masked context; verify the phone
    /// (`/public/otp/request|verify`) and ask again with the device token.
    pub verify_required: bool,
    pub org_id: Uuid,
    pub org_name: String,
    pub logo_url: Option<String>,
    /// First word of the name on file.
    pub first_name: String,
    /// `•••• 4567`.
    pub phone_hint: String,
    /// NAME only on the masked path; the full context carries the id.
    pub last_branch_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full: Option<OrderNowFull>,
}

async fn member_by_token(pool: &PgPool, token: &str) -> Result<MemberRow, AppError> {
    model::find_by_token(pool, token)
        .await?
        .ok_or_else(not_found)
}

#[derive(sqlx::FromRow)]
struct LastOrder {
    branch_id: Uuid,
    branch_name: String,
    branch_live: bool,
    channel: String,
    payment_method_hint: Option<String>,
}

async fn last_order(
    pool: &PgPool,
    org: Uuid,
    customer: Uuid,
) -> Result<Option<LastOrder>, AppError> {
    Ok(sqlx::query_as(
        "SELECT d.branch_id, b.name AS branch_name,
                (b.is_active AND b.deleted_at IS NULL) AS branch_live,
                d.channel::text AS channel, d.payment_method_hint
           FROM delivery_orders d JOIN branches b ON b.id = d.branch_id
          WHERE d.customer_id = $1 AND d.org_id = $2 AND d.status::text <> 'rejected'
          ORDER BY d.created_at DESC LIMIT 1",
    )
    .bind(customer)
    .bind(org)
    .fetch_optional(pool)
    .await?)
}

#[utoipa::path(get, path = "/public/order-now/{token}", tag = "order-now",
    operation_id = "order_now_context",
    params(("token" = String, Path, description = "Member token (the card's QR)"), OrderNowQuery),
    responses((status = 200, description = "Masked without a valid device token for the customer's current phone; full with one", body = OrderNowContext), AppErrorResponse))]
pub async fn context(
    pool: web::Data<PgPool>,
    secret: web::Data<JwtSecret>,
    token: web::Path<String>,
    query: web::Query<OrderNowQuery>,
) -> Result<HttpResponse, AppError> {
    let pool = pool.get_ref();
    let member = member_by_token(pool, token.as_str()).await?;
    let brand = crate::orgs::branding::load(pool, member.org_id).await?;
    let last = last_order(pool, member.org_id, member.id).await?;
    let verified = device_ok(&secret.0, &member.phone, query.device_token.as_deref());

    let mut out = OrderNowContext {
        verify_required: !verified,
        org_id: member.org_id,
        org_name: brand.name.clone(),
        logo_url: brand.logo_url.clone(),
        first_name: first_name(&member.name),
        phone_hint: phone_hint(&member.phone),
        last_branch_name: last.as_ref().map(|l| l.branch_name.clone()),
        full: None,
    };
    if !verified {
        return Ok(HttpResponse::Ok().json(out));
    }

    // Re-validated NOW, never trusted (design §4.2). Stale is reported, not
    // dropped: the client keeps the rest and opens its chooser for that step.
    let last_branch = match &last {
        Some(l) => {
            let reason = if !l.branch_live {
                Some("branch_unavailable")
            } else if !crate::delivery::public::channel_open_now(pool, l.branch_id, &l.channel)
                .await?
            {
                Some("channel_closed")
            } else {
                None
            };
            Some(OrderNowBranch {
                id: l.branch_id,
                name: l.branch_name.clone(),
                channel: l.channel.clone(),
                stale: reason.is_some(),
                stale_reason: reason.map(str::to_string),
            })
        }
        None => None,
    };

    let mut conn = pool.acquire().await?;
    let saved = handlers::addresses_of(&mut conn, member.org_id, member.id).await?;
    drop(conn);
    let mut addresses = Vec::new();
    for a in saved.into_iter().take(MAX_ADDRESSES) {
        let reason = address_stale_reason(pool, &a, last.as_ref()).await?;
        addresses.push(OrderNowAddress {
            address: a,
            stale: reason.is_some(),
            stale_reason: reason.map(str::to_string),
        });
    }

    out.full = Some(OrderNowFull {
        customer_id: member.id,
        name: member.name.clone(),
        phone: member.phone.clone(),
        locale: member.locale.clone(),
        last_payment_hint: last.as_ref().and_then(|l| l.payment_method_hint.clone()),
        last_branch,
        addresses,
    });
    Ok(HttpResponse::Ok().json(out))
}

/// Can this address still be ordered to? An outside address is matched against
/// the live zone rings of the branch it would be ordered from (the last branch,
/// else the one it was last used with) by the same function order create uses.
/// Any other address belongs to its branch, which must still be there.
async fn address_stale_reason(
    pool: &PgPool,
    a: &CustomerAddress,
    last: Option<&LastOrder>,
) -> Result<Option<&'static str>, AppError> {
    use crate::delivery::public::{FeeOutcome, compute_outside_fee};
    let outside = a.channel == crate::delivery::CHANNEL_OUTSIDE;
    let branch = if outside {
        last.map(|l| l.branch_id).or(a.branch_id)
    } else {
        a.branch_id
    };
    let Some(branch) = branch else {
        return Ok(Some("branch_unavailable"));
    };
    let live: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM branches WHERE id = $1 AND is_active AND deleted_at IS NULL)",
    )
    .bind(branch)
    .fetch_one(pool)
    .await?;
    if !live {
        return Ok(Some("branch_unavailable"));
    }
    if !outside {
        return Ok(None);
    }
    let (Some(lat), Some(lng)) = (a.lat, a.lng) else {
        return Ok(Some("zone_unavailable"));
    };
    Ok(
        match compute_outside_fee(pool, branch, crate::geo::osrm::LatLng { lat, lng }).await? {
            FeeOutcome::Ok { .. } => None,
            FeeOutcome::OutOfRange => Some("out_of_zone"),
            FeeOutcome::Unavailable => Some("zone_unavailable"),
        },
    )
}

// ── identity classification at order create (§4.4) ──────────────────────────

/// What the server decided about the name/phone typed on an order placed from
/// a card.
#[derive(Debug, PartialEq, Eq)]
pub struct Classified {
    /// The snapshot phone is not the customer's own ("ordered by X for Y").
    pub contact_override: bool,
    /// Save the address to the customer's profile.
    pub save_address: bool,
    /// `update_name`: the stored name becomes this.
    pub rename: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ClassifyRefusal {
    /// The phone differs and the customer has not said what that means.
    /// 409 `IDENTITY_CHOICE_REQUIRED`, `{kind: "phone"}`.
    ChoiceRequired,
    /// `identity_change` is not one of the two values.
    UnknownChoice,
}

/// Pure. `customer_key` / `typed_key` are canonical phones.
pub fn classify(
    customer_name: &str,
    customer_key: &str,
    typed_name: &str,
    typed_key: &str,
    identity_change: Option<&str>,
    save_address: Option<bool>,
) -> Result<Classified, ClassifyRefusal> {
    let phone_differs = customer_key != typed_key;
    let name_differs = !handlers::same_name(customer_name, typed_name);
    let choice = match identity_change {
        None => None,
        Some("one_time") => Some(false),
        Some("update_name") => Some(true),
        Some(_) => return Err(ClassifyRefusal::UnknownChoice),
    };
    match (phone_differs, choice) {
        // A different number is never guessed at. `update_name` does not answer
        // it either: replacing a phone is its own, doubly-verified act.
        (true, None) | (true, Some(true)) => Err(ClassifyRefusal::ChoiceRequired),
        (true, Some(false)) => Ok(Classified {
            contact_override: true,
            save_address: save_address == Some(true),
            rename: None,
        }),
        (false, Some(true)) => Ok(Classified {
            contact_override: false,
            save_address: save_address != Some(false),
            rename: name_differs.then(|| typed_name.trim().to_string()),
        }),
        // Name only, said or unsaid: just this order. The snapshot carries the
        // typed name; the address is theirs to keep only if they ask.
        (false, _) if name_differs => Ok(Classified {
            contact_override: false,
            save_address: save_address == Some(true),
            rename: None,
        }),
        // Nothing differs: an ordinary order.
        (false, _) => Ok(Classified {
            contact_override: false,
            save_address: save_address != Some(false),
            rename: None,
        }),
    }
}

/// The JSON body of the two identity 409s, which carry one field more than
/// `AppError` has room for.
pub fn conflict(code: &str, reason: &str, extra: serde_json::Value) -> HttpResponse {
    let mut body = serde_json::json!({ "error": reason, "code": code });
    if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
        b.extend(e.clone());
    }
    HttpResponse::Conflict().json(body)
}

// ── replace identity / combine ──────────────────────────────────────────────

#[derive(Debug, Deserialize, ToSchema)]
pub struct ReplaceIdentityRequest {
    /// Proof of the CURRENT phone.
    pub device_token: String,
    pub new_phone: String,
    /// Proof of the NEW phone (the client runs `/public/otp/request|verify` on it).
    pub new_phone_device_token: String,
    /// Also correct the name.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ReplaceIdentityResponse {
    pub customer_id: Uuid,
    /// The new number, masked. The client already holds its device token.
    pub phone_hint: String,
    pub first_name: String,
    /// True when two customers were combined into this one.
    pub combined: bool,
}

/// Both proofs, or nothing changes. Returns the member and the new canonical
/// phone.
async fn doubly_verified(
    pool: &PgPool,
    secret: &str,
    token: &str,
    body: &ReplaceIdentityRequest,
) -> Result<(MemberRow, String), AppError> {
    let member = member_by_token(pool, token).await?;
    let new_key = crate::phone::normalize_phone(&body.new_phone)?;
    if !device_ok(secret, &member.phone, Some(&body.device_token)) {
        return Err(AppError::Unauthorized(
            "Phone not verified on this device.".into(),
        ));
    }
    if !device_ok(secret, &new_key, Some(&body.new_phone_device_token)) {
        return Err(AppError::Unauthorized(
            "The new phone is not verified.".into(),
        ));
    }
    Ok((member, new_key))
}

/// Max 2 self-service replacements per rolling 30 days, and none for 24 h
/// after a merge (design §4.4). Counted from the trail itself.
async fn check_limits(
    conn: &mut sqlx::PgConnection,
    org: Uuid,
    customer: Uuid,
) -> Result<(), AppError> {
    let merged_recently: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM customers WHERE org_id = $1 AND merged_into = $2
                          AND merged_at > now() - interval '24 hours')",
    )
    .bind(org)
    .bind(customer)
    .fetch_one(&mut *conn)
    .await?;
    if merged_recently {
        return Err(AppError::Coded {
            status: 409,
            code: "IDENTITY_LOCKED_AFTER_MERGE",
            reason: "This profile was combined with another in the last 24 hours. Please try again tomorrow.".into(),
        });
    }
    let recent: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM customer_phone_history
          WHERE org_id = $1 AND customer_id = $2 AND reason = 'self'
            AND replaced_at > now() - interval '30 days'",
    )
    .bind(org)
    .bind(customer)
    .fetch_one(&mut *conn)
    .await?;
    if recent >= MAX_REPLACEMENTS_30D {
        return Err(AppError::Coded {
            status: 429,
            code: "IDENTITY_REPLACE_LIMIT",
            reason: "The phone number on this profile was changed twice in the last 30 days. Please ask the shop.".into(),
        });
    }
    Ok(())
}

#[utoipa::path(post, path = "/public/order-now/{token}/replace-identity", tag = "order-now",
    operation_id = "order_now_replace_identity", request_body = ReplaceIdentityRequest,
    params(("token" = String, Path, description = "Member token")),
    responses((status = 200, body = ReplaceIdentityResponse),
              (status = 409, description = "`PHONE_BELONGS_TO_ANOTHER` (`can_combine: true`) or `IDENTITY_LOCKED_AFTER_MERGE`"),
              (status = 429, description = "`IDENTITY_REPLACE_LIMIT`"),
              AppErrorResponse))]
pub async fn replace_identity(
    pool: web::Data<PgPool>,
    secret: web::Data<JwtSecret>,
    token: web::Path<String>,
    body: web::Json<ReplaceIdentityRequest>,
) -> Result<HttpResponse, AppError> {
    let pool = pool.get_ref();
    let (member, _new_key) = doubly_verified(pool, &secret.0, token.as_str(), &body).await?;
    let mut tx = pool.begin().await?;
    check_limits(&mut tx, member.org_id, member.id).await?;
    match handlers::replace_phone(
        &mut tx,
        member.org_id,
        member.id,
        &body.new_phone,
        IdentityActor::CustomerSelf,
    )
    .await?
    {
        Ok(()) => {}
        Err(ReplaceRefusal::BelongsTo(_)) => {
            // They have just proven control of both numbers: offer to combine.
            return Ok(conflict(
                "PHONE_BELONGS_TO_ANOTHER",
                "That number already belongs to another profile.",
                serde_json::json!({ "can_combine": true }),
            ));
        }
    }
    if let Some(name) = body.name.as_deref().filter(|n| !n.trim().is_empty()) {
        handlers::rename(
            &mut tx,
            member.org_id,
            member.id,
            name,
            IdentityActor::CustomerSelf,
        )
        .await?;
    }
    tx.commit().await?;
    handlers::after_identity_change(pool, member.id).await;
    respond(pool, member.id, false).await
}

#[utoipa::path(post, path = "/public/order-now/{token}/combine", tag = "order-now",
    operation_id = "order_now_combine", request_body = ReplaceIdentityRequest,
    params(("token" = String, Path, description = "Member token — this customer survives")),
    responses((status = 200, body = ReplaceIdentityResponse),
              (status = 409, description = "`NOTHING_TO_COMBINE` or `IDENTITY_LOCKED_AFTER_MERGE`"),
              AppErrorResponse))]
pub async fn combine(
    pool: web::Data<PgPool>,
    secret: web::Data<JwtSecret>,
    token: web::Path<String>,
    body: web::Json<ReplaceIdentityRequest>,
) -> Result<HttpResponse, AppError> {
    let pool = pool.get_ref();
    let (member, new_key) = doubly_verified(pool, &secret.0, token.as_str(), &body).await?;
    let mut tx = pool.begin().await?;
    check_limits(&mut tx, member.org_id, member.id).await?;
    let other = handlers::live_with_phone(&mut tx, member.org_id, &new_key)
        .await?
        .filter(|o| *o != member.id)
        .ok_or(AppError::Coded {
            status: 409,
            code: "NOTHING_TO_COMBINE",
            reason: "No other profile holds that number.".into(),
        })?;
    // The standard merge, the PASS HOLDER surviving (design §2.7, §4.4) — with
    // the both-members rule when the other profile has a card of its own.
    let retired = handlers::merge_inner(&mut tx, member.org_id, other, member.id, None).await?;
    handlers::audit_identity(
        &mut tx,
        member.org_id,
        member.id,
        "combine",
        IdentityActor::CustomerSelf,
        Some(&other.to_string()),
        Some(&member.id.to_string()),
    )
    .await?;
    // "This is my new number": the merge freed it, so it becomes THE number.
    if let Err(ReplaceRefusal::BelongsTo(_)) = handlers::replace_phone(
        &mut tx,
        member.org_id,
        member.id,
        &body.new_phone,
        IdentityActor::CustomerSelf,
    )
    .await?
    {
        return Err(AppError::Conflict("Please try again".into()));
    }
    // The merge filed that number as one they USED to have; it is current now.
    sqlx::query("DELETE FROM customer_phone_history WHERE customer_id = $1 AND phone_key = $2")
        .bind(member.id)
        .bind(&new_key)
        .execute(&mut *tx)
        .await?;
    if let Some(name) = body.name.as_deref().filter(|n| !n.trim().is_empty()) {
        handlers::rename(
            &mut tx,
            member.org_id,
            member.id,
            name,
            IdentityActor::CustomerSelf,
        )
        .await?;
    }
    tx.commit().await?;
    if let Some(loser) = retired {
        handlers::after_merge(pool, member.id, loser);
    }
    handlers::after_identity_change(pool, member.id).await;
    respond(pool, member.id, true).await
}

async fn respond(pool: &PgPool, customer: Uuid, combined: bool) -> Result<HttpResponse, AppError> {
    let fresh = model::find_by_id(pool, customer)
        .await?
        .ok_or_else(not_found)?;
    Ok(HttpResponse::Ok().json(ReplaceIdentityResponse {
        customer_id: fresh.id,
        phone_hint: phone_hint(&fresh.phone),
        first_name: first_name(&fresh.name),
        combined,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "201001234567";
    const OTHER: &str = "201112345678";

    #[test]
    fn the_hint_shows_four_digits_and_no_more() {
        assert_eq!(phone_hint(ME), "•••• 4567");
        assert_eq!(phone_hint("12"), "•••• 12");
        assert_eq!(first_name("  Omar  Hassan "), "Omar");
    }

    #[test]
    fn a_different_phone_is_never_guessed_at() {
        assert_eq!(
            classify("Omar", ME, "Omar", OTHER, None, None),
            Err(ClassifyRefusal::ChoiceRequired)
        );
        assert_eq!(
            classify("Omar", ME, "Omar", OTHER, Some("update_name"), None),
            Err(ClassifyRefusal::ChoiceRequired)
        );
        let c = classify("Omar", ME, "Sara", OTHER, Some("one_time"), None).unwrap();
        assert!(c.contact_override && !c.save_address && c.rename.is_none());
        let c = classify("Omar", ME, "Sara", OTHER, Some("one_time"), Some(true)).unwrap();
        assert!(c.save_address);
    }

    #[test]
    fn a_name_alone_is_one_time_unless_they_say_otherwise() {
        let c = classify("Omar", ME, "Omar Hassan", ME, None, None).unwrap();
        assert_eq!(
            c,
            Classified {
                contact_override: false,
                save_address: false,
                rename: None
            }
        );
        let c = classify("Omar", ME, " Omar Hassan ", ME, Some("update_name"), None).unwrap();
        assert_eq!(c.rename.as_deref(), Some("Omar Hassan"));
        assert!(c.save_address);
        // Case, spacing and composition are not an edit.
        let c = classify("Omar  Hassan", ME, "omar hassan", ME, None, None).unwrap();
        assert_eq!(
            c,
            Classified {
                contact_override: false,
                save_address: true,
                rename: None
            }
        );
        assert!(handlers::same_name("Cafe\u{301}", "CAFÉ"));
        assert_eq!(
            classify("a", ME, "a", ME, Some("whatever"), None),
            Err(ClassifyRefusal::UnknownChoice)
        );
    }
}
