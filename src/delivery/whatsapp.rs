//! WhatsApp send (fire-and-forget) + the device-trust token.
//!
//! The gateway is send-only, copying the apex-petroapp pattern:
//! `POST {WHATSAPP_SERVICE_URL}/send/message {phone, message}`. Failures are
//! logged, never surfaced to the caller, and never block the request. When
//! `WHATSAPP_SERVICE_URL` is unset the send is skipped (dev / degrade-safe).
//!
//! Device-trust token: on a successful OTP verify the browser is handed a signed
//! token bound to the phone. Future orders from that device skip OTP; a new
//! device re-verifies. Signed with the app's JWT secret (HS256), 90-day expiry.

use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::delivery::gateway;
use crate::errors::AppError;

#[derive(Serialize, Deserialize)]
struct DeviceClaims {
    /// Normalised phone number this device verified.
    sub: String,
    /// Marks the token type so an app JWT can never be mistaken for a device token.
    kind: String,
    exp: usize,
}

const DEVICE_KIND: &str = "delivery_device";

/// Mint a 90-day device-trust token for a verified phone.
pub fn issue_device_token(secret: &str, phone: &str) -> Result<String, AppError> {
    let exp = (Utc::now() + Duration::days(90)).timestamp().max(0) as usize;
    let claims = DeviceClaims {
        sub: phone.to_string(),
        kind: DEVICE_KIND.to_string(),
        exp,
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|_| AppError::Internal)
}

/// True when `token` is a valid, unexpired device token for `phone`.
pub fn verify_device_token(secret: &str, phone: &str, token: &str) -> bool {
    let validation = Validation::default(); // HS256 + exp enforced
    decode::<DeviceClaims>(token, &DecodingKey::from_secret(secret.as_bytes()), &validation)
        .map(|d| d.claims.kind == DEVICE_KIND && d.claims.sub == phone)
        .unwrap_or(false)
}

/// Send a WhatsApp message, fire-and-forget. Returns immediately; the request is
/// never blocked and failures are logged only. Honors the super-admin pause
/// switch ([`gateway::is_paused`]) — when paused the send is skipped entirely,
/// so the gateway can be muted for maintenance without unlinking the number.
pub fn send_message(pool: PgPool, phone: String, message: String) {
    let Ok(base) = std::env::var("WHATSAPP_SERVICE_URL") else {
        tracing::info!(phone = %phone, "WHATSAPP_SERVICE_URL unset — skipping WhatsApp send");
        return;
    };
    let auth = std::env::var("WHATSAPP_AUTH_HEADER").ok();
    tokio::spawn(async move {
        if gateway::is_paused(&pool).await {
            tracing::info!(phone = %phone, "WhatsApp sending paused — skipping send");
            return;
        }
        let url = format!("{}/send/message", base.trim_end_matches('/'));
        let client = reqwest::Client::new();
        let mut req = client
            .post(&url)
            .json(&serde_json::json!({ "phone": phone, "message": message }));
        if let Some(h) = auth {
            req = req.header("Authorization", h);
        }
        match req.send().await {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => tracing::warn!(status = %resp.status(), "WhatsApp gateway returned non-2xx"),
            Err(e) => tracing::warn!(error = %e, "WhatsApp send failed"),
        }
    });
}

/// The customer-facing tracking link for a delivery order, or `None` when
/// `PUBLIC_ORDER_BASE_URL` is unset (degrade-safe — the message is sent without
/// a link, exactly like the `WHATSAPP_SERVICE_URL`-unset send-skip).
pub fn tracking_url(order_id: Uuid) -> Option<String> {
    std::env::var("PUBLIC_ORDER_BASE_URL")
        .ok()
        .map(|base| format!("{}/track/{}", base.trim_end_matches('/'), order_id))
}

/// Append the tracking link to a message when one is configured.
fn with_tracking(message: String, order_id: Uuid) -> String {
    match tracking_url(order_id) {
        Some(url) => format!("{message}\nTrack your order: {url}"),
        None => message,
    }
}

pub fn build_otp_message(code: &str) -> String {
    format!("Your Sufrix verification code is {code}. It expires in 5 minutes.")
}

pub fn build_order_received_message(delivery_ref: &str, order_id: Uuid) -> String {
    with_tracking(
        format!("We've received your order {delivery_ref}. We'll let you know when it's on the way."),
        order_id,
    )
}

pub fn build_order_accepted_message(delivery_ref: &str, order_id: Uuid) -> String {
    with_tracking(
        format!("Your order {delivery_ref} has been accepted and is being prepared."),
        order_id,
    )
}

pub fn build_out_for_delivery_message(delivery_ref: &str, order_id: Uuid) -> String {
    with_tracking(format!("Your order {delivery_ref} is on the way!"), order_id)
}

pub fn build_delivered_message(delivery_ref: &str, order_id: Uuid) -> String {
    with_tracking(
        format!("Your order {delivery_ref} has been delivered. Enjoy!"),
        order_id,
    )
}
