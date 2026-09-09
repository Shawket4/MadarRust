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
pub async fn check(org_id: Option<uuid::Uuid>) -> Result<String, String> {
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
    // Everything above is Madar's, and the same answer for every shop on the
    // box: the keys, the issuer, the service account. Only what follows is
    // per-shop, because a card class belongs to one. Refusing the whole check
    // for want of an org gated three answers on the one that was optional.
    let Some(org_id) = org_id else {
        return Ok(
            "Ready. The service account can talk to Google. Pick a shop to check \
             its card class."
                .into(),
        );
    };
    let id = class_id(&issuer, org_id);
    let resp = reqwest::Client::new()
        .get(format!("{WALLET_API}/loyaltyClass/{id}"))
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| format!("Could not reach Google: {e}"))?;
    match resp.status() {
        s if s.is_success() => {
            // Not just "it exists". Two things about a class decide whether a
            // saved card behaves, and neither is visible from the outside: a
            // class stuck UNDER_REVIEW does not get everything an approved one
            // does, and a class with no merchant locations cannot anchor a card
            // to a shop. Both were invisible while this reported existence
            // alone.
            let body = resp.text().await.unwrap_or_default();
            let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let review = v["reviewStatus"]
                .as_str()
                .unwrap_or("unknown")
                .to_lowercase();
            // `merchantLocations` is what triggers a nearby notification;
            // `locations` is the deprecated field that no longer does. A class
            // carrying only the old one looks configured and is inert, which is
            // exactly the state this panel existed to make visible.
            let places = v["merchantLocations"].as_array().map_or(0, |a| a.len());
            let stale = v["locations"].as_array().map_or(0, |a| a.len());
            let note = if places == 0 && stale > 0 {
                format!(
                    " {stale} on the DEPRECATED `locations` field, which no longer \
                     triggers notifications — the class needs rewriting."
                )
            } else {
                String::new()
            };
            Ok(format!(
                "Ready. This shop's card class ({id}) exists — review status \
                 {review}, {places} branch location(s) that can notify.{note}"
            ))
        }
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
/// The shop's card template.
///
/// Deliberately carries no `reviewStatus`: `ensure_class` sets it on both the
/// insert and the update, for a reason that only shows up on an approved class
/// and is written down there.
/// Branch coordinates in the shape that actually raises a notification.
///
/// `merchantLocations`, NOT `locations`. They look interchangeable and are not:
/// Google's reference marks `locations` (a `walletobjects#latLongPoint` array)
/// deprecated with the words "this field is currently not supported to trigger
/// geo notifications", while `merchantLocations` "will trigger a notification
/// when a user enters within a Google-set radius of the point".
///
/// That distinction was the whole bug. Passes carried branch coordinates the
/// entire time, in the field Google stopped reading, so an approved card that
/// saved perfectly never asked for location permission and never surfaced when
/// its owner was standing in the shop. Nothing was refused and nothing was
/// logged, because nothing was malformed — the coordinates were simply being
/// filed somewhere inert.
///
/// The shape is narrower too: latitude and longitude only. No `kind`, and no
/// name — the old points carried one and these do not.
///
/// Ten maximum on the class and ten on the object, each; anything past ten is
/// rejected outright, so the slice is cut here as well as at the query.
fn merchant_locations(locations: &[super::PassLocation]) -> Vec<serde_json::Value> {
    locations
        .iter()
        .take(super::MAX_LOCATIONS)
        .map(|l| {
            json!({
                "latitude": l.latitude,
                "longitude": l.longitude,
            })
        })
        .collect()
}

/// Drop keys whose value is an empty array.
///
/// `"merchantLocations": []` is not the same as omitting it. Google has refused
/// a class carrying locations before — which is why there is a retry without
/// them — and sending an empty one asks that question for no benefit, on every
/// shop that has not set a single branch coordinate.
fn drop_empty_arrays(v: &mut serde_json::Value) {
    if let Some(map) = v.as_object_mut() {
        map.retain(|_, value| !value.as_array().is_some_and(|a| a.is_empty()));
    }
}

pub fn loyalty_class(
    issuer: &str,
    org_id: uuid::Uuid,
    brand: &OrgBrand,
    settings: &LoyaltySettings,
    locations: &[super::PassLocation],
) -> serde_json::Value {
    let mut class = json!({
        "id": class_id(issuer, org_id),
        // Whose card this is. Always set, even when nothing else has been
        // configured — falling back to the programme name rather than leaving a
        // wallet entry with no owner on it.
        "issuerName": if brand.name.trim().is_empty() { settings.program_name.as_str() } else { brand.name.as_str() },
        "programName": settings.program_name,
        // Google FETCHES this, so it must be absolute and publicly reachable —
        // unlike Apple's, which is packed into the archive as bytes.
        "hexBackgroundColor": brand.palette.background,
        // The shop's branches, on the shop's template.
        //
        // The object carries merchant locations too — the member's own nearest
        // ten —
        // and which of the two Google reads for a nearby prompt is not
        // something I could establish by reasoning about it. So both carry
        // them, which is cheap and is true either way: "where this shop is" is
        // a fact about the shop, and this is the resource that describes one.
        "merchantLocations": merchant_locations(locations),
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
    drop_empty_arrays(&mut class);
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
    copy: &super::CardCopy,
    headline: &str,
) -> serde_json::Value {
    let mode = settings.mode();
    let balance = member.balance_in(mode);

    // Google localises inline, field by field, from the same table Apple's
    // strings file is built from — one place to be wrong rather than two. A
    // string with no Arabic is emitted as itself: a shop's own reward names and
    // its branches are not ours to translate.
    let pairs = super::i18n::strings_for(settings, settings.program_name_ar.as_deref());
    let localized = |en: &str| -> serde_json::Value {
        match pairs.iter().find(|p| p.en == en) {
            Some(p) => json!({
                "defaultValue": { "language": "en", "value": en },
                "translatedValues": [{ "language": "ar", "value": p.ar }]
            }),
            None => serde_json::Value::Null,
        }
    };
    let with_localized = |mut field: serde_json::Value, key: &str, en: &str| {
        let l = localized(en);
        if !l.is_null() {
            field[key] = l;
        }
        field
    };

    let mut object = json!({
        "id": object_id(issuer, member),
        "classId": class_id(issuer, member.org_id),
        "state": "ACTIVE",
        "accountId": member.member_token,
        "accountName": member.name,
        // Both of these render ON THE CARD. `textModulesData` does not — it
        // renders in the details list BELOW it, which is where the progress
        // used to sit: a customer opening their wallet saw a balance and had to
        // scroll past the card to find out how close they were.
        // Google gives two slots, so the earned rewards take the first when
        // there are any and the live stepper drops to the second — a card does
        // not stop at full, and six against a reward every five is one waiting
        // AND one step towards the next.
        "loyaltyPoints": match earned_line(balance, settings.default_reward_cost) {
            Some(earned) => json!({
                "label": earned_label(balance, settings.default_reward_cost),
                "balance": { "string": earned }
            }),
            None => with_localized(
                json!({
                    "label": balance_label(mode),
                    "balance": { "string": progress_line(balance, settings.default_reward_cost) }
                }),
                "localizedLabel",
                balance_label(mode),
            ),
        },
        // What they are working towards, not how far along they are — the
        // figures are already in the slot above. "Get a free drink" is the
        // thing a customer opens the card to be reminded of.
        // The second slot: the live stepper when the first is showing earned
        // rewards, otherwise the reward's own name — and NOTHING when a shop
        // has curated none. It used to print the cost there under a heading
        // reading "Reward", which says the reward is five orders. It is the
        // price of one.
        "secondaryLoyaltyPoints": match (
            earned_line(balance, settings.default_reward_cost),
            headline.trim().is_empty(),
        ) {
            (Some(_), _) => with_localized(
                json!({
                    "label": balance_label(mode),
                    "balance": { "string": progress_line(balance, settings.default_reward_cost) }
                }),
                "localizedLabel",
                balance_label(mode),
            ),
            (None, false) => with_localized(
                json!({
                    "label": "Reward",
                    "balance": { "string": headline }
                }),
                "localizedLabel",
                "Reward",
            ),
            (None, true) => serde_json::Value::Null,
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
        "merchantLocations": merchant_locations(locations),
        // What Apple puts on the BACK of the card. Google shows these under it
        // rather than behind it, which is the same content in the same order —
        // built by `wallet::back_of_card`, once, so the two cannot drift into
        // telling a customer different things about one programme.
        // Google renders `accountName` and `accountId` as rows of its own, so
        // the shared "Member" line would appear a third time — and it carries a
        // phone number, which is the last thing to print twice. Apple keeps it:
        // it has no automatic equivalent.
        // Google's own place for links, rather than URLs printed as text in a
        // details row — it renders them as buttons and they open in the app the
        // shop actually wants them opened in.
        "linksModuleData": {
            "uris": super::card_link(member)
                .map(|url| json!({
                    "kind": "walletobjects#uri",
                    "uri": url,
                    "description": "Your card online",
                    "id": "mycard"
                }))
                .into_iter()
                .chain(copy
                .social
                .iter()
                .map(|l| json!({
                    "kind": "walletobjects#uri",
                    "uri": l.url,
                    "description": l.label,
                    "id": l.key
                })))
                .collect::<Vec<_>>()
        },
        "textModulesData": super::back_of_card(member, settings, copy)
            .into_iter()
            .filter(|l| l.key != "member")
            .map(|l| {
                let row = json!({ "id": l.key, "header": l.label, "body": l.value });
                let row = with_localized(row, "localizedHeader", &l.label);
                with_localized(row, "localizedBody", &l.value)
            })
            .collect::<Vec<_>>(),
    });
    drop_empty_arrays(&mut object);
    object
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
/// Where they are on the CURRENT card, not on the whole history.
///
/// A card does not stop at full. Someone who has bought six with a reward every
/// five has one reward waiting and one step towards the next, and a stepper
/// clamped at five could only say "finished" — losing the fact that the sixth
/// order counted for something. So the progress shown is what is left after the
/// earned ones are set aside, which is the same arithmetic the web card has
/// always done (`model::earned_and_progress`), and the earned ones get a row of
/// their own.
pub fn progress_line(balance: i32, threshold: i32) -> String {
    let (_, progress) = crate::loyalty::model::earned_and_progress(balance, threshold);
    match stepper(progress, threshold) {
        // The steps ALONE. Printing "3 / 5" beside three filled circles and two
        // empty ones says the same thing twice, in a field whose width is the
        // scarce thing — and the doubling is most of what made the line look
        // cramped.
        Some(steps) => steps,
        // Only where there are no steps to show: past the countable cap the
        // figures are all there is, and they are enough.
        None => format!("{progress} / {threshold}"),
    }
}

/// The rewards already sitting on the card, as a filled row.
///
/// `None` when there are none, which is the usual case and must render as no
/// row at all rather than as an empty one.
pub fn earned_line(balance: i32, threshold: i32) -> Option<String> {
    let (earned, _) = crate::loyalty::model::earned_and_progress(balance, threshold);
    if earned <= 0 {
        return None;
    }
    // A full row of filled steps, which is what "earned" looks like — and for
    // more than one, the count, because five identical full rows would be a
    // worse way of saying "five".
    let full = stepper(threshold, threshold).unwrap_or_default();
    Some(if earned == 1 {
        full
    } else if full.is_empty() {
        format!("×{earned}")
    } else {
        format!("{full}  ×{earned}")
    })
}

/// What to call the earned row.
pub fn earned_label(balance: i32, threshold: i32) -> &'static str {
    let (earned, _) = crate::loyalty::model::earned_and_progress(balance, threshold);
    if earned > 1 {
        "Rewards ready"
    } else {
        "Reward ready"
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
    copy: &super::CardCopy,
    headline: &str,
) -> Result<Option<String>, AppError> {
    save_url_recorded(
        pool,
        member,
        settings,
        brand,
        locations,
        copy,
        headline,
        &mut Vec::new(),
    )
    .await
}

/// The same, keeping a transcript of everything Google was asked and answered.
///
/// See [`WalletStep`]. The customer-facing path throws the transcript away; the
/// diagnostic keeps it, and because it is the SAME path, what it reports is
/// what actually happened.
#[allow(clippy::too_many_arguments)]
pub async fn save_url_recorded(
    pool: &PgPool,
    member: &MemberRow,
    settings: &LoyaltySettings,
    brand: &OrgBrand,
    locations: &[super::PassLocation],
    copy: &super::CardCopy,
    headline: &str,
    steps: &mut Vec<WalletStep>,
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
    // The class describes the SHOP, so it gets the shop's branches — not this
    // member's nearest ten, which is what the object carries.
    let org_locations = super::locations_for_org(pool, member.org_id)
        .await
        .unwrap_or_default();
    ensure_class(
        &token,
        &issuer,
        member.org_id,
        brand,
        settings,
        &org_locations,
        steps,
    )
    .await?;
    let object_id = ensure_object(
        &token, &issuer, member, settings, locations, copy, headline, brand, steps,
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
#[allow(clippy::too_many_arguments)]
async fn ensure_class(
    token: &str,
    issuer: &str,
    org_id: uuid::Uuid,
    brand: &OrgBrand,
    settings: &LoyaltySettings,
    locations: &[super::PassLocation],
    steps: &mut Vec<WalletStep>,
) -> Result<(), AppError> {
    let body = loyalty_class(issuer, org_id, brand, settings, locations);
    let id = class_id(issuer, org_id);
    let http = reqwest::Client::new();
    // `UNDER_REVIEW` is what a class written through the API must carry, on the
    // update as well as the insert — and it is not optional, which cost this
    // feature a fortnight to learn.
    //
    // A PATCH MERGES. Omitting the field does not leave it alone; it leaves
    // Google's own value in place, and Google then rejects its own value:
    //
    //     400 Invalid review status "APPROVED". Use "UNDER_REVIEW" instead.
    //
    // `approved` is a status Google grants and an issuer may not send, so an
    // approved class simply cannot be updated without restating `UNDER_REVIEW`.
    // I removed it once on the theory that repeating it demoted approved
    // classes back under review. It does not — the classes that have been sent
    // it on every write for months are approved today, because an approved
    // issuer's classes are re-approved automatically. What removing it actually
    // did was fail every class update with that 400, silently, for every shop:
    // colours, programme name, logo and branches all frozen at whatever they
    // were the day the class was created, while the web card and the Apple pass
    // moved on without it.
    let mut sent = body.clone();
    sent["reviewStatus"] = json!("UNDER_REVIEW");
    let insert = sent.clone();
    let resp = http
        .post(format!("{WALLET_API}/loyaltyClass"))
        .bearer_auth(token)
        .json(&insert)
        .send()
        .await
        .map_err(|e| {
            steps.push(WalletStep::new("insert the class", 0, e.to_string()));
            AppError::ServiceUnavailable(format!("Google Wallet class: {e}"))
        })?;
    let created = resp.status();
    if created.is_success() {
        steps.push(WalletStep::new(
            "insert the class",
            created.as_u16(),
            resp.text().await.unwrap_or_default(),
        ));
        return Ok(());
    }
    if created != reqwest::StatusCode::CONFLICT {
        let body = resp.text().await.unwrap_or_default();
        steps.push(WalletStep::new(
            "insert the class",
            created.as_u16(),
            body.clone(),
        ));
        return Err(AppError::ServiceUnavailable(format!(
            "Google refused the loyalty class ({created}): {}",
            first_reason(&body)
        )));
    }
    steps.push(WalletStep::new(
        "insert the class",
        created.as_u16(),
        "already exists — updating it instead".into(),
    ));
    // CONFLICT means the class is already there, which is all a save link
    // actually needs. What follows is a REFRESH — new colours, a new logo — and
    // a refresh that fails must leave the card it was refreshing alone. Letting
    // it fail the whole call is how a shop's button disappeared from the card
    // page over an image Google would not take.
    let attempt =
        async |what: &str, sent: &serde_json::Value, steps: &mut Vec<WalletStep>| match http
            .patch(format!("{WALLET_API}/loyaltyClass/{id}"))
            .bearer_auth(token)
            .json(sent)
            .send()
            .await
        {
            Ok(r) => {
                let status = r.status();
                let answer = r.text().await.unwrap_or_default();
                steps.push(WalletStep::new(what, status.as_u16(), answer.clone()));
                (!status.is_success()).then_some(answer)
            }
            Err(e) => {
                steps.push(WalletStep::new(what, 0, e.to_string()));
                Some(e.to_string())
            }
        };

    let mut refused = attempt("update the class", &sent, steps).await;

    // Then again without the branches.
    //
    // They are on the class as insurance: the member's own object carries
    // branches too, and which of the two Google reads for a nearby prompt was
    // never something I could establish by reasoning. But insurance that voids
    // the policy is not insurance. If Google will not accept a class carrying
    // `locations`, then adding them stopped EVERY class update — colours,
    // programme name, logo, all frozen at whatever they were the day it was
    // created, on a resource otherwise rewritten on every card view. Which is
    // exactly what a shop saw: an Android card still wearing Madar's teal weeks
    // after it had been given its own.
    //
    // So the branches are the part we give up, never the shop's identity. And
    // because both attempts are in the transcript, production tells us which it
    // was rather than another round of guessing.
    if refused.is_some() && sent.get("merchantLocations").is_some() {
        let mut without = sent.clone();
        if let Some(o) = without.as_object_mut() {
            o.remove("merchantLocations");
        }
        if attempt("update the class without branches", &without, steps)
            .await
            .is_none()
        {
            tracing::warn!(
                class = %id,
                "loyalty: Google refuses branch locations on a card class — the class \
                 updated without them, and the member's own card carries them instead"
            );
            refused = None;
        }
    }

    if let Some(answer) = refused {
        tracing::warn!(
            detail = %first_reason(&answer), class = %id,
            "loyalty: Google would not update the card class; \
             customers keep the one it already has"
        );
    }
    Ok(())
}

/// Create the member's object. Returns its id either way.
#[allow(clippy::too_many_arguments)]
async fn ensure_object(
    token: &str,
    issuer: &str,
    member: &MemberRow,
    settings: &LoyaltySettings,
    locations: &[super::PassLocation],
    copy: &super::CardCopy,
    headline: &str,
    brand: &OrgBrand,
    steps: &mut Vec<WalletStep>,
) -> Result<String, AppError> {
    let id = object_id(issuer, member);
    let resp = reqwest::Client::new()
        .post(format!("{WALLET_API}/loyaltyObject"))
        .bearer_auth(token)
        .json(&loyalty_object(
            issuer, member, settings, locations, copy, headline,
        ))
        .send()
        .await
        .map_err(|e| {
            steps.push(WalletStep::new("insert the object", 0, e.to_string()));
            AppError::ServiceUnavailable(format!("Google Wallet object: {e}"))
        })?;
    let created = resp.status();
    if created.is_success() {
        steps.push(WalletStep::new(
            "insert the object",
            created.as_u16(),
            resp.text().await.unwrap_or_default(),
        ));
        decorate(token, &id, member.org_id, brand, steps).await;
        return Ok(id);
    }
    if created != reqwest::StatusCode::CONFLICT {
        let body = resp.text().await.unwrap_or_default();
        steps.push(WalletStep::new(
            "insert the object",
            created.as_u16(),
            body.clone(),
        ));
        return Err(AppError::ServiceUnavailable(format!(
            "Google refused the loyalty object ({created}): {}",
            first_reason(&body)
        )));
    }
    steps.push(WalletStep::new(
        "insert the object",
        created.as_u16(),
        "already exists — updating it instead".into(),
    ));

    // The member already has one — and it is whatever shape this code produced
    // the day they saved it. Returning here left every card issued before a
    // change permanently on the old fields: the progress stayed in the details
    // list below the card long after it moved onto the face, and nothing short
    // of a balance change would ever have moved it.
    //
    // PUT, not PATCH. A patch MERGES, and `loyaltyPoints.balance` is a union —
    // Google takes exactly one of `int` / `string` / `double` / `money`. Moving
    // that field from an int to a string therefore left BOTH set on any object
    // saved before the change, which Google rejects. A put replaces the
    // resource with the body, and the body below is the whole card.
    let resp = reqwest::Client::new()
        .put(format!("{WALLET_API}/loyaltyObject/{id}"))
        .bearer_auth(token)
        .json(&loyalty_object(
            issuer, member, settings, locations, copy, headline,
        ))
        .send()
        .await;
    // And, as with the class: this is a refresh of something that already
    // exists. If Google will not take the new shape, the customer keeps the
    // card they have — which is stale, not missing. Hiding the button instead
    // takes away a card that works.
    match resp {
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            steps.push(WalletStep::new(
                "update the object",
                status.as_u16(),
                body.clone(),
            ));
            if !status.is_success() {
                tracing::warn!(
                    status = %status, detail = %first_reason(&body), object = %id,
                    "loyalty: Google would not update this card; the customer keeps the older one"
                );
            }
        }
        Err(e) => {
            steps.push(WalletStep::new("update the object", 0, e.to_string()));
            tracing::warn!(error = %e, "loyalty: could not refresh the card");
        }
    }
    decorate(token, &id, member.org_id, brand, steps).await;
    Ok(id)
}

/// Put the shop's photograph on an object that already exists.
///
/// Best effort by construction: it returns nothing, so no caller can make a
/// customer's card depend on it. An image Google will not take costs the band
/// and nothing else.
async fn decorate(
    token: &str,
    id: &str,
    org_id: uuid::Uuid,
    brand: &OrgBrand,
    steps: &mut Vec<WalletStep>,
) {
    let Some(hero) = hero_image(org_id, brand) else {
        steps.push(WalletStep::new(
            "add the card image",
            0,
            "skipped — this shop has no card image".into(),
        ));
        return;
    };
    let resp = reqwest::Client::new()
        .patch(format!("{WALLET_API}/loyaltyObject/{id}"))
        .bearer_auth(token)
        .json(&json!({ "heroImage": hero }))
        .send()
        .await;
    match resp {
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            steps.push(WalletStep::new(
                "add the card image",
                status.as_u16(),
                body.clone(),
            ));
            if !status.is_success() {
                tracing::warn!(
                    status = %status, body = %body,
                    "loyalty: Google would not take the card image; the card is fine without it"
                );
            }
        }
        Err(e) => {
            steps.push(WalletStep::new("add the card image", 0, e.to_string()));
            tracing::warn!(error = %e, "loyalty: could not send the card image");
        }
    }
}

/// One request to Google and what it answered, kept verbatim.
///
/// Provisioning is four requests deep and every one of them can fail in a way
/// the customer never sees: a refused class, an image Google will not fetch, a
/// field it silently drops. A failed REFRESH is deliberately only a warning —
/// the customer keeps the card they have — which means the reason lands in a
/// log nobody is reading at the moment it matters.
///
/// So the same code path can be asked to keep a transcript. `save_url` throws
/// it away; the super-admin diagnostic returns it. One path, so what the
/// diagnostic reports is what actually happens, rather than a second
/// implementation that agrees with the first until it doesn't.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct WalletStep {
    /// What was attempted, in words: "insert the class", "update the object".
    pub step: String,
    /// HTTP status, or 0 when the request never reached Google.
    pub status: u16,
    /// Google's answer, as it came. Truncated only if it is enormous.
    pub body: String,
}

/// Bodies echo the whole resource back, so they are long but not unbounded.
const MAX_STEP_BODY: usize = 8_000;

impl WalletStep {
    fn new(step: &str, status: u16, body: String) -> Self {
        let body = if body.len() > MAX_STEP_BODY {
            format!("{}… [truncated]", &body[..MAX_STEP_BODY])
        } else {
            body
        };
        Self {
            step: step.to_string(),
            status,
            body,
        }
    }
}

/// What Google is actually holding for this member, verbatim.
///
/// Every question about this feature so far has been answered by guessing, and
/// twice by guessing wrong. The object is the fact: either the locations are on
/// it and the problem is what Google does with them, or they are not and the
/// problem is ours. One request settles which.
///
/// Returns Google's own response body on failure rather than a status code —
/// the reason for a refusal is only ever in the body.
pub async fn read_object(member: &MemberRow) -> Result<serde_json::Value, String> {
    let Some(issuer) = issuer_id() else {
        return Err("LOYALTY_GOOGLE_ISSUER_ID is not set".into());
    };
    let id = member
        .google_object_id
        .clone()
        .unwrap_or_else(|| object_id(&issuer, member));
    let token = access_token().await.map_err(|e| e.to_string())?;
    let resp = reqwest::Client::new()
        .get(format!("{WALLET_API}/loyaltyObject/{id}"))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| format!("Could not reach Google: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("Google returned {status}: {}", first_reason(&body)));
    }
    serde_json::from_str(&body).map_err(|e| format!("Google sent something unreadable: {e}"))
}

/// Put a message on the member's card, and notify their phone.
///
/// Google's direct equivalent of Apple's field trick, and a much better fit:
/// arbitrary text, pushed and shown on the card, with no pretending it is a
/// balance that changed.
///
/// `TEXT_AND_NOTIFY` is throttled by Google and meant to be used sparingly — a
/// birthday and a win-back are exactly what it is for. The response body says
/// what Google made of it, and that is what goes in the log rather than a
/// status code.
pub async fn add_message(member: &MemberRow, body: &str) -> Result<(), AppError> {
    let Some(issuer) = issuer_id() else {
        return Err(AppError::ServiceUnavailable(
            "Google Wallet is not configured".into(),
        ));
    };
    let id = member
        .google_object_id
        .clone()
        .unwrap_or_else(|| object_id(&issuer, member));
    let token = access_token().await?;
    let resp = reqwest::Client::new()
        .post(format!("{WALLET_API}/loyaltyObject/{id}/addMessage"))
        .bearer_auth(token)
        .json(&json!({
            "message": {
                // Its own id, so a retry replaces the message rather than
                // stacking a second copy of it on the card.
                "id": format!("notice-{}", member.id),
                "header": "",
                "body": body,
                "messageType": "TEXT_AND_NOTIFY"
            }
        }))
        .send()
        .await
        .map_err(|e| AppError::ServiceUnavailable(format!("Google Wallet message: {e}")))?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let detail = first_reason(&resp.text().await.unwrap_or_default());
    Err(AppError::ServiceUnavailable(format!(
        "Google refused the message ({status}): {detail}"
    )))
}

/// Has this member actually saved their Google card?
///
/// Holding a `google_object_id` only means WE created an object; a customer who
/// opened the card page and never tapped the badge has one too. Sending their
/// message to a card nobody saved and calling it delivered is how someone stops
/// hearing from a shop they still use.
///
/// Google answers it on the object as `hasUsers`. It costs a read, which is why
/// this is only ever asked before a message and never on a page load. A failure
/// answers "no": the fallback is a WhatsApp, and sending one message too many
/// is a smaller wrong than sending none.
pub async fn has_saved_card(member: &MemberRow) -> bool {
    if member.google_object_id.is_none() {
        return false;
    }
    match read_object(member).await {
        Ok(o) => o["hasUsers"].as_bool().unwrap_or(false),
        Err(e) => {
            tracing::warn!(
                customer_id = %member.id, error = %e,
                "loyalty: could not ask Google whether the card was saved"
            );
            false
        }
    }
}

/// The shop's class as Google holds it — the other half of the picture.
///
/// The object says what one member's card is; the class says what the shop's
/// cards ARE, and that is where the review status and (now) the branches live.
pub async fn read_class(org_id: uuid::Uuid) -> Result<serde_json::Value, String> {
    let Some(issuer) = issuer_id() else {
        return Err("LOYALTY_GOOGLE_ISSUER_ID is not set".into());
    };
    let token = access_token().await.map_err(|e| e.to_string())?;
    let resp = reqwest::Client::new()
        .get(format!(
            "{WALLET_API}/loyaltyClass/{}",
            class_id(&issuer, org_id)
        ))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| format!("Could not reach Google: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("Google returned {status}: {}", first_reason(&body)));
    }
    serde_json::from_str(&body).map_err(|e| format!("Google sent something unreadable: {e}"))
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
    // The branches too, not just the balance. The refresh sweep exists to tell
    // cards about a branch that opened after they were issued, and patching
    // only the figures would have fixed Apple and left every Android card
    // listing the shops that existed the day it was saved.
    let locations = super::locations_for_member(pool, member)
        .await
        .unwrap_or_default();
    let headline = super::reward_headline(pool, member.org_id, &settings).await;
    let copy = super::card_copy(pool, member.org_id, &settings).await;
    // The whole card, through the same builder the save path uses, and PUT
    // rather than PATCH — for both of the reasons `ensure_object` gives. One
    // writer and one shape: a card that changed on a sale and a card that
    // changed on a save cannot end up different objects.
    let body = loyalty_object(&issuer, member, &settings, &locations, &copy, &headline);
    let http = reqwest::Client::new();
    let resp = http
        .put(format!("{WALLET_API}/loyaltyObject/{object_id}"))
        .bearer_auth(&token)
        .json(&body)
        .send()
        .await
        .map_err(|e| AppError::ServiceUnavailable(format!("Google Wallet PUT: {e}")))?;
    if !resp.status().is_success() {
        return Err(google_error("updating the loyalty object", resp).await);
    }
    // A put replaces the resource, so the photograph goes back on after it.
    if let Ok(brand) = crate::orgs::branding::load(pool, member.org_id).await {
        decorate(&token, &object_id, member.org_id, &brand, &mut Vec::new()).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_branch(lat: f64, lng: f64, name: &str) -> super::super::PassLocation {
        super::super::PassLocation {
            latitude: lat,
            longitude: lng,
            name: name.into(),
        }
    }

    /// Branch coordinates must go in the field that still triggers a
    /// notification.
    ///
    /// `locations` and `merchantLocations` look interchangeable and are not.
    /// Google's reference marks the first deprecated — "this field is currently
    /// not supported to trigger geo notifications" — while the second "will
    /// trigger a notification when a user enters within a Google-set radius".
    ///
    /// The card shipped writing the deprecated one. Nothing failed: the class
    /// and object were accepted, the pass saved, the coordinates were on it,
    /// and Android never once asked for location permission because there was
    /// nothing there for it to geofence. A silent difference between two
    /// spellings of the same idea, so it is pinned here rather than left to be
    /// rediscovered.
    #[test]
    fn branch_coordinates_go_in_merchant_locations_not_the_deprecated_field() {
        let settings = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        let brand = OrgBrand {
            name: "RUE Coffee".into(),
            logo_url: None,
            palette: crate::orgs::branding::Palette::default(),
            logo_is_mark: false,
            custom_branding: true,
            card_image_url: None,
            social_links: Default::default(),
        };
        let places = [
            a_branch(30.0444, 31.2357, "Downtown"),
            a_branch(31.2001, 29.9187, "Alexandria"),
        ];

        let class = loyalty_class(
            "3388000000000000000",
            uuid::Uuid::nil(),
            &brand,
            &settings,
            &places,
        );
        let on_class = class["merchantLocations"]
            .as_array()
            .expect("the class geofences through merchantLocations");
        assert_eq!(on_class.len(), 2);
        assert!(
            class.get("locations").is_none(),
            "the deprecated field must not be written: it does nothing and \
             reads as if the card were configured"
        );

        // Latitude and longitude only. The old points carried a `kind` and a
        // name; a MerchantLocation takes neither, and Google rejects extras.
        assert_eq!(on_class[0]["latitude"], 30.0444);
        assert_eq!(on_class[0]["longitude"], 31.2357);
        assert!(
            on_class[0].get("kind").is_none(),
            "no kind on a MerchantLocation"
        );
        assert!(
            on_class[0].get("name").is_none(),
            "no name on a MerchantLocation"
        );
    }

    /// A shop with no coordinates sends no key at all.
    ///
    /// `"merchantLocations": []` is not the same as omitting it, and asking
    /// Google to accept an empty array buys nothing.
    #[test]
    fn a_shop_with_no_coordinates_omits_the_key_rather_than_sending_an_empty_one() {
        let settings = LoyaltySettings::defaults(uuid::Uuid::nil(), None);
        let brand = OrgBrand {
            name: "RUE Coffee".into(),
            logo_url: None,
            palette: crate::orgs::branding::Palette::default(),
            logo_is_mark: false,
            custom_branding: true,
            card_image_url: None,
            social_links: Default::default(),
        };
        let class = loyalty_class(
            "3388000000000000000",
            uuid::Uuid::nil(),
            &brand,
            &settings,
            &[],
        );
        assert!(
            class.get("merchantLocations").is_none(),
            "an empty geofence list should not be sent at all"
        );
    }

    /// Ten is Google's ceiling, and the eleventh is rejected outright rather
    /// than ignored — which would take the whole class update down with it.
    #[test]
    fn no_more_than_ten_merchant_locations_are_sent() {
        let many: Vec<_> = (0..15)
            .map(|i| a_branch(30.0 + i as f64 * 0.01, 31.0, "Branch"))
            .collect();
        assert_eq!(merchant_locations(&many).len(), super::super::MAX_LOCATIONS);
    }

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
            social_links: vec![],
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
            "loyaltyClasses": [loyalty_class("3388000000022345678", uuid::Uuid::nil(), &brand, &s, &[])],
            "loyaltyObjects": [loyalty_object("3388000000022345678", &m, &s, &locs, &crate::loyalty::wallet::CardCopy::default(), "Free espresso")],
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
            social_links: vec![],
        };
        let places = [super::super::PassLocation {
            latitude: 30.06,
            longitude: 31.22,
            name: "Maadi".into(),
        }];
        let class = loyalty_class(
            "3388000000000000000",
            uuid::Uuid::nil(),
            &brand,
            &s,
            &places,
        );
        assert_eq!(class["issuerName"], "RUE Coffee");
        // No review status in the body. `ensure_class` states it on the INSERT
        // and never again — this body is also what the UPDATE sends, and every
        // update carrying UNDER_REVIEW walked an approved class backwards, on
        // every card view, forever.
        // The BODY carries none; `ensure_class` puts `UNDER_REVIEW` on both the
        // insert and the update. Which is not a style choice: a PATCH merges,
        // so omitting it leaves Google's own `approved` in place and Google
        // then refuses its own value — "Invalid review status APPROVED. Use
        // UNDER_REVIEW instead" — and every class update fails silently.
        assert!(
            class["reviewStatus"].is_null(),
            "the status belongs to the writer, not the body"
        );
        // The shop's branches ride on the shop's template, as well as on each
        // member's object — in `merchantLocations`, the field that still
        // triggers a nearby notification. See
        // `branch_coordinates_go_in_merchant_locations_not_the_deprecated_field`.
        assert_eq!(class["merchantLocations"][0]["latitude"], 30.06);
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
            social_links: vec![],
            ..OrgBrand::default()
        };
        let class = loyalty_class("338", uuid::Uuid::nil(), &brand, &s, &[]);
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
            social_links: vec![],
            ..OrgBrand::default()
        };
        assert_eq!(
            loyalty_class("338", uuid::Uuid::nil(), &bare, &s, &[])["programLogo"]["sourceUri"]["uri"],
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
        let class = loyalty_class("338", uuid::Uuid::nil(), &OrgBrand::default(), &s, &[]);
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
            &crate::loyalty::wallet::CardCopy::default(),
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

        // Reaching the target does not end the card. The progress line shows
        // what is left AFTER the earned rewards are set aside, and the earned
        // ones get a row of their own — so a customer who has bought six with a
        // reward every five sees one waiting and one step towards the next,
        // rather than a card that has simply stopped.
        assert_eq!(progress_line(5, 5), "○─○─○─○─○", "a fresh card underneath");
        assert_eq!(progress_line(6, 5), "●─○─○─○─○", "and the sixth counted");
        assert_eq!(progress_line(130, 100), "30 / 100");

        assert_eq!(earned_line(4, 5), None, "nothing earned yet, so no row");
        assert_eq!(earned_line(5, 5).unwrap(), "●─●─●─●─●");
        assert_eq!(earned_line(6, 5).unwrap(), "●─●─●─●─●");
        // Five identical full rows would be a worse way of saying "five".
        assert_eq!(earned_line(26, 5).unwrap(), "●─●─●─●─●  ×5");
        assert_eq!(earned_label(5, 5), "Reward ready");
        assert_eq!(earned_label(12, 5), "Rewards ready");
    }

    #[test]
    fn a_zero_threshold_never_claims_a_reward_is_ready() {
        // Defensive: the column is CHECK (> 0), but a pass that told every
        // customer their reward was ready would be a bad way to find out.
        assert_eq!(progress_line(0, 0), "0 / 0");
    }
}
