//! Punches and pings recorded while the phone was offline (CL-10, CL-11).
//!
//! The phone's wall clock is never trusted. It sends the last server time it
//! saw and the time-since-boot elapsed from that moment, plus the GPS fix's
//! own satellite time; the server rebuilds the real time from those. A phone
//! that restarted in between has lost its time-since-boot, so its punch is
//! marked "time unverified" for the manager.
//!
//! THE SIGNED ANCHOR (audit 03 CL-11, P0/P1). "The last server time the phone
//! saw" used to be whatever the phone said it was, so any moment of the last
//! week could be sent and was taken as fact. Now every response the staff app
//! gets carries [`ANCHOR_HEADER`]: the server's time, signed with HMAC-SHA256
//! for that phone's device row. The phone keeps the latest one and sends it
//! back inside its offline stamp. Only a valid anchor for the SAME device
//! dates a punch as verified; a missing, forged, altered or another phone's
//! anchor still dates it (from the time the phone claims) but the punch is
//! marked `time_unverified` for the manager.
//!
//! What the anchor bounds: the rebuilt time can never be before the last
//! moment the server actually spoke to that phone, nor after now. Within that
//! window the phone's time-since-boot is still the phone's word; a shorter
//! elapsed than real would need a tampered OS clock, and a reboot (which resets
//! it) is detected and marks the punch unverified.

use chrono::{DateTime, Duration, TimeZone, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::errors::AppError;

/// The response header every staff-app response carries (see the module doc).
pub const ANCHOR_HEADER: &str = "x-dawam-time";
const ANCHOR_VERSION: &str = "v1";

type HmacSha256 = Hmac<Sha256>;

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct OfflineStamp {
    /// The last server time the phone saw. Only a guide: a valid `anchor`
    /// replaces it, and without one the punch is marked unverified.
    pub server_time: DateTime<Utc>,
    /// Time-since-boot elapsed from `server_time` to the event, in ms.
    pub elapsed_ms: i64,
    /// The phone restarted after `server_time`, so `elapsed_ms` means nothing.
    #[serde(default)]
    pub rebooted: bool,
    /// The GPS fix's own satellite time, when the platform gives one (Android's
    /// GNSS provider; iOS gives none).
    #[serde(default)]
    pub gps_time: Option<DateTime<Utc>>,
    /// The `X-Dawam-Time` value of the last response the phone saw (signed).
    #[serde(default)]
    pub anchor: Option<String>,
}

/// GPS and the rebuilt clock may disagree by this much before it is doubted.
const GPS_TOLERANCE_MIN: i64 = 5;
/// A queue older than this is refused rather than priced.
const MAX_AGE_DAYS: i64 = 7;

/// When the event happened, and whether the manager should doubt it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamped {
    pub at: DateTime<Utc>,
    pub offline: bool,
    pub unverified: bool,
}

fn mac(secret: &JwtSecret, device: Uuid, ms: i64) -> HmacSha256 {
    let mut key = b"dawam-anchor:".to_vec();
    key.extend_from_slice(secret.0.as_bytes());
    let mut m = HmacSha256::new_from_slice(&key).expect("HMAC takes any key length");
    m.update(format!("{ANCHOR_VERSION}|{device}|{ms}").as_bytes());
    m
}

/// The signed server time for `device` at `at`: `v1.<epoch ms>.<hex hmac>`.
pub fn sign_anchor(secret: &JwtSecret, device: Uuid, at: DateTime<Utc>) -> String {
    let ms = at.timestamp_millis();
    let tag = mac(secret, device, ms).finalize().into_bytes();
    let hex: String = tag.iter().map(|b| format!("{b:02x}")).collect();
    format!("{ANCHOR_VERSION}.{ms}.{hex}")
}

/// The server time an anchor carries, if it was signed for `device`.
pub fn verify_anchor(secret: &JwtSecret, device: Uuid, anchor: &str) -> Option<DateTime<Utc>> {
    let mut parts = anchor.trim().splitn(3, '.');
    if parts.next()? != ANCHOR_VERSION {
        return None;
    }
    let ms: i64 = parts.next()?.parse().ok()?;
    let hex = parts.next()?;
    if hex.len() != 64 {
        return None;
    }
    let bytes: Vec<u8> = (0..32)
        .map(|i| u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok())
        .collect::<Option<_>>()?;
    // `verify_slice` compares in constant time.
    mac(secret, device, ms).verify_slice(&bytes).ok()?;
    Utc.timestamp_millis_opt(ms).single()
}

/// Who may vouch for an offline stamp: the phone's device row and the key.
#[derive(Clone, Copy)]
pub struct Verifier<'a> {
    pub secret: &'a JwtSecret,
    /// The staff device the request came from; `None` (a dashboard session)
    /// can never present a valid anchor.
    pub device: Option<Uuid>,
}

pub fn rebuild(
    stamp: Option<&OfflineStamp>,
    now: DateTime<Utc>,
    verifier: Verifier<'_>,
) -> Result<Stamped, AppError> {
    let Some(s) = stamp else {
        return Ok(Stamped {
            at: now,
            offline: false,
            unverified: false,
        });
    };
    // The server's own word for when it last spoke to this phone, else the
    // phone's claim — which is dated but never trusted.
    let signed = match (s.anchor.as_deref(), verifier.device) {
        (Some(a), Some(d)) => verify_anchor(verifier.secret, d, a).filter(|t| *t <= now),
        _ => None,
    };
    let base = signed.unwrap_or(s.server_time);
    let mut unverified = signed.is_none();
    let at = if s.rebooted {
        // Time-since-boot was lost. Satellite time can still place it, but
        // only inside what is possible: after the last contact, not after now.
        unverified = true;
        s.gps_time
            .filter(|g| *g >= base && *g <= now + Duration::minutes(1))
            .unwrap_or(base)
    } else {
        let at = base + Duration::milliseconds(s.elapsed_ms.max(0));
        if s.gps_time
            .is_some_and(|g| (g - at).num_minutes().abs() > GPS_TOLERANCE_MIN)
        {
            unverified = true;
        }
        at
    };
    if at > now + Duration::minutes(1) {
        return Err(AppError::BadRequest(
            "That punch is dated in the future.".into(),
        ));
    }
    if at < now - Duration::days(MAX_AGE_DAYS) {
        return Err(AppError::BadRequest(
            "That punch is more than a week old; ask your manager to add it.".into(),
        ));
    }
    Ok(Stamped {
        at: at.min(now),
        offline: true,
        unverified,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn key() -> JwtSecret {
        JwtSecret("unit".into())
    }

    #[test]
    fn rebuilds_from_a_signed_anchor_and_uptime() {
        let now = t("2026-09-22T10:00:00Z");
        let dev = Uuid::new_v4();
        let v = Verifier {
            secret: &key(),
            device: Some(dev),
        };
        let seen = t("2026-09-22T08:00:00Z");
        let s = OfflineStamp {
            // The phone's claim is ignored when the anchor is valid.
            server_time: t("2026-09-20T08:00:00Z"),
            elapsed_ms: 30 * 60 * 1000,
            rebooted: false,
            gps_time: Some(t("2026-09-22T08:31:00Z")),
            anchor: Some(sign_anchor(&key(), dev, seen)),
        };
        let r = rebuild(Some(&s), now, v).unwrap();
        assert_eq!(r.at, t("2026-09-22T08:30:00Z"));
        assert!(r.offline && !r.unverified);
        // GPS far from the rebuilt time: doubted, not replaced.
        let far = OfflineStamp {
            gps_time: Some(t("2026-09-22T09:30:00Z")),
            ..s.clone()
        };
        assert!(rebuild(Some(&far), now, v).unwrap().unverified);
        // Rebooted: satellite time places it, and it is still doubted.
        let boot = OfflineStamp {
            rebooted: true,
            ..s.clone()
        };
        let r = rebuild(Some(&boot), now, v).unwrap();
        assert_eq!((r.at, r.unverified), (t("2026-09-22T08:31:00Z"), true));
        // …but never before the last contact the server signed.
        let early = OfflineStamp {
            rebooted: true,
            gps_time: Some(t("2026-09-22T07:00:00Z")),
            ..s.clone()
        };
        assert_eq!(rebuild(Some(&early), now, v).unwrap().at, seen);
        // The future and a stale queue are refused.
        let fut = OfflineStamp {
            elapsed_ms: 3 * 3600 * 1000,
            ..s.clone()
        };
        assert!(rebuild(Some(&fut), now, v).is_err());
        let old = OfflineStamp {
            anchor: Some(sign_anchor(&key(), dev, t("2026-09-01T08:00:00Z"))),
            ..s.clone()
        };
        assert!(rebuild(Some(&old), now, v).is_err());
        assert_eq!(rebuild(None, now, v).unwrap().at, now);
    }

    #[test]
    fn a_forged_or_missing_anchor_is_dated_but_unverified() {
        let now = t("2026-09-22T10:00:00Z");
        let dev = Uuid::new_v4();
        let v = Verifier {
            secret: &key(),
            device: Some(dev),
        };
        let claimed = t("2026-09-22T06:00:00Z");
        let base = OfflineStamp {
            server_time: claimed,
            elapsed_ms: 60_000,
            rebooted: false,
            gps_time: None,
            anchor: None,
        };
        let r = rebuild(Some(&base), now, v).unwrap();
        assert_eq!(r.at, claimed + Duration::minutes(1));
        assert!(r.unverified, "no anchor: the phone's word only");

        let mut forged = sign_anchor(&key(), dev, claimed);
        forged.replace_range(forged.len() - 2.., "00");
        for bad in [
            forged,
            sign_anchor(&JwtSecret("other".into()), dev, claimed),
            sign_anchor(&key(), Uuid::new_v4(), claimed),
            format!("v1.{}.zz", claimed.timestamp_millis()),
            "garbage".to_string(),
        ] {
            let s = OfflineStamp {
                anchor: Some(bad.clone()),
                ..base.clone()
            };
            assert!(rebuild(Some(&s), now, v).unwrap().unverified, "{bad}");
        }
        // A dashboard session has no device: never verified.
        let good = OfflineStamp {
            anchor: Some(sign_anchor(&key(), dev, claimed)),
            ..base.clone()
        };
        let no_dev = Verifier {
            secret: &key(),
            device: None,
        };
        assert!(rebuild(Some(&good), now, no_dev).unwrap().unverified);
        assert!(!rebuild(Some(&good), now, v).unwrap().unverified);
        // An anchor "signed" in the future is not the server's.
        let ahead = OfflineStamp {
            anchor: Some(sign_anchor(&key(), dev, now + Duration::hours(1))),
            elapsed_ms: 0,
            ..base
        };
        assert!(rebuild(Some(&ahead), now, v).unwrap().unverified);
    }

    #[test]
    fn an_anchor_round_trips_only_for_its_device() {
        let dev = Uuid::new_v4();
        let at = t("2026-09-22T08:00:00.123Z");
        let a = sign_anchor(&key(), dev, at);
        assert_eq!(verify_anchor(&key(), dev, &a), Some(at));
        assert_eq!(verify_anchor(&key(), Uuid::new_v4(), &a), None);
        assert_eq!(verify_anchor(&JwtSecret("x".into()), dev, &a), None);
    }
}
