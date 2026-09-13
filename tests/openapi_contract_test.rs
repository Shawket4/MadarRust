//! OpenAPI contract checks for the tills rework follow-ups.
use std::collections::HashMap;

use madar_rust::openapi::ApiDoc;
use serde_json::Value;
use utoipa::OpenApi;

fn doc() -> Value {
    serde_json::from_str(&ApiDoc::openapi().to_json().unwrap()).unwrap()
}

#[test]
fn operation_ids_are_unique() {
    let doc = doc();
    let mut seen: HashMap<String, Vec<String>> = HashMap::new();
    for (path, ops) in doc["paths"].as_object().unwrap() {
        for (method, op) in ops.as_object().unwrap() {
            if let Some(id) = op.get("operationId").and_then(|v| v.as_str()) {
                seen.entry(id.to_string()).or_default().push(format!("{method} {path}"));
            }
        }
    }
    let dups: Vec<_> = seen.into_iter().filter(|(_, v)| v.len() > 1).collect();
    assert!(dups.is_empty(), "duplicate operationIds: {dups:?}");
    assert_eq!(doc["paths"]["/bundles/{id}"]["get"]["operationId"], "get_bundle");
    assert_eq!(doc["paths"]["/tills/{till_id}/cash-movements"]["post"]["operationId"], "add_cash_movement");
}

fn enum_of(doc: &Value, schema: &str, field: &str) -> Vec<String> {
    let prop = &doc["components"]["schemas"][schema]["properties"][field];
    let r = prop["$ref"]
        .as_str()
        .or_else(|| prop["oneOf"].as_array().and_then(|a| a.iter().find_map(|x| x["$ref"].as_str())))
        .unwrap_or_else(|| panic!("{schema}.{field} is not a $ref: {prop}"));
    let name = r.rsplit('/').next().unwrap();
    doc["components"]["schemas"][name]["enum"]
        .as_array()
        .unwrap_or_else(|| panic!("{name} has no enum"))
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn till_and_device_vocabularies_are_enums() {
    let doc = doc();
    assert_eq!(enum_of(&doc, "Till", "status"), ["open", "closed", "force_closed"]);
    assert_eq!(enum_of(&doc, "Till", "verification"), ["server", "lan", "unverified", "legacy"]);
    assert_eq!(enum_of(&doc, "TillBrief", "status"), ["open", "closed", "force_closed"]);
    assert_eq!(enum_of(&doc, "Device", "kind"), ["pos", "kds", "waiter"]);
    assert_eq!(enum_of(&doc, "RegisterDeviceRequest", "kind"), ["pos", "kds", "waiter"]);
}

#[test]
fn branch_settings_and_client_versions_are_documented() {
    let doc = doc();
    assert!(doc["paths"]["/branches/{id}"]["patch"].is_object());
    assert!(doc["paths"]["/branches/{id}"]["put"].is_object());
    let branch = &doc["components"]["schemas"]["Branch"]["properties"];
    assert!(branch["old_bill_hours"].is_object() && branch["standard_float"].is_object());
    let update = &doc["components"]["schemas"]["UpdateBranchRequest"]["properties"];
    assert!(update["old_bill_hours"].is_object() && update["standard_float"].is_object());
    assert!(doc["paths"]["/devices/client-versions"]["get"].is_object());
}
