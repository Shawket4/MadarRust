//! A keyed fingerprint of a PIN, so a PIN can be FOUND instead of scanned
//! (POS_SIGNIN_OVERHAUL.md §2).
//!
//! Stored PINs are salted hashes: the same PIN hashed twice looks different, so
//! a PIN can only be verified one row at a time, each verify deliberately slow.
//! With no name to narrow by — which is where PIN-only sign-in is going —
//! sign-in would scan every PIN holder in the org, seconds of work that grows
//! with the payroll.
//!
//! ```text
//! pin_fingerprint = HMAC-SHA256(server_key, org_id || 0x00 || pin)
//! ```
//!
//! - Deterministic, so it is an indexed lookup: one row, instantly. Sign-in is
//!   then ONE slow verify whatever the headcount.
//! - The key lives in the server's environment, **never in the database and
//!   never in the repo**: a stolen database cannot be tested against at all.
//! - Scoped by org, so the same PIN in two orgs fingerprints differently and
//!   one org's table says nothing about another's.
//!
//! **The salted hash stays.** A 4–6 digit PIN is at most a million
//! possibilities: with the fingerprint ALONE, a leaked key lets an attacker
//! precompute every one and read off every PIN by lookup — one leak, total
//! compromise. With the hash still verifying, that attacker has learned WHICH
//! row to attack and must still grind each PIN separately. Two secrets, two
//! places. The cost is one slow verify on the single row the fingerprint found;
//! the scan, which was the slow part, is gone either way.
//!
//! **Rotation** (§2.5): `MADAR_PIN_FINGERPRINT_KEY_OLD` stays readable for a
//! grace period. A lookup tries the current key and then the old one, and a
//! successful sign-in re-stamps the row under the current key — the plaintext
//! PIN is in hand exactly then.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

/// `MADAR_PIN_FINGERPRINT_KEY` (>= 32 bytes). Debug and test builds fall back
/// to a key derived from `JWT_SECRET`, exactly like the asset URL secret, so
/// local runs work; a release build without one refuses to start
/// ([`crate::boot_config`]).
fn key() -> Vec<u8> {
    match std::env::var("MADAR_PIN_FINGERPRINT_KEY") {
        Ok(s) if s.len() >= 32 => s.into_bytes(),
        _ => {
            let jwt = std::env::var("JWT_SECRET").unwrap_or_default();
            format!("madar-dev-pin-fingerprint-key:{jwt}").into_bytes()
        }
    }
}

/// The previous key, while one is being rotated out.
fn old_key() -> Option<Vec<u8>> {
    std::env::var("MADAR_PIN_FINGERPRINT_KEY_OLD")
        .ok()
        .filter(|s| s.len() >= 32)
        .map(String::into_bytes)
}

fn compute(k: &[u8], org: Uuid, pin: &str) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(k).expect("hmac takes any key length");
    mac.update(org.as_bytes());
    mac.update(&[0]);
    mac.update(pin.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// The fingerprint to STORE: always under the current key.
pub fn fingerprint(org: Uuid, pin: &str) -> Vec<u8> {
    compute(&key(), org, pin)
}

/// The fingerprints to LOOK UP by: the current key first, then the old one
/// while a rotation is in flight. A row found under the old key is re-stamped
/// by the caller.
pub fn lookup_fingerprints(org: Uuid, pin: &str) -> Vec<Vec<u8>> {
    let mut v = vec![fingerprint(org, pin)];
    if let Some(k) = old_key() {
        let old = compute(&k, org, pin);
        if old != v[0] {
            v.push(old);
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_scoped_by_org() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        assert_eq!(fingerprint(a, "1234"), fingerprint(a, "1234"));
        assert_ne!(fingerprint(a, "1234"), fingerprint(b, "1234"));
        assert_ne!(fingerprint(a, "1234"), fingerprint(a, "1235"));
        assert_eq!(fingerprint(a, "1234").len(), 32);
    }

    #[test]
    fn the_org_and_the_pin_cannot_run_into_each_other() {
        // Without a separator, org bytes and PIN digits could be re-cut to the
        // same input. The 0x00 byte between them is what makes that impossible.
        let org = Uuid::new_v4();
        assert_ne!(fingerprint(org, "12\u{0}34"), fingerprint(org, "1234"));
    }
}
