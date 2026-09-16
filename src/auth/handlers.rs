use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    auth::jwt::{Claims, JwtSecret, create_token},
    errors::{AppError, AppErrorResponse},
    models::{User, UserPublic, UserRole},
};

// ── Request / Response types ─────────────────────────────────

/// Login is dual-mode:
///
/// - **Email + password** (admins, managers, super-admins): supply
///   `email` and `password`. `org_id` is optional — if provided, the
///   user must belong to that org; if omitted, lookup is by email only.
/// - **PIN + name** (tellers): supply `name`, `pin`, and **`branch_id`**
///   (required). The teller must be assigned to that branch. `org_id` is
///   derived server-side from the branch — never trusted from the client.
#[derive(Deserialize, ToSchema)]
pub struct LoginRequest {
    pub org_id: Option<Uuid>,
    #[schema(format = Email, example = "ahmed@therue.cafe")]
    pub email: Option<String>,
    pub password: Option<String>,
    #[schema(
        pattern = "^[0-9]{4,6}$",
        min_length = 4,
        max_length = 6,
        example = "1234"
    )]
    pub pin: Option<String>,
    /// The person's display name. Optional for PIN login: without it the PIN
    /// alone identifies the person (PIN-only sign-in, org-wide unique PINs).
    /// Old tablets send it and keep the name-narrowed lookup.
    #[schema(example = "Mariam")]
    pub name: Option<String>,
    /// Required for PIN login. The org is derived from this branch server-side.
    pub branch_id: Option<Uuid>,
}

#[derive(Deserialize, ToSchema)]
pub struct ResolveBranchRequest {
    /// Organization to search within.
    pub org_id: Uuid,
    /// Device GPS latitude (WGS-84).
    pub latitude: f64,
    /// Device GPS longitude (WGS-84).
    pub longitude: f64,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct ResolveBranchResponse {
    pub branch_id: Uuid,
    pub branch_name: String,
    /// Straight-line distance from the supplied coordinates to the branch, in metres.
    pub distance_meters: f64,
}

/// The tax policy the caller should price under, resolved for their branch
/// where they have one and their organisation otherwise.
///
/// Sent at login and on every `/auth/me`, which is what makes a rate change
/// reach a till that has been running for weeks. The flat `tax_rate` beside it
/// is kept for builds that predate this object; both describe the same rate.
#[derive(Serialize, Deserialize, ToSchema, Clone, Copy, Debug)]
pub struct TaxPolicyPublic {
    /// Fraction, NOT a percentage: `0.14` is 14%.
    #[schema(example = 0.14)]
    pub tax_rate: f64,
    /// `true` = menu prices already contain the tax.
    pub tax_inclusive: bool,
    /// Fraction of the bill added as a service charge; `0` disables it.
    #[schema(example = 0.0)]
    pub service_charge_rate: f64,
    /// Whether the service charge is itself taxed.
    pub service_charge_taxable: bool,
}

impl From<crate::tax::TaxPolicy> for TaxPolicyPublic {
    fn from(p: crate::tax::TaxPolicy) -> Self {
        use rust_decimal::prelude::ToPrimitive;
        Self {
            tax_rate: p.tax_rate.to_f64().unwrap_or(0.0),
            tax_inclusive: p.tax_inclusive,
            service_charge_rate: p.service_charge_rate.to_f64().unwrap_or(0.0),
            service_charge_taxable: p.service_charge_taxable,
        }
    }
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct LoginResponse {
    /// JWT to send as `Authorization: Bearer <token>` on subsequent requests.
    pub token: String,
    pub user: UserPublic,
    /// Org tax rate as a decimal (e.g. 0.14 = 14% VAT); 0.0 when no org. Mirrors
    /// /auth/me so the POS has it immediately after login.
    #[schema(example = 0.14)]
    pub tax_rate: f64,
    /// The full policy, including tax-inclusive pricing and service charge.
    /// Prefer this over the flat `tax_rate` above.
    pub tax_policy: TaxPolicyPublic,
    /// Every dine-in sale belongs to a table.
    ///
    /// The till needs this, not just the server: the rule changes what the POS
    /// puts in front of a teller — the floor becomes the home screen and a sale
    /// starts by picking a table — and a refusal AFTER the items are rung up is
    /// far too late to be useful.
    #[serde(default)]
    pub require_table_for_orders: bool,
    #[schema(example = "EGP")]
    pub currency_code: String,
    /// The person's open till at the branch they signed into (any device), so
    /// the device can resume it or show where it is open.
    #[serde(default)]
    pub open_till: Option<crate::tills::handlers::TillBrief>,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct MeResponse {
    pub user: UserPublic,
    /// Org tax rate as a decimal (e.g. 0.14 = 14% VAT); 0.0 when the user has no
    /// org. Exposed so the POS can compute a tax-inclusive cart total client-side.
    #[schema(example = 0.14)]
    pub tax_rate: f64,
    /// The full policy, including tax-inclusive pricing and service charge.
    ///
    /// A till re-reads this whenever it syncs, which is what makes a rate
    /// changed in the dashboard reach a device that has not signed in for
    /// weeks. Without it the till prices under a stale rate and — now that the
    /// server refuses totals it disagrees with — cannot sell at all.
    pub tax_policy: TaxPolicyPublic,
    /// Every dine-in sale belongs to a table. Re-read on every `/auth/me`, so
    /// switching it on reaches a till that has been running for weeks.
    #[serde(default)]
    pub require_table_for_orders: bool,
    /// Org currency code (e.g. "EGP").
    #[schema(example = "EGP")]
    pub currency_code: String,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct UserPermissionItem {
    #[schema(example = "menu_items")]
    pub resource: String,
    #[schema(example = "read")]
    pub action: String,
    pub granted: bool,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct AuthPermissionsResponse {
    pub permissions: Vec<UserPermissionItem>,
}

// ── POST /auth/login ─────────────────────────────────────────

/// Columns of `User`, for the PIN look-ups below.
const PIN_USER_COLUMNS: &str = "u.id, u.org_id, u.name, u.email, u.phone, \
     u.password_hash, u.pin_hash, u.role, u.is_active, u.last_login_at, \
     u.created_at, u.updated_at, u.deleted_at";

/// Who may type a PIN at all: anyone who works a till (owners and managers
/// too); whether they may at THIS branch is the `pos.sign_in` check.
const PIN_HOLDER_FILTER: &str = "u.org_id = $1 \
     AND u.pin_hash IS NOT NULL \
     AND u.role <> 'super_admin' \
     AND NOT u.is_guest_principal \
     AND u.is_active = TRUE \
     AND u.deleted_at IS NULL";

fn pin_verifies(u: &User, pin: &str) -> bool {
    u.pin_hash
        .as_deref()
        .is_some_and(|h| bcrypt::verify(pin, h).unwrap_or(false))
}

/// The name-narrowed path old tablets use: names are unique per org.
async fn find_pin_holder_by_name(
    pool: &PgPool,
    org: Uuid,
    name: &str,
    pin: &str,
) -> Result<Option<User>, AppError> {
    let rows = sqlx::query_as::<_, User>(&format!(
        "SELECT {PIN_USER_COLUMNS} FROM users u \
          WHERE {PIN_HOLDER_FILTER} AND LOWER(u.name) = LOWER($2)"
    ))
    .bind(org)
    .bind(name)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().find(|u| pin_verifies(u, pin)))
}

/// PIN-only (§2, §6): FIND the holder by the keyed fingerprint — one indexed
/// row, one slow verify — and only if that finds nobody, scan the holders who
/// have no fingerprint yet (the backfill happens at their next sign-in).
/// `Some(Err(()))` when the scan finds the PIN on more than one person.
async fn find_pin_holder_by_pin(
    pool: &PgPool,
    org: Uuid,
    pin: &str,
) -> Result<Option<Result<User, ()>>, AppError> {
    let fingerprints = crate::auth::pin_fingerprint::lookup_fingerprints(org, pin);
    let by_fingerprint = sqlx::query_as::<_, User>(&format!(
        "SELECT {PIN_USER_COLUMNS} FROM users u \
          WHERE {PIN_HOLDER_FILTER} AND u.pin_fingerprint = ANY($2)"
    ))
    .bind(org)
    .bind(&fingerprints)
    .fetch_all(pool)
    .await?;
    if let Some(u) = by_fingerprint.into_iter().find(|u| pin_verifies(u, pin)) {
        return Ok(Some(Ok(u)));
    }
    let unstamped = sqlx::query_as::<_, User>(&format!(
        "SELECT {PIN_USER_COLUMNS} FROM users u \
          WHERE {PIN_HOLDER_FILTER} AND u.pin_fingerprint IS NULL"
    ))
    .bind(org)
    .fetch_all(pool)
    .await?;
    let mut hits = unstamped.into_iter().filter(|u| pin_verifies(u, pin));
    Ok(match (hits.next(), hits.next()) {
        (Some(u), None) => Some(Ok(u)),
        (Some(first), Some(second)) => {
            tracing::warn!(
                target: "madar.authz",
                %org,
                first = %first.id,
                second = %second.id,
                "PIN-only sign-in matched more than one person"
            );
            Some(Err(()))
        }
        _ => None,
    })
}

#[utoipa::path(
    post,
    path = "/auth/login",
    tag = "auth",
    request_body = LoginRequest,
    responses(
        (status = 200, description = "Authentication succeeded; JWT issued", body = LoginResponse),
        AppErrorResponse,
    )
)]
pub async fn login(
    http_req: HttpRequest,
    pool: web::Data<PgPool>,
    secret: web::Data<JwtSecret>,
    body: web::Json<LoginRequest>,
) -> Result<HttpResponse, AppError> {
    let user: User = match (&body.email, &body.pin) {
        (Some(email), None) => {
            let password = body.password.as_deref().ok_or_else(|| {
                AppError::BadRequest("password is required for email login".into())
            })?;

            let u = sqlx::query_as::<_, User>(
                r#"
                SELECT id, org_id, name, email, phone,
                       password_hash, pin_hash, role,
                       is_active, last_login_at,
                       created_at, updated_at, deleted_at
                FROM users
                WHERE email = $1
                  AND ($2::uuid IS NULL OR org_id = $2)
                  AND deleted_at IS NULL
                "#,
            )
            .bind(email)
            .bind(body.org_id)
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::Unauthorized("Invalid credentials".into()))?;

            let hash = u
                .password_hash
                .as_deref()
                .ok_or_else(|| AppError::Unauthorized("No password set for this account".into()))?;
            if !bcrypt::verify(password, hash).unwrap_or(false) {
                return Err(AppError::Unauthorized("Invalid credentials".into()));
            }
            u
        }

        (None, Some(pin)) => {
            // PIN-only sign-in (POS_SIGNIN_OVERHAUL.md §2, §8.5): `name` is
            // optional on the wire. Old tablets keep sending it and keep the
            // name-narrowed path exactly as it was.
            let name = body
                .name
                .as_deref()
                .map(str::trim)
                .filter(|n| !n.is_empty());

            let branch_id = body.branch_id.ok_or_else(|| {
                AppError::BadRequest("branch_id is required for PIN login".into())
            })?;

            // Derive org_id from the branch — never trust the client to supply it
            let branch_org_id: Uuid = sqlx::query_scalar(
                "SELECT org_id FROM branches WHERE id = $1 AND is_active = TRUE AND deleted_at IS NULL"
            )
            .bind(branch_id)
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::Unauthorized("Invalid branch".into()))?;

            // A growing delay on wrong PINs, counted against the device and the
            // branch (POS_SIGNIN_OVERHAUL.md §3.4). Checked BEFORE any lookup,
            // so a grinder gets no work out of us and no timing signal either.
            let device_id = http_req
                .headers()
                .get(crate::tickets::DEVICE_ID_HEADER)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            // Old tablets (v0.5–v0.7) send a name and no device id on login;
            // they are left exactly as they were. The delay applies to a client
            // that identifies its device, and to any attempt without a name
            // (PIN-only, §8.5) — the widened haystack it exists for.
            let throttled = device_id.is_some() || body.name.is_none();
            if throttled {
                crate::auth::pin_throttle::check(pool.get_ref(), device_id.as_deref(), branch_id)
                    .await?;
            }

            // ORG-scoped lookup: resolve the person within the branch's org
            // FIRST. Branch access is checked separately below — that lets us
            // distinguish "wrong PIN (or wrong org)" → 401 from "valid person,
            // but no access to THIS branch" → 403, instead of conflating both.
            let found = match name {
                Some(name) => find_pin_holder_by_name(pool.get_ref(), branch_org_id, name, pin)
                    .await?
                    .map(Ok),
                None => find_pin_holder_by_pin(pool.get_ref(), branch_org_id, pin).await?,
            };
            let matched = match found {
                Some(Ok(u)) => u,
                // Two people hold this PIN and nothing names which one is
                // typing. Not a wrong PIN, so not counted against the delay.
                Some(Err(())) => {
                    return Err(AppError::Refused {
                        code: "PIN_NOT_UNIQUE",
                        reason:
                            "This PIN belongs to more than one person. Ask a manager for a new PIN."
                                .into(),
                    });
                }
                // Nobody in this org matches (includes a real person from a
                // DIFFERENT org) → generic invalid credentials, and one more
                // against the delay.
                None => {
                    if throttled {
                        crate::auth::pin_throttle::record_failure(
                            pool.get_ref(),
                            device_id.as_deref(),
                            branch_id,
                        )
                        .await;
                    }
                    return Err(AppError::Unauthorized("Invalid credentials".into()));
                }
            };

            // Architecture E: signing in at a till is the `pos.sign_in` capability
            // at this branch (tellers and waiters always hold it; owners hold it;
            // a manager at the branches they are assigned to).
            let matched_eff =
                crate::authz::require::effective(pool.get_ref(), matched.id, Some(branch_id))
                    .await?;
            if !matched_eff.can(crate::authz::Cap::PosSignIn) {
                // The ONE case with an identity behind it (§3.4): a CORRECT PIN
                // typed at a branch its holder does not work at. Someone else's
                // PIN turning up in the wrong shop is a real signal, so it is
                // logged against the person rather than the place, and it does
                // not feed the anonymous delay — that would let a wrong shop
                // slow down the right one.
                tracing::warn!(
                    target: "madar.authz",
                    user_id = %matched.id,
                    branch_id = %branch_id,
                    device_id = device_id.as_deref().unwrap_or("-"),
                    "correct PIN at a branch this person is not allowed at"
                );
                crate::auth::pin_throttle::record_wrong_branch(
                    pool.get_ref(),
                    branch_org_id,
                    matched.id,
                    branch_id,
                    device_id.as_deref(),
                )
                .await;
                return Err(AppError::Forbidden(
                    "You can't sign in at a till in this branch".into(),
                ));
            }

            // A correct PIN ends the run of failures for this device and shop.
            if throttled {
                crate::auth::pin_throttle::clear(pool.get_ref(), device_id.as_deref(), branch_id)
                    .await;
            }

            // Owner decision 2026-09-16: an owner never signs in with a PIN on a
            // pre-0.8 tablet. Those builds cannot hold an owner's access safely
            // (no capability snapshot, role-name gates), so the till would show
            // the wrong things. A pre-0.8 client is one that sends no
            // `X-Madar-Device-Id` on login — the same marker the PIN delay uses;
            // every 0.8+ build sends it. Owners on 0.8+ are unaffected.
            if device_id.is_none()
                && (matched_eff.owner || matched.role == crate::models::UserRole::OrgAdmin)
            {
                tracing::warn!(
                    target: "madar.authz",
                    user_id = %matched.id,
                    branch_id = %branch_id,
                    "owner PIN sign-in refused on a pre-0.8 tablet"
                );
                return Err(AppError::Coded {
                    status: 403,
                    code: "OWNER_PIN_NEEDS_UPDATE",
                    reason: "Owners sign in on this tablet after updating the Madar app.".into(),
                });
            }

            // Decision D13 ("tellers are ORG-scoped, no per-branch gate at the
            // till") is SUPERSEDED by the owner decision of 2026-09-16
            // (POS_SIGNIN_OVERHAUL.md §5.2, "A + B"). The branch allow-list now
            // gates PIN sign-in, and it does so through the check above: a role
            // assignment that does not cover THIS branch contributes no role
            // kind, so the person holds no `pos.sign_in` here and gets the 403.
            // The allow-list itself lives in the role assignment; a person with
            // no explicit branches is org-wide (§5.3), so nobody who works today
            // is locked out by the change.

            // Backfill the keyed PIN fingerprint (POS_SIGNIN_OVERHAUL.md §2,
            // §6). Salted hashes cannot be fingerprinted in a migration — the
            // plaintext is only ever in hand here, at a successful sign-in — so
            // the column fills in as people work. It is written under the
            // CURRENT key, which is also how a key rotation completes itself.
            // Best-effort: a duplicate or a write failure must never block a
            // valid login. A duplicate means two people share a PIN, which is
            // logged and otherwise ignored: every PIN is being re-issued at
            // rollout, so there is no backlog to manage (§6).
            let fp = crate::auth::pin_fingerprint::fingerprint(branch_org_id, pin);
            if let Err(e) = sqlx::query(
                "UPDATE users SET pin_fingerprint = $1 WHERE id = $2
                   AND (pin_fingerprint IS NULL OR pin_fingerprint <> $1)",
            )
            .bind(&fp)
            .bind(matched.id)
            .execute(pool.get_ref())
            .await
            {
                tracing::warn!(user_id = %matched.id, "pin fingerprint not stored: {e}");
            }

            // Layer 3: silently (re)derive the teller's OFFLINE PIN verifier
            // (argon2id, distinct from the bcrypt login hash) so the org's
            // offline-auth bundle can let them unlock offline later. Best-effort
            // — a hashing/store failure must never block a valid login.
            if let Ok(off_hash) = crate::auth::offline::hash_offline_pin(pin) {
                let _ = sqlx::query("UPDATE users SET offline_pin_hash = $1 WHERE id = $2")
                    .bind(&off_hash)
                    .bind(matched.id)
                    .execute(pool.get_ref())
                    .await;
            }

            matched
        }

        _ => {
            return Err(AppError::BadRequest(
                "Provide either (email + password) or pin".into(),
            ));
        }
    };

    if !user.is_active {
        return Err(AppError::Unauthorized("Account is disabled".into()));
    }

    // Refuse to issue a token to a suspended / soft-deleted org. The middleware
    // also rejects live requests for such an org, but stopping it here means we
    // never hand out a fresh token in the first place. Super admins carry no
    // org_id and are unaffected.
    if let Some(org_id) = user.org_id
        && !crate::auth::org_status::org_is_allowed(pool.get_ref(), org_id).await?
    {
        return Err(AppError::OrgSuspended);
    }

    // Open-shift login rules (authoritative — the backend is the source of truth):
    //   • same teller, SAME branch as their open shift  → allow (resume; e.g.
    //     after a token expiry — don't lock them out of the shift they must close)
    //   • same teller, DIFFERENT branch                 → reject (no two live places)
    //   • DIFFERENT teller at a branch that already has open shifts → ALLOW: with
    //     multi-teller tills, several tellers operate concurrently at one branch,
    //     each on their own till/drawer. The one-open-per-till index (not login)
    //     prevents two people sharing one drawer.
    //
    // (1) This teller's own open shift must be at the branch they're signing into.
    let open_shift_branch: Option<Uuid> =
        sqlx::query_scalar("SELECT branch_id FROM tills WHERE teller_id = $1 AND status = 'open'")
            .bind(user.id)
            .fetch_optional(pool.get_ref())
            .await?;
    if let Some(open_branch) = open_shift_branch
        && body.branch_id != Some(open_branch)
    {
        tracing::warn!(
            target: "auth.login.blocked_open_shift",
            user_id = %user.id, role = ?user.role,
            open_shift_branch = %open_branch, attempted_branch = ?body.branch_id,
            "login blocked: user has an open shift at a different branch"
        );
        crate::client_seen::legacy_hit_for_org(
            crate::client_seen::KIND_ERROR_WORDING,
            "login_blocked_open_shift",
            user.org_id,
        );
        return Err(AppError::Conflict(
            "You already have an open shift at another branch. Close it before signing in here."
                .into(),
        ));
    }

    // (2) [removed for multi-teller] A branch may now hold several tellers' open
    //     shifts at once — one per till — so signing in alongside another teller's
    //     live shift is allowed. The previous `X-Madar-Closing-Shifts` handover
    //     handshake is no longer needed (a closing shift simply replays its close).

    // Tellers, waiters AND kitchen users are device-bound (PIN) and branch-bound;
    // waiters/kitchen just never hold a shift. All get the short device TTL.
    // A PIN sign-in is a till session whatever the role: branch-bound, short.
    let via_pin = body.pin.is_some() && body.email.is_none();
    let token_branch_id = if via_pin
        || matches!(
            user.role,
            UserRole::Teller | UserRole::Waiter | UserRole::Kitchen
        ) {
        body.branch_id
    } else {
        None
    };

    let hours = if token_branch_id.is_some() { 12 } else { 24 };

    let token = create_token(
        &secret,
        user.id,
        user.org_id,
        user.role.clone(),
        token_branch_id,
        hours,
    )
    .map_err(|_| AppError::Internal)?;

    sqlx::query("UPDATE users SET last_login_at = NOW() WHERE id = $1")
        .bind(user.id)
        .execute(pool.get_ref())
        .await?;

    // For tellers/waiters, branch_id is the device branch (from body.branch_id).
    // For other roles, fall back to looking up the first assignment.
    let branch_id_for_response: Option<Uuid> = if token_branch_id.is_some() {
        body.branch_id
    } else {
        sqlx::query_scalar(
            "SELECT branch_id FROM user_branch_assignments WHERE user_id = $1 LIMIT 1",
        )
        .bind(user.id)
        .fetch_optional(pool.get_ref())
        .await?
        .flatten()
    };

    // The policy this caller prices under: their BRANCH's where they have one,
    // because a branch may override its org, and the till needs the rate that
    // applies where it is standing — not the org average.
    let (tax_policy, currency_code, require_table_for_orders) =
        resolve_tax_context(pool.get_ref(), user.org_id, branch_id_for_response).await?;
    let tax_rate = tax_policy.tax_rate;

    let mut user_public = UserPublic::from(user);
    user_public.branch_id = branch_id_for_response;

    let open_till = match branch_id_for_response {
        Some(b) => sqlx::query_as::<_, crate::tills::handlers::Till>(&format!(
            "SELECT {} {} WHERE s.teller_id = $1 AND s.branch_id = $2 AND s.status = 'open' ORDER BY s.opened_at DESC LIMIT 1",
            crate::tills::handlers::TILL_COLUMNS,
            crate::tills::handlers::TILL_FROM
        ))
        .bind(user_public.id)
        .bind(b)
        .fetch_optional(pool.get_ref())
        .await?
        .as_ref()
        .map(crate::tills::handlers::TillBrief::from),
        None => None,
    };

    Ok(HttpResponse::Ok().json(LoginResponse {
        token,
        user: user_public,
        tax_rate,
        tax_policy,
        require_table_for_orders,
        currency_code,
        open_till,
    }))
}

// ── GET /auth/me ─────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/auth/me",
    tag = "auth",
    responses(
        (status = 200, description = "Current authenticated user", body = MeResponse),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn me(req: HttpRequest, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let claims = req
        .extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))?;

    let user = sqlx::query_as::<_, User>(
        r#"
        SELECT id, org_id, name, email, phone,
               password_hash, pin_hash, role,
               is_active, last_login_at,
               created_at, updated_at, deleted_at
        FROM users
        WHERE id = $1 AND deleted_at IS NULL
        "#,
    )
    .bind(claims.user_id())
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("User not found".into()))?;

    // Prefer the branch this token is actually bound to (tellers always carry
    // one). An arbitrary LIMIT-1 assignment would, for a teller assigned to more
    // than one branch, report a DIFFERENT branch than the token — the POS adopts
    // that branch as `user.branchId` and then calls branch-scoped endpoints with
    // it, tripping require_branch_access's teller token-branch binding (403),
    // while /auth/me itself still returns 200. Non-branch-bound roles (admins)
    // have no token branch, so they fall back to an assignment lookup.
    let branch_id: Option<Uuid> = match claims.branch_id() {
        Some(b) => Some(b),
        None => {
            sqlx::query_scalar(
                "SELECT branch_id FROM user_branch_assignments WHERE user_id = $1 LIMIT 1",
            )
            .bind(user.id)
            .fetch_optional(pool.get_ref())
            .await?
        }
    };

    // The policy this caller prices under. Re-read on every /auth/me, which is
    // the path a running till uses to notice a rate it has not seen.
    let (tax_policy, currency_code, require_table_for_orders) =
        resolve_tax_context(pool.get_ref(), user.org_id, branch_id).await?;

    let mut user_public = UserPublic::from(user);
    user_public.branch_id = branch_id;

    Ok(HttpResponse::Ok().json(MeResponse {
        user: user_public,
        tax_rate: tax_policy.tax_rate,
        tax_policy,
        require_table_for_orders,
        currency_code,
    }))
}

// ── POST /auth/resolve-branch ────────────────────────────────

#[utoipa::path(
    post,
    path = "/auth/resolve-branch",
    tag = "auth",
    request_body = ResolveBranchRequest,
    responses(
        (status = 200, description = "Nearest branch within its geofence radius", body = ResolveBranchResponse),
        AppErrorResponse,
    )
)]
pub async fn resolve_branch(
    pool: web::Data<PgPool>,
    body: web::Json<ResolveBranchRequest>,
) -> Result<HttpResponse, AppError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        id: Uuid,
        name: String,
        distance_meters: f64,
    }

    let row: Option<Row> = sqlx::query_as(
        r#"
        SELECT b.id, b.name,
            (6371000.0 * ACOS(LEAST(1.0,
                SIN(RADIANS($2)) * SIN(RADIANS(b.latitude))
              + COS(RADIANS($2)) * COS(RADIANS(b.latitude))
              * COS(RADIANS(b.longitude - $3))
            ))) AS distance_meters
        FROM branches b
        WHERE b.org_id     = $1
          AND b.is_active  = TRUE
          AND b.deleted_at IS NULL
          AND b.latitude   IS NOT NULL
          AND b.longitude  IS NOT NULL
          AND (6371000.0 * ACOS(LEAST(1.0,
                SIN(RADIANS($2)) * SIN(RADIANS(b.latitude))
              + COS(RADIANS($2)) * COS(RADIANS(b.latitude))
              * COS(RADIANS(b.longitude - $3))
              ))) <= COALESCE(b.geo_radius_meters, 200)
        ORDER BY distance_meters ASC
        LIMIT 1
        "#,
    )
    .bind(body.org_id)
    .bind(body.latitude)
    .bind(body.longitude)
    .fetch_optional(pool.get_ref())
    .await?;

    match row {
        Some(r) => Ok(HttpResponse::Ok().json(ResolveBranchResponse {
            branch_id: r.id,
            branch_name: r.name,
            distance_meters: r.distance_meters,
        })),
        None => Err(AppError::NotFound("No branch found within range".into())),
    }
}

// ── GET /auth/permissions ────────────────────────────────────

#[utoipa::path(
    get,
    path = "/auth/permissions",
    tag = "auth",
    // operation_id overrides the default `permissions` (the function name)
    // to avoid collision with the permissions module's handlers and to
    // give generated clients a clearer method name (`getMyPermissions`).
    operation_id = "get_my_permissions",
    responses(
        (status = 200, description = "Effective permission grants for the authenticated user", body = AuthPermissionsResponse),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn permissions(req: HttpRequest, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let claims = req
        .extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))?;

    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users WHERE id = $1 AND deleted_at IS NULL)",
    )
    .bind(claims.user_id())
    .fetch_one(pool.get_ref())
    .await?;
    if !exists {
        return Err(AppError::NotFound("User not found".into()));
    }

    // Architecture E: the grid is the person's effective capabilities at the
    // token's branch, projected onto the legacy cells (one capability per cell),
    // in the same order as before.
    let eff =
        crate::authz::require::effective_for_claims(pool.get_ref(), &claims, claims.branch_id())
            .await?;
    let mut permissions: Vec<UserPermissionItem> = crate::permissions::permission_cells()
        .map(|(resource, action)| UserPermissionItem {
            resource: resource.to_string(),
            action: action.to_string(),
            granted: crate::authz::legacy::granted(&eff, resource, action),
        })
        .collect();

    // POS v0.5.1 / v0.6.0 gate on `has_permission("shifts", …)`: mirror every
    // `tills:<action>` as `shifts:<action>` while those builds are in the field.
    // Checks in this codebase use `tills` only.
    let legacy: Vec<UserPermissionItem> = permissions
        .iter()
        .filter(|p| p.resource == "tills")
        .map(|p| UserPermissionItem {
            resource: "shifts".into(),
            action: p.action.clone(),
            granted: p.granted,
        })
        .collect();
    permissions.extend(legacy);
    if crate::client_seen::is_legacy_pos_request(req.headers()) {
        crate::client_seen::legacy_hit(crate::client_seen::KIND_PERM_PAYLOAD_OLD);
    }

    Ok(HttpResponse::Ok().json(AuthPermissionsResponse { permissions }))
}

/// The tax policy and currency to hand a caller.
///
/// Branch-first, org-second, and tax-free when the user belongs to neither —
/// deliberately NOT the old `unwrap_or(0.14)`, which invented Egyptian VAT for
/// anyone whose org could not be read.
async fn resolve_tax_context(
    pool: &sqlx::PgPool,
    org_id: Option<Uuid>,
    branch_id: Option<Uuid>,
) -> Result<(TaxPolicyPublic, String, bool), AppError> {
    let Some(org_id) = org_id else {
        return Ok((
            crate::tax::TaxPolicy::default().into(),
            "EGP".to_string(),
            false,
        ));
    };

    let row: Option<(String, bool)> = sqlx::query_as(
        "SELECT currency_code, require_table_for_orders FROM organizations WHERE id = $1",
    )
    .bind(org_id)
    .fetch_optional(pool)
    .await?;
    let (currency, require_table) = row.unwrap_or_else(|| ("EGP".to_string(), false));

    // A branch that no longer exists (or belongs to another org) falls back to
    // the org rather than failing the login: being unable to sign in is worse
    // than pricing at the org rate for one shift.
    let policy = match branch_id {
        Some(b) => match crate::tax::policy::for_branch(pool, b).await {
            Ok(p) => p,
            Err(_) => crate::tax::policy::for_org(pool, org_id).await?,
        },
        None => crate::tax::policy::for_org(pool, org_id).await?,
    };
    Ok((policy.into(), currency, require_table))
}
