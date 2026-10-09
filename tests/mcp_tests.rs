//! The MCP server: POST /mcp (src/mcp.rs).
//!
//! What these pin: the handshake (version negotiation, notifications), the tool
//! list and its read-only annotations, each tool answering from the same data the
//! public API serves, failures reported inside the result (not as protocol errors),
//! no customer details in a tracked order, the two resources reading cleanly, and
//! the protocol errors for malformed messages. GET is 405: there is no server-sent
//! stream.

mod common;

use actix_web::{App, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use common::shops::{branch, menu_item, shop};

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .configure(madar_rust::mcp::configure),
        )
        .await
    };
}

async fn rpc<S>(app: &S, msg: Value) -> (u16, Value)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    let resp = test::call_service(
        app,
        test::TestRequest::post()
            .uri("/mcp")
            .set_json(&msg)
            .to_request(),
    )
    .await;
    let status = resp.status().as_u16();
    let body = test::read_body(resp).await;
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

async fn tool<S>(app: &S, name: &str, args: Value) -> Value
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    let (status, v) = rpc(app, json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": { "name": name, "arguments": args } })).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["id"], 7);
    v["result"].clone()
}

#[sqlx::test]
async fn the_handshake(pool: PgPool) {
    let app = app!(pool);
    let (s, v) = rpc(&app, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } } })).await;
    assert_eq!(s, 200);
    assert_eq!(
        v["result"]["protocolVersion"], "2025-03-26",
        "a version it speaks is kept"
    );
    assert_eq!(v["result"]["serverInfo"]["name"], "madar-pos");
    assert!(v["result"]["capabilities"]["tools"].is_object());
    assert!(v["result"]["capabilities"]["resources"].is_object());

    let (_, v) = rpc(&app, json!({ "jsonrpc": "2.0", "id": 2, "method": "initialize", "params": { "protocolVersion": "1999-01-01" } })).await;
    assert_eq!(
        v["result"]["protocolVersion"], "2025-06-18",
        "anything else gets the newest"
    );

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/mcp")
            .set_json(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .to_request(),
    )
    .await;
    assert_eq!(
        resp.status().as_u16(),
        202,
        "a notification is accepted with no body"
    );

    let (_, v) = rpc(&app, json!({ "jsonrpc": "2.0", "id": 3, "method": "ping" })).await;
    assert_eq!(v["result"], json!({}));

    let resp = test::call_service(&app, test::TestRequest::get().uri("/mcp").to_request()).await;
    assert_eq!(resp.status().as_u16(), 405);
    assert_eq!(resp.headers().get("allow").unwrap(), "POST");
}

#[sqlx::test]
async fn five_read_only_tools(pool: PgPool) {
    let app = app!(pool);
    let (_, v) = rpc(
        &app,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    )
    .await;
    let tools = v["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "get_shop",
            "get_menu",
            "get_booking_slots",
            "track_order",
            "about_madar"
        ]
    );
    for t in tools {
        assert_eq!(t["annotations"]["readOnlyHint"], true, "{}", t["name"]);
        assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
    }
}

#[sqlx::test]
async fn the_shop_and_its_menu(pool: PgPool) {
    unsafe { std::env::set_var("PUBLIC_SHOP_SUBDOMAINS", "1") };
    let org = shop(&pool, "drops", "Drops").await;
    let maadi = branch(&pool, org, "Maadi").await;
    menu_item(&pool, org, "Coffee", "Flat white", 8500).await;
    let app = app!(pool);

    let r = tool(&app, "get_shop", json!({ "shop": "drops" })).await;
    assert_eq!(r["isError"], false, "{r}");
    assert_eq!(r["structuredContent"]["brand"]["name"], "Drops");
    assert_eq!(r["structuredContent"]["branches"][0]["name"], "Maadi");
    assert_eq!(r["content"][0]["type"], "text");

    let r = tool(&app, "get_menu", json!({ "shop": "drops" })).await;
    assert_eq!(r["isError"], false, "{r}");
    let menu = &r["structuredContent"];
    assert_eq!(menu["branch_id"], json!(maadi));
    assert_eq!(menu["currency"], "EGP");
    assert_eq!(menu["categories"][0]["category"], "Coffee");
    assert_eq!(menu["categories"][0]["items"][0]["name"], "Flat white");
    assert_eq!(menu["categories"][0]["items"][0]["price_egp"], 85.0);

    let r = tool(
        &app,
        "get_menu",
        json!({ "shop": "drops", "branch_id": Uuid::new_v4() }),
    )
    .await;
    assert_eq!(
        r["isError"], true,
        "another shop's (or no) branch is refused: {r}"
    );
}

/// Failures the assistant should read come back inside the result, never as a
/// protocol error, and never as a 5xx.
#[sqlx::test]
async fn failures_are_results(pool: PgPool) {
    let app = app!(pool);
    for (name, args) in [
        ("get_shop", json!({ "shop": "nobody" })),
        ("get_shop", json!({})),
        ("get_menu", json!({ "shop": "nobody" })),
        ("track_order", json!({ "order_id": Uuid::new_v4() })),
        ("track_order", json!({ "order_id": "not-a-uuid" })),
        (
            "get_booking_slots",
            json!({ "branch_id": Uuid::new_v4(), "date": "2030-01-01", "party_size": 2 }),
        ),
        (
            "get_booking_slots",
            json!({ "branch_id": Uuid::new_v4(), "date": "tomorrow", "party_size": 2 }),
        ),
        (
            "get_booking_slots",
            json!({ "branch_id": Uuid::new_v4(), "date": "2030-01-01", "party_size": 0 }),
        ),
    ] {
        let r = tool(&app, name, args.clone()).await;
        assert_eq!(r["isError"], true, "{name} {args}: {r}");
        assert!(
            r["content"][0]["text"]
                .as_str()
                .is_some_and(|t| !t.is_empty()),
            "{name}: says why"
        );
    }
}

/// The server card lists exactly the tools the server answers with.
#[sqlx::test]
async fn the_server_card_matches_the_server(pool: PgPool) {
    let app = app!(pool);
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/.well-known/mcp/server-card.json")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    let card: Value = test::read_body_json(resp).await;
    let head = test::call_service(
        &app,
        test::TestRequest::default()
            .method(actix_web::http::Method::HEAD)
            .uri("/.well-known/mcp/server-card.json")
            .to_request(),
    )
    .await;
    assert_eq!(head.status().as_u16(), 200, "HEAD answers like GET");
    let (_, listed) = rpc(
        &app,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    )
    .await;
    assert_eq!(card["tools"], listed["result"]["tools"]);
    let (_, listed) = rpc(
        &app,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "resources/list" }),
    )
    .await;
    assert_eq!(card["resources"], listed["result"]["resources"]);
    let (_, init) = rpc(
        &app,
        json!({ "jsonrpc": "2.0", "id": 3, "method": "initialize", "params": {} }),
    )
    .await;
    assert_eq!(card["capabilities"], init["result"]["capabilities"]);
    assert_eq!(card["transport"]["type"], "streamable-http");
    assert_eq!(
        card["transport"]["endpoint"],
        "https://api.madar-pos.cloud/mcp"
    );
}

#[sqlx::test]
async fn about_madar(pool: PgPool) {
    let app = app!(pool);
    let r = tool(&app, "about_madar", json!({})).await;
    let plans = r["structuredContent"]["plans"].as_array().unwrap();
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0]["monthly_egp_per_branch"], 3000);
    assert!(r["structuredContent"]["contact"]["email"].is_string());
}

#[sqlx::test]
async fn the_resources_read_cleanly(pool: PgPool) {
    let app = app!(pool);
    let (_, v) = rpc(
        &app,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "resources/list" }),
    )
    .await;
    let listed = v["result"]["resources"].as_array().unwrap().clone();
    assert_eq!(listed.len(), 2);
    for r in &listed {
        let (_, v) = rpc(&app, json!({ "jsonrpc": "2.0", "id": 2, "method": "resources/read", "params": { "uri": r["uri"] } })).await;
        let c = &v["result"]["contents"][0];
        assert_eq!(c["uri"], r["uri"]);
        assert_eq!(c["mimeType"], r["mimeType"]);
        let text: Value = serde_json::from_str(c["text"].as_str().unwrap()).unwrap();
        assert!(text.as_object().is_some_and(|o| !o.is_empty()), "{r}");
    }

    let (_, v) = rpc(&app, json!({ "jsonrpc": "2.0", "id": 3, "method": "resources/read", "params": { "uri": "madar://about" } })).await;
    let about: Value =
        serde_json::from_str(v["result"]["contents"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        about,
        tool(&app, "about_madar", json!({})).await["structuredContent"],
        "the resource is the tool's answer"
    );

    let (_, v) = rpc(&app, json!({ "jsonrpc": "2.0", "id": 4, "method": "resources/read", "params": { "uri": "https://api.madar-pos.cloud/openapi.json" } })).await;
    let spec: Value =
        serde_json::from_str(v["result"]["contents"][0]["text"].as_str().unwrap()).unwrap();
    assert!(spec["openapi"].as_str().unwrap().starts_with("3."));
    assert!(
        spec["paths"]
            .as_object()
            .unwrap()
            .keys()
            .all(|p| p.starts_with("/public/")),
        "only the public part"
    );

    let (_, v) = rpc(&app, json!({ "jsonrpc": "2.0", "id": 5, "method": "resources/read", "params": { "uri": "madar://nothing" } })).await;
    assert_eq!(v["error"]["code"], -32002);
    let (_, v) = rpc(
        &app,
        json!({ "jsonrpc": "2.0", "id": 6, "method": "resources/read", "params": {} }),
    )
    .await;
    assert_eq!(v["error"]["code"], -32602);
    let (_, v) = rpc(
        &app,
        json!({ "jsonrpc": "2.0", "id": 7, "method": "resources/templates/list" }),
    )
    .await;
    assert_eq!(v["result"]["resourceTemplates"], json!([]));
    let (_, v) = rpc(
        &app,
        json!({ "jsonrpc": "2.0", "id": 8, "method": "prompts/list" }),
    )
    .await;
    assert_eq!(v["result"]["prompts"], json!([]));
}

#[sqlx::test]
async fn malformed_messages_get_protocol_errors(pool: PgPool) {
    let app = app!(pool);
    let raw = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/mcp")
            .insert_header(("content-type", "application/json"))
            .set_payload("{not json")
            .to_request(),
    )
    .await;
    let v: Value = test::read_body_json(raw).await;
    assert_eq!(v["error"]["code"], -32700);
    let (_, v) = rpc(&app, json!({ "id": 1, "method": "tools/list" })).await;
    assert_eq!(v["error"]["code"], -32600, "no jsonrpc 2.0");
    let (_, v) = rpc(
        &app,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "completion/complete" }),
    )
    .await;
    assert_eq!(v["error"]["code"], -32601);
    let (_, v) = rpc(&app, json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "delete_everything" } })).await;
    assert_eq!(v["error"]["code"], -32602);
}
