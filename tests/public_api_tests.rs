//! What the API host tells someone who did not sign in: `GET /`, the public
//! spec at `GET /openapi.json`, a JSON 404 for an address nothing serves and
//! a JSON 429 from the per-route limiters.

use std::collections::BTreeSet;

use actix_web::{App, HttpResponse, test, web};
use serde_json::Value;

use madar_rust::public_api;

macro_rules! app {
    () => {
        test::init_service(
            App::new()
                .configure(public_api::configure)
                .route("/public/ping", web::get().to(HttpResponse::Ok))
                .default_service(web::to(public_api::not_found)),
        )
        .await
    };
}

async fn get_json<S>(app: &S, uri: &str) -> (u16, String, Value)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    let resp = test::call_service(app, test::TestRequest::get().uri(uri).to_request()).await;
    let status = resp.status().as_u16();
    let ctype = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let body = test::read_body(resp).await;
    (
        status,
        ctype,
        serde_json::from_slice(&body).unwrap_or(Value::Null),
    )
}

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

#[actix_web::test]
async fn the_root_says_what_this_is() {
    let app = app!();
    let (status, ctype, body) = get_json(&app, "/").await;
    assert_eq!(status, 200);
    assert_eq!(ctype, "application/json");
    assert_eq!(body["name"], "Madar POS API");
    assert_eq!(body["openapi"], "https://api.madar-pos.cloud/openapi.json");
    assert_eq!(body["website"], "https://get.madar-pos.cloud/");

    // Link checkers probe with HEAD: it answers like GET.
    for uri in ["/", "/openapi.json"] {
        let resp = test::call_service(
            &app,
            test::TestRequest::default()
                .method(actix_web::http::Method::HEAD)
                .uri(uri)
                .to_request(),
        )
        .await;
        assert_eq!(resp.status().as_u16(), 200, "HEAD {uri}");
    }
}

/// Only the `/public/` endpoints, minus the shell nginx calls, and a document
/// that stands on its own: every reference resolves inside it.
#[actix_web::test]
async fn the_spec_is_the_public_part_and_complete() {
    let app = app!();
    let (status, ctype, spec) = get_json(&app, "/openapi.json").await;
    assert_eq!(status, 200);
    assert_eq!(ctype, "application/json");
    assert_eq!(spec["info"]["title"], "Madar POS public API");
    assert_eq!(spec["info"]["contact"]["email"], "shawket.4@icloud.com");
    assert_eq!(spec["servers"][0]["url"], "https://api.madar-pos.cloud");
    // The contract's version and its policy, for agents deciding to integrate.
    assert_eq!(spec["info"]["version"], "1");
    let about = spec["info"]["description"].as_str().unwrap();
    for promise in [
        "Deprecation",
        "Sunset",
        "six months",
        "RateLimit-Policy",
        "Retry-After",
    ] {
        assert!(about.contains(promise), "the policy names {promise}");
    }
    assert_eq!(
        spec["externalDocs"]["url"],
        "https://get.madar-pos.cloud/en/developers/"
    );

    let paths = spec["paths"].as_object().unwrap();
    assert!(paths.len() > 10, "{}", paths.len());
    for (p, ops) in paths {
        assert!(p.starts_with("/public/"), "{p} is not public");
        // Self-describing for agents: a doc comment on the handler is its summary.
        for (method, op) in ops.as_object().unwrap() {
            assert!(
                op["operationId"].is_string(),
                "{method} {p} has no operationId"
            );
            assert!(
                op["summary"].as_str().is_some_and(|s| !s.is_empty()),
                "{method} {p} has no summary: add a /// line above its #[utoipa::path]"
            );
        }
    }
    assert!(!paths.contains_key("/public/tenant-shell"));
    assert!(paths.contains_key("/public/orgs/links"));
    assert!(paths.contains_key("/public/branches/{id}/menu"));

    let mut wanted = BTreeSet::new();
    refs(&spec, &mut wanted);
    assert!(!wanted.is_empty());
    for r in wanted {
        let mut parts = r.trim_start_matches("#/components/").splitn(2, '/');
        let (kind, name) = (parts.next().unwrap(), parts.next().unwrap());
        assert!(
            spec["components"][kind].get(name).is_some(),
            "{r} does not resolve"
        );
    }

    // Nothing from the staff side came along.
    let schemas = spec["components"]["schemas"].as_object().unwrap();
    for staff in ["OpenTillRequest", "CreateOrderRequest", "UpdateOrgRequest"] {
        assert!(!schemas.contains_key(staff), "{staff} is not public");
    }
    let full =
        serde_json::to_value(<madar_rust::openapi::ApiDoc as utoipa::OpenApi>::openapi()).unwrap();
    assert!(
        schemas.len() < full["components"]["schemas"].as_object().unwrap().len() / 2,
        "pruned to what the public paths reach"
    );
}

#[actix_web::test]
async fn an_address_nothing_serves_is_a_json_404() {
    let app = app!();
    let (status, ctype, body) = get_json(&app, "/no/such/thing").await;
    assert_eq!(status, 404);
    assert_eq!(ctype, "application/json");
    assert_eq!(body["error"], "Not found");
}

/// A route's own limiter answers in the API's shape, with how long to wait.
#[actix_web::test]
async fn a_route_limiter_answers_429_in_json() {
    use actix_governor::{Governor, GovernorConfigBuilder};
    let gov = GovernorConfigBuilder::default()
        .key_extractor(madar_rust::rate_limit::PeerIpOrLocalhost)
        .seconds_per_request(60)
        .burst_size(1)
        .finish()
        .unwrap();
    let app = test::init_service(
        App::new().service(
            web::resource("/public/ping")
                .wrap(Governor::new(&gov))
                .route(web::get().to(HttpResponse::Ok)),
        ),
    )
    .await;
    let call = || {
        test::TestRequest::get()
            .uri("/public/ping")
            .peer_addr("10.20.0.1:4000".parse().unwrap())
            .to_request()
    };
    assert_eq!(
        test::call_service(&app, call()).await.status().as_u16(),
        200
    );
    let resp = test::call_service(&app, call()).await;
    assert_eq!(resp.status().as_u16(), 429);
    let retry: u64 = resp
        .headers()
        .get("retry-after")
        .unwrap()
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(retry > 0 && retry <= 60, "{retry}");
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/json"
    );
    let body: Value = test::read_body_json(resp).await;
    assert_eq!(body["code"], "RATE_LIMITED");
    let said = body["retry_after_seconds"].as_u64().unwrap();
    assert!(said.abs_diff(retry) <= 1, "{said} vs {retry}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .starts_with("Too many requests")
    );
}
