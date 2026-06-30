//! OSRM road-distance client.
//!
//! Backend-only: the OSRM instance is never exposed to browsers. The public
//! delivery-quote endpoint calls this, returns only `{zone, distance, fee}`,
//! and degrades gracefully when OSRM is unset or unreachable (the caller maps
//! the typed error to an `unavailable` quote rather than a hard HTTP failure).
//!
//! `OSRM_URL` (e.g. `http://10.0.0.5:5000`) is read from the environment on each
//! call so the box can be pointed/repointed without a rebuild. Unset ⟹
//! `NotConfigured`, which the quote endpoint surfaces as "delivery unavailable".

use std::time::Duration;

/// A WGS84 coordinate. `lat`/`lng` in decimal degrees.
#[derive(Debug, Clone, Copy)]
pub struct LatLng {
    pub lat: f64,
    pub lng: f64,
}

/// Why a road-distance lookup could not produce a number. All map to a
/// graceful "quote unavailable" at the endpoint, never a 5xx the client sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsrmError {
    /// `OSRM_URL` is not set — delivery distance cannot be computed.
    NotConfigured,
    /// Network error, timeout, or non-2xx from OSRM.
    Unreachable,
    /// OSRM responded but found no route (`code != "Ok"` or no routes).
    NoRoute,
    /// OSRM responded with a body we could not parse.
    BadResponse,
}

/// Road distance in metres between two points via OSRM's `driving` profile.
///
/// OSRM takes coordinates as `lng,lat`. We request `overview=false` (no
/// geometry) with a short timeout — we only need the scalar distance.
pub async fn road_distance_meters(from: LatLng, to: LatLng) -> Result<f64, OsrmError> {
    let base = std::env::var("OSRM_URL").map_err(|_| OsrmError::NotConfigured)?;
    let base = base.trim_end_matches('/');
    if base.is_empty() {
        return Err(OsrmError::NotConfigured);
    }

    let url = format!(
        "{base}/route/v1/driving/{flng:.6},{flat:.6};{tlng:.6},{tlat:.6}\
         ?overview=false&alternatives=false&steps=false",
        flng = from.lng,
        flat = from.lat,
        tlng = to.lng,
        tlat = to.lat,
    );

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(4))
        .build()
        .map_err(|_| OsrmError::Unreachable)?;

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|_| OsrmError::Unreachable)?;

    if !resp.status().is_success() {
        return Err(OsrmError::Unreachable);
    }

    let body: serde_json::Value = resp.json().await.map_err(|_| OsrmError::BadResponse)?;

    if body.get("code").and_then(|c| c.as_str()) != Some("Ok") {
        return Err(OsrmError::NoRoute);
    }

    body.get("routes")
        .and_then(|r| r.as_array())
        .and_then(|a| a.first())
        .and_then(|r| r.get("distance"))
        .and_then(|d| d.as_f64())
        .ok_or(OsrmError::NoRoute)
}

/// Road **travel time** in seconds between two points via OSRM's `driving`
/// profile. Free-flow only — OSRM models no live traffic. Same request as
/// [`road_distance_meters`] but reads `duration` instead of `distance`. Used by
/// the reservations waitlist "head out" nudge and the host-board ETA display;
/// degrades to the typed error (never a 5xx) exactly like the distance lookup.
pub async fn road_eta_seconds(from: LatLng, to: LatLng) -> Result<f64, OsrmError> {
    let base = std::env::var("OSRM_URL").map_err(|_| OsrmError::NotConfigured)?;
    let base = base.trim_end_matches('/');
    if base.is_empty() {
        return Err(OsrmError::NotConfigured);
    }

    let url = format!(
        "{base}/route/v1/driving/{flng:.6},{flat:.6};{tlng:.6},{tlat:.6}\
         ?overview=false&alternatives=false&steps=false",
        flng = from.lng,
        flat = from.lat,
        tlng = to.lng,
        tlat = to.lat,
    );

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(4))
        .build()
        .map_err(|_| OsrmError::Unreachable)?;

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|_| OsrmError::Unreachable)?;

    if !resp.status().is_success() {
        return Err(OsrmError::Unreachable);
    }

    let body: serde_json::Value = resp.json().await.map_err(|_| OsrmError::BadResponse)?;

    if body.get("code").and_then(|c| c.as_str()) != Some("Ok") {
        return Err(OsrmError::NoRoute);
    }

    body.get("routes")
        .and_then(|r| r.as_array())
        .and_then(|a| a.first())
        .and_then(|r| r.get("duration"))
        .and_then(|d| d.as_f64())
        .ok_or(OsrmError::NoRoute)
}

/// Great-circle (straight-line) distance in metres between two points — the
/// fallback used when OSRM is unset or unreachable, so a delivery quote can
/// still be produced from the configured zone rings. Underestimates real road
/// distance, which is acceptable for a degraded fallback.
pub fn haversine_meters(from: LatLng, to: LatLng) -> f64 {
    const R: f64 = 6_371_000.0; // mean Earth radius, metres
    let lat1 = from.lat.to_radians();
    let lat2 = to.lat.to_radians();
    let dlat = (to.lat - from.lat).to_radians();
    let dlng = (to.lng - from.lng).to_radians();
    let a = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlng / 2.0).sin().powi(2);
    R * 2.0 * a.sqrt().atan2((1.0 - a).sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    const EARTH_R: f64 = 6_371_000.0;
    const HALF_CIRCUMFERENCE: f64 = std::f64::consts::PI * EARTH_R; // max great-circle distance

    #[test]
    fn same_point_is_zero() {
        let p = LatLng {
            lat: 30.0444,
            lng: 31.2357,
        }; // Cairo
        assert!(haversine_meters(p, p).abs() < 1e-6);
    }

    #[test]
    fn is_symmetric() {
        let a = LatLng {
            lat: 30.0444,
            lng: 31.2357,
        };
        let b = LatLng {
            lat: 31.2001,
            lng: 29.9187,
        }; // Alexandria
        assert!((haversine_meters(a, b) - haversine_meters(b, a)).abs() < 1e-6);
    }

    #[test]
    fn one_degree_of_latitude_is_about_111km() {
        // One degree of latitude ≈ R · π/180 ≈ 111_195 m, anywhere.
        let d = haversine_meters(LatLng { lat: 0.0, lng: 0.0 }, LatLng { lat: 1.0, lng: 0.0 });
        assert!((d - 111_195.0).abs() < 50.0, "got {d}");
    }

    #[test]
    fn antipodal_is_bounded_by_half_circumference() {
        // Points half the globe apart are the farthest possible.
        let d = haversine_meters(
            LatLng { lat: 0.0, lng: 0.0 },
            LatLng {
                lat: 0.0,
                lng: 180.0,
            },
        );
        assert!(d <= HALF_CIRCUMFERENCE + 1.0, "got {d}");
        assert!(d > HALF_CIRCUMFERENCE - 1.0, "got {d}");
    }
}
