use madar_rust::openapi::ApiDoc;
use utoipa::OpenApi;

#[test]
fn test_openapi_generation_is_valid() {
    let doc = ApiDoc::openapi();
    let json = doc.to_json().expect("Failed to serialize OpenAPI to JSON");
    assert!(!json.is_empty());
}

/// Every `$ref` in the spec names a schema that is registered. A type reached
/// only through `IntoParams` (a query param) is not registered by utoipa, and
/// its dangling `$ref` crashed the Dart client generator (ArchiveFilter,
/// 2026-10-18): list such types in `components(schemas(...))`.
#[test]
fn no_ref_dangles() {
    fn walk(v: &serde_json::Value, out: &mut Vec<String>) {
        match v {
            serde_json::Value::Object(m) => {
                if let Some(serde_json::Value::String(r)) = m.get("$ref") {
                    out.push(r.clone());
                }
                m.values().for_each(|x| walk(x, out));
            }
            serde_json::Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    let doc: serde_json::Value =
        serde_json::from_str(&ApiDoc::openapi().to_json().unwrap()).unwrap();
    let schemas = doc["components"]["schemas"].as_object().expect("schemas");
    let mut refs = Vec::new();
    walk(&doc, &mut refs);
    let mut dangling: Vec<String> = refs
        .into_iter()
        .filter(|r| match r.strip_prefix("#/components/schemas/") {
            Some(name) => !schemas.contains_key(name),
            None => true,
        })
        .collect();
    dangling.sort();
    dangling.dedup();
    assert!(dangling.is_empty(), "dangling $refs: {dangling:?}");
}
