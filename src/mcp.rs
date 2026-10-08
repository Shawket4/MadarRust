//! The MCP server (Model Context Protocol): read-only tools over the public API,
//! so an assistant can answer questions about a café that runs on Madar.
//!
//! Streamable HTTP, stateless: each POST to `/mcp` carries one JSON-RPC message and
//! is answered with `application/json`; there is no session and no SSE stream, so
//! GET answers 405 (both allowed by the transport). No authentication, and nothing
//! here writes: ordering and booking need the customer's WhatsApp code, so the tools
//! hand back the shop's links for the person to finish there.
//!
//! The tools reuse what the public endpoints use (the links page and menu loaders,
//! the booking-slots and tracking handlers themselves), so an answer here is the
//! answer the public API gives. Like the API, nothing lists every shop: a tool
//! names one shop by its address name ("drops" for drops.madar-pos.cloud).

use actix_web::{HttpResponse, http::StatusCode, web};
use chrono::NaiveDate;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;
use crate::orgs::public::{BrandQuery, resolve_org};

/// Protocol versions spoken, newest first; a client asking for another gets the newest.
const PROTOCOLS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
/// Menu items one answer carries at most.
const MAX_ITEMS: usize = 300;

fn tools() -> Value {
    let read_only =
        json!({ "readOnlyHint": true, "destructiveHint": false, "openWorldHint": false });
    let shop = json!({ "type": "string", "description": "The shop's address name: \"drops\" for drops.madar-pos.cloud." });
    let uuid = |what: &str| json!({ "type": "string", "format": "uuid", "description": what });
    json!([
        {
            "name": "get_shop",
            "title": "A shop's details",
            "description": "A café or restaurant on Madar POS: its name and tagline, its links (order online, menu, book a table, rewards), social links, and branches with address, phone and directions.",
            "inputSchema": { "type": "object", "properties": { "shop": shop }, "required": ["shop"] },
            "annotations": read_only,
        },
        {
            "name": "get_menu",
            "title": "A shop's menu",
            "description": "A shop's menu: categories and items with prices in Egyptian pounds (EGP), sizes where an item has them. Without branch_id, the shop's first branch.",
            "inputSchema": { "type": "object", "properties": { "shop": shop, "branch_id": uuid("A branch id from get_shop (optional).") }, "required": ["shop"] },
            "annotations": read_only,
        },
        {
            "name": "get_booking_slots",
            "title": "Table availability",
            "description": "A branch's bookable times on a date for a party size, and which are available. Booking itself needs the guest's WhatsApp code: send them the shop's booking link.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "branch_id": uuid("A branch id from get_shop."),
                    "date": { "type": "string", "format": "date", "description": "YYYY-MM-DD" },
                    "party_size": { "type": "integer", "minimum": 1 },
                },
                "required": ["branch_id", "date", "party_size"],
            },
            "annotations": read_only,
        },
        {
            "name": "track_order",
            "title": "An online order's status",
            "description": "An online order's status and timeline, from the id in its tracking link. Customer details are left out.",
            "inputSchema": { "type": "object", "properties": { "order_id": uuid("The order id from the tracking link.") }, "required": ["order_id"] },
            "annotations": read_only,
        },
        {
            "name": "about_madar",
            "title": "About Madar POS",
            "description": "What Madar POS is, its plans and monthly prices per branch, and how to reach sales.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": read_only,
        },
    ])
}

/// A tool's failure the assistant should read (an unknown shop, a closed branch):
/// reported inside the result, as the protocol asks, not as a JSON-RPC error.
struct ToolError(String);
impl From<AppError> for ToolError {
    fn from(e: AppError) -> Self {
        ToolError(e.to_string())
    }
}

fn arg<'a>(args: &'a Value, key: &str) -> Result<&'a Value, ToolError> {
    args.get(key)
        .filter(|v| !v.is_null())
        .ok_or_else(|| ToolError(format!("\"{key}\" is required")))
}
fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    arg(args, key)?
        .as_str()
        .ok_or_else(|| ToolError(format!("\"{key}\" must be a string")))
}
fn arg_uuid(args: &Value, key: &str) -> Result<Uuid, ToolError> {
    Uuid::parse_str(arg_str(args, key)?).map_err(|_| ToolError(format!("\"{key}\" must be a UUID")))
}

async fn shop_id(pool: &PgPool, shop: &str) -> Result<Uuid, ToolError> {
    Ok(resolve_org(
        pool,
        &BrandQuery {
            org_id: None,
            slug: Some(shop.to_string()),
        },
    )
    .await?)
}

/// A public handler's answer as JSON, or its error message.
async fn body_of(resp: Result<HttpResponse, AppError>) -> Result<Value, ToolError> {
    let resp = resp?;
    let bytes = actix_web::body::to_bytes(resp.into_body())
        .await
        .map_err(|_| ToolError("could not read the answer".into()))?;
    serde_json::from_slice(&bytes).map_err(|_| ToolError("the answer was not JSON".into()))
}

fn piastres(p: i32) -> f64 {
    f64::from(p) / 100.0
}

async fn call(pool: &web::Data<PgPool>, name: &str, args: &Value) -> Result<Value, ToolError> {
    match name {
        "get_shop" => {
            let org = shop_id(pool, arg_str(args, "shop")?).await?;
            let page = crate::orgs::links_page::public_links_page(pool, org).await?;
            serde_json::to_value(page).map_err(|_| ToolError("could not read the shop".into()))
        }
        "get_menu" => {
            let org = shop_id(pool, arg_str(args, "shop")?).await?;
            let wanted = match args.get("branch_id").filter(|v| !v.is_null()) {
                Some(_) => Some(arg_uuid(args, "branch_id")?),
                None => None,
            };
            let branch = crate::tenant_shell::shop_branch(pool, org, wanted)
                .await
                .ok_or_else(|| ToolError("this shop has no open branch".into()))?;
            if wanted.is_some_and(|w| w != branch) {
                return Err(ToolError(
                    "that branch is not one of this shop's open branches".into(),
                ));
            }
            let menu = crate::delivery::public::load_public_menu(pool, org, branch, None).await?;
            let mut left = MAX_ITEMS;
            let categories: Vec<Value> = menu
                .categories
                .iter()
                .map(|c| {
                    let items: Vec<Value> = menu
                        .items
                        .iter()
                        .filter(|i| i.category_id == Some(c.id))
                        .take(left)
                        .map(|i| {
                            json!({
                                "name": i.name,
                                "description": i.description,
                                "price_egp": piastres(i.price),
                                "sizes": i.sizes.iter().map(|s| json!({ "label": s.label, "price_egp": piastres(s.price) })).collect::<Vec<_>>(),
                            })
                        })
                        .collect();
                    left -= items.len();
                    json!({ "category": c.name, "items": items })
                })
                .filter(|c| c["items"].as_array().is_some_and(|i| !i.is_empty()))
                .collect();
            Ok(json!({ "branch_id": branch, "currency": "EGP", "categories": categories }))
        }
        "get_booking_slots" => {
            let branch = arg_uuid(args, "branch_id")?;
            let date = NaiveDate::parse_from_str(arg_str(args, "date")?, "%Y-%m-%d")
                .map_err(|_| ToolError("\"date\" must be YYYY-MM-DD".into()))?;
            let party_size = arg(args, "party_size")?
                .as_i64()
                .and_then(|n| i32::try_from(n).ok())
                .filter(|n| *n > 0)
                .ok_or_else(|| ToolError("\"party_size\" must be a positive integer".into()))?;
            body_of(
                crate::bookings::public::booking_slots(
                    pool.clone(),
                    web::Path::from(branch),
                    web::Query(crate::bookings::public::SlotsQuery { date, party_size }),
                )
                .await,
            )
            .await
        }
        "track_order" => {
            let id = arg_uuid(args, "order_id")?;
            let mut v = body_of(
                crate::delivery::public::track_delivery_order(pool.clone(), web::Path::from(id))
                    .await,
            )
            .await?;
            if let Some(o) = v.as_object_mut() {
                for k in [
                    "customer_name",
                    "place_name",
                    "floor",
                    "unit_number",
                    "address_line",
                ] {
                    o.remove(k);
                }
            }
            Ok(v)
        }
        // ponytail: plans, prices and contact are copied from the marketing site
        // (site/src/i18n/en.ts pricing, site/src/lib/site.ts); change them together.
        "about_madar" => Ok(json!({
            "what": "Madar POS is a point of sale for cafés and restaurants in Egypt, built in Cairo: the till, kitchen screens, recipe costing, stock, loyalty with wallet cards, online ordering and table reservations, in Arabic and English, online or offline.",
            "plans": [
                { "name": "Essential Café", "monthly_egp_per_branch": 3000 },
                { "name": "Advanced Operation", "monthly_egp_per_branch": 3500 },
            ],
            "first_month": "free",
            "contact": { "whatsapp": "https://wa.me/201211116899", "phone": "+201211116899", "email": "shawket.4@icloud.com" },
            "links": {
                "site": "https://get.madar-pos.cloud/",
                "pricing": "https://get.madar-pos.cloud/en/pricing/",
                "contact": "https://get.madar-pos.cloud/en/contact/",
                "developers": "https://get.madar-pos.cloud/en/developers/",
            },
        })),
        other => Err(ToolError(format!("no tool named \"{other}\""))),
    }
}

fn reply(id: Value, result: Result<Value, (i32, &str)>) -> HttpResponse {
    let body = match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err((code, message)) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
        }
    };
    HttpResponse::Ok().json(body)
}

/// One JSON-RPC message in, one answer out (a notification gets 202 and no body).
#[utoipa::path(post, path = "/mcp", tag = "public", operation_id = "mcp",
    request_body(content = Object, description = "One JSON-RPC 2.0 message (MCP Streamable HTTP)"),
    responses(
        (status = 200, description = "The JSON-RPC answer", content_type = "application/json"),
        (status = 202, description = "A notification, accepted"),
    ))]
pub async fn post(pool: web::Data<PgPool>, body: web::Bytes) -> HttpResponse {
    let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
        return reply(Value::Null, Err((-32700, "Parse error")));
    };
    if !msg.is_object() || msg["jsonrpc"] != "2.0" || !msg["method"].is_string() {
        return reply(
            msg.get("id").cloned().unwrap_or(Value::Null),
            Err((-32600, "Invalid Request")),
        );
    }
    let Some(id) = msg.get("id").cloned() else {
        return HttpResponse::Accepted().finish();
    };
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    match msg["method"].as_str().unwrap_or_default() {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or_default();
            let version = PROTOCOLS
                .iter()
                .find(|v| **v == asked)
                .unwrap_or(&PROTOCOLS[0]);
            reply(
                id,
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "madar-pos", "title": "Madar POS", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": "Read-only tools for cafés and restaurants on Madar POS (Egypt). Name a shop by its address name, e.g. \"drops\" for drops.madar-pos.cloud; there is no list of all shops. Ordering and booking need the customer's WhatsApp code, so send them the shop's links from get_shop to finish.",
                })),
            )
        }
        "ping" => reply(id, Ok(json!({}))),
        "tools/list" => reply(id, Ok(json!({ "tools": tools() }))),
        "tools/call" => {
            let Some(name) = params["name"].as_str() else {
                return reply(id, Err((-32602, "Invalid params: \"name\" is required")));
            };
            if !tools()
                .as_array()
                .is_some_and(|t| t.iter().any(|x| x["name"] == name))
            {
                return reply(id, Err((-32602, "Unknown tool")));
            }
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let result = match call(&pool, name, &args).await {
                Ok(v) => json!({
                    "content": [{ "type": "text", "text": v.to_string() }],
                    "structuredContent": v,
                    "isError": false,
                }),
                Err(ToolError(m)) => {
                    json!({ "content": [{ "type": "text", "text": m }], "isError": true })
                }
            };
            reply(id, Ok(result))
        }
        _ => reply(id, Err((-32601, "Method not found"))),
    }
}

/// No server-sent stream: every answer comes back on its own POST.
pub async fn get() -> HttpResponse {
    HttpResponse::build(StatusCode::METHOD_NOT_ALLOWED)
        .insert_header(("Allow", "POST"))
        .finish()
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/mcp")
            .route(web::post().to(post))
            .route(web::get().to(get)),
    );
}
