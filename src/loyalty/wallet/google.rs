//! Google Wallet loyalty objects.
//!
//! Two things happen here:
//!   * **Save link** — a JWT signed with the issuer's service-account key,
//!     handed to the customer as `https://pay.google.com/gp/v/save/<jwt>`. It
//!     carries the whole loyalty object, so a member can be saved without the
//!     object having been created through the API first.
//!   * **Balance push** — a PATCH to the Wallet Objects API when points move.
//!     Google needs no device registry (that is Apple's model); the object is
//!     the record and every device holding it follows.
//!
//! Configured by `LOYALTY_GOOGLE_ISSUER_ID` and `LOYALTY_GOOGLE_SA_KEY` (the
//! service account's PEM private key) plus `LOYALTY_GOOGLE_SA_EMAIL`. Unset
//! means no Google button and no push — never an error at signup.

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::Serialize;
use serde_json::json;
use sqlx::PgPool;

use crate::errors::AppError;
use crate::loyalty::model::MemberRow;
use crate::loyalty::settings::LoyaltySettings;
use crate::orgs::branding::OrgBrand;

const SAVE_URL_PREFIX: &str = "https://pay.google.com/gp/v/save/";
const WALLET_API: &str = "https://walletobjects.googleapis.com/walletobjects/v1";

fn issuer_id() -> Option<String> {
    std::env::var("LOYALTY_GOOGLE_ISSUER_ID")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// A Wallet issuer id is NUMERIC.
///
/// The Google Pay & Wallet Console shows a MERCHANT id too — `BCR2DN6…` — on a
/// neighbouring page, and the two are not interchangeable. Paste the merchant
/// id here and Google answers "Invalid resource ID:
/// BCR2DN6DVK7MJ3IV.madar-685f…", naming the resource rather than the setting,
/// which is a long way from "you copied the wrong number". Every card in the
/// estate silently loses its Google button until someone works that out.
fn issuer_format_problem(issuer: &str) -> Option<String> {
    if issuer.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(format!(
        "LOYALTY_GOOGLE_ISSUER_ID is \"{issuer}\", which is not a Wallet issuer id — \
         those are numeric, like 3388000000022345678. A value starting \"BCR2DN6\" is \
         the Google Pay MERCHANT id, which the same console shows on another page. \
         Copy the Issuer ID from the Google Wallet API page of the Google Pay & \
         Wallet Console."
    ))
}

fn sa_email() -> Option<String> {
    std::env::var("LOYALTY_GOOGLE_SA_EMAIL")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// Accepts `LOYALTY_GOOGLE_SA_KEY_FILE` (a path) or `LOYALTY_GOOGLE_SA_KEY`
/// (inline PEM). Prefer the file: see [`super::key_material`].
fn sa_key() -> Option<Vec<u8>> {
    super::key_material("LOYALTY_GOOGLE_SA_KEY")
}

pub fn is_configured() -> bool {
    missing_env().is_empty()
}

/// Which settings Google still needs, by name.
///
/// Reported rather than merely counted: "no Add to Google Wallet button"
/// currently looks identical whether a variable is unset, a key file is
/// unreadable, or Google refused the service account — and every one of those
/// was a real afternoon.
pub fn missing_env() -> Vec<String> {
    let mut out = Vec::new();
    if issuer_id().is_none() {
        out.push("LOYALTY_GOOGLE_ISSUER_ID".into());
    }
    if sa_email().is_none() {
        out.push("LOYALTY_GOOGLE_SA_EMAIL".into());
    }
    if sa_key().is_none() {
        out.push("LOYALTY_GOOGLE_SA_KEY (or LOYALTY_GOOGLE_SA_KEY_FILE)".into());
    }
    out
}

/// Ask Google whether this issuer will actually answer for us.
///
/// Two questions, in the order they fail: can the service account get a token
/// at all, and will Google let it read this org's class? Anything else that
/// goes wrong at save time is downstream of these two.
pub async fn check(org_id: uuid::Uuid) -> Result<String, String> {
    let Some(issuer) = issuer_id() else {
        return Err("LOYALTY_GOOGLE_ISSUER_ID is not set".into());
    };
    // Answerable without asking Google, and a far more useful answer than the
    // one Google gives for it.
    if let Some(problem) = issuer_format_problem(&issuer) {
        return Err(problem);
    }
    let token = access_token()
        .await
        .map_err(|e| format!("The service account could not get a token from Google. {e}"))?;
    let id = class_id(&issuer, org_id);
    let resp = reqwest::Client::new()
        .get(format!("{WALLET_API}/loyaltyClass/{id}"))
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| format!("Could not reach Google: {e}"))?;
    match resp.status() {
        s if s.is_success() => Ok(format!("Ready. This shop's card class ({id}) exists.")),
        // Nothing wrong: the class is made when the first customer saves a card.
        reqwest::StatusCode::NOT_FOUND => Ok(format!(
            "Ready. No card class yet ({id}) — it is created when the first \
             customer saves their card."
        )),
        s => {
            let body = resp.text().await.unwrap_or_default();
            Err(format!(
                "Google refused this service account ({s}). Check that it is \
                 granted access to issuer {issuer} in the Google Wallet \
                 console. {}",
                first_reason(&body)
            ))
        }
    }
}

/// Google's own reason, out of the error envelope it wraps everything in.
fn first_reason(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| body.chars().take(300).collect())
}

/// The Wallet object id for a member. Google requires `<issuer>.<suffix>` with
/// the suffix restricted to alphanumerics, `.`, `_` and `-`; a UUID qualifies.
pub fn object_id(issuer: &str, member: &MemberRow) -> String {
    format!("{issuer}.{}", member.id)
}

/// The class every member of one org shares — this is what carries the tenant's
/// branding, so a class exists per org rather than per program.
pub fn class_id(issuer: &str, org_id: uuid::Uuid) -> String {
    format!("{issuer}.madar-{org_id}")
}

#[derive(Serialize)]
struct SaveClaims {
    iss: String,
    aud: &'static str,
    typ: &'static str,
    iat: usize,
    payload: serde_json::Value,
}

/// The loyalty CLASS: the programme itself, as opposed to one member's card.
///
/// Google will not accept an object whose class does not exist, so the class
/// rides in the same "save" JWT and is created on first use. That is the whole
/// reason this exists — without it every save fails with a class-not-found that
/// the customer sees only as a dead link.
///
/// One class per ORG, not per programme: it carries the tenant's identity, and
/// a customer looking at their wallet should see the shop's name on the card.
/// Where Madar's own mark is served, for a shop that has not uploaded one.
/// Google REQUIRES a class to carry a logo.
pub const MADAR_LOGO_PATH: &str = "/public/loyalty/brand/logo.png";

/// The brand comes from the ORGANISATION, like Apple's. It used to be read from
/// `loyalty_settings`, whose UI was removed when branding moved to the org — so
/// the class carried no logo and no colour, and every Android card came back in
/// Google's default white.
pub fn loyalty_class(
    issuer: &str,
    org_id: uuid::Uuid,
    brand: &OrgBrand,
    settings: &LoyaltySettings,
) -> serde_json::Value {
    let mut class = json!({
        "id": class_id(issuer, org_id),
        // Whose card this is. Always set, even when nothing else has been
        // configured — falling back to the programme name rather than leaving a
        // wallet entry with no owner on it.
        "issuerName": if brand.name.trim().is_empty() { settings.program_name.as_str() } else { brand.name.as_str() },
        "programName": settings.program_name,
        // `UNDER_REVIEW` is what a class inserted through a save JWT must carry;
        // Google promotes it when the issuer account is approved. `APPROVED`
        // here is rejected outright.
        "reviewStatus": "UNDER_REVIEW",
        // Google FETCHES this, so it must be absolute and publicly reachable —
        // unlike Apple's, which is packed into the archive as bytes.
        "hexBackgroundColor": brand.palette.background,
    });
    // REQUIRED by Google, and the cause of "Something went wrong" on a save
    // that still routed to the app: a loyalty class without a `programLogo` is
    // rejected, and a shop with no logo produced exactly that.
    //
    // Composed for Google's slot rather than handed the raw upload. Google
    // masks this to a CIRCLE, so a wide wordmark loses its ends and a
    // transparent mark gets whatever backing Google chooses — which is how a
    // shop's logo came to read as a pale sticker on its own card. Madar's own
    // mark stands in for a shop that has none, so a class is always valid.
    let logo = brand
        .logo_url
        .as_deref()
        .map(|u| {
            format!(
                "/public/loyalty/brand/{}/logo/{}.png",
                org_id,
                crate::orgs::branding::asset_key(u)
            )
        })
        .unwrap_or_else(|| MADAR_LOGO_PATH.to_string());
    if let Some(uri) = super::absolute_api_url(&logo) {
        class["programLogo"] = json!({ "sourceUri": { "uri": uri } });
    }
    class
}

/// The loyalty object as Google models it.
///
/// `loyaltyPoints` is how far along, in the slot Google renders largest, and
/// `accountId` is the member token — so the barcode and the account agree, and
/// a scan resolves the same member whichever wallet produced it.
pub fn loyalty_object(
    issuer: &str,
    member: &MemberRow,
    settings: &LoyaltySettings,
    locations: &[super::PassLocation],
    rewards: &[String],
    headline: &str,
) -> serde_json::Value {
    let mode = settings.mode();
    let balance = member.balance_in(mode);
    json!({
        "id": object_id(issuer, member),
        "classId": class_id(issuer, member.org_id),
        "state": "ACTIVE",
        "accountId": member.member_token,
        "accountName": member.name,
        // Both of these render ON THE CARD. `textModulesData` does not — it
        // renders in the details list BELOW it, which is where the progress
        // used to sit: a customer opening their wallet saw a balance and had to
        // scroll past the card to find out how close they were.
        "loyaltyPoints": {
            "label": balance_label(mode),
            "balance": { "string": progress_line(balance, settings.default_reward_cost) }
        },
        // What they are working towards, not how far along they are — the
        // figures are already in the slot above. "Get a free drink" is the
        // thing a customer opens the card to be reminded of.
        "secondaryLoyaltyPoints": {
            "label": "Reward",
            "balance": { "string": headline }
        },
        "barcode": {
            "type": "QR_CODE",
            "value": member.member_token,
            "alternateText": member.name
        },
        // Geofences the card to the shop's branches, so it surfaces on the
        // phone when the customer is there — Google's counterpart to Apple's
        // lock-screen `locations`. Only branches whose coordinates an admin has
        // actually set; a branch without them simply does not surface.
        "locations": locations
            .iter()
            .map(|l| json!({
                "kind": "walletobjects#latLongPoint",
                "latitude": l.latitude,
                "longitude": l.longitude,
            }))
            .collect::<Vec<_>>(),
        // What Apple puts on the BACK of the card. Google shows these under it
        // rather than behind it, which is the same content in the same order —
        // built by `wallet::back_of_card`, once, so the two cannot drift into
        // telling a customer different things about one programme.
        // Google renders `accountName` and `accountId` as rows of its own, so
        // the shared "Member" line would appear a third time — and it carries a
        // phone number, which is the last thing to print twice. Apple keeps it:
        // it has no automatic equivalent.
        "textModulesData": super::back_of_card(member, settings, locations, rewards)
            .into_iter()
            .filter(|l| l.key != "member")
            .map(|l| json!({ "id": l.key, "header": l.label, "body": l.value }))
            .collect::<Vec<_>>(),
    })
}

/// The shop's photograph, as Google's banner — Apple's strip, by another name.
///
/// Google FETCHES this, so it needs an absolute URL and the file has to be
/// publicly reachable. It is applied AFTER the object exists rather than inside
/// it: Google validates an image when it accepts a resource, and an image it
/// dislikes would fail the whole insert — which is a decoration taking down the
/// card it decorates. That already happened once.
pub fn hero_image(org_id: uuid::Uuid, brand: &OrgBrand) -> Option<serde_json::Value> {
    let url = brand.card_image_url.as_deref()?;
    let uri = super::absolute_api_url(&format!(
        "/public/loyalty/brand/{}/banner/{}.png",
        org_id,
        crate::orgs::branding::asset_key(url)
    ))?;
    Some(json!({ "sourceUri": { "uri": uri } }))
}

/// The stepper, in text, at whatever density the field can hold.
///
/// A pass field is one line that iOS SHRINKS to fit its width, so the only way
/// to stay legible is to stay short. The row therefore thins out in two stages
/// rather than being drawn one way until it stops working:
///
///   * up to [`CONNECTED_UP_TO`] — `●─●─●─○─○`, joined, so it reads as a
///     journey and not a handful of dots;
///   * up to [`MAX_STEPS`] — `●●●○○○○○○`, unjoined. The connectors are what
///     make it long (they nearly double the character count), and they are the
///     part worth losing first: the order still reads left to right without
///     them.
///
/// Past that it is not drawn at all and [`progress_line`] shows the figures
/// alone. Twelve dots is the point where counting them stops being quicker
/// than reading "9 / 12", and a hundred is not a stepper at any density.
const MAX_STEPS: i32 = 12;

/// Past this many, the joins cost more width than they earn.
const CONNECTED_UP_TO: i32 = 6;

pub fn stepper(balance: i32, threshold: i32) -> Option<String> {
    if threshold <= 0 || threshold > MAX_STEPS {
        return None;
    }
    let filled = balance.clamp(0, threshold);
    let step = |i: i32| if i < filled { "●" } else { "○" };
    let mut out = String::from(step(0));
    for i in 1..threshold {
        if threshold <= CONNECTED_UP_TO {
            out.push('\u{2500}');
        }
        out.push_str(step(i));
    }
    Some(out)
}

/// "3 / 5" beside the steps, or on its own once there are too many to draw.
///
/// The figures are never dropped. They are the fact; the dots are the glance,
/// and a font that substitutes a glyph must still leave a readable card.
pub fn progress_line(balance: i32, threshold: i32) -> String {
    if threshold > 0 && balance >= threshold {
        // Short, because this shares a row with the reward's name. The full
        // stepper is filled anyway, which says the same thing in pictures.
        return "Reward earned".to_string();
    }
    match stepper(balance, threshold) {
        // The steps ALONE. Printing "3 / 5" beside three filled circles and two
        // empty ones says the same thing twice, in a field whose width is the
        // scarce thing — and the doubling is most of what made the line look
        // cramped.
        Some(steps) => steps,
        // Only where there are no steps to show: past the countable cap the
        // figures are all there is, and they are enough.
        None => format!("{balance} / {threshold}"),
    }
}

/// What this program calls what it collects, for the pass's field label.
pub fn balance_label(mode: crate::loyalty::earn::Mode) -> &'static str {
    match mode {
        crate::loyalty::earn::Mode::Points => "Points",
        // "Orders", not "Visits": the customer counts the things they bought,
        // and that is the word the counter uses back to them.
        crate::loyalty::earn::Mode::Visits => "Orders",
    }
}

/// Google's cap on a JWT carried in a save URL.
///
/// Past this the save page fails — "Something went wrong", while still opening
/// the Wallet app, which is a remarkably hard symptom to attribute. A class and
/// an object embedded together measured about 2,150 characters for a shop with
/// six branches, so the link was over the limit from the day it was written and
/// grew with every branch added.
const MAX_SAVE_JWT: usize = 1800;

/// Create the class and the member's object, and return a link that REFERENCES
/// the object rather than carrying it.
///
/// This is the shape Google documents for production, and it is the only shape
/// that fits: a reference-only JWT is a few hundred characters whatever the
/// shop looks like, so branches, a long name and a logo URL can no longer push
/// a customer's save link over the limit.
///
/// The writes are idempotent and happen ONCE per member — the object id is kept
/// on the row, so an existing member's link costs no Google calls at all. A
/// failure here is reported and swallowed by the caller: a wallet that will not
/// provision must not take a signup down with it.
pub async fn save_url(
    pool: &PgPool,
    member: &MemberRow,
    settings: &LoyaltySettings,
    brand: &OrgBrand,
    locations: &[super::PassLocation],
    rewards: &[String],
    headline: &str,
) -> Result<Option<String>, AppError> {
    let (Some(issuer), Some(email), Some(key)) = (issuer_id(), sa_email(), sa_key()) else {
        // Silence here is how "no Add to Google Wallet button" came to look
        // identical to a wallet that was configured and refusing.
        tracing::warn!(
            missing = ?missing_env(),
            "loyalty: Google Wallet is not configured, so no button is offered"
        );
        return Ok(None);
    };
    if let Some(problem) = issuer_format_problem(&issuer) {
        // Every save under this would be refused, so there is nothing to gain
        // by asking Google once per card view to be told so.
        tracing::error!("loyalty: {problem}");
        return Ok(None);
    }
    // Always, rather than only the first time. A save link points at something
    // Google is holding, and that thing is only as current as the last time we
    // wrote it — so a card saved before any change to its shape stayed on the
    // old one forever. The token is cached, so the cost of being right here is
    // two requests on a page a customer opens rarely.
    let token = access_token().await?;
    ensure_class(&token, &issuer, member.org_id, brand, settings).await?;
    let object_id = ensure_object(
        &token, &issuer, member, settings, locations, rewards, headline, brand,
    )
    .await?;
    if member.google_object_id.as_deref() != Some(object_id.as_str()) {
        // Recorded so `push_balance` has something to PATCH — it reads this
        // column, which nothing used to write, so no Google pass ever saw a
        // balance change.
        sqlx::query("UPDATE loyalty_customers SET google_object_id = $2 WHERE id = $1")
            .bind(member.id)
            .bind(&object_id)
            .execute(pool)
            .await?;
    }

    let claims = SaveClaims {
        iss: email,
        aud: "google",
        typ: "savetowallet",
        iat: chrono::Utc::now().timestamp().max(0) as usize,
        payload: json!({ "loyaltyObjects": [{ "id": object_id }] }),
    };
    let encoding = EncodingKey::from_rsa_pem(&key).map_err(|e| {
        tracing::error!(error = %e, "LOYALTY_GOOGLE_SA_KEY is not a usable RSA PEM");
        AppError::ServiceUnavailable("Google Wallet key is not usable".into())
    })?;
    let jwt = jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &encoding)
        .map_err(|_| AppError::Internal)?;
    if jwt.len() > MAX_SAVE_JWT {
        tracing::error!(
            len = jwt.len(),
            "loyalty: Google save JWT is over the URL limit; the save page will fail"
        );
    }
    Ok(Some(format!("{SAVE_URL_PREFIX}{jwt}")))
}

/// Create the org's class, or bring an existing one up to date.
///
/// PATCH on conflict rather than leaving it: a class created once and never
/// touched again would keep a shop's first logo and colours forever, and there
/// is no other moment that would notice the branding had changed.
async fn ensure_class(
    token: &str,
    issuer: &str,
    org_id: uuid::Uuid,
    brand: &OrgBrand,
    settings: &LoyaltySettings,
) -> Result<(), AppError> {
    let body = loyalty_class(issuer, org_id, brand, settings);
    let id = class_id(issuer, org_id);
    let http = reqwest::Client::new();
    let resp = http
        .post(format!("{WALLET_API}/loyaltyClass"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .map_err(|e| AppError::ServiceUnavailable(format!("Google Wallet class: {e}")))?;
    if resp.status().is_success() {
        return Ok(());
    }
    if resp.status() != reqwest::StatusCode::CONFLICT {
        return Err(google_error("creating the loyalty class", resp).await);
    }
    let resp = http
        .patch(format!("{WALLET_API}/loyaltyClass/{id}"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .map_err(|e| AppError::ServiceUnavailable(format!("Google Wallet class: {e}")))?;
    if resp.status().is_success() {
        return Ok(());
    }
    Err(google_error("updating the loyalty class", resp).await)
}

/// Create the member's object. Returns its id either way.
#[allow(clippy::too_many_arguments)]
async fn ensure_object(
    token: &str,
    issuer: &str,
    member: &MemberRow,
    settings: &LoyaltySettings,
    locations: &[super::PassLocation],
    rewards: &[String],
    headline: &str,
    brand: &OrgBrand,
) -> Result<String, AppError> {
    let id = object_id(issuer, member);
    let resp = reqwest::Client::new()
        .post(format!("{WALLET_API}/loyaltyObject"))
        .bearer_auth(token)
        .json(&loyalty_object(
            issuer, member, settings, locations, rewards, headline,
        ))
        .send()
        .await
        .map_err(|e| AppError::ServiceUnavailable(format!("Google Wallet object: {e}")))?;
    if resp.status().is_success() {
        decorate(token, &id, member.org_id, brand).await;
        return Ok(id);
    }
    if resp.status() != reqwest::StatusCode::CONFLICT {
        return Err(google_error("creating the loyalty object", resp).await);
    }

    // The member already has one — and it is whatever shape this code produced
    // the day they saved it. Returning here left every card issued before a
    // change permanently on the old fields: the progress stayed in the details
    // list below the card long after it moved onto the face, and nothing short
    // of a balance change would ever have moved it.
    let resp = reqwest::Client::new()
        .patch(format!("{WALLET_API}/loyaltyObject/{id}"))
        .bearer_auth(token)
        .json(&loyalty_object(
            issuer, member, settings, locations, rewards, headline,
        ))
        .send()
        .await
        .map_err(|e| AppError::ServiceUnavailable(format!("Google Wallet object: {e}")))?;
    if resp.status().is_success() {
        decorate(token, &id, member.org_id, brand).await;
        return Ok(id);
    }
    Err(google_error("updating the loyalty object", resp).await)
}

/// Put the shop's photograph on an object that already exists.
///
/// Best effort by construction: it returns nothing, so no caller can make a
/// customer's card depend on it. An image Google will not take costs the band
/// and nothing else.
async fn decorate(token: &str, id: &str, org_id: uuid::Uuid, brand: &OrgBrand) {
    let Some(hero) = hero_image(org_id, brand) else {
        return;
    };
    let resp = reqwest::Client::new()
        .patch(format!("{WALLET_API}/loyaltyObject/{id}"))
        .bearer_auth(token)
        .json(&json!({ "heroImage": hero }))
        .send()
        .await;
    match resp {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            tracing::warn!(
                status = %status, body = %body,
                "loyalty: Google would not take the card image; the card is fine without it"
            );
        }
        Err(e) => tracing::warn!(error = %e, "loyalty: could not send the card image"),
    }
}

/// Google's own words for why it refused, in the log.
///
/// Worth the round trip: the alternative is a status code, and every failure
/// mode here — an unlinked service account, a wrong issuer id, a class Google
/// will not accept — arrives as the same 400 or 403 with the reason only in the
/// body.
async fn google_error(what: &str, resp: reqwest::Response) -> AppError {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    tracing::error!(status = %status, body = %body, "loyalty: Google refused {what}");
    AppError::ServiceUnavailable(format!("Google Wallet refused {what} ({status})"))
}

/// Google's tokens last an hour; refreshed early so a request never races the
/// expiry it was checked against.
const TOKEN_TTL: std::time::Duration = std::time::Duration::from_secs(50 * 60);
static CACHED_TOKEN: std::sync::Mutex<Option<(String, std::time::Instant)>> =
    std::sync::Mutex::new(None);

/// Exchange the service-account key for an access token (the JWT bearer grant).
///
/// Cached in process, like the APNs one. Without it, keeping a customer's card
/// up to date would cost a token exchange on every view — which is why it was
/// not kept up to date at all.
async fn access_token() -> Result<String, AppError> {
    if let Ok(guard) = CACHED_TOKEN.lock()
        && let Some((token, minted)) = guard.as_ref()
        && minted.elapsed() < TOKEN_TTL
    {
        return Ok(token.clone());
    }
    let token = mint_access_token().await?;
    if let Ok(mut guard) = CACHED_TOKEN.lock() {
        *guard = Some((token.clone(), std::time::Instant::now()));
    }
    Ok(token)
}

async fn mint_access_token() -> Result<String, AppError> {
    let (Some(email), Some(key)) = (sa_email(), sa_key()) else {
        return Err(AppError::ServiceUnavailable(
            "Google Wallet is not configured".into(),
        ));
    };
    let now = chrono::Utc::now().timestamp().max(0) as usize;
    let claims = json!({
        "iss": email,
        "scope": "https://www.googleapis.com/auth/wallet_object.issuer",
        "aud": "https://oauth2.googleapis.com/token",
        "iat": now,
        "exp": now + 3600,
    });
    let encoding = EncodingKey::from_rsa_pem(&key).map_err(|_| AppError::Internal)?;
    let assertion = jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &encoding)
        .map_err(|_| AppError::Internal)?;

    // Built by hand rather than with `.form()`: reqwest is pulled in with only
    // the `json` feature and the form encoder is not compiled in. The two values
    // are a fixed grant name and a JWT (base64url + dots), neither of which
    // needs percent-encoding, so the body is exact.
    let body =
        format!("grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer&assertion={assertion}");
    let resp = reqwest::Client::new()
        .post("https://oauth2.googleapis.com/token")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| AppError::ServiceUnavailable(format!("Google token endpoint: {e}")))?;
    if !resp.status().is_success() {
        // Google's OAuth errors say exactly what is wrong — `invalid_grant` for
        // a key that does not match the account, `unauthorized_client` for one
        // that is not allowed the scope. Reporting the status alone turned all
        // of them into "400", which is how this took four rounds to place.
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let reason = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .map(|v| {
                format!(
                    "{} {}",
                    v["error"].as_str().unwrap_or(""),
                    v["error_description"].as_str().unwrap_or("")
                )
                .trim()
                .to_string()
            })
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| body.chars().take(300).collect());
        tracing::error!(
            status = %status, reason = %reason,
            "loyalty: Google would not issue a token for the service account"
        );
        return Err(AppError::ServiceUnavailable(format!(
            "Google refused the service account ({status}): {reason}"
        )));
    }
    let body: serde_json::Value = resp.json().await.map_err(|_| AppError::Internal)?;
    body["access_token"]
        .as_str()
        .map(str::to_string)
        .ok_or(AppError::Internal)
}

/// PATCH the member's balance onto their Wallet object.
pub async fn push_balance(pool: &PgPool, member: &MemberRow) -> Result<(), AppError> {
    let Some(issuer) = issuer_id() else {
        return Ok(());
    };
    let Some(object_id) = member.google_object_id.clone() else {
        return Ok(());
    };
    let settings = crate::loyalty::settings::load_scope(pool, member.org_id, None)
        .await?
        .unwrap_or_else(|| LoyaltySettings::defaults(member.org_id, None));

    let token = access_token().await?;
    let mode = settings.mode();
    let balance = member.balance_in(mode);
    // The branches too, not just the balance. The refresh sweep exists to tell
    // cards about a branch that opened after they were issued, and patching
    // only the figures would have fixed Apple and left every Android card
    // listing the shops that existed the day it was saved.
    let locations = super::locations_for_member(pool, member)
        .await
        .unwrap_or_default();
    let headline = super::reward_headline(pool, member.org_id, &settings).await;
    let body = json!({
        // Exactly the shape the object was created with. Patching a different
        // one would change what the card looks like on the customer's first
        // sale, which is a strange moment for a card to rearrange itself.
        "loyaltyPoints": {
            "label": balance_label(mode),
            "balance": { "string": progress_line(balance, settings.default_reward_cost) }
        },
        "locations": locations
            .iter()
            .map(|l| json!({
                "kind": "walletobjects#latLongPoint",
                "latitude": l.latitude,
                "longitude": l.longitude,
            }))
            .collect::<Vec<_>>(),
        "secondaryLoyaltyPoints": {
            "label": "Reward",
            "balance": { "string": headline }
        }
    });
    let http = reqwest::Client::new();
    let resp = http
        .patch(format!("{WALLET_API}/loyaltyObject/{object_id}"))
        .bearer_auth(&token)
        .json(&body)
        .send()
        .await
        .map_err(|e| AppError::ServiceUnavailable(format!("Google Wallet PATCH: {e}")))?;
    if !resp.status().is_success() {
        return Err(google_error("updating the loyalty object", resp).await);
    }
    let _ = issuer;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The save link must not grow with the shop.
    ///
    /// It used to carry the whole class AND the whole object. For a shop with
    /// six branches that measured about 2,150 characters against Google's
    /// ~1,800 limit for a JWT in a save URL — so the save page failed with
    /// "Something went wrong" while still opening the Wallet app, and every
    /// branch added made it worse. The link now references an object Google
    /// already holds, so its size is fixed whatever the shop looks like.
    #[test]
    fn the_save_link_references_the_object_rather_than_carrying_it() {
        let s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        let brand = OrgBrand {
            name: "RUE Coffee".into(),
            logo_url: Some(
                "https://api.madar-pos.cloud/uploads/logos/3f2a1b8c-7d4e-4a91-bc22-0e5f61a8d904.png"
                    .into(),
            ),
            palette: crate::orgs::branding::Palette::default(),
            logo_is_mark: true,
            custom_branding: true,
            card_image_url: None,
        };
        let m = super::super::apple::tests::member();
        let locs: Vec<super::super::PassLocation> = (0..6)
            .map(|i| super::super::PassLocation {
                name: format!("RUE Coffee — Branch {i}"),
                latitude: 30.0444 + i as f64 * 0.01,
                longitude: 31.2357 + i as f64 * 0.01,
            })
            .collect();

        // Base64 costs a third on top, and an RS256 signature adds ~350 chars.
        let as_jwt = |v: &serde_json::Value| {
            let raw = serde_json::to_string(v).unwrap();
            40 + raw.len().div_ceil(3) * 4 + 350
        };

        // What we send now: an id, and nothing else.
        let reference =
            json!({ "loyaltyObjects": [{ "id": object_id("3388000000022345678", &m) }] });
        assert!(
            as_jwt(&reference) < MAX_SAVE_JWT,
            "a reference link must fit: {}",
            as_jwt(&reference)
        );

        // What we used to send, on the same shop. Kept as a measurement, so the
        // reason for the REST provisioning is checkable rather than folklore.
        let embedded = json!({
            "loyaltyClasses": [loyalty_class("3388000000022345678", uuid::Uuid::nil(), &brand, &s)],
            "loyaltyObjects": [loyalty_object("3388000000022345678", &m, &s, &locs, &[], "Free espresso")],
        });
        assert!(
            as_jwt(&embedded) > MAX_SAVE_JWT,
            "embedding the card was over the limit, which is why it is gone: {}",
            as_jwt(&embedded)
        );
    }

    /// The class Google is asked to hold: whose card it is, and what it looks
    /// like. Sent over REST before the first save, not embedded in the link.
    #[test]
    fn the_class_carries_the_shop_and_a_review_status_google_accepts() {
        let _guard = super::super::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // SAFETY: the lock makes this the only thread touching the environment.
        unsafe {
            std::env::set_var("PUBLIC_LOYALTY_BASE_URL", "https://loyalty.madar-pos.cloud");
        }
        let s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        let brand = OrgBrand {
            name: "RUE Coffee".into(),
            logo_url: Some("https://api.madar-pos.cloud/api/uploads/logos/rue.png".into()),
            palette: crate::orgs::branding::Palette {
                background: "#7B1E3A".into(),
                foreground: "#EFF3F4".into(),
                accent: "#C8607F".into(),
            },
            card_image_url: None,
            logo_is_mark: true,
            custom_branding: true,
        };
        let class = loyalty_class("3388000000000000000", uuid::Uuid::nil(), &brand, &s);
        assert_eq!(class["issuerName"], "RUE Coffee");
        assert_eq!(class["reviewStatus"], "UNDER_REVIEW");
        assert_eq!(
            class["id"],
            format!("3388000000000000000.madar-{}", uuid::Uuid::nil())
        );
        // The shop's brand reaches the Android card. These used to be read from
        // `loyalty_settings`, which nothing writes any more, so the class went
        // out with no logo and no colour.
        assert_eq!(class["hexBackgroundColor"], "#7B1E3A");
        unsafe {
            std::env::remove_var("PUBLIC_LOYALTY_BASE_URL");
        }
        // Composed by us for Google's circular slot, not the raw upload —
        // Google masks this to a circle, and a file made for a web page comes
        // out as a pale sticker. The key in the path is the uploaded file's
        // own name, so swapping the logo changes the URL and Google refetches.
        assert_eq!(
            class["programLogo"]["sourceUri"]["uri"],
            format!(
                "https://loyalty.madar-pos.cloud/api/public/loyalty/brand/{}/logo/rue.png",
                uuid::Uuid::nil()
            )
        );
    }

    #[test]
    fn google_is_never_pointed_at_a_relative_logo() {
        // Google FETCHES the logo from its own servers, so a site-relative path
        // would resolve against nothing — and a class with NO logo is rejected
        // outright, which is what "something went wrong" on an otherwise
        // working save link turned out to be.
        //
        // Both are now impossible by construction: Google is handed OUR badge
        // endpoint, which is absolute whatever the stored URL looks like, and a
        // shop with no logo at all gets Madar's mark.
        let _guard = super::super::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // SAFETY: the lock makes this the only thread touching the environment.
        unsafe {
            std::env::set_var("PUBLIC_LOYALTY_BASE_URL", "https://loyalty.madar-pos.cloud");
        }
        let s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        let brand = OrgBrand {
            name: "RUE".into(),
            logo_url: Some("/api/uploads/logos/rue.png".into()),
            custom_branding: true,
            ..OrgBrand::default()
        };
        let class = loyalty_class("338", uuid::Uuid::nil(), &brand, &s);
        assert_eq!(
            class["programLogo"]["sourceUri"]["uri"],
            format!(
                "https://loyalty.madar-pos.cloud/api/public/loyalty/brand/{}/logo/rue.png",
                uuid::Uuid::nil()
            ),
            "a stored URL of any shape becomes our own absolute badge"
        );

        // A shop with no logo at all still gets one, or the class is refused.
        let bare = OrgBrand {
            name: "RUE".into(),
            custom_branding: true,
            ..OrgBrand::default()
        };
        assert_eq!(
            loyalty_class("338", uuid::Uuid::nil(), &bare, &s)["programLogo"]["sourceUri"]["uri"],
            format!("https://loyalty.madar-pos.cloud/api{MADAR_LOGO_PATH}"),
            "never no logo at all"
        );
        unsafe {
            std::env::remove_var("PUBLIC_LOYALTY_BASE_URL");
        }
        // The colour still lands — it needs no fetching.
        assert_eq!(
            class["hexBackgroundColor"],
            crate::orgs::branding::MADAR_TEAL
        );
    }

    #[test]
    fn a_nameless_org_still_gets_an_issuer_on_the_card() {
        let s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        let class = loyalty_class("338", uuid::Uuid::nil(), &OrgBrand::default(), &s);
        assert_eq!(class["issuerName"], s.program_name);
    }

    #[test]
    fn a_merchant_id_pasted_as_an_issuer_id_is_named_as_such() {
        // The real one, from the shop this cost an evening. Google's own answer
        // was "Invalid resource ID: BCR2DN6DVK7MJ3IV.madar-685f…" — the
        // resource, not the setting, and no hint that a different number was
        // wanted.
        let problem = issuer_format_problem("BCR2DN6DVK7MJ3IV").expect("not a wallet issuer");
        assert!(problem.contains("LOYALTY_GOOGLE_ISSUER_ID"), "{problem}");
        assert!(problem.contains("numeric"), "{problem}");
        assert!(problem.contains("MERCHANT"), "{problem}");

        // A real issuer id passes without comment.
        assert_eq!(issuer_format_problem("3388000000022345678"), None);
        assert_eq!(issuer_format_problem("338"), None);

        // Anything else that is not a number is caught the same way, including
        // a value someone quoted or left a stray character in.
        assert!(issuer_format_problem("\"3388000000022345678\"").is_some());
        assert!(issuer_format_problem("3388000000022345678 ").is_some());
    }

    #[test]
    fn the_progress_is_on_the_card_not_in_the_list_under_it() {
        let mut s = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        s.mode = "visits".into();
        s.default_reward_cost = 5;
        let obj = loyalty_object(
            "338",
            &super::super::apple::tests::member(),
            &s,
            &[],
            &[],
            "Free espresso",
        );

        // `loyaltyPoints` and `secondaryLoyaltyPoints` render on the card face:
        // how far along in the larger slot, and what it is FOR in the other.
        assert_eq!(obj["loyaltyPoints"]["label"], "Orders");
        assert_eq!(obj["loyaltyPoints"]["balance"]["string"], "●─●─●─○─○");
        assert_eq!(obj["secondaryLoyaltyPoints"]["label"], "Reward");
        assert_eq!(
            obj["secondaryLoyaltyPoints"]["balance"]["string"],
            "Free espresso"
        );

        // `textModulesData` does NOT render on the face — it is the list below
        // the card, which is Google's version of Apple's BACK. The same content
        // belongs there, and it comes from the same `back_of_card` so the two
        // wallets cannot describe one programme differently.
        let modules = obj["textModulesData"].as_array().expect("a back of card");
        let headers: Vec<&str> = modules
            .iter()
            .map(|m| m["header"].as_str().unwrap_or(""))
            .collect();
        assert!(headers.contains(&"How it works"), "{headers:?}");
        // NOT "Member": Google renders `accountName` and `accountId` as rows of
        // its own, so the shared line would be a third copy — and it carries a
        // phone number, which is the last thing to print twice.
        assert!(!headers.contains(&"Member"), "{headers:?}");

        // What must NOT be down there is the progress. That is the whole
        // complaint: a customer opening their wallet saw a balance and had to
        // scroll past the card to find out how close they were.
        let bodies = modules
            .iter()
            .map(|m| m["body"].as_str().unwrap_or(""))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            !bodies.contains('●'),
            "the progress belongs on the card, not under it: {bodies}"
        );
    }

    #[test]
    fn the_stepper_thins_out_before_it_gets_squeezed() {
        // A pass field is one line that iOS shrinks to fit, so the row has to
        // stay short rather than stay pretty.

        // Small: joined, so it reads as a journey.
        assert_eq!(stepper(3, 5).as_deref(), Some("●─●─●─○─○"));
        assert_eq!(stepper(0, 3).as_deref(), Some("○─○─○"));
        assert_eq!(
            stepper(0, 1).as_deref(),
            Some("○"),
            "one step joins nothing"
        );
        assert_eq!(stepper(6, 6).as_deref(), Some("●─●─●─●─●─●"));

        // Bigger: the joins go first. They nearly double the width and the
        // order still reads left to right without them.
        assert_eq!(stepper(3, 7).as_deref(), Some("●●●○○○○"));
        assert_eq!(stepper(9, 12).as_deref(), Some("●●●●●●●●●○○○"));

        // Whatever the density, it fits the field it has to live in. The old
        // row was 23 characters at twelve steps, plus the figures.
        for target in 1..=MAX_STEPS {
            let row = stepper(target / 2, target).unwrap();
            assert!(
                row.chars().count() <= 12,
                "target {target} drew {} characters: {row}",
                row.chars().count()
            );
        }

        // Clamped both ways: redemption leaves a remainder and an adjustment
        // can overshoot; neither should draw a broken row.
        assert_eq!(stepper(9, 3).as_deref(), Some("●─●─●"));
        assert_eq!(stepper(-4, 3).as_deref(), Some("○─○─○"));

        // Past the cap there is nothing worth drawing.
        assert_eq!(stepper(30, 100), None);
        assert_eq!(stepper(1, MAX_STEPS + 1), None);
        assert!(stepper(1, MAX_STEPS).is_some());
        assert_eq!(stepper(1, 0), None);
    }

    #[test]
    fn a_programme_too_big_to_draw_falls_back_to_the_bare_ratio() {
        // Counting a hundred dots is not quicker than reading the figures, and
        // the label already says which programme this is.
        assert_eq!(progress_line(30, 100), "30 / 100");
        assert_eq!(progress_line(7, 20), "7 / 20");

        // Where there ARE steps, the steps are the whole line. Printing "3 / 5"
        // beside three filled circles and two empty ones says the same thing
        // twice, in a field whose width is the scarce thing — and the doubling
        // is most of what made the line look cramped.
        assert_eq!(progress_line(3, 5), "●─●─●─○─○");
        assert_eq!(progress_line(3, 8), "●●●○○○○○");

        // Reaching the target is said in words, and kept short because it
        // shares a row with the reward's name.
        assert_eq!(progress_line(100, 100), "Reward earned");
        assert_eq!(progress_line(5, 5), "Reward earned");
        assert_eq!(progress_line(130, 100), "Reward earned");
    }

    #[test]
    fn a_zero_threshold_never_claims_a_reward_is_ready() {
        // Defensive: the column is CHECK (> 0), but a pass that told every
        // customer their reward was ready would be a bad way to find out.
        assert_eq!(progress_line(0, 0), "0 / 0");
    }
}
