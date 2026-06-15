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
