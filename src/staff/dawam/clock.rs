//! Punches and pings recorded while the phone was offline (CL-10, CL-11).
//!
//! The phone's wall clock is never trusted. It sends the last server time it
//! saw and the time-since-boot elapsed from that moment, plus the GPS fix's
//! own satellite time; the server rebuilds the real time from those. A phone
//! that restarted in between has lost its time-since-boot, so its punch is
//! marked "time unverified" for the manager.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::errors::AppError;

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct OfflineStamp {
    /// The last server time the phone saw (a response's `Date`).
    pub server_time: DateTime<Utc>,
    /// Time-since-boot elapsed from `server_time` to the event, in ms.
    pub elapsed_ms: i64,
    /// The phone restarted after `server_time`, so `elapsed_ms` means nothing.
    #[serde(default)]
    pub rebooted: bool,
    /// The GPS fix's own satellite time, when it had one.
    #[serde(default)]
    pub gps_time: Option<DateTime<Utc>>,
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

pub fn rebuild(stamp: Option<&OfflineStamp>, now: DateTime<Utc>) -> Result<Stamped, AppError> {
    let Some(s) = stamp else {
        return Ok(Stamped {
            at: now,
            offline: false,
            unverified: false,
        });
    };
    let (at, unverified) = if s.rebooted {
        // Satellite time is still good; without it the last server time is
        // only a lower bound.
        (s.gps_time.unwrap_or(s.server_time), true)
    } else {
        let at = s.server_time + Duration::milliseconds(s.elapsed_ms.max(0));
        let doubt = s
            .gps_time
            .is_some_and(|g| (g - at).num_minutes().abs() > GPS_TOLERANCE_MIN);
        (at, doubt)
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
        at,
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

    #[test]
    fn rebuilds_from_server_time_and_uptime() {
        let now = t("2026-09-22T10:00:00Z");
        let s = OfflineStamp {
            server_time: t("2026-09-22T08:00:00Z"),
            elapsed_ms: 30 * 60 * 1000,
            rebooted: false,
            gps_time: Some(t("2026-09-22T08:31:00Z")),
        };
        let r = rebuild(Some(&s), now).unwrap();
        assert_eq!(r.at, t("2026-09-22T08:30:00Z"));
        assert!(r.offline && !r.unverified);
        // GPS far from the rebuilt time: doubted, not replaced.
        let far = OfflineStamp {
            gps_time: Some(t("2026-09-22T09:30:00Z")),
            ..s.clone()
        };
        assert!(rebuild(Some(&far), now).unwrap().unverified);
        // Rebooted: satellite time wins, and it is still doubted.
        let boot = OfflineStamp {
            rebooted: true,
            ..s.clone()
        };
        let r = rebuild(Some(&boot), now).unwrap();
        assert_eq!((r.at, r.unverified), (t("2026-09-22T08:31:00Z"), true));
        // The future and a stale queue are refused.
        let fut = OfflineStamp {
            elapsed_ms: 3 * 3600 * 1000,
            ..s.clone()
        };
        assert!(rebuild(Some(&fut), now).is_err());
        let old = OfflineStamp {
            server_time: t("2026-09-01T08:00:00Z"),
            ..s
        };
        assert!(rebuild(Some(&old), now).is_err());
        assert_eq!(rebuild(None, now).unwrap().at, now);
    }
}
