//! Offline-PIN verifier (argon2id) for the offline-auth bundle (POS rebuild,
//! Layer 3). Distinct from the bcrypt login `pin_hash`: this is the ONLY thing
//! shipped to devices (via `GET /orgs/{id}/offline-auth-bundle`), so it is
//! memory-hard and a leak is never the login credential. The device verifies a
//! typed PIN against it OFFLINE; the server only ever DERIVES it.
use argon2::password_hash::{rand_core::OsRng, PasswordHash, SaltString};
use argon2::{Argon2, PasswordHasher, PasswordVerifier};

/// Derive an argon2id PHC string for a teller's offline PIN.
pub fn hash_offline_pin(pin: &str) -> Result<String, argon2::password_hash::Error> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(pin.as_bytes(), &salt)?
        .to_string())
}

/// Verify a typed PIN against a stored argon2id PHC string. The shipping
/// verification runs in the rust-core on the device; this is kept for tests
/// and completeness.
#[allow(dead_code)]
pub fn verify_offline_pin(pin: &str, phc: &str) -> bool {
    PasswordHash::new(phc)
        .map(|h| Argon2::default().verify_password(pin.as_bytes(), &h).is_ok())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_then_verify_roundtrip() {
        let phc = hash_offline_pin("1234").unwrap();
        assert!(phc.starts_with("$argon2id$"), "should be argon2id PHC, got {phc}");
        assert!(verify_offline_pin("1234", &phc));
        assert!(!verify_offline_pin("9999", &phc));
    }

    #[test]
    fn distinct_salts_per_hash() {
        assert_ne!(hash_offline_pin("1234").unwrap(), hash_offline_pin("1234").unwrap());
    }
}
