//! Push notifications through Firebase Cloud Messaging, shared by every app
//! (Dawam today; a future management app registers under its own `app` name
//! and reuses this same sender). See `push_devices` (migration
//! `20260926000400_push_devices.sql`), which superseded the Dawam-only
//! `staff_devices.push_token`.
//!
//! Off unless `FCM_SERVICE_ACCOUNT_FILE` names a Firebase service-account
//! JSON; callers work without it (the inbox/notification row is still
//! written, just nothing is pushed to a phone).

#[doc(hidden)]
pub mod fake;
pub mod handlers;
pub mod pos;
pub mod routes;
pub mod words;

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::errors::AppError;

/// Who a push registration belongs to: a Madar user (any app), or a Dawam
/// employee (the staff app signs in as the employee, who may have no user).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recipient {
    User(Uuid),
    Employee(Uuid),
}

impl Recipient {
    fn cols(self) -> (Option<Uuid>, Option<Uuid>) {
        match self {
            Recipient::User(u) => (Some(u), None),
            Recipient::Employee(e) => (None, Some(e)),
        }
    }
}

/// Register or rebind a device's push token (PUT /push/token and the Dawam
/// alias PUT /staff/me/push-token both call this).
///
/// `device_id` is the install that sent it (`X-Madar-Device`), when the app
/// sends one. One install holds one live token per app: registering a new
/// token from the same install revokes the old one, so a refreshed FCM token
/// never makes a till ring twice for one order.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn register(
    pool: &PgPool,
    org_id: Uuid,
    who: Recipient,
    app: &str,
    token: &str,
    locale: &str,
    platform: &str,
    device_id: Option<Uuid>,
) -> Result<(), AppError> {
    let (user_id, employee_id) = who.cols();
    let mut tx = pool.begin().await?;
    if let Some(device) = device_id {
        sqlx::query(
            "UPDATE push_devices SET revoked_at = now() \
              WHERE device_id = $1 AND app = $2 AND token <> $3 AND revoked_at IS NULL",
        )
        .bind(device)
        .bind(app)
        .bind(token)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "INSERT INTO push_devices (org_id, user_id, employee_id, app, token, locale, platform, device_id) \
          VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
          ON CONFLICT (token) WHERE revoked_at IS NULL DO UPDATE SET \
            org_id = EXCLUDED.org_id, user_id = EXCLUDED.user_id, \
            employee_id = EXCLUDED.employee_id, app = EXCLUDED.app, \
            locale = EXCLUDED.locale, platform = EXCLUDED.platform, \
            device_id = EXCLUDED.device_id, last_seen_at = now()",
    )
    .bind(org_id)
    .bind(user_id)
    .bind(employee_id)
    .bind(app)
    .bind(token)
    .bind(locale)
    .bind(platform)
    .bind(device_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Revoke one device's registration (sign-out). Idempotent: revoking a token
/// that is not (or no longer) this recipient's is a no-op, never an error.
pub(crate) async fn unregister(
    pool: &PgPool,
    who: Recipient,
    app: &str,
    token: &str,
) -> Result<(), AppError> {
    let (user_id, employee_id) = who.cols();
    sqlx::query(
        "UPDATE push_devices SET revoked_at = now() \
          WHERE token = $1 AND user_id IS NOT DISTINCT FROM $2 \
            AND employee_id IS NOT DISTINCT FROM $3 AND app = $4 AND revoked_at IS NULL",
    )
    .bind(token)
    .bind(user_id)
    .bind(employee_id)
    .bind(app)
    .execute(pool)
    .await?;
    Ok(())
}

/// Revoke every device of a recipient for `app` (used when a phone/account is
/// force-revoked, e.g. Dawam's RO-4/RO-10).
pub(crate) async fn revoke_all(pool: &PgPool, who: Recipient, app: &str) -> Result<(), AppError> {
    let (user_id, employee_id) = who.cols();
    sqlx::query(
        "UPDATE push_devices SET revoked_at = now() \
          WHERE user_id IS NOT DISTINCT FROM $1 AND employee_id IS NOT DISTINCT FROM $2 \
            AND app = $3 AND revoked_at IS NULL",
    )
    .bind(user_id)
    .bind(employee_id)
    .bind(app)
    .execute(pool)
    .await?;
    Ok(())
}

/// A word from the table the phones share (`words`, generated from the POS
/// core's i18n) or the server's own POS push words (`pos::WORDS`).
fn word(key: &str, ar: bool) -> Option<&'static str> {
    words::WORDS
        .iter()
        .chain(pos::WORDS)
        .find(|(k, ..)| *k == key)
        .map(|(_, en, a)| if ar { *a } else { *en })
}

/// A notification line in the phone's language. `{kind}` resolves through a
/// `<key>.kind_*` word first (falling back to the raw value); `*amount*`
/// arguments render as piastres.
pub fn render(key: &str, args: &Value, ar: bool) -> Option<String> {
    let mut s = word(key, ar)?.to_string();
    for (k, v) in args.as_object().into_iter().flatten() {
        let text = match (k.as_str(), v) {
            (k, Value::Number(n)) if k.contains("amount") => {
                let p = n.as_i64().unwrap_or(0);
                format!(
                    "{}{}.{:02} EGP",
                    if p < 0 { "-" } else { "" },
                    p.abs() / 100,
                    p.abs() % 100
                )
            }
            ("kind", Value::String(kind)) => {
                let prefix = key.rsplit_once('.').map_or("", |(p, _)| p);
                word(&format!("{prefix}.kind_{kind}"), ar)
                    .map_or_else(|| kind.clone(), str::to_string)
            }
            (_, Value::String(x)) => x.clone(),
            (_, x) => x.to_string(),
        };
        s = s.replace(&format!("{{{k}}}"), &text);
    }
    Some(s)
}

struct Fcm {
    project: String,
    email: String,
    key: jsonwebtoken::EncodingKey,
    token: Mutex<Option<(String, Instant)>>,
}

fn fcm() -> Option<&'static Fcm> {
    static FCM: OnceLock<Option<Fcm>> = OnceLock::new();
    FCM.get_or_init(|| {
        let path = std::env::var("FCM_SERVICE_ACCOUNT_FILE").ok()?;
        let v: Value = match std::fs::read_to_string(&path).map(|s| serde_json::from_str(&s)) {
            Ok(Ok(v)) => v,
            _ => {
                tracing::error!(path, "FCM service account unreadable; pushes are off");
                return None;
            }
        };
        let key =
            jsonwebtoken::EncodingKey::from_rsa_pem(v["private_key"].as_str()?.as_bytes()).ok()?;
        tracing::info!("FCM pushes on");
        Some(Fcm {
            project: v["project_id"].as_str()?.to_string(),
            email: v["client_email"].as_str()?.to_string(),
            key,
            token: Mutex::new(None),
        })
    })
    .as_ref()
}

/// True once the config is checked and readable — used to decide whether to
/// warn at boot, never to gate correctness (the sender itself no-ops when
/// `fcm()` fails for any reason).
pub fn configured() -> bool {
    std::env::var("FCM_SERVICE_ACCOUNT_FILE").is_ok_and(|p| std::fs::metadata(&p).is_ok())
}

/// An OAuth access token for the FCM API, reused for 50 minutes.
async fn access_token(f: &Fcm, http: &reqwest::Client) -> Option<String> {
    let mut cached = f.token.lock().await;
    if let Some((t, at)) = cached.as_ref()
        && at.elapsed() < Duration::from_secs(50 * 60)
    {
        return Some(t.clone());
    }
    let now = chrono::Utc::now().timestamp();
    let claims = json!({
        "iss": f.email, "aud": "https://oauth2.googleapis.com/token",
        "scope": "https://www.googleapis.com/auth/firebase.messaging",
        "iat": now, "exp": now + 3600,
    });
    let jwt = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
        &claims,
        &f.key,
    )
    .ok()?;
    let resp: Value = http
        .post("https://oauth2.googleapis.com/token")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!(
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion={jwt}"
        ))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let t = resp["access_token"].as_str()?.to_string();
    *cached = Some((t.clone(), Instant::now()));
    Some(t)
}

/// The FCM base URL, overridable in debug builds so tests can point it at a
/// local stub instead of ever calling Google.
fn fcm_send_url(project: &str) -> String {
    #[cfg(debug_assertions)]
    if let Ok(base) = std::env::var("MADAR_FCM_STUB_URL") {
        return format!("{base}/v1/projects/{project}/messages:send");
    }
    format!("https://fcm.googleapis.com/v1/projects/{project}/messages:send")
}

/// Where messages go: Google, or (debug builds, tests only) the in-memory
/// [`fake`] sink.
#[derive(Clone, Copy)]
enum Transport {
    Fcm(&'static Fcm),
    #[cfg(debug_assertions)]
    Fake,
}

fn transport() -> Option<Transport> {
    #[cfg(debug_assertions)]
    if fake::installed() {
        return Some(Transport::Fake);
    }
    fcm().map(Transport::Fcm)
}

/// FCM's answer to one send: the HTTP status and the error body (`Null` on
/// success or when the body is not JSON).
type Reply = (u16, Value);

/// One outbound send, split out so retry can call it more than once.
async fn post_one(http: &reqwest::Client, url: &str, bearer: &str, msg: &Value) -> Option<Reply> {
    match http.post(url).bearer_auth(bearer).json(msg).send().await {
        Ok(r) => {
            let status = r.status().as_u16();
            let body = if status >= 300 {
                r.json().await.unwrap_or(Value::Null)
            } else {
                Value::Null
            };
            Some((status, body))
        }
        Err(e) => {
            tracing::warn!(error = %e, "push not sent");
            None
        }
    }
}

/// Does FCM's answer say the TOKEN is dead (so the registration is dropped)?
///
/// Only when FCM names the token: 404 / `UNREGISTERED` (the app was
/// uninstalled or the token rotated), or a 400 `INVALID_ARGUMENT` whose field
/// violation is `message.token`. Any other 400 is about the MESSAGE — a bad
/// payload would otherwise unregister every device it was sent to.
pub fn token_is_dead(status: u16, body: &Value) -> bool {
    let details = body["error"]["details"].as_array();
    let named = |pred: &dyn Fn(&Value) -> bool| details.is_some_and(|d| d.iter().any(pred));
    if status == 404 || named(&|d| d["errorCode"] == "UNREGISTERED") {
        return true;
    }
    status == 400
        && named(&|d| {
            d["fieldViolations"]
                .as_array()
                .is_some_and(|v| v.iter().any(|f| f["field"] == "message.token"))
        })
}

/// An authenticated sender for one batch.
enum Sender {
    Fcm {
        http: reqwest::Client,
        url: String,
        bearer: String,
    },
    #[cfg(debug_assertions)]
    Fake,
}

impl Sender {
    async fn open(t: Transport) -> Option<Sender> {
        match t {
            Transport::Fcm(f) => {
                let http = reqwest::Client::new();
                let Some(bearer) = access_token(f, &http).await else {
                    tracing::warn!("FCM auth failed; push dropped");
                    return None;
                };
                Some(Sender::Fcm {
                    url: fcm_send_url(&f.project),
                    http,
                    bearer,
                })
            }
            #[cfg(debug_assertions)]
            Transport::Fake => Some(Sender::Fake),
        }
    }

    async fn post(&self, msg: &Value) -> Option<Reply> {
        match self {
            Sender::Fcm { http, url, bearer } => post_one(http, url, bearer, msg).await,
            #[cfg(debug_assertions)]
            Sender::Fake => fake::post(msg),
        }
    }
}

/// Send each `(token, message)` once, retrying a transient failure (429/5xx)
/// one time, and revoke a token FCM reports as dead ([`token_is_dead`]).
/// Never fails: a push is best-effort, and every error is logged.
async fn deliver(pool: &PgPool, t: Transport, batch: Vec<(String, Value)>) {
    if batch.is_empty() {
        return;
    }
    let Some(sender) = Sender::open(t).await else {
        return;
    };
    for (token, msg) in batch {
        let mut reply = sender.post(&msg).await;
        if matches!(reply, Some((s, _)) if s == 429 || s >= 500) {
            tokio::time::sleep(Duration::from_millis(500)).await;
            reply = sender.post(&msg).await;
        }
        match reply {
            Some((s, body)) if token_is_dead(s, &body) => {
                if let Err(e) = sqlx::query(
                    "UPDATE push_devices SET revoked_at = now() \
                      WHERE token = $1 AND revoked_at IS NULL",
                )
                .bind(&token)
                .execute(pool)
                .await
                {
                    tracing::warn!(error = %e, "could not drop an unregistered push token");
                }
            }
            Some((s, body)) if s >= 300 => {
                tracing::warn!(status = s, error = %body["error"]["status"], "push refused")
            }
            _ => {}
        }
    }
}

/// Send a notification to every live device of `who` registered under any of
/// `apps`, in the background. Runs even if the caller's transaction
/// later rolls back is not a concern here — call this only after the write
/// it announces has committed. `title_key` is looked up the same way as
/// `key` (so each app supplies its own app-name word).
pub fn send(
    pool: &PgPool,
    who: Recipient,
    apps: &'static [&'static str],
    title_key: &str,
    key: &str,
    args: &Value,
) {
    let Some(t) = transport() else { return };
    let (pool, title_key, key, args) = (
        pool.clone(),
        title_key.to_string(),
        key.to_string(),
        args.clone(),
    );
    tokio::spawn(async move {
        let (user_id, employee_id) = who.cols();
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT token, locale FROM push_devices \
              WHERE user_id IS NOT DISTINCT FROM $1 AND employee_id IS NOT DISTINCT FROM $2 \
                AND app = ANY($3) AND revoked_at IS NULL",
        )
        .bind(user_id)
        .bind(employee_id)
        .bind(apps)
        .fetch_all(&pool)
        .await
        .unwrap_or_default();
        let batch = rows
            .into_iter()
            .filter_map(|(token, locale)| {
                let ar = locale != "en";
                let body = render(&key, &args, ar)?;
                let msg = json!({ "message": {
                    "token": token,
                    "notification": { "title": word(&title_key, ar), "body": body },
                    "data": { "key": key, "args": args.to_string() },
                    "android": { "priority": "high" },
                }});
                Some((token, msg))
            })
            .collect();
        deliver(&pool, t, batch).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_answer_about_the_token_drops_it() {
        // 404 / UNREGISTERED: the app is gone.
        let unregistered = json!({"error": {"code": 404, "status": "NOT_FOUND",
            "details": [{"@type": "type.googleapis.com/google.firebase.fcm.v1.FcmError",
                         "errorCode": "UNREGISTERED"}]}});
        assert!(token_is_dead(404, &unregistered));
        assert!(
            token_is_dead(404, &Value::Null),
            "a bare 404 is still a dead token"
        );
        // A 400 naming the token.
        let bad_token = json!({"error": {"code": 400, "status": "INVALID_ARGUMENT",
            "details": [
                {"@type": "type.googleapis.com/google.firebase.fcm.v1.FcmError",
                 "errorCode": "INVALID_ARGUMENT"},
                {"@type": "type.googleapis.com/google.rpc.BadRequest",
                 "fieldViolations": [{"field": "message.token",
                                      "description": "Invalid registration token"}]}]}});
        assert!(token_is_dead(400, &bad_token));
        // A 400 about the MESSAGE must never unregister the device.
        let bad_payload = json!({"error": {"code": 400, "status": "INVALID_ARGUMENT",
            "details": [
                {"@type": "type.googleapis.com/google.firebase.fcm.v1.FcmError",
                 "errorCode": "INVALID_ARGUMENT"},
                {"@type": "type.googleapis.com/google.rpc.BadRequest",
                 "fieldViolations": [{"field": "message.android.notification.channel_id"}]}]}});
        assert!(!token_is_dead(400, &bad_payload));
        assert!(
            !token_is_dead(400, &Value::Null),
            "an unexplained 400 keeps the token"
        );
        // Everything else keeps it too.
        for s in [200, 401, 403, 429, 500, 503] {
            assert!(!token_is_dead(s, &Value::Null), "{s}");
        }
    }

    #[test]
    fn a_push_reads_in_the_phones_language() {
        let args = json!({ "reason": "Late", "amount": 5000 });
        assert_eq!(
            render("staff.n_deduction_added", &args, false).unwrap(),
            "A deduction was added: Late (50.00 EGP)"
        );
        let args = json!({ "kind": "leave", "date": "2026-09-30" });
        assert!(
            render("staff.n_request_approved", &args, true)
                .unwrap()
                .contains("إجازة")
        );
        // Every key the server notifies has a word, in both languages.
        for k in [
            "staff.n_cover",
            "staff.n_new_phone",
            "staff.n_paid",
            "staff.n_flag_suspicious",
        ] {
            assert!(word(k, false).is_some() && word(k, true).is_some(), "{k}");
        }
    }
}
