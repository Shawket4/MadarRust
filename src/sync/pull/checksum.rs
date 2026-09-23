//! R-checksum: first 16 hex of sha256 over sorted `"<entity_id>:<seq>"` joined
//! by `\n`. One copy with the POS core, in madar-shared (`madar_sync`), pinned
//! by its `sync_checksum_vector.json`.
pub use madar_sync::checksum_of;
