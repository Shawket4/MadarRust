//! Who a rate limit counts: `rate_limit::client_ip`.
//!
//! Behind nginx every request reaches the container from one address (Docker's
//! port proxy on the bridge gateway), so the socket's peer alone made each
//! "per IP" allowance one allowance for the whole platform. The visitor is the
//! `X-Real-IP` nginx sets, believed ONLY from a trusted proxy. What these pin:
//!   * behind the trusted proxy, two visitors have two allowances, on a route
//!     governor (the delivery OTP) and on the general bucket alike;
//!   * an `X-Real-IP` from any other source (another container on the network,
//!     a public address) is ignored, however often it changes;
//!   * a missing or garbage header from the proxy falls back to the peer.
//!
//! Every test sets the same `MADAR_TRUSTED_PROXIES` (the set is read once per
//! process) and uses addresses no other test in this binary uses.

use actix_governor::Governor;
use actix_web::{App, HttpResponse, http::StatusCode, test, web};

use madar_rust::rate_limit::{
    PeerIpOrLocalhost, client_ip, default_gateway, route_governor, throttle_exports,
};

/// The Docker bridge gateway on the box.
const PROXY: &str = "172.21.0.1";

fn env() {
    // SAFETY: nextest runs each test in its own process; every test in this
    // binary sets the same values.
    unsafe {
        std::env::set_var("MADAR_TRUSTED_PROXIES", PROXY);
        std::env::set_var("MADAR_RL_DELIVERY_OTP_BURST", "2");
        std::env::set_var("MADAR_RL_DELIVERY_OTP_MS_PER_REQUEST", "90000");
        std::env::set_var("MADAR_RATE_LIMIT_PER_MINUTE", "3");
        std::env::remove_var("MADAR_DISABLE_RATE_LIMIT");
    }
}

/// A request from `peer`, saying `real_ip` in `X-Real-IP` when given.
fn from(peer: &str, real_ip: Option<&str>) -> test::TestRequest {
    let mut r = test::TestRequest::get()
        .uri("/public/otp/request")
        .peer_addr(format!("{peer}:41000").parse().unwrap());
    if let Some(ip) = real_ip {
        r = r.insert_header(("X-Real-IP", ip));
    }
    r
}

macro_rules! otp_app {
    () => {{
        let gov = route_governor(PeerIpOrLocalhost, "DELIVERY", "OTP");
        test::init_service(
            App::new().service(
                web::resource("/public/otp/request")
                    .wrap(Governor::new(&gov))
                    .route(web::get().to(HttpResponse::Ok)),
            ),
        )
        .await
    }};
}

macro_rules! general_app {
    () => {
        test::init_service(
            App::new()
                .wrap(actix_web::middleware::from_fn(throttle_exports))
                .route("/public/otp/request", web::get().to(HttpResponse::Ok)),
        )
        .await
    };
}

#[actix_web::test]
async fn the_gateway_is_read_from_the_route_table() {
    // The backend container's table on the box.
    let table = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
                 eth0\t00000000\t010015AC\t0003\t0\t0\t0\t00000000\t0\t0\t0\n\
                 eth0\t000015AC\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0\n";
    assert_eq!(default_gateway(table), Some("172.21.0.1".parse().unwrap()));
    let no_default = "Iface\tDestination\tGateway\n eth0\t000015AC\t00000000\n";
    assert_eq!(default_gateway(no_default), None);
    assert_eq!(default_gateway(""), None);
}

#[actix_web::test]
async fn the_proxy_s_x_real_ip_is_the_caller() {
    env();
    let a = from(PROXY, Some("41.33.10.1")).to_srv_request();
    assert_eq!(client_ip(&a), Some("41.33.10.1".parse().unwrap()));
    let v6 = from(PROXY, Some("2c0f:fc88::1")).to_srv_request();
    assert_eq!(client_ip(&v6), Some("2c0f:fc88::1".parse().unwrap()));
    // Missing or garbage: the peer, never a guess.
    let none = from(PROXY, None).to_srv_request();
    assert_eq!(client_ip(&none), Some(PROXY.parse().unwrap()));
    let junk = from(PROXY, Some("41.33.10.1, 10.0.0.1")).to_srv_request();
    assert_eq!(client_ip(&junk), Some(PROXY.parse().unwrap()));
}

/// The case that matters: a forged header from anywhere but the proxy changes
/// nothing. Another container on the bridge, a public address: each is keyed
/// by its own socket.
#[actix_web::test]
async fn a_spoofed_x_real_ip_from_anywhere_else_is_ignored() {
    env();
    for peer in ["172.21.0.7", "203.0.113.9", "127.0.0.1"] {
        let r = from(peer, Some("41.33.10.2")).to_srv_request();
        assert_eq!(client_ip(&r), Some(peer.parse().unwrap()), "{peer}");
    }
}

/// Behind the proxy, each visitor has their own OTP allowance.
#[actix_web::test]
async fn behind_the_proxy_each_visitor_has_their_own_otp_allowance() {
    env();
    let app = otp_app!();
    for _ in 0..2 {
        let r = test::call_service(&app, from(PROXY, Some("41.33.20.1")).to_request()).await;
        assert_eq!(r.status(), StatusCode::OK);
    }
    let r = test::call_service(&app, from(PROXY, Some("41.33.20.1")).to_request()).await;
    assert_eq!(
        r.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "one visitor's burst is spent"
    );
    let r = test::call_service(&app, from(PROXY, Some("41.33.20.2")).to_request()).await;
    assert_eq!(
        r.status(),
        StatusCode::OK,
        "another visitor behind the same proxy is not"
    );
}

/// Rotating the header from an untrusted source buys nothing: the bucket is
/// the source's own, and it runs dry at the burst.
#[actix_web::test]
async fn rotating_a_spoofed_header_does_not_buy_more_otp_requests() {
    env();
    let app = otp_app!();
    for (i, expect) in [
        StatusCode::OK,
        StatusCode::OK,
        StatusCode::TOO_MANY_REQUESTS,
    ]
    .into_iter()
    .enumerate()
    {
        let spoof = format!("41.33.30.{}", i + 1);
        let r = test::call_service(&app, from("172.21.0.8", Some(&spoof)).to_request()).await;
        assert_eq!(r.status(), expect, "call {}", i + 1);
    }
}

/// The general bucket (anonymous callers per address, and the per-address
/// ceiling) counts the same visitor.
#[actix_web::test]
async fn the_general_bucket_counts_the_visitor_too() {
    env();
    let app = general_app!();
    for _ in 0..3 {
        let r = test::call_service(&app, from(PROXY, Some("41.33.40.1")).to_request()).await;
        assert!(r.status().is_success());
    }
    let r = test::call_service(&app, from(PROXY, Some("41.33.40.1")).to_request()).await;
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    let r = test::call_service(&app, from(PROXY, Some("41.33.40.2")).to_request()).await;
    assert!(
        r.status().is_success(),
        "a second visitor behind the proxy has their own bucket"
    );

    // From an untrusted peer, rotating the header is one caller.
    for i in 1..=3 {
        let spoof = format!("41.33.50.{i}");
        let r = test::call_service(&app, from("203.0.113.10", Some(&spoof)).to_request()).await;
        assert!(r.status().is_success(), "call {i}");
    }
    let r = test::call_service(&app, from("203.0.113.10", Some("41.33.50.9")).to_request()).await;
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
}
