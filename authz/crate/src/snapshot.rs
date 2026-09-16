//! Device permission snapshots (PERMISSIONS_ARCHITECTURE §4.4). The wire types
//! and the canonical bytes a server signature covers; signing and verification
//! live with the key material (backend: `authz::keys`, core: `authz_offline`).

use serde::{Deserialize, Serialize};

use crate::{EffectiveSet, OrgPolicy, SPEC_VERSION};

/// One person on a device's snapshot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SnapshotUser {
    pub user_id: String,
    pub name: String,
    /// The role name older code paths route on (teller / waiter / kitchen / ...).
    pub legacy_role: String,
    pub is_owner: bool,
    pub active: bool,
    /// Effective capabilities at the snapshot's branch.
    pub eff: EffectiveSet,
}

/// Everything a till needs to authorize offline, for one branch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SnapshotBody {
    pub v: u32,
    pub spec_version: u32,
    pub org_id: String,
    pub branch_id: String,
    pub device_id: String,
    pub org_epoch: i64,
    pub issued_at: i64,
    pub expires_at: i64,
    pub policy: OrgPolicy,
    pub users: Vec<SnapshotUser>,
}

/// A snapshot and the server's signature over [`SnapshotBody::signing_bytes`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SignedSnapshot {
    pub kid: String,
    pub body: SnapshotBody,
    /// Hex-encoded Ed25519 signature.
    pub sig: String,
}

impl SnapshotBody {
    pub const VERSION: u32 = 1;

    /// Canonical bytes: compact JSON of the body. serde_json writes struct fields
    /// in declaration order and `BTreeMap`s sorted, so both sides produce the
    /// same bytes for the same body.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut out = b"madar-authz-snapshot-v1\n".to_vec();
        out.extend(serde_json::to_vec(self).unwrap_or_default());
        out
    }

    pub fn is_current_spec(&self) -> bool {
        self.spec_version == SPEC_VERSION
    }

    pub fn user(&self, user_id: &str) -> Option<&SnapshotUser> {
        self.users.iter().find(|u| u.user_id == user_id)
    }
}

/// Why a snapshot is not usable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotProblem {
    BadSignature,
    UnknownKey,
    WrongDevice,
    WrongBranch,
    Expired,
}

/// The checks that need no key: device, branch and expiry.
pub fn check_binding(
    body: &SnapshotBody,
    device_id: &str,
    branch_id: &str,
    now: i64,
) -> Result<(), SnapshotProblem> {
    if body.device_id != device_id {
        return Err(SnapshotProblem::WrongDevice);
    }
    if body.branch_id != branch_id {
        return Err(SnapshotProblem::WrongBranch);
    }
    if now >= body.expires_at {
        return Err(SnapshotProblem::Expired);
    }
    Ok(())
}
