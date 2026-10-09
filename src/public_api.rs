//! What the API host says about itself to someone who did not sign in.
//!
//! `api.madar-pos.cloud` answered its root and `/openapi.json` with an empty
//! 404, so an agent that found the host learned nothing. Two routes now:
//!  - `GET /`: what this is and where the documentation is;
//!  - `GET /openapi.json`: the public part of the API (`/public/…`), the
//!    endpoints the shop pages use without an account. The full spec stays
//!    behind `MADAR_ENABLE_SWAGGER_UI`: it describes every staff and owner
//!    route, which is nobody else's business.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use actix_web::{HttpResponse, web};
use serde_json::{Value, json};
use utoipa::OpenApi;

use crate::openapi::ApiDoc;

const SITE: &str = "https://get.madar-pos.cloud/";
pub(crate) const SERVER: &str = "https://api.madar-pos.cloud";
const CONTACT_EMAIL: &str = "shawket.4@icloud.com";

/// Paths under `/public/` that are not for callers: the shop pages' HTML,
/// requested by nginx.
const INTERNAL: &[&str] = &["/public/tenant-shell"];

/// Every `#/components/<kind>/<name>` reference under `v`.
fn refs(v: &Value, out: &mut BTreeSet<String>) {
    match v {
        Value::Object(m) => {
            if let Some(Value::String(r)) = m.get("$ref") {
                out.insert(r.clone());
            }
            m.values().for_each(|x| refs(x, out));
        }
        Value::Array(a) => a.iter().for_each(|x| refs(x, out)),
        _ => {}
    }
}

/// The public subset of a full spec: its `/public/` paths, the components they
/// reach (transitively), the security schemes they name and the tags they use.
pub fn public_subset(full: &Value) -> Value {
    let paths: serde_json::Map<String, Value> = full["paths"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(p, _)| p.starts_with("/public/") && !INTERNAL.contains(&p.as_str()))
                .map(|(p, v)| (p.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();

    let mut wanted = BTreeSet::new();
    refs(&Value::Object(paths.clone()), &mut wanted);
    let mut components = serde_json::Map::new();
    let mut done = BTreeSet::new();
    while let Some(r) = wanted.iter().find(|r| !done.contains(*r)).cloned() {
        done.insert(r.clone());
        let mut parts = r.trim_start_matches("#/components/").splitn(2, '/');
        let (Some(kind), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        let Some(def) = full["components"][kind].get(name) else {
            continue;
        };
        refs(def, &mut wanted);
        components
            .entry(kind.to_string())
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .expect("object")
            .insert(name.to_string(), def.clone());
    }

    let mut schemes = BTreeSet::new();
    let mut tags = BTreeSet::new();
    for item in paths.values() {
        for op in item.as_object().into_iter().flat_map(|m| m.values()) {
            for req in op["security"].as_array().into_iter().flatten() {
                schemes.extend(req.as_object().into_iter().flat_map(|m| m.keys().cloned()));
            }
            tags.extend(
                op["tags"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|t| t.as_str().map(str::to_string)),
            );
        }
    }
    if !schemes.is_empty() {
        let all = &full["components"]["securitySchemes"];
        let kept: serde_json::Map<String, Value> = schemes
            .iter()
            .filter_map(|s| all.get(s).map(|d| (s.clone(), d.clone())))
            .collect();
        components.insert("securitySchemes".into(), Value::Object(kept));
    }
    let tags: Vec<Value> = full["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t["name"].as_str().is_some_and(|n| tags.contains(n)))
        .cloned()
        .collect();

    json!({
        "openapi": full["openapi"],
        "info": {
            "title": "Madar POS public API",
            // The contract's version, not the server build's: see the policy below.
            "version": "1",
            "description": "The endpoints a café's own pages use without an account: \
                            its brand and links page, branches and menus, delivery and \
                            table ordering, order tracking, bookings and the rewards card. \
                            Every call names the shop (`slug` or an id); there is no \
                            cross-shop listing. Madar POS is a point of sale for cafés \
                            and restaurants in Egypt: https://get.madar-pos.cloud/\n\n\
                            **Versioning.** This is version 1. Changes within it only add \
                            (new endpoints, new optional fields). A breaking change comes \
                            at a new path; the old endpoint keeps working for at least six \
                            months after it is marked `deprecated` here, and meanwhile \
                            answers with `Deprecation` (RFC 9745) and `Sunset` (RFC 8594) \
                            headers and a `Link` to its replacement.\n\n\
                            **Rate limits.** Every answer carries `RateLimit-Policy` and \
                            `RateLimit` (draft-ietf-httpapi-ratelimit-headers): the \
                            caller's quota per 60-second window, what is left, and the \
                            seconds until it is full again. A 429 carries `Retry-After`.",
            "contact": { "name": "Madar POS", "email": CONTACT_EMAIL, "url": SITE },
            "license": full["info"]["license"],
        },
        "externalDocs": { "description": "Guide, versioning policy and the MCP server", "url": format!("{SITE}en/developers/") },
        "servers": [{ "url": SERVER, "description": "Production" }],
        "tags": tags,
        "paths": paths,
        "components": components,
    })
}

pub(crate) fn spec() -> &'static str {
    static SPEC: OnceLock<String> = OnceLock::new();
    SPEC.get_or_init(|| {
        let full = serde_json::to_value(ApiDoc::openapi()).unwrap_or(Value::Null);
        serde_json::to_string(&public_subset(&full)).unwrap_or_else(|_| "{}".into())
    })
}

/// The public part of this API as OpenAPI 3.1.
#[utoipa::path(get, path = "/openapi.json", tag = "public",
    operation_id = "public_openapi",
    responses((status = 200, description = "OpenAPI 3.1 document of the `/public/` endpoints", content_type = "application/json")))]
pub async fn openapi_json() -> HttpResponse {
    HttpResponse::Ok()
        .content_type("application/json")
        .insert_header(("Cache-Control", "public, max-age=3600"))
        .body(spec())
}

/// What this host is, for a person or an agent that opens it.
#[utoipa::path(get, path = "/", tag = "public", operation_id = "api_root",
    responses((status = 200, description = "The API's name and where its documentation is", content_type = "application/json")))]
pub async fn root() -> HttpResponse {
    HttpResponse::Ok()
        .insert_header(("Cache-Control", "public, max-age=3600"))
        .json(json!({
            "name": "Madar POS API",
            "description": "The API behind Madar POS, a point of sale for cafés and restaurants in Egypt. \
                            Endpoints under /public/ need no account; everything else is for a shop's \
                            own staff and apps.",
            "openapi": format!("{SERVER}/openapi.json"),
            "mcp": format!("{SERVER}/mcp"),
            "website": SITE,
            "contact": CONTACT_EMAIL,
        }))
}

/// The answer for an address nothing serves: JSON, like every other error
/// here, instead of an empty body. Mounted as the app's default service.
pub async fn not_found() -> HttpResponse {
    HttpResponse::NotFound().json(json!({ "error": "Not found" }))
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    // HEAD as well as GET: link checkers and crawlers probe with HEAD.
    cfg.route("/", web::get().to(root))
        .route("/", web::head().to(root))
        .route("/openapi.json", web::get().to(openapi_json))
        .route("/openapi.json", web::head().to(openapi_json));
}
