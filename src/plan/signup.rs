//! Self-service signup (FREEMIUM_SPEC_2026-10-11.md §4.7): a shop makes itself
//! on the Free plan, proving its phone by WhatsApp first.
//!
//! - `POST /public/signup/otp` sends a six-digit code to the phone (the same
//!   WhatsApp gateway and `delivery_otp` store as every other phone proof, with
//!   its own purpose, `signup`).
//! - `POST /public/signup/verify` checks the code ON THE SERVER, at the code
//!   step (a wrong code is refused there, before the shop's details are asked
//!   for), and answers with a signup token good for 30 minutes.
//! - `POST /public/signup` takes that token and creates the organization with
//!   `orgs::provision::provision` (the call `POST /orgs/provision` makes), as
//!   `plan = free`, `signup_source = self`, modules `{pos}`, and answers with a
//!   session in the shape `POST /auth/login` returns, so the dashboard signs
//!   straight in.
//!
//! Abuse limits: per IP (the `SIGNUP` governors) and per phone (one code a
//! minute, five an hour). A phone or email that already owns a shop is a 409
//! with a "sign in instead" hint, checked BEFORE a code is sent for the phone
//! and again at signup. Not behind any plan gate: the signup itself is how an
//! org comes to have a plan.

use actix_governor::Governor;
use actix_web::{HttpResponse, middleware::Condition, web};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::auth::handlers::LoginResponse;
use crate::auth::jwt::{JwtSecret, create_token};
use crate::delivery::{normalize_phone, whatsapp};
use crate::errors::{AppError, AppErrorResponse};
use crate::models::{User, UserPublic, UserRole};
use crate::orgs::provision::{
    ProvisionBranch, ProvisionOpts, ProvisionOrgRequest, ProvisionOwner, provision,
};
use crate::rate_limit::{PeerIpOrLocalhost, rate_limiting_enabled};

/// The code lives five minutes, like every phone proof.
const CODE_TTL_SECONDS: i64 = 300;
/// One code per phone per minute.
const COOLDOWN_SECONDS: i64 = 60;
/// At most this many codes to one phone per hour.
const MAX_CODES_PER_HOUR: i64 = 5;
const PURPOSE: &str = "signup";
/// How long a verified phone stays good for finishing the signup.
const PROOF_TTL_MINUTES: i64 = 30;

#[derive(Debug, Deserialize, ToSchema)]
pub struct SignupOtpRequest {
    /// The owner's phone (any common Egyptian spelling; stored canonical).
    #[schema(example = "01001234567")]
    pub phone: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SignupOtpResponse {
    pub sent: bool,
    /// How long the code is good for.
    pub expires_in_seconds: i64,
    /// How long before another code may be asked for.
    pub resend_after_seconds: i64,
}

/// Six digits from the OS RNG (v4 uuid bytes), for an account's proof.
fn new_code() -> String {
    let b = *Uuid::new_v4().as_bytes();
    let n = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) % 1_000_000;
    format!("{n:06}")
}

fn phone_taken_error() -> AppError {
    AppError::Refused {
        code: "SIGNUP_PHONE_TAKEN",
        reason: "A shop already uses this phone number. Sign in instead.".into(),
    }
}

fn email_taken_error() -> AppError {
    AppError::Refused {
        code: "SIGNUP_EMAIL_TAKEN",
        reason: "An account with this email already exists. Sign in instead.".into(),
    }
}

async fn phone_has_a_shop(pool: &PgPool, phone: &str) -> Result<bool, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users
                        WHERE phone = $1 AND role = 'org_admin' AND deleted_at IS NULL)",
    )
    .bind(phone)
    .fetch_one(pool)
    .await?)
}

/// Send a code by WhatsApp to prove the owner's phone.
///
/// 409 `SIGNUP_PHONE_TAKEN` when a shop already owns the phone; 429 when a code
/// went out in the last minute (`SIGNUP_OTP_COOLDOWN`) or five did in the last
/// hour (`SIGNUP_OTP_LIMIT`); the per-IP governor answers 429 `RATE_LIMITED`.
#[utoipa::path(post, path = "/public/signup/otp", tag = "signup",
    operation_id = "signup_otp", request_body = SignupOtpRequest,
    responses(
        (status = 200, body = SignupOtpResponse),
        (status = 409, description = "SIGNUP_PHONE_TAKEN: sign in instead"),
        (status = 429, description = "SIGNUP_OTP_COOLDOWN, SIGNUP_OTP_LIMIT or RATE_LIMITED"),
        AppErrorResponse))]
pub async fn otp_request(
    pool: web::Data<PgPool>,
    body: web::Json<SignupOtpRequest>,
) -> Result<HttpResponse, AppError> {
    let phone = normalize_phone(&body.phone)?;
    if phone_has_a_shop(pool.get_ref(), &phone).await? {
        return Err(phone_taken_error());
    }
    let (last_minute, last_hour): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE created_at > now() - make_interval(secs => $2)),
                count(*)
           FROM delivery_otp
          WHERE phone = $1 AND purpose = $3 AND created_at > now() - interval '1 hour'",
    )
    .bind(&phone)
    .bind(COOLDOWN_SECONDS as f64)
    .bind(PURPOSE)
    .fetch_one(pool.get_ref())
    .await?;
    if last_minute > 0 {
        return Err(AppError::TooManyRequests {
            code: "SIGNUP_OTP_COOLDOWN",
            reason: "A code was just sent. Please wait a minute.".into(),
        });
    }
    if last_hour >= MAX_CODES_PER_HOUR {
        return Err(AppError::TooManyRequests {
            code: "SIGNUP_OTP_LIMIT",
            reason: "Too many codes for this number. Try again later.".into(),
        });
    }
    let code = new_code();
    sqlx::query(
        "INSERT INTO delivery_otp (phone, code, purpose, expires_at)
         VALUES ($1, $2, $3, now() + make_interval(secs => $4))",
    )
    .bind(&phone)
    .bind(&code)
    .bind(PURPOSE)
    .bind(CODE_TTL_SECONDS as f64)
    .execute(pool.get_ref())
    .await?;
    whatsapp::send_message(
        pool.get_ref().clone(),
        phone,
        whatsapp::build_otp_message(&code),
    );
    Ok(HttpResponse::Ok().json(SignupOtpResponse {
        sent: true,
        expires_in_seconds: CODE_TTL_SECONDS,
        resend_after_seconds: COOLDOWN_SECONDS,
    }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SignupRequest {
    /// The token `POST /public/signup/verify` answered with: the proof that the
    /// owner holds the phone. The phone is the one it was issued for.
    #[serde(default)]
    pub signup_token: Option<String>,
    /// Deprecated, for the dashboard build before the verify step: the phone
    /// and code checked here instead of by `/public/signup/verify`.
    #[serde(default)]
    #[schema(example = "01001234567")]
    pub phone: Option<String>,
    #[serde(default)]
    #[schema(example = "483920")]
    pub code: Option<String>,
    /// The shop's name. It names the organization and its first branch.
    #[schema(example = "The Rue")]
    pub shop_name: String,
    pub owner_name: String,
    #[schema(example = "owner@therue.com")]
    pub email: String,
    /// At least 8 characters.
    pub password: String,
    /// `cafe` | `restaurant`: the role template the roles start from.
    #[schema(example = "cafe")]
    pub template: String,
    /// The version of the terms the owner accepted (stored with the org).
    #[schema(example = "2026-10")]
    pub terms_version: String,
    /// The campaign tags the signup page was opened with, if any. Stored with the
    /// org for analysis only; a missing or odd value never refuses a signup.
    #[serde(default)]
    pub attribution: Option<SignupAttribution>,
}

/// Campaign tags from the signup page's URL (the landing copies them there).
#[derive(Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct SignupAttribution {
    #[schema(example = "instagram")]
    pub utm_source: Option<String>,
    #[schema(example = "bio")]
    pub utm_medium: Option<String>,
    #[schema(example = "free-plan")]
    pub utm_campaign: Option<String>,
    pub utm_term: Option<String>,
    pub utm_content: Option<String>,
    /// Meta's click id, for a later Conversions API event.
    pub fbclid: Option<String>,
    /// Google's click id.
    pub gclid: Option<String>,
}

impl SignupAttribution {
    /// Trimmed, each capped at 200 characters, empty ones dropped; `None` when
    /// nothing is left. Untrusted input: it is stored, never interpreted.
    pub fn cleaned(self) -> Option<serde_json::Value> {
        let clean = |v: Option<String>| {
            v.map(|s| s.trim().chars().take(200).collect::<String>())
                .filter(|s| !s.is_empty())
        };
        let a = SignupAttribution {
            utm_source: clean(self.utm_source),
            utm_medium: clean(self.utm_medium),
            utm_campaign: clean(self.utm_campaign),
            utm_term: clean(self.utm_term),
            utm_content: clean(self.utm_content),
            fbclid: clean(self.fbclid),
            gclid: clean(self.gclid),
        };
        let v = serde_json::to_value(a).ok()?;
        let m: serde_json::Map<_, _> = v
            .as_object()?
            .iter()
            .filter(|(_, x)| !x.is_null())
            .map(|(k, x)| (k.clone(), x.clone()))
            .collect();
        (!m.is_empty()).then_some(serde_json::Value::Object(m))
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SignupVerifyRequest {
    #[schema(example = "01001234567")]
    pub phone: String,
    /// The six-digit code from WhatsApp.
    #[schema(example = "483920")]
    pub code: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SignupVerifyResponse {
    /// Send it with `POST /public/signup`. Single use.
    pub signup_token: String,
    /// How long it is good for.
    pub expires_in_seconds: i64,
}

/// A session like `POST /auth/login`'s, for the owner just made, plus the
/// ids the dashboard lands on.
#[derive(Serialize, ToSchema)]
pub struct SignupResponse {
    #[serde(flatten)]
    pub session: LoginResponse,
    pub org_id: Uuid,
    /// The first (and, on Free, only) branch.
    pub branch_id: Uuid,
}

/// `<ascii part of the name>-<4 random chars>`: unique, valid and unused (a Free
/// shop has no subdomain, so nobody types it).
fn auto_slug(name: &str) -> String {
    let mut base: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while base.contains("--") {
        base = base.replace("--", "-");
    }
    let base = base.trim_matches('-');
    let base: String = base.chars().take(40).collect();
    let base = base.trim_matches('-');
    let base = if base.len() < 2 { "shop" } else { base };
    let tail: String = Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .take(4)
        .collect();
    format!("{base}-{tail}")
}

/// The server's answer to a code: refused with a stable `code` the signup page
/// words in its own language.
fn code_refusal(e: AppError) -> AppError {
    match e {
        AppError::BadRequest(m) => {
            let code = if m.starts_with("Too many") {
                "SIGNUP_CODE_LOCKED"
            } else if m.starts_with("No active") {
                "SIGNUP_CODE_EXPIRED"
            } else {
                "SIGNUP_CODE_WRONG"
            };
            AppError::Coded {
                status: 400,
                code,
                reason: m,
            }
        }
        other => other,
    }
}

fn proof_hash(token: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A token nobody can guess: 256 bits from the OS RNG (two v4 uuids).
fn new_proof() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// Check the code at the code step.
///
/// The server compares it (constant time, five attempts per code across every
/// caller) and, when it is right, answers with a signup token for
/// `POST /public/signup`. 400 `SIGNUP_CODE_WRONG` / `SIGNUP_CODE_EXPIRED` /
/// `SIGNUP_CODE_LOCKED`; 409 `SIGNUP_PHONE_TAKEN` when the phone already owns
/// a shop (said only to someone who proved it).
#[utoipa::path(post, path = "/public/signup/verify", tag = "signup",
    operation_id = "signup_verify", request_body = SignupVerifyRequest,
    responses(
        (status = 200, description = "The code is right", body = SignupVerifyResponse),
        (status = 400, description = "SIGNUP_CODE_WRONG, SIGNUP_CODE_EXPIRED or SIGNUP_CODE_LOCKED"),
        (status = 409, description = "SIGNUP_PHONE_TAKEN: sign in instead"),
        (status = 429, description = "RATE_LIMITED"),
        AppErrorResponse))]
pub async fn verify(
    pool: web::Data<PgPool>,
    body: web::Json<SignupVerifyRequest>,
) -> Result<HttpResponse, AppError> {
    let phone = normalize_phone(&body.phone)?;
    let code = body.code.trim();
    if code.is_empty()
        || code.len() > crate::delivery::MAX_OTP_CODE_LEN
        || !code.chars().all(|c| c.is_ascii_digit())
    {
        return Err(code_refusal(AppError::BadRequest("Incorrect code.".into())));
    }
    let otp = crate::delivery::otp::check_code(pool.get_ref(), &phone, code, PURPOSE)
        .await
        .map_err(code_refusal)?;
    if phone_has_a_shop(pool.get_ref(), &phone).await? {
        return Err(phone_taken_error());
    }
    // A new token each time the right code is sent: an earlier one stops working.
    let token = new_proof();
    sqlx::query(
        "UPDATE delivery_otp SET verified_at = now(), proof_hash = $2
          WHERE id = $1 AND consumed_at IS NULL",
    )
    .bind(otp)
    .bind(proof_hash(&token))
    .execute(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(SignupVerifyResponse {
        signup_token: token,
        expires_in_seconds: PROOF_TTL_MINUTES * 60,
    }))
}

/// Make a shop on the Free plan.
///
/// 400 for a wrong or expired code (or a bad field), 409 `SIGNUP_PHONE_TAKEN` /
/// `SIGNUP_EMAIL_TAKEN` when the phone or email already owns an account (sign in
/// instead), 429 from the per-IP governor. A code is spent only by a signup that
/// succeeds: a duplicate email leaves it good for another try (within its five
/// attempts).
#[utoipa::path(post, path = "/public/signup", tag = "signup",
    operation_id = "signup", request_body = SignupRequest,
    responses(
        (status = 201, description = "The shop, and a session for its owner", body = SignupResponse),
        (status = 409, description = "SIGNUP_PHONE_TAKEN or SIGNUP_EMAIL_TAKEN: sign in instead"),
        (status = 429, description = "RATE_LIMITED"),
        AppErrorResponse))]
pub async fn signup(
    pool: web::Data<PgPool>,
    secret: web::Data<JwtSecret>,
    body: web::Json<SignupRequest>,
) -> Result<HttpResponse, AppError> {
    let b = body.into_inner();
    // Fields first: a typo costs nothing.
    let shop_name = b.shop_name.trim().to_string();
    if shop_name.chars().count() < 2 || shop_name.chars().count() > 80 {
        return Err(AppError::BadRequest(
            "shop_name must be 2 to 80 characters".into(),
        ));
    }
    let owner_name = b.owner_name.trim().to_string();
    if owner_name.is_empty() || owner_name.chars().count() > 80 {
        return Err(AppError::BadRequest(
            "owner_name must be 1 to 80 characters".into(),
        ));
    }
    let email = crate::auth::email::normalize(&b.email);
    if !email.contains('@') || email.len() > 254 {
        return Err(AppError::BadRequest("A valid email is required".into()));
    }
    if b.password.chars().count() < 8 {
        return Err(AppError::BadRequest(
            "password must be at least 8 characters".into(),
        ));
    }
    if !matches!(b.template.as_str(), "cafe" | "restaurant") {
        return Err(AppError::BadRequest(
            "template must be cafe or restaurant".into(),
        ));
    }
    let terms = b.terms_version.trim().to_string();
    if terms.is_empty() || terms.len() > 40 {
        return Err(AppError::BadRequest(
            "terms_version is required: the terms must be accepted".into(),
        ));
    }
    // The phone proof: the verify step's token (the phone is the one it was
    // issued for), or, from a dashboard build before that step, the code itself.
    let (otp, phone) = match (b.signup_token.as_deref(), b.code.as_deref()) {
        (Some(token), _) => {
            let row: Option<(Uuid, String)> = sqlx::query_as(
                "SELECT id, phone FROM delivery_otp
                  WHERE purpose = $1 AND proof_hash = $2 AND consumed_at IS NULL
                    AND verified_at > now() - make_interval(mins => $3)",
            )
            .bind(PURPOSE)
            .bind(proof_hash(token.trim()))
            .bind(PROOF_TTL_MINUTES as i32)
            .fetch_optional(pool.get_ref())
            .await?;
            let (id, phone) = row.ok_or_else(|| AppError::Coded {
                status: 400,
                code: "SIGNUP_TOKEN_EXPIRED",
                reason: "Your phone check has expired. Verify your number again.".into(),
            })?;
            (id, phone)
        }
        (None, Some(code)) => {
            let phone = normalize_phone(b.phone.as_deref().unwrap_or_default())?;
            let code = code.trim();
            if code.is_empty()
                || code.len() > crate::delivery::MAX_OTP_CODE_LEN
                || !code.chars().all(|c| c.is_ascii_digit())
            {
                return Err(code_refusal(AppError::BadRequest("Incorrect code.".into())));
            }
            let otp = crate::delivery::otp::check_code(pool.get_ref(), &phone, code, PURPOSE)
                .await
                .map_err(code_refusal)?;
            (otp, phone)
        }
        (None, None) => {
            return Err(AppError::BadRequest(
                "signup_token is required: verify the phone first".into(),
            ));
        }
    };

    // Already an account? Said only to someone who proved the phone.
    if phone_has_a_shop(pool.get_ref(), &phone).await? {
        return Err(phone_taken_error());
    }
    let email_taken: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users WHERE lower(email) = $1 AND deleted_at IS NULL)",
    )
    .bind(&email)
    .fetch_one(pool.get_ref())
    .await?;
    if email_taken {
        return Err(email_taken_error());
    }

    // Spend the code at once, so two requests carrying it cannot both create a
    // shop; a failure below gives it back.
    let spent = sqlx::query(
        "UPDATE delivery_otp SET consumed_at = now() WHERE id = $1 AND consumed_at IS NULL",
    )
    .bind(otp)
    .execute(pool.get_ref())
    .await?
    .rows_affected();
    if spent != 1 {
        return Err(AppError::BadRequest(
            "No active code — request a new one.".into(),
        ));
    }

    let made = make_shop(
        pool.get_ref(),
        &b.template,
        &shop_name,
        &owner_name,
        &email,
        &b.password,
        &terms,
        &phone,
        b.attribution.and_then(SignupAttribution::cleaned),
    )
    .await;
    let done = match made {
        Ok(d) => d,
        Err(e) => {
            let _ = sqlx::query("UPDATE delivery_otp SET consumed_at = NULL WHERE id = $1")
                .bind(otp)
                .execute(pool.get_ref())
                .await;
            // A unique violation on the email is the race the check above
            // cannot close.
            return Err(match e {
                AppError::Db(ref db)
                    if db
                        .as_database_error()
                        .and_then(|d| d.constraint())
                        .is_some_and(|c| c.contains("email")) =>
                {
                    email_taken_error()
                }
                other => other,
            });
        }
    };

    let user: User = sqlx::query_as(
        "SELECT id, org_id, name, email, phone, password_hash, pin_hash, role, is_active,
                last_login_at, created_at, updated_at, deleted_at
           FROM users WHERE id = $1",
    )
    .bind(done.owner_id)
    .fetch_one(pool.get_ref())
    .await?;
    let token = create_token(&secret, user.id, user.org_id, UserRole::OrgAdmin, None, 24)
        .map_err(|_| AppError::Internal)?;
    sqlx::query("UPDATE users SET last_login_at = NOW() WHERE id = $1")
        .bind(user.id)
        .execute(pool.get_ref())
        .await?;
    let (tax_policy, currency_code, require_table_for_orders) =
        crate::auth::handlers::resolve_tax_context(pool.get_ref(), user.org_id, None).await?;
    let plan = crate::plan::plan_name(pool.get_ref(), user.org_id).await?;
    let session = LoginResponse {
        token,
        user: UserPublic::from(user),
        tax_rate: tax_policy.tax_rate,
        tax_policy,
        require_table_for_orders,
        currency_code,
        open_till: None,
        plan,
        // A new shop has no add-ons yet (the super admin grants them).
        addons: vec![],
    };
    Ok(HttpResponse::Created().json(SignupResponse {
        session,
        org_id: done.org.id,
        branch_id: done.branch_id,
    }))
}

#[allow(clippy::too_many_arguments)]
async fn make_shop(
    pool: &PgPool,
    template: &str,
    shop_name: &str,
    owner_name: &str,
    email: &str,
    password: &str,
    terms: &str,
    phone: &str,
    attribution: Option<serde_json::Value>,
) -> Result<crate::orgs::provision::ProvisionedOrg, AppError> {
    let mut last = None;
    // The slug carries four random characters: a clash is a retry.
    for _ in 0..5 {
        let req = ProvisionOrgRequest {
            name: shop_name.to_string(),
            slug: auto_slug(shop_name),
            template: template.to_string(),
            currency_code: None,
            timezone: None,
            tax_rate: None,
            branch: ProvisionBranch {
                name: shop_name.to_string(),
                address: None,
                phone: Some(phone.to_string()),
            },
            owner: ProvisionOwner {
                name: owner_name.to_string(),
                email: email.to_string(),
                password: password.to_string(),
                pin: None,
            },
            modules: Some(vec!["pos".to_string()]),
        };
        let opts = ProvisionOpts {
            plan: madar_plan::Plan::Free,
            signup_source: "self",
            terms_version: Some(terms.to_string()),
            owner_phone: Some(phone.to_string()),
            attribution: attribution.clone(),
        };
        match provision(pool, req, opts).await {
            Err(AppError::Conflict(m)) if m.contains("Slug") => last = Some(AppError::Conflict(m)),
            other => return other,
        }
    }
    Err(last.unwrap_or(AppError::Internal))
}

/// `/public/signup/*`, each with its own per-IP governor.
pub fn configure(cfg: &mut web::ServiceConfig) {
    let otp_gov = crate::rate_limit::route_governor(PeerIpOrLocalhost, "SIGNUP", "OTP");
    let verify_gov = crate::rate_limit::route_governor(PeerIpOrLocalhost, "SIGNUP", "VERIFY");
    let create_gov = crate::rate_limit::route_governor(PeerIpOrLocalhost, "SIGNUP", "CREATE");
    let limited = rate_limiting_enabled();
    cfg.service(
        web::resource("/public/signup/otp")
            .wrap(Condition::new(limited, Governor::new(&otp_gov)))
            .route(web::post().to(otp_request)),
    )
    .service(
        web::resource("/public/signup/verify")
            .wrap(Condition::new(limited, Governor::new(&verify_gov)))
            .route(web::post().to(verify)),
    )
    .service(
        web::resource("/public/signup")
            .wrap(Condition::new(limited, Governor::new(&create_gov)))
            .route(web::post().to(signup)),
    );
}
