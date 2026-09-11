//! The public signup site (`loyalty.madar-pos.cloud`).
//!
//! A customer scans the counter's join QR, lands on a plain webform — no app, no
//! install — gives a name and phone, and walks away with a Wallet pass. These
//! endpoints are unauthenticated and rate-limited exactly like the ordering and
//! booking public endpoints they sit beside.
//!
//! OTP is not reimplemented here. The existing `/public/otp/request` and
//! `/public/otp/verify` already prove a phone and mint the 90-day device-trust
//! token; this module simply requires that token when the branch asks for it,
//! the same way delivery intake does.

use actix_web::{HttpResponse, web};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::model::{self, MemberRow};
use super::settings::{load_effective, load_effective_rewards};
use super::wallet::{self, PassLinks};
use super::{mint_member_token, resolve_branch_org};
use crate::auth::jwt::JwtSecret;
use crate::delivery::{normalize_phone, whatsapp};
use crate::errors::{AppError, AppErrorResponse};

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BranchQuery {
    /// The counter QR of one branch. Its settings and its catalogue apply.
    pub branch_id: Option<Uuid>,
    /// The organisation's own code, for a shop that wants ONE card to hand out
    /// — a poster, a receipt footer, a link in a bio. The programme's org-level
    /// settings apply, which is also what the wallet pass has always used.
    pub org_id: Option<Uuid>,
}

/// Which programme a public link is asking about.
///
/// A branch link carries that branch's overrides; an org link carries the org's
/// defaults. Both end at the same membership — a member belongs to the SHOP,
/// never to a branch, which is why the pass has always been org-wide and only
/// the way in was not.
struct Scope {
    org_id: Uuid,
    branch_id: Option<Uuid>,
    /// The branch's name, for a page that wants to say where you are.
    branch_name: Option<String>,
}

async fn resolve_scope(
    pool: &PgPool,
    branch_id: Option<Uuid>,
    org_id: Option<Uuid>,
) -> Result<Scope, AppError> {
    if let Some(b) = branch_id {
        let name: Option<String> = sqlx::query_scalar(
            "SELECT name FROM branches WHERE id = $1 AND is_active AND deleted_at IS NULL",
        )
        .bind(b)
        .fetch_optional(pool)
        .await?;
        let branch_name = name.ok_or_else(|| AppError::NotFound("Branch not found".into()))?;
        return Ok(Scope {
            org_id: resolve_branch_org(pool, b).await?,
            branch_id: Some(b),
            branch_name: Some(branch_name),
        });
    }
    let o =
        org_id.ok_or_else(|| AppError::BadRequest("Name a branch or an organisation".into()))?;
    let exists: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM organizations WHERE id = $1 AND deleted_at IS NULL")
            .bind(o)
            .fetch_optional(pool)
            .await?;
    exists.ok_or_else(|| AppError::NotFound("Organisation not found".into()))?;
    Ok(Scope {
        org_id: o,
        branch_id: None,
        branch_name: None,
    })
}

/// What the signup page needs to render itself before anyone types anything.
#[derive(Serialize, ToSchema)]
pub struct JoinInfo {
    /// Absent for an org-wide code — the customer has not told us where they
    /// are, and nothing in the programme needs to know.
    pub branch_id: Option<Uuid>,
    pub branch_name: Option<String>,
    /// Whose programme this is, and how the page should look.
    pub brand: CardBrand,
    /// False when the program is off here — the page says so instead of taking
    /// a signup that would go nowhere.
    pub enabled: bool,
    /// The page collects an OTP only when the branch asks for one.
    pub require_otp: bool,
    /// `"points"` (earned on spend) or `"visits"` (a stamp per order) — which
    /// sentence the page writes.
    pub mode: String,
    /// The cheapest reward on offer, in `mode`'s currency.
    pub next_reward_cost: i32,
    /// EGP that earns one point — the page's "a point for every N EGP" line.
    /// Piastres on the wire, as everywhere; the page divides by 100. Only
    /// meaningful when `mode` is `"points"`.
    pub earn_piastres_per_point: i32,
    /// The rewards on offer, each with what it costs.
    pub rewards: Vec<PublicReward>,
    /// Ask for a date of birth. False means the form does not show the field —
    /// a shop that does not run birthday rewards is not given one to hold.
    pub birthday_enabled: bool,
    /// What the birthday is worth here, so the page can say what it is FOR
    /// rather than asking for a date of birth and explaining nothing.
    pub birthday_reward_amount: Option<i32>,
    pub terms: Option<String>,
    pub terms_ar: Option<String>,
}

/// How a tenant's card should look.
///
/// Every field is optional and the site falls back to Madar's own palette, so a
/// tenant who has set nothing still gets a finished card rather than an
/// unstyled one. `org_name` is NOT optional: whose card this is must always be
/// on it, however little else has been configured.
#[derive(Serialize, ToSchema, Clone)]
pub struct CardBrand {
    /// The organisation's name. Always present.
    pub org_name: String,
    /// What the programme calls itself ("Rewards", "Bean Club").
    pub program_name: String,
    pub program_name_ar: Option<String>,
    pub logo_url: Option<String>,
    /// True when the logo is a shape on transparency, so the card may repaint
    /// it in the foreground for contrast. False for a logo with its background
    /// baked in, which gets a plate to sit on instead — repainting that one
    /// would give a solid rectangle. See `orgs::branding::is_mark`.
    pub logo_is_mark: bool,
    /// The wide photograph across the card — Apple's strip, Google's hero
    /// image, and the band at the top of the web card. Absent is a finished
    /// card, not a broken one.
    pub card_image_url: Option<String>,
    /// `#RRGGBB`, validated on write.
    pub background_color: Option<String>,
    pub foreground_color: Option<String>,
    pub label_color: Option<String>,
    /// Where else to find the shop, in the order a card prints them. Empty is
    /// the common case, and the page draws nothing for it — no row, no
    /// placeholder.
    ///
    /// NOT gated on the branding tier, like `OrgBrand::social_links` it is read
    /// from: a shop's Instagram is a fact about the shop in the way its name
    /// is, so a Madar-coloured card carries the links too.
    pub social_links: Vec<PublicSocialLink>,
}

/// One place the shop can be found, as a page prints it.
///
/// The same three things the wallet passes render (`wallet::apple`,
/// `wallet::google`), so the card in the phone and the card on the page list
/// the same links in the same order.
#[derive(Serialize, ToSchema, Clone)]
pub struct PublicSocialLink {
    /// One of `orgs::social::PLATFORMS` — what the page picks its glyph by.
    pub key: String,
    /// What a human calls it. The page falls back to this where it has no
    /// glyph for `key`, so a platform added on the server still renders.
    pub label: String,
    /// `https://…` and nothing else — checked on write and again on read, see
    /// `orgs::social::links_of`.
    pub url: String,
}

/// The organisation's own identity: its name, its logo, and the palette derived
/// from that logo when it was uploaded.
///
/// Branding lives on the ORGANISATION, not on the loyalty programme. A shop has
/// one mark and one set of colours; asking them to configure it again per
/// feature is how two surfaces end up disagreeing about who the shop is. The
/// colours are DERIVED from the logo (`orgs::branding`), so there is nothing to
/// set and no way to pick two nobody can read.
fn card_brand(
    org: &crate::orgs::branding::OrgBrand,
    s: &super::settings::LoyaltySettings,
) -> CardBrand {
    CardBrand {
        org_name: org.name.clone(),
        program_name: s.program_name.clone(),
        program_name_ar: s.program_name_ar.clone(),
        logo_url: org.logo_url.clone(),
        logo_is_mark: org.logo_is_mark,
        card_image_url: org.card_image_url.clone(),
        background_color: Some(org.palette.background.clone()),
        foreground_color: Some(org.palette.foreground.clone()),
        label_color: Some(org.palette.accent.clone()),
        social_links: org
            .social_links
            .iter()
            .map(|l| PublicSocialLink {
                key: l.key.to_string(),
                label: l.label.to_string(),
                url: l.url.clone(),
            })
            .collect(),
    }
}

/// Madar's own mark, for a shop that has not uploaded one.
///
/// Exists because Google REQUIRES a loyalty class to carry a `programLogo` and
/// fetches it from its own servers, so "no logo" cannot be expressed by leaving
/// the field out — that is a rejected class and a customer seeing "something
/// went wrong". Served from the bytes already compiled into the binary, so it
/// needs no uploads directory, no static mount and no deployment step.
pub async fn brand_logo() -> HttpResponse {
    const LOGO: &[u8] = include_bytes!("../../static/wallet/logo@3x.png");
    HttpResponse::Ok()
        .content_type("image/png")
        // It changes when the binary does, and Google caches aggressively.
        .insert_header(("Cache-Control", "public, max-age=86400"))
        .body(LOGO)
}

/// A month and a day that could actually be someone's birthday.
///
/// Both or neither, both in range, and the day real for that month — 31
/// February is a typo, and one stored would be a greeting that never fires.
/// Bad input is DROPPED rather than refused: a signup is not worth failing over
/// an optional field, and the customer keeps their card.
fn valid_birthday(month: Option<i16>, day: Option<i16>) -> Option<(i16, i16)> {
    let (m, d) = (month?, day?);
    if !(1..=12).contains(&m) || d < 1 {
        return None;
    }
    // February is given 29 so that someone born on the 29th can say so; the
    // sweep decides what to do about it in a common year.
    let longest = match m {
        2 => 29,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    (d <= longest).then_some((m, d))
}

/// The settings and catalogue in force for a scope.
///
/// A branch reads its own overrides; an org reads its defaults — the same ones
/// the wallet pass has always used, so an org-wide signup and the card it
/// produces cannot describe different programmes.
async fn load_for_scope(
    pool: &PgPool,
    scope: &Scope,
) -> Result<
    (
        super::settings::LoyaltySettings,
        Vec<super::settings::RewardItem>,
    ),
    AppError,
> {
    match scope.branch_id {
        Some(b) => {
            let settings = load_effective(pool, scope.org_id, b).await?;
            let (rewards, _) = load_effective_rewards(pool, scope.org_id, b).await?;
            Ok((settings, rewards))
        }
        None => {
            let settings = super::settings::load_scope(pool, scope.org_id, None)
                .await?
                .unwrap_or_else(|| super::settings::LoyaltySettings::defaults(scope.org_id, None));
            let rewards = super::settings::load_effective_rewards_org(pool, scope.org_id).await?;
            Ok((settings, rewards))
        }
    }
}

/// One organisation's logo, composed for Google's circular slot.
///
/// Public and unauthenticated because GOOGLE fetches it, from its own servers,
/// on a schedule we do not control. It reveals nothing a customer's card does
/// not already show. The `{v}` segment is a cache key, not an input — Google
/// will not re-fetch a URL it has seen, so a shop swapping its logo needs a
/// different address or the old one lives on every card forever.
pub async fn org_logo_badge(
    pool: web::Data<PgPool>,
    path: web::Path<(Uuid, String)>,
) -> Result<HttpResponse, AppError> {
    let brand = crate::orgs::branding::load(pool.get_ref(), path.0).await?;
    let img = crate::orgs::branding::logo_badge(&brand, 512)
        .ok_or_else(|| AppError::NotFound("That shop has no logo".into()))?;
    png_response(img)
}

/// One organisation's card photograph, cropped to a banner.
pub async fn org_card_banner(
    pool: web::Data<PgPool>,
    path: web::Path<(Uuid, String)>,
) -> Result<HttpResponse, AppError> {
    let brand = crate::orgs::branding::load(pool.get_ref(), path.0).await?;
    // Google's hero is about 3:1 and much wider than Apple's strip; each wallet
    // gets a crop made for its own slot rather than one shape squeezed into both.
    let img = crate::orgs::branding::card_banner(&brand, 1032, 336)
        .ok_or_else(|| AppError::NotFound("That shop has no card image".into()))?;
    png_response(img)
}

fn png_response(img: image::DynamicImage) -> Result<HttpResponse, AppError> {
    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png)
        .map_err(|_| AppError::Internal)?;
    Ok(HttpResponse::Ok()
        .content_type("image/png")
        // Immutable against the key in the URL, which changes with the file.
        .insert_header(("Cache-Control", "public, max-age=31536000, immutable"))
        .body(buf.into_inner()))
}

/// A reward as the signup page lists it: what it is, and what it costs.
#[derive(Serialize, ToSchema)]
pub struct PublicReward {
    pub name: String,
    pub cost_currency: String,
    pub cost_amount: i32,
}

#[utoipa::path(get, path = "/public/loyalty/join-info", tag = "loyalty-public", operation_id = "loyalty_join_info", params(BranchQuery),
    responses((status = 200, body = JoinInfo), AppErrorResponse))]
pub async fn join_info(
    pool: web::Data<PgPool>,
    query: web::Query<BranchQuery>,
) -> Result<HttpResponse, AppError> {
    let scope = resolve_scope(pool.get_ref(), query.branch_id, query.org_id).await?;
    let org_id = scope.org_id;
    let (settings, rewards) = load_for_scope(pool.get_ref(), &scope).await?;

    Ok(HttpResponse::Ok().json(JoinInfo {
        branch_id: scope.branch_id,
        branch_name: scope.branch_name,
        brand: card_brand(
            &crate::orgs::branding::load(pool.get_ref(), org_id).await?,
            &settings,
        ),
        enabled: settings.enabled,
        require_otp: settings.require_otp,
        birthday_enabled: settings.birthday_enabled,
        birthday_reward_amount: settings.birthday_reward_amount,
        mode: settings.mode.clone(),
        next_reward_cost: model::reward_target(&settings, &rewards),
        earn_piastres_per_point: settings.earn_piastres_per_point,
        rewards: rewards
            .into_iter()
            .map(|r| PublicReward {
                name: r.name,
                cost_currency: r.cost_currency,
                cost_amount: r.cost_amount,
            })
            .collect(),
        terms: settings.terms.clone(),
        terms_ar: settings.terms_ar.clone(),
    }))
}

#[derive(Deserialize, ToSchema)]
pub struct JoinInput {
    /// The branch whose counter code was scanned, when one was. Absent for an
    /// org-wide code — see [`BranchQuery`].
    #[serde(default)]
    pub branch_id: Option<Uuid>,
    #[serde(default)]
    pub org_id: Option<Uuid>,
    pub name: String,
    pub phone: String,
    /// The day of their birthday, 1–12 and 1–31. Accepted ONLY where the org
    /// asked for one: a field the shop turned off must not be storable by
    /// posting past the form.
    ///
    /// No year, deliberately. A greeting needs to know WHEN, not how old — and
    /// a full date of birth is an identity credential, which is a great deal
    /// more than an annual message needs.
    #[serde(default)]
    pub birth_month: Option<i16>,
    #[serde(default)]
    pub birth_day: Option<i16>,
    /// Device-trust token from `/public/otp/verify`. Required only when the
    /// branch's `require_otp` is on.
    #[serde(default)]
    pub device_token: Option<String>,
    /// 'en' or 'ar' — the language the pass is written in.
    #[serde(default)]
    pub locale: Option<String>,
}

/// What the customer sees after signing up: their card, and the buttons — or,
/// for a phone that is already a member and has not been proved, an invitation
/// to prove it.
///
/// The member token is a bearer credential: whoever holds it holds the card,
/// the balance, the purchase history and the wallet passes. So it is handed out
/// on exactly two occasions — to a NEW member, whose token nobody else could
/// want yet, and to an existing member whose device has verified THIS phone by
/// OTP. Typing a phone number is not proof of owning it; anyone who knows a
/// customer's number can type it.
#[derive(Serialize, ToSchema)]
pub struct JoinResult {
    /// Absent when `verify_required`: the page has nothing to show yet.
    pub member_token: Option<String>,
    /// The name as the caller typed it. For a returning member the name ON FILE
    /// is not echoed until they have verified — it is a fact about the person
    /// who owns the phone, not about the person typing it.
    pub name: String,
    /// The live balance, in `mode`'s currency. Zero for a fresh member, and zero
    /// (not the real figure) while `verify_required`.
    pub balance: i32,
    pub mode: String,
    pub next_reward_cost: i32,
    pub brand: CardBrand,
    /// Absent when `verify_required`.
    pub passes: Option<PassLinks>,
    /// True when this phone was already a member — the page says "welcome back"
    /// rather than pretending to have made a new card.
    pub already_member: bool,
    /// This phone already has a card and the device has not proved it owns the
    /// phone. The page should run the ordinary OTP flow (`/public/otp/request`
    /// then `/public/otp/verify`) and POST here again with the `device_token`
    /// it is handed; the card comes back on that call.
    pub verify_required: bool,
    /// While `verify_required`: the card link was also sent to the number on
    /// file, by WhatsApp — the one channel that proves possession without a
    /// code. False when no gateway is configured or there is no public base to
    /// build a link on; the page then offers only the OTP.
    pub card_link_sent: bool,
}

#[utoipa::path(post, path = "/public/loyalty/join", tag = "loyalty-public", operation_id = "loyalty_join", request_body = JoinInput,
    responses((status = 200, body = JoinResult), AppErrorResponse))]
pub async fn join(
    pool: web::Data<PgPool>,
    secret: web::Data<JwtSecret>,
    body: web::Json<JoinInput>,
) -> Result<HttpResponse, AppError> {
    let scope = resolve_scope(pool.get_ref(), body.branch_id, body.org_id).await?;
    let org_id = scope.org_id;
    let (settings, _) = load_for_scope(pool.get_ref(), &scope).await?;
    let birthday = settings
        .birthday_enabled
        .then(|| valid_birthday(body.birth_month, body.birth_day))
        .flatten();
    if !settings.enabled {
        return Err(AppError::Conflict(
            "This branch is not running a loyalty program".into(),
        ));
    }

    let name = body.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("Please enter your name".into()));
    }
    if name.chars().count() > 80 {
        return Err(AppError::BadRequest("That name is too long".into()));
    }
    let phone = normalize_phone(&body.phone)?;

    // Has THIS device proved it owns THIS phone? The same 90-day device-trust
    // token the delivery intake issues after an OTP; it is bound to the phone,
    // so a token verified for one number says nothing about another.
    let verified = body
        .device_token
        .as_deref()
        .is_some_and(|t| whatsapp::verify_device_token(&secret.0, &phone, t));
    // A branch may demand the proof for every signup, new members included —
    // an admin turns OTP off per tenant exactly as they do for ordering and
    // bookings.
    if settings.require_otp && !verified {
        return Err(AppError::Unauthorized(
            "Verify your phone number first".into(),
        ));
    }

    let locale = match body.locale.as_deref() {
        Some("ar") => "ar",
        _ => "en",
    };

    let (_, rewards) = load_for_scope(pool.get_ref(), &scope).await?;
    let mode = settings.mode();
    let org = crate::orgs::branding::load(pool.get_ref(), org_id).await?;
    let brand = card_brand(&org, &settings);
    let next_reward_cost = model::reward_target(&settings, &rewards);

    // Joining twice from the same phone is a normal thing to do — a customer who
    // lost their pass rescans the counter QR — so an existing member is not an
    // error. But it used to return that member's card to whoever typed the
    // number, and the card IS the credential. Now: an unverified device is told
    // the phone has a card and offered the OTP, and nothing else; a verified
    // one gets the card back.
    //
    // Two people submitting the same new number at once race to the INSERT.
    // The partial unique index on (org_id, phone) makes one of them lose, and
    // ON CONFLICT turns that loss into a no-op rather than a 409 — the loser
    // then finds the row the winner made and is treated as a returning member,
    // which is what they are by the time they look.
    let inserted: Option<MemberRow> = sqlx::query_as(&format!(
        "INSERT INTO loyalty_customers \
            (org_id, phone, name, member_token, joined_branch_id, locale, \
             apple_auth_token, birth_month, birth_day) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) \
         ON CONFLICT (org_id, phone) WHERE deleted_at IS NULL DO NOTHING \
         RETURNING {}",
        model::MEMBER_COLS
    ))
    .bind(org_id)
    .bind(&phone)
    .bind(name)
    .bind(mint_member_token())
    // Reporting only, and honestly null for an org-wide code: we do not know
    // where they were, and a membership belongs to the shop rather than to a
    // branch.
    .bind(scope.branch_id)
    .bind(locale)
    // Apple authenticates pass updates with this; minted now so a pass issued
    // later needs no second write.
    .bind(mint_member_token())
    // Dropped unless the shop asked for one, and only ever as a valid PAIR — a
    // month with no day greets nobody, a day with no month greets everybody
    // twelve times.
    .bind(birthday.map(|(m, _)| m))
    .bind(birthday.map(|(_, d)| d))
    .fetch_optional(pool.get_ref())
    .await?;

    let (member, already_member) = match inserted {
        Some(fresh) => (fresh, false),
        None => {
            let existing = model::find_by_phone(pool.get_ref(), org_id, &phone)
                .await?
                // The conflict target is exactly this lookup, so a miss here
                // means the row vanished between the two statements: forgotten
                // by an admin in the same instant. Vanishingly rare; the retry
                // the page will make lands on a clean insert.
                .ok_or_else(|| AppError::Conflict("Please try again".into()))?;
            if !verified {
                return Ok(HttpResponse::Ok().json(welcome_back(
                    pool.get_ref(),
                    &existing,
                    name,
                    &settings,
                    &org,
                    brand,
                    next_reward_cost,
                )));
            }
            (existing, true)
        }
    };

    let locations = wallet::locations_for_member(pool.get_ref(), &member).await?;
    let passes = wallet::links_for(pool.get_ref(), &member, &settings, &org, &locations).await;
    Ok(HttpResponse::Ok().json(JoinResult {
        member_token: Some(member.member_token.clone()),
        name: member.name.clone(),
        balance: member.balance_in(mode),
        mode: settings.mode.clone(),
        next_reward_cost,
        brand,
        passes: Some(passes),
        already_member,
        verify_required: false,
        card_link_sent: false,
    }))
}

/// The answer for a phone that already has a card, from a device that has not
/// proved it owns the phone.
///
/// Nothing that belongs to the member leaves here: no token, no passes, no
/// balance, not even the name on file. The page gets what it needs to offer the
/// OTP, and — where a WhatsApp gateway is configured — the member gets their
/// card link on the number itself. That message is the one channel that proves
/// possession without a code: it can only be read by whoever holds the phone,
/// and if that is the person at the counter they are done; if it is not, the
/// real owner has just learned someone typed their number, which is the right
/// person to know.
#[allow(clippy::too_many_arguments)]
fn welcome_back(
    pool: &PgPool,
    member: &MemberRow,
    typed_name: &str,
    settings: &super::settings::LoyaltySettings,
    org: &crate::orgs::branding::OrgBrand,
    brand: CardBrand,
    next_reward_cost: i32,
) -> JoinResult {
    let card_link_sent = match wallet::card_link(member) {
        Some(link) if std::env::var("WHATSAPP_SERVICE_URL").is_ok() => {
            // In the member's own language, not the page's: the page is being
            // read by whoever typed the number, and the message goes to the
            // phone's owner.
            let program = if member.locale.starts_with("ar") {
                settings
                    .program_name_ar
                    .as_deref()
                    .unwrap_or(&settings.program_name)
            } else {
                &settings.program_name
            };
            let text = if member.locale.starts_with("ar") {
                format!(
                    "بطاقتك في {program} ({org}) موجودة هنا: {link}\n\n\
                     لو مش أنت اللي طلب البطاقة دي، تجاهل الرسالة — محدش يقدر يوصلها من غير الرابط ده.",
                    org = org.name
                )
            } else {
                format!(
                    "Your {program} card at {org} is here: {link}\n\n\
                     If you didn't just ask for it, ignore this — nobody can reach your card without this link.",
                    org = org.name
                )
            };
            whatsapp::send_message(pool.clone(), member.phone.clone(), text);
            true
        }
        _ => false,
    };
    JoinResult {
        member_token: None,
        name: typed_name.to_string(),
        balance: 0,
        mode: settings.mode.clone(),
        next_reward_cost,
        brand,
        passes: None,
        already_member: true,
        verify_required: true,
        card_link_sent,
    }
}

/// The member's own card page — what they see when they open the link again.
///
/// The token in the path is the member's secret, which is why this returns only
/// what the pass already shows and never the phone number in full.
#[derive(Serialize, ToSchema)]
pub struct CardView {
    pub name: String,
    /// The live balance, in `mode`'s currency.
    pub balance: i32,
    pub mode: String,
    pub next_reward_cost: i32,
    /// Rewards the balance has already earned — a card does not stop at full.
    pub rewards_ready: i32,
    /// Progress towards the next one, after the earned ones are set aside.
    pub progress_to_next: i32,
    pub points_to_next_reward: i32,
    pub can_redeem: bool,
    pub member_token: String,
    pub rewards: Vec<PublicReward>,
    pub passes: PassLinks,
    /// Whose card this is, and how it should look.
    pub brand: CardBrand,
    /// They have asked this shop to stop sending them things.
    pub marketing_opt_out: bool,
}

/// One past visit, as the customer's own page shows it.
#[derive(Debug, Serialize, ToSchema)]
pub struct PastOrder {
    pub id: Uuid,
    pub placed_at: chrono::DateTime<chrono::Utc>,
    pub branch_name: String,
    /// In piastres, like every other figure on the wire.
    pub total: i32,
    /// What they had. The customer asked for this to be here; see the note on
    /// the handler about who else can see it.
    pub items: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PastOrders {
    pub orders: Vec<PastOrder>,
}

/// The member's own purchase history.
///
/// Authenticated by the token in the URL — the same one their pass carries and
/// the till scans — because that is the only credential a loyalty member has.
/// Which means anyone holding the link can read it, and that is worth stating
/// rather than glossing: a forwarded card link forwards the history with it.
/// The shop decides whether to run a programme on those terms, and the privacy
/// policy says so plainly.
///
/// Voided orders are excluded. A sale that was reversed is not something the
/// customer bought, and showing it invites a question the page cannot answer.
#[utoipa::path(get, path = "/public/loyalty/card/{token}/orders", tag = "loyalty-public",
    operation_id = "loyalty_card_orders",
    params(("token" = String, Path, description = "Member token from the pass barcode")),
    responses((status = 200, body = PastOrders), AppErrorResponse))]
pub async fn card_orders(
    pool: web::Data<PgPool>,
    token: web::Path<String>,
) -> Result<HttpResponse, AppError> {
    let member = model::find_by_token(pool.get_ref(), token.as_str())
        .await?
        .ok_or_else(|| AppError::NotFound("Card not found".into()))?;

    #[derive(sqlx::FromRow)]
    struct Row {
        id: Uuid,
        placed_at: chrono::DateTime<chrono::Utc>,
        branch_name: String,
        total: i32,
        items: Vec<String>,
    }
    // Capped rather than paged: a card page is a glance, not an archive, and a
    // customer who wants every receipt they have ever had is asking the shop.
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT o.id, o.created_at AS placed_at, b.name AS branch_name, \
                o.total_amount AS total, \
                COALESCE(ARRAY( \
                    SELECT CASE WHEN i.quantity > 1 \
                                THEN i.quantity || ' × ' || i.item_name \
                                ELSE i.item_name END \
                      FROM order_items i WHERE i.order_id = o.id \
                     ORDER BY i.item_name), '{}') AS items \
           FROM orders o \
           JOIN branches b ON b.id = o.branch_id \
          WHERE o.loyalty_customer_id = $1 AND o.voided_at IS NULL \
          ORDER BY o.created_at DESC \
          LIMIT 50",
    )
    .bind(member.id)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(PastOrders {
        orders: rows
            .into_iter()
            .map(|r| PastOrder {
                id: r.id,
                placed_at: r.placed_at,
                branch_name: r.branch_name,
                total: r.total,
                items: r.items,
            })
            .collect(),
    }))
}

/// What a customer can change about their own card, without an account.
///
/// The token in the URL is the credential — the same one their pass carries and
/// the till scans. That is deliberate: a person who has just been messaged must
/// be able to stop the messages by tapping the link in the message, not by
/// remembering a password they never made.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CardPreferences {
    /// Stop sending marketing. Covers the birthday greeting as well as the
    /// win-back: someone asking us to stop is asking the SHOP to stop, not to
    /// be excluded from one campaign.
    #[serde(default)]
    pub marketing_opt_out: Option<bool>,
    /// The language they are reading this page in.
    ///
    /// Sent by the page itself rather than chosen in a form. We stored whatever
    /// their phone said at signup, and a phone that has since changed language
    /// is a customer still being written to in the wrong one. Opening their own
    /// card is the moment we can tell.
    #[serde(default)]
    pub locale: Option<String>,
}

#[utoipa::path(post, path = "/public/loyalty/card/{token}/preferences", tag = "loyalty-public",
    operation_id = "set_loyalty_card_preferences",
    params(("token" = String, Path, description = "Member token from the pass barcode")),
    request_body = CardPreferences,
    responses((status = 204, description = "Saved"), AppErrorResponse))]
pub async fn set_preferences(
    pool: web::Data<PgPool>,
    token: web::Path<String>,
    body: web::Json<CardPreferences>,
) -> Result<HttpResponse, AppError> {
    let member = model::find_by_token(pool.get_ref(), token.as_str())
        .await?
        .ok_or_else(|| AppError::NotFound("Card not found".into()))?;
    let locale = match body.locale.as_deref() {
        Some(l) if l.starts_with("ar") => Some("ar"),
        Some(_) => Some("en"),
        None => None,
    };
    // COALESCE so a page that only reports its language cannot silently switch
    // marketing back on for someone who turned it off.
    sqlx::query(
        "UPDATE loyalty_customers \
            SET marketing_opt_out = COALESCE($2, marketing_opt_out), \
                locale = COALESCE($3, locale) \
          WHERE id = $1",
    )
    .bind(member.id)
    .bind(body.marketing_opt_out)
    .bind(locale)
    .execute(pool.get_ref())
    .await?;
    Ok(HttpResponse::NoContent().finish())
}

#[utoipa::path(get, path = "/public/loyalty/card/{token}", tag = "loyalty-public", operation_id = "loyalty_card",
    params(("token" = String, Path, description = "Member token from the pass barcode")),
    responses((status = 200, body = CardView), AppErrorResponse))]
pub async fn card(
    pool: web::Data<PgPool>,
    token: web::Path<String>,
) -> Result<HttpResponse, AppError> {
    let member = model::find_by_token(pool.get_ref(), token.as_str())
        .await?
        .ok_or_else(|| AppError::NotFound("Card not found".into()))?;
    // The org default is the right scope here: a customer opening their card at
    // home is not standing in any particular branch.
    let settings = super::settings::load_scope(pool.get_ref(), member.org_id, None)
        .await?
        .unwrap_or_else(|| super::settings::LoyaltySettings::defaults(member.org_id, None));
    let catalogue =
        super::settings::load_effective_rewards_org(pool.get_ref(), member.org_id).await?;
    let mode = settings.mode();
    let target = model::reward_target(&settings, &catalogue);
    let org = crate::orgs::branding::load(pool.get_ref(), member.org_id).await?;
    let brand = card_brand(&org, &settings);
    let locations = wallet::locations_for_member(pool.get_ref(), &member).await?;
    let passes = wallet::links_for(pool.get_ref(), &member, &settings, &org, &locations).await;
    let marketing_opt_out = member.marketing_opt_out;
    let view = member.view(mode, target);
    Ok(HttpResponse::Ok().json(CardView {
        name: view.name,
        balance: view.balance,
        mode: view.mode,
        next_reward_cost: view.next_reward_cost,
        rewards_ready: view.rewards_ready,
        progress_to_next: view.progress_to_next,
        points_to_next_reward: view.points_to_next_reward,
        can_redeem: view.can_redeem,
        brand,
        marketing_opt_out,
        member_token: token.into_inner(),
        rewards: catalogue
            .into_iter()
            .map(|r| PublicReward {
                name: r.name,
                cost_currency: r.cost_currency,
                cost_amount: r.cost_amount,
            })
            .collect(),
        passes,
    }))
}

/// Download the signed `.pkpass`.
///
/// 503s until Apple credentials are configured — see
/// `wallet::apple::sign_manifest`. The button that leads here is only rendered
/// when `apple::is_configured()`, so a customer does not meet this by accident.
#[utoipa::path(get, path = "/public/loyalty/pass/{token}/apple.pkpass", tag = "loyalty-public", operation_id = "loyalty_apple_pass",
    params(("token" = String, Path, description = "Member token")),
    responses((status = 200, description = "Apple Wallet pass"), AppErrorResponse))]
pub async fn apple_pass(
    pool: web::Data<PgPool>,
    token: web::Path<String>,
) -> Result<HttpResponse, AppError> {
    let member = model::find_by_token(pool.get_ref(), token.as_str())
        .await?
        .ok_or_else(|| AppError::NotFound("Card not found".into()))?;
    // The same builder the device's own refetch uses, so the pass a customer
    // downloads and the pass their phone later pulls are the same shape.
    let bytes = wallet::apple::build_pass_for(pool.get_ref(), &member).await?;

    // Record the serial so pass updates can find this member later.
    sqlx::query(
        "UPDATE loyalty_customers SET apple_serial = $2, pass_updated_at = now() \
         WHERE id = $1 AND apple_serial IS NULL",
    )
    .bind(member.id)
    .bind(member.id.to_string())
    .execute(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok()
        .content_type("application/vnd.apple.pkpass")
        .append_header((
            "Content-Disposition",
            "attachment; filename=\"madar.pkpass\"",
        ))
        .body(bytes))
}

/// The member's QR as a PNG.
///
/// Rendered server-side with the same renderer the printed cards use, rather
/// than shipping a QR library to the browser — and rendered from the token
/// DIRECTLY, never through a Shlink short link: a short URL is a public
/// redirect, and the member token is the one value here that has to stay
/// between the customer and the till.
///
/// This is the fallback that makes the program usable before either wallet is
/// configured — and the answer for a customer whose phone has no wallet app.
#[utoipa::path(get, path = "/public/loyalty/card/{token}/qr.png", tag = "loyalty-public",
    operation_id = "loyalty_card_qr",
    params(("token" = String, Path, description = "Member token")),
    responses((status = 200, description = "Member QR as a PNG"), AppErrorResponse))]
pub async fn card_qr(
    pool: web::Data<PgPool>,
    token: web::Path<String>,
) -> Result<HttpResponse, AppError> {
    // Resolve first: rendering a QR of an arbitrary string a caller supplied
    // would turn this into an open QR generator for anyone who found the URL.
    let member = model::find_by_token(pool.get_ref(), token.as_str())
        .await?
        .ok_or_else(|| AppError::NotFound("Card not found".into()))?;
    let png = crate::qr_card::render_qr_receipt_png(&member.member_token, 8)
        .map_err(|_| AppError::Internal)?;
    Ok(HttpResponse::Ok()
        .content_type("image/png")
        // The token never changes, but the pass and page around it might, and a
        // stale cached QR is indistinguishable from a broken card.
        .append_header(("Cache-Control", "private, max-age=3600"))
        .body(png))
}

#[cfg(test)]
mod birthday_tests {
    use super::valid_birthday;

    #[test]
    fn a_real_day_is_kept() {
        assert_eq!(valid_birthday(Some(3), Some(17)), Some((3, 17)));
        assert_eq!(valid_birthday(Some(1), Some(1)), Some((1, 1)));
        assert_eq!(valid_birthday(Some(12), Some(31)), Some((12, 31)));
        // A leap-day birthday is a real birthday. The sweep decides what to do
        // about it in a common year; refusing to record it is not the answer.
        assert_eq!(valid_birthday(Some(2), Some(29)), Some((2, 29)));
    }

    #[test]
    fn a_day_that_month_does_not_have_is_dropped() {
        // 31 February is a typo, and one stored would be a greeting that never
        // fires — the worst kind, because nothing ever reports it.
        assert_eq!(valid_birthday(Some(2), Some(30)), None);
        assert_eq!(valid_birthday(Some(4), Some(31)), None);
        assert_eq!(valid_birthday(Some(6), Some(31)), None);
        assert_eq!(valid_birthday(Some(9), Some(31)), None);
        assert_eq!(valid_birthday(Some(11), Some(31)), None);
    }

    #[test]
    fn half_a_birthday_is_no_birthday() {
        // A month with no day greets nobody; a day with no month greets
        // everybody, twelve times a year.
        assert_eq!(valid_birthday(Some(5), None), None);
        assert_eq!(valid_birthday(None, Some(5)), None);
        assert_eq!(valid_birthday(None, None), None);
    }

    #[test]
    fn nonsense_is_dropped_rather_than_clamped() {
        // Clamping would silently move someone's birthday.
        assert_eq!(valid_birthday(Some(0), Some(10)), None);
        assert_eq!(valid_birthday(Some(13), Some(10)), None);
        assert_eq!(valid_birthday(Some(3), Some(0)), None);
        assert_eq!(valid_birthday(Some(3), Some(32)), None);
        assert_eq!(valid_birthday(Some(-1), Some(-1)), None);
    }
}
