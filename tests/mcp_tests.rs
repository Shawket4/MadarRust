//! The MCP server: POST /mcp (src/mcp.rs).
//!
//! What these pin: the handshake (version negotiation, notifications), the tool
//! list and its read-only annotations, each tool answering from the same data the
//! public API serves, failures reported inside the result (not as protocol errors),
//! no customer details in a tracked order, and the protocol errors for malformed
//! messages. GET is 405: there is no server-sent stream.

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
        json!({ "jsonrpc": "2.0", "id": 1, "method": "resources/list" }),
    )
    .await;
    assert_eq!(v["error"]["code"], -32601);
    let (_, v) = rpc(&app, json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "delete_everything" } })).await;
    assert_eq!(v["error"]["code"], -32602);
}
