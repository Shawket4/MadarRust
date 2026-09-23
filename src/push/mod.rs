//! Push notifications through Firebase Cloud Messaging, shared by every app
//! (Dawam today; a future management app registers under its own `app` name
//! and reuses this same sender). See `push_devices` (migration
//! `20260926000400_push_devices.sql`), which superseded the Dawam-only
//! `staff_devices.push_token`.
//!
//! Off unless `FCM_SERVICE_ACCOUNT_FILE` names a Firebase service-account
//! JSON; callers work without it (the inbox/notification row is still
//! written, just nothing is pushed to a phone).

pub mod handlers;
pub mod routes;
pub mod words;

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::errors::AppError;

/// Register or rebind a device's push token (PUT /push/token and the Dawam
/// alias PUT /staff/me/push-token both call this).
pub(crate) async fn register(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
    app: &str,
    token: &str,
    locale: &str,
    platform: &str,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO push_devices (org_id, user_id, app, token, locale, platform) \
          VALUES ($1, $2, $3, $4, $5, $6) \
          ON CONFLICT (token) WHERE revoked_at IS NULL DO UPDATE SET \
            org_id = EXCLUDED.org_id, user_id = EXCLUDED.user_id, app = EXCLUDED.app, \
            locale = EXCLUDED.locale, platform = EXCLUDED.platform, last_seen_at = now()",
    )
    .bind(org_id)
    .bind(user_id)
    .bind(app)
    .bind(token)
    .bind(locale)
    .bind(platform)
    .execute(pool)
    .await?;
    Ok(())
}

/// Revoke one device's registration (sign-out). Idempotent: revoking a token
/// that is not (or no longer) this user's is a no-op, never an error.
pub(crate) async fn unregister(
    pool: &PgPool,
    user_id: Uuid,
    app: &str,
    token: &str,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE push_devices SET revoked_at = now() \
          WHERE token = $1 AND user_id = $2 AND app = $3 AND revoked_at IS NULL",
    )
    .bind(token)
    .bind(user_id)
    .bind(app)
    .execute(pool)
    .await?;
    Ok(())
}

/// Revoke every device of `user_id` for `app` (used when a phone/account is
/// force-revoked, e.g. Dawam's RO-4/RO-10).
pub(crate) async fn revoke_all(pool: &PgPool, user_id: Uuid, app: &str) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE push_devices SET revoked_at = now() \
          WHERE user_id = $1 AND app = $2 AND revoked_at IS NULL",
    )
    .bind(user_id)
    .bind(app)
    .execute(pool)
    .await?;
    Ok(())
}

fn word(key: &str, ar: bool) -> Option<&'static str> {
    words::WORDS
        .iter()
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
    if let Some((t, at)) = cached.as_ref() {
        if at.elapsed() < Duration::from_secs(50 * 60) {
            return Some(t.clone());
        }
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

/// One outbound send, split out so retry can call it more than once.
async fn post_one(http: &reqwest::Client, url: &str, bearer: &str, msg: &Value) -> Option<u16> {
    match http.post(url).bearer_auth(bearer).json(msg).send().await {
        Ok(r) => Some(r.status().as_u16()),
        Err(e) => {
            tracing::warn!(error = %e, "push not sent");
            None
        }
    }
}

/// Send a notification to every live device of `user_id` registered under
/// any of `apps`, in the background. Runs even if the caller's transaction
/// later rolls back is not a concern here — call this only after the write
/// it announces has committed. `title_key` is looked up the same way as
/// `key` (so each app supplies its own app-name word).
pub fn send(
    pool: &PgPool,
    user_id: Uuid,
    apps: &'static [&'static str],
    title_key: &str,
    key: &str,
    args: &Value,
) {
    let Some(f) = fcm() else { return };
    let (pool, title_key, key, args) = (
        pool.clone(),
        title_key.to_string(),
        key.to_string(),
        args.clone(),
    );
    tokio::spawn(async move {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT token, locale FROM push_devices \
              WHERE user_id = $1 AND app = ANY($2) AND revoked_at IS NULL",
        )
        .bind(user_id)
        .bind(apps)
        .fetch_all(&pool)
        .await
        .unwrap_or_default();
        if rows.is_empty() {
            return;
        }
        let http = reqwest::Client::new();
        let Some(bearer) = access_token(f, &http).await else {
            tracing::warn!("FCM auth failed; push dropped");
            return;
        };
        let url = fcm_send_url(&f.project);
        for (token, locale) in rows {
            let ar = locale != "en";
            let Some(body) = render(&key, &args, ar) else {
                continue;
            };
            let msg = json!({ "message": {
                "token": token,
                "notification": { "title": word(&title_key, ar), "body": body },
                "data": { "key": key, "args": args.to_string() },
                "android": { "priority": "high" },
            }});
            // One retry on a transient failure (429/5xx); anything else is final.
            let mut status = post_one(&http, &url, &bearer, &msg).await;
            if matches!(status, Some(s) if s == 429 || s >= 500) {
                tokio::time::sleep(Duration::from_millis(500)).await;
                status = post_one(&http, &url, &bearer, &msg).await;
            }
            match status {
                Some(400) | Some(404) => {
                    let _ =
                        sqlx::query("UPDATE push_devices SET revoked_at = now() WHERE token = $1")
                            .bind(&token)
                            .execute(&pool)
                            .await;
                }
                Some(s) if s >= 300 => tracing::warn!(status = s, "push refused"),
                _ => {}
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

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
