//! The server's Ed25519 key for signed permission snapshots
//! (PERMISSIONS_ARCHITECTURE §4.4).
//!
//! - `MADAR_AUTHZ_SIGNING_KEY`: 64 hex chars, the 32-byte Ed25519 seed. It
//!   lives in the environment, never in the database or the repo.
//! - `MADAR_AUTHZ_SIGNING_KEY_OLD`: the previous seed while a rotation is in
//!   flight. Its PUBLIC half is still published, so a tablet holding a snapshot
//!   signed by it keeps verifying until it fetches a fresh one.
//! - Debug and test builds fall back to a key derived from `JWT_SECRET`, so
//!   local runs work. A RELEASE build without a real key does not fall back:
//!   the snapshot endpoint answers 503 instead of signing with a key printed in
//!   this repository. (Deliberately not a boot requirement: a missing key must
//!   not stop the till API from starting.)
//! - `kid` is the first 16 hex characters of SHA-256(public key), so it names
//!   the key itself and needs no registry.

use ed25519_dalek::{Signer, SigningKey};
use serde::Serialize;
use sha2::{Digest, Sha256};
use utoipa::ToSchema;

use madar_authz::snapshot::{SignedSnapshot, SnapshotBody};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex32(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
    }
    Some(out)
}

fn seed_from_env(var: &str) -> Option<[u8; 32]> {
    std::env::var(var).ok().and_then(|s| unhex32(&s))
}

/// The current signing key, or `None` when a release build has none.
pub fn signing_key() -> Option<SigningKey> {
    if let Some(seed) = seed_from_env("MADAR_AUTHZ_SIGNING_KEY") {
        return Some(SigningKey::from_bytes(&seed));
    }
    if cfg!(debug_assertions) || cfg!(test) {
        let jwt = std::env::var("JWT_SECRET").unwrap_or_default();
        let seed: [u8; 32] = Sha256::digest(format!("madar-dev-authz-signing-key:{jwt}")).into();
        return Some(SigningKey::from_bytes(&seed));
    }
    None
}

pub fn kid_of(key: &SigningKey) -> String {
    hex(&Sha256::digest(key.verifying_key().as_bytes()))[..16].to_string()
}

/// A public key a device verifies snapshots with.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AuthzPublicKey {
    pub kid: String,
    /// Hex-encoded 32-byte Ed25519 public key.
    pub public_key: String,
}

/// Current key first, then the one being rotated out.
pub fn public_keys() -> Vec<AuthzPublicKey> {
    let mut keys: Vec<SigningKey> = signing_key().into_iter().collect();
    if let Some(seed) = seed_from_env("MADAR_AUTHZ_SIGNING_KEY_OLD") {
        keys.push(SigningKey::from_bytes(&seed));
    }
    keys.iter()
        .map(|k| AuthzPublicKey {
            kid: kid_of(k),
            public_key: hex(k.verifying_key().as_bytes()),
        })
        .collect()
}

/// Sign a snapshot body with the current key.
pub fn sign(body: SnapshotBody) -> Option<SignedSnapshot> {
    let key = signing_key()?;
    let sig = key.sign(&body.signing_bytes());
    Some(SignedSnapshot {
        kid: kid_of(&key),
        body,
        sig: hex(&sig.to_bytes()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    #[test]
    fn a_signed_body_verifies_under_its_published_key_and_not_when_changed() {
        let body = SnapshotBody {
            v: SnapshotBody::VERSION,
            spec_version: madar_authz::SPEC_VERSION,
            org_id: "o".into(),
            branch_id: "b".into(),
            device_id: "d".into(),
            org_epoch: 3,
            issued_at: 1,
            expires_at: i64::MAX,
            policy: Default::default(),
            users: vec![],
        };
        let signed = sign(body.clone()).unwrap();
        let pk = public_keys()
            .into_iter()
            .find(|k| k.kid == signed.kid)
            .unwrap();
        let vk = VerifyingKey::from_bytes(&unhex32(&pk.public_key).unwrap()).unwrap();
        let mut sig = [0u8; 64];
        for (i, c) in signed.sig.as_bytes().chunks(2).enumerate() {
            sig[i] = u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap();
        }
        let sig = Signature::from_bytes(&sig);
        assert!(vk.verify(&body.signing_bytes(), &sig).is_ok());
        let mut tampered = body;
        tampered.org_epoch = 4;
        assert!(vk.verify(&tampered.signing_bytes(), &sig).is_err());
    }
}
