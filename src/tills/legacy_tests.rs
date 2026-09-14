//! Legacy `/shifts` + `/tills` adapters and replay aliases vs the goldens POS
//! v0.5.1 / v0.6.0 decode (`tests/fixtures/legacy_till_api`, captured from the
//! pre-rename backend `097a653` by `scripts/legacy_till_golden/capture.sh`).
//!
//! The test loads the same `seed.sql`, replays the same `scenario.json` step by
//! step against the current backend, and compares every saved response
//! STRICTLY: same status, every golden key present with the same value (nulls
//! included), every array element, same array lengths. Keys the current backend
//! ADDS are allowed (old generated models ignore unknown fields). The only
//! values not compared are the explicit lists in `manifest.json`
//! (`volatile_timestamp_keys`, `dated_ref_keys`) and server-minted UUIDs, which
//! must map one-to-one between golden and actual across the whole scenario.
use std::collections::{HashMap, HashSet};

use actix_web::{App, test, web};
use serde_json::Value;
use sqlx::PgPool;

use crate::auth::jwt::{JwtSecret, create_token};
use crate::models::UserRole;

const ROOT: &str = env!("CARGO_MANIFEST_DIR");
const SECRET: &str = "test_secret";
const FIXED_TS: &str = "2026-01-01T00:00:00Z";

fn read_json(rel: &str) -> Value {
    let text =
        std::fs::read_to_string(format!("{ROOT}/{rel}")).unwrap_or_else(|e| panic!("{rel}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36 && uuid::Uuid::parse_str(s).is_ok()
}

fn is_timestamp(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 19 && b[4] == b'-' && b[7] == b'-' && b[10] == b'T' && b[13] == b':' && b[16] == b':'
}

struct Rules {
    ts_keys: HashSet<String>,
    ref_keys: HashSet<String>,
}

impl Rules {
    /// Same normalisation capture.py applied to the golden.
    fn scrub(&self, v: &mut Value, key: Option<&str>) {
        match v {
            Value::Object(m) => {
                for (k, x) in m.iter_mut() {
                    self.scrub(x, Some(k));
                }
            }
            Value::Array(a) => a.iter_mut().for_each(|x| self.scrub(x, key)),
            Value::String(s) => {
                let Some(k) = key else { return };
                if self.ts_keys.contains(k) && is_timestamp(s) {
                    *s = FIXED_TS.into();
                } else if self.ref_keys.contains(k) {
                    *s = normalise_ref_date(s);
                }
            }
            _ => {}
        }
    }
}

/// `<CODE>-YYMMDD-…` → `<CODE>-YYMMDD-…` with the first 6-digit segment masked.
fn normalise_ref_date(s: &str) -> String {
    let parts: Vec<&str> = s.split('-').collect();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 && i + 1 < parts.len() && p.len() == 6 && p.bytes().all(|b| b.is_ascii_digit()) {
            let mut out: Vec<&str> = parts.clone();
            out[i] = "YYMMDD";
            return out.join("-");
        }
    }
    s.to_string()
}

struct Ids {
    known: HashSet<String>,
    fwd: HashMap<String, String>,
    back: HashMap<String, String>,
}

fn compare(path: &str, golden: &Value, actual: &Value, ids: &mut Ids, diffs: &mut Vec<String>) {
    match (golden, actual) {
        (Value::Object(g), Value::Object(a)) => {
            for (k, gv) in g {
                match a.get(k) {
                    None => diffs.push(format!("{path}.{k}: missing (golden {gv})")),
                    Some(av) => compare(&format!("{path}.{k}"), gv, av, ids, diffs),
                }
            }
        }
        (Value::Array(g), Value::Array(a)) => {
            if g.len() != a.len() {
                diffs.push(format!("{path}: array length {} → {}", g.len(), a.len()));
            }
            for (i, (gv, av)) in g.iter().zip(a).enumerate() {
                compare(&format!("{path}[{i}]"), gv, av, ids, diffs);
            }
        }
        (Value::String(g), Value::String(a))
            if g != a && is_uuid(g) && is_uuid(a) && !ids.known.contains(g) =>
        {
            // Server-minted id: must map one-to-one for the whole scenario.
            let f = ids
                .fwd
                .entry(g.clone())
                .or_insert_with(|| a.clone())
                .clone();
            let b = ids
                .back
                .entry(a.clone())
                .or_insert_with(|| g.clone())
                .clone();
            if &f != a || &b != g {
                diffs.push(format!(
                    "{path}: id {g} → {a} breaks the mapping ({g} ↦ {f}, {b} ↦ {a})"
                ));
            }
        }
        (Value::Number(g), Value::Number(a)) if g.as_f64() == a.as_f64() => {}
        (g, a) if g == a => {}
        (g, a) => diffs.push(format!("{path}: golden {g} → actual {a}")),
    }
}

fn subst(v: &Value, vars: &HashMap<String, Value>) -> Value {
    match v {
        Value::Object(m) => {
            Value::Object(m.iter().map(|(k, x)| (k.clone(), subst(x, vars))).collect())
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| subst(x, vars)).collect()),
        Value::String(s) => Value::String(subst_str(s, vars)),
        other => other.clone(),
    }
}

fn subst_str(s: &str, vars: &HashMap<String, Value>) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let end = rest[start..].find("}}").expect("unterminated {{") + start;
        let name = &rest[start + 2..end];
        let val = vars
            .get(name)
            .unwrap_or_else(|| panic!("unbound var {name}"));
        out.push_str(
            val.as_str()
                .map(str::to_string)
                .unwrap_or_else(|| val.to_string())
                .as_str(),
        );
        rest = &rest[end + 2..];
    }
    out.push_str(rest);
    out
}

fn dig<'a>(v: &'a Value, path: &str) -> &'a Value {
    path.split('.')
        .fold(v, |v, part| match part.parse::<usize>() {
            Ok(i) if v.is_array() => &v[i],
            _ => &v[part],
        })
}

#[sqlx::test]
async fn legacy_goldens_match_value_for_value(pool: PgPool) {
    crate::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();
    let seed =
        std::fs::read_to_string(format!("{ROOT}/scripts/legacy_till_golden/seed.sql")).unwrap();
    sqlx::raw_sql(&seed).execute(&pool).await.expect("seed.sql");

    let hub = web::Data::new(crate::realtime::hub::BranchEventHub::new());
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(JwtSecret(SECRET.into())))
            .app_data(hub.clone())
            .configure(crate::tills::legacy_routes::configure)
            .configure(crate::tills::routes::configure)
            .configure(crate::tickets::routes::configure)
            .configure(crate::sync::routes::configure)
            .configure(crate::orders::routes::configure)
            .configure(crate::refunds::routes::configure)
            .configure(|c| crate::reports::routes::configure(c, web::Data::new(pool.clone())))
            .configure(crate::delivery::routes::configure),
    )
    .await;

    let scenario = read_json("scripts/legacy_till_golden/scenario.json");
    let manifest = read_json("tests/fixtures/legacy_till_api/manifest.json");
    let keys = |k: &str| -> HashSet<String> {
        manifest[k]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    };
    let rules = Rules {
        ts_keys: keys("volatile_timestamp_keys"),
        ref_keys: keys("dated_ref_keys"),
    };

    let mut vars: HashMap<String, Value> = scenario["vars"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mut ids = Ids {
        known: vars
            .values()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        fwd: HashMap::new(),
        back: HashMap::new(),
    };
    let phone = crate::delivery::normalize_phone("01000000000").unwrap();
    vars.insert(
        "device_token".into(),
        Value::String(crate::delivery::whatsapp::issue_device_token(SECRET, &phone).unwrap()),
    );
    let org = uuid::Uuid::parse_str(vars["org"].as_str().unwrap()).unwrap();
    let mut tokens = HashMap::new();
    for (who, role) in [
        ("admin", UserRole::OrgAdmin),
        ("teller_a", UserRole::Teller),
        ("teller_b", UserRole::Teller),
        ("waiter", UserRole::Waiter),
    ] {
        let uid = uuid::Uuid::parse_str(vars[who].as_str().unwrap()).unwrap();
        let tok = create_token(&JwtSecret(SECRET.into()), uid, Some(org), role, None, 24).unwrap();
        tokens.insert(who.to_string(), tok);
    }

    let mut report = Vec::new();
    let mut saved = HashSet::new();
    for step in scenario["steps"].as_array().unwrap() {
        let method = step["method"].as_str().unwrap();
        let path = subst_str(step["path"].as_str().unwrap(), &vars);
        let mut req = match method {
            "GET" => test::TestRequest::get(),
            "POST" => test::TestRequest::post(),
            "DELETE" => test::TestRequest::delete(),
            m => panic!("method {m}"),
        }
        .uri(&path);
        if let Some(who) = step["as"].as_str() {
            req = req.insert_header(("Authorization", format!("Bearer {}", tokens[who])));
        }
        if !step["body"].is_null() {
            req = req.set_json(subst(&step["body"], &vars));
        }
        let resp = test::call_service(&app, req.to_request()).await;
        let status = resp.status().as_u16();
        let bytes = test::read_body(resp).await;
        let mut body: Value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()))
        };
        if let Some(expect) = step["expect"].as_u64() {
            assert_eq!(u64::from(status), expect, "{method} {path}: {body}");
        }
        if let Some(bind) = step["bind"].as_object() {
            for (var, p) in bind {
                let v = dig(&body, p.as_str().unwrap());
                assert!(
                    !v.is_null(),
                    "{method} {path}: cannot bind {var} from {p}: {body}"
                );
                vars.insert(var.clone(), v.clone());
            }
        }
        let Some(name) = step["save"].as_str() else {
            continue;
        };
        saved.insert(format!("{name}.json"));
        let golden = read_json(&format!("tests/fixtures/legacy_till_api/{name}.json"));
        let mut diffs = Vec::new();
        if u64::from(status) != golden["status"].as_u64().unwrap() {
            diffs.push(format!("status {} → {status}", golden["status"]));
        }
        rules.scrub(&mut body, None);
        compare("$", &golden["body"], &body, &mut ids, &mut diffs);
        if !diffs.is_empty() {
            report.push(format!(
                "{name} ({method} {path}):\n    {}",
                diffs.join("\n    ")
            ));
        }
    }

    // Every golden on disk is exercised.
    for f in manifest["files"].as_array().unwrap() {
        let file = f["file"].as_str().unwrap();
        assert!(
            saved.contains(file),
            "golden {file} is not wired into scenario.json"
        );
    }
    assert!(
        report.is_empty(),
        "legacy drift in {} golden(s):\n{}",
        report.len(),
        report.join("\n")
    );
}

/// `DELETE /shifts/{id}` (T13 through the legacy route): an empty, closed
/// till is deleted (204) and is gone afterwards; a till with sales is not.
#[sqlx::test]
async fn legacy_delete_shift_route(pool: PgPool) {
    crate::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();
    let seed =
        std::fs::read_to_string(format!("{ROOT}/scripts/legacy_till_golden/seed.sql")).unwrap();
    sqlx::raw_sql(&seed).execute(&pool).await.expect("seed.sql");
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(JwtSecret(SECRET.into())))
            .app_data(web::Data::new(crate::realtime::hub::BranchEventHub::new()))
            .configure(crate::tills::legacy_routes::configure)
            .configure(crate::orders::routes::configure),
    )
    .await;
    let org = uuid::Uuid::parse_str("10000000-0000-4000-8000-000000000001").unwrap();
    let admin = uuid::Uuid::parse_str("10000000-0000-4000-8000-00000000ad01").unwrap();
    let branch = "10000000-0000-4000-8000-0000000000a1";
    let tok = create_token(
        &JwtSecret(SECRET.into()),
        admin,
        Some(org),
        UserRole::OrgAdmin,
        None,
        24,
    )
    .unwrap();
    let auth = ("Authorization", format!("Bearer {tok}"));

    let empty = uuid::Uuid::new_v4();
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/branches/{branch}/open"))
            .insert_header(auth.clone())
            .set_json(serde_json::json!({ "id": empty, "opening_cash": 0 }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201);
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/{empty}/close"))
            .insert_header(auth.clone())
            .set_json(serde_json::json!({ "closing_cash_declared": 0 }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);

    let resp = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri(&format!("/shifts/{empty}"))
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 204);
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/shifts/{empty}"))
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 404);

    // A till with a sale on it cannot be deleted.
    let busy = uuid::Uuid::new_v4();
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/branches/{branch}/open"))
            .insert_header(auth.clone())
            .set_json(serde_json::json!({ "id": busy, "opening_cash": 0 }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201);
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/orders")
            .insert_header(auth.clone())
            .set_json(serde_json::json!({
                "branch_id": branch, "shift_id": busy, "payment_method": "cash",
                "idempotency_key": uuid::Uuid::new_v4(),
                "items": [{ "menu_item_id": "10000000-0000-4000-8000-0000000e0001", "quantity": 1, "unit_price": 5000,
                            "addons": [], "optional_field_ids": [] }],
                "subtotal": 5000, "tax_amount": 0, "total_amount": 5000
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201);
    let resp = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri(&format!("/shifts/{busy}"))
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    assert!(resp.status().is_client_error(), "{}", resp.status());
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM tills WHERE id = $1")
        .bind(busy)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
}
