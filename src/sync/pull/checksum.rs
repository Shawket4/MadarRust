//! R-checksum: first 16 hex of sha256 over sorted `"<entity_id>:<seq>"` joined
//! by `\n`. Same bytes as `madar-core::sync_pull::checksum_of`; the shared vector
//! lives in `tests/fixtures/sync_checksum_vector.json`.
use sha2::{Digest, Sha256};

pub fn checksum_of(rows: &[(String, i64)]) -> String {
    let mut lines: Vec<String> = rows.iter().map(|(id, seq)| format!("{id}:{seq}")).collect();
    lines.sort();
    let digest = Sha256::digest(lines.join("\n").as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn checksum_formula_matches_pos_vector() {
        let v: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/sync_checksum_vector.json"
        )))
        .unwrap();
        let rows: Vec<(String, i64)> = v["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r["id"].as_str().unwrap().to_string(),
                    r["seq"].as_i64().unwrap(),
                )
            })
            .collect();
        assert_eq!(super::checksum_of(&rows), v["checksum"].as_str().unwrap());
    }
}
