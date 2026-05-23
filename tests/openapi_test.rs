use sufrix_rust::openapi::ApiDoc;
use utoipa::OpenApi;

#[test]
fn test_openapi_generation_is_valid() {
    let doc = ApiDoc::openapi();
    let json = doc.to_json().expect("Failed to serialize OpenAPI to JSON");
    assert!(!json.is_empty());
}
