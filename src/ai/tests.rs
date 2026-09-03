//! AI chat pipeline tests.
//!
//! These exercise the whole path — HTTP → agent loop → tools → compiler →
//! read-only executor → RLS-scoped query → response — with a deterministic
//! transport, so no network and no API key are needed.
//!
//! Two things are being proven, and the second is the interesting one:
//!
//!   1. that a reasonable model gets correct, tenant-scoped answers; and
//!   2. that an *unreasonable* one cannot do damage. The scripted transport
//!      stands in for a model that is confused, adversarial, or fully
//!      attacker-controlled — the assumption to design against once a language
//!      model is deciding what to run.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use actix_web::{App, test, web};
use async_trait::async_trait;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::ai::AiState;
use crate::ai::llm::{Completion, LlmProvider, ProviderError, Turn};
use crate::ai::mock::MockProvider;
use crate::ai::tools;
use crate::analytics::tests::{org_admin_token, org_admin_token_for, secret, seed};
use crate::models::UserRole;

async fn app(
    pool: &PgPool,
    provider: Arc<dyn LlmProvider>,
) -> impl actix_web::dev::Service<
    actix_http::Request,
    Response = actix_web::dev::ServiceResponse,
    Error = actix_web::Error,
> {
    crate::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
    test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret()))
            .app_data(web::Data::new(AiState::with_provider(provider)))
            .configure(crate::ai::routes::configure),
    )
    .await
}

async fn ask(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    token: &str,
    question: &str,
) -> (actix_web::http::StatusCode, Value) {
    let req = test::TestRequest::post()
        .uri("/ai/chat")
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(json!({ "question": question }))
        .to_request();
    let resp = test::call_service(app, req).await;
    let status = resp.status();
    let body: Value = if status.is_success() {
        test::read_body_json(resp).await
    } else {
        Value::Null
    };
    (status, body)
}

/// Wraps a transport and counts calls, so caching can be asserted on what
/// actually reached the model rather than on response equality.
struct Counting {
    inner: Arc<dyn LlmProvider>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl LlmProvider for Counting {
    async fn complete(&self, req: Completion<'_>) -> Result<Turn, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.complete(req).await
    }
    fn name(&self) -> String {
        self.inner.name()
    }
}

// ── The happy path ──────────────────────────────────────────────────────────

#[sqlx::test]
async fn a_question_becomes_an_answer_backed_by_real_rows(pool: PgPool) {
    let s = seed(&pool, "a").await;
    let app = app(&pool, Arc::new(MockProvider::router())).await;
    let (status, body) = ask(&app, &org_admin_token(s.org), "what are my top products?").await;

    assert!(status.is_success());
    assert_eq!(body["kind"], "answer");
    assert!(!body["text"].as_str().unwrap().is_empty());

    let block = &body["results"][0];
    assert_eq!(block["preset_id"], "top_products");
    assert_eq!(block["title"], "Top products");
    let rows = block["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["product"], "Latte");
    assert_eq!(block["viz"], "bar");
    assert_eq!(body["timezone"], "Africa/Cairo");
}

#[sqlx::test]
async fn the_spec_comes_back_so_an_answer_can_be_pinned_as_a_widget(pool: PgPool) {
    // The payoff of the AI and the dashboard sharing one IR: what the assistant
    // just ran IS a valid widget definition.
    let s = seed(&pool, "a").await;
    let app = app(&pool, Arc::new(MockProvider::router())).await;
    let (_, body) = ask(&app, &org_admin_token(s.org), "revenue by branch").await;

    let spec = &body["results"][0]["spec"];
    assert_eq!(spec["dataset"], "orders");
    assert!(
        spec["dimensions"]
            .as_array()
            .unwrap()
            .contains(&json!("branch"))
    );
    // It round-trips through the metrics endpoint unchanged.
    let parsed: crate::analytics::spec::QuerySpec =
        serde_json::from_value(spec.clone()).expect("the echoed spec must be a valid spec");
    assert_eq!(parsed.dataset, "orders");
}

#[sqlx::test]
async fn an_ambiguous_question_asks_rather_than_guessing(pool: PgPool) {
    let s = seed(&pool, "a").await;
    let app = app(&pool, Arc::new(MockProvider::router())).await;
    let (status, body) = ask(&app, &org_admin_token(s.org), "how is everything going").await;

    // A clarifying question is a conversation, not an error.
    assert_eq!(status, 200);
    assert_eq!(body["kind"], "clarify");
    assert!(!body["question"].as_str().unwrap().is_empty());
}

// ── Recovery: the reason this is a loop and not one call ────────────────────

#[sqlx::test]
async fn the_model_recovers_from_a_rejected_query(pool: PgPool) {
    // A single-shot router would have returned an error to the merchant here.
    let s = seed(&pool, "a").await;
    let provider = MockProvider::scripted(vec![
        // Names a measure that does not exist on this dataset.
        MockProvider::call(
            tools::QUERY_METRICS,
            json!({ "dataset": "orders", "measures": ["gross_profit_margin"] }),
        ),
        // Having been told what is valid, tries again correctly.
        MockProvider::call(
            tools::QUERY_METRICS,
            json!({ "dataset": "orders", "measures": ["revenue"],
                    "period": { "preset": "all_time" } }),
        ),
        MockProvider::answer("Your revenue was 163 pounds."),
    ]);
    let app = app(&pool, Arc::new(provider)).await;
    let (status, body) = ask(&app, &org_admin_token(s.org), "revenue").await;

    assert!(status.is_success());
    assert_eq!(body["kind"], "answer");
    // Only the successful query produced a result block.
    assert_eq!(body["results"].as_array().unwrap().len(), 1);
    assert_eq!(body["results"][0]["rows"][0]["revenue"], 16300);
}

#[sqlx::test]
async fn a_typo_in_a_field_name_is_rejected_loudly_and_recovered_from(pool: PgPool) {
    let s = seed(&pool, "a").await;
    let provider = MockProvider::scripted(vec![
        // `dimension` instead of `dimensions` — silently ignoring this would
        // return ungrouped totals that look like a valid answer.
        MockProvider::call(
            tools::QUERY_METRICS,
            json!({ "dataset": "orders", "dimension": ["branch"] }),
        ),
        MockProvider::call(
            tools::QUERY_METRICS,
            json!({ "dataset": "orders", "dimensions": ["branch"],
                    "period": { "preset": "all_time" } }),
        ),
        MockProvider::answer("One branch."),
    ]);
    let app = app(&pool, Arc::new(provider)).await;
    let (_, body) = ask(&app, &org_admin_token(s.org), "revenue by branch").await;

    assert_eq!(body["kind"], "answer");
    assert_eq!(body["results"].as_array().unwrap().len(), 1);
    assert!(body["results"][0]["rows"][0]["branch"].is_string());
}

#[sqlx::test]
async fn a_loop_that_never_answers_ends_honestly(pool: PgPool) {
    let s = seed(&pool, "a").await;
    // Describes the schema forever and never answers.
    let provider = MockProvider::scripted(vec![
        MockProvider::call(tools::DESCRIBE_DATASET, json!({ "dataset": "orders" })),
        MockProvider::call(tools::DESCRIBE_DATASET, json!({ "dataset": "payments" })),
        MockProvider::call(tools::DESCRIBE_DATASET, json!({ "dataset": "shifts" })),
        MockProvider::call(tools::DESCRIBE_DATASET, json!({ "dataset": "orders" })),
        MockProvider::call(tools::DESCRIBE_DATASET, json!({ "dataset": "orders" })),
    ]);
    let app = app(&pool, Arc::new(provider)).await;
    let (status, body) = ask(&app, &org_admin_token(s.org), "revenue").await;

    assert_eq!(status, 200);
    assert_eq!(body["kind"], "incomplete");
    // It says it could not answer rather than inventing a figure.
    let text = body["text"].as_str().unwrap();
    assert!(text.contains("couldn't"), "{text}");
}

#[sqlx::test]
async fn the_query_budget_stops_a_runaway_model(pool: PgPool) {
    let s = seed(&pool, "a").await;
    let q = || {
        MockProvider::call(
            tools::QUERY_METRICS,
            json!({ "dataset": "orders", "period": { "preset": "all_time" } }),
        )
    };
    let provider = MockProvider::scripted(vec![q(), q(), q(), q()]);
    let app = app(&pool, Arc::new(provider)).await;
    let (status, body) = ask(&app, &org_admin_token(s.org), "revenue").await;

    assert_eq!(status, 200);
    // Never more than the budget, however many the model asks for.
    assert!(
        body["results"].as_array().unwrap().len() <= crate::ai::agent::MAX_QUERIES,
        "query budget was not enforced"
    );
}

// ── Empty results ───────────────────────────────────────────────────────────

#[sqlx::test]
async fn an_empty_result_is_returned_as_empty_not_as_zero(pool: PgPool) {
    let s = seed(&pool, "a").await;
    let provider = MockProvider::scripted(vec![
        MockProvider::call(
            tools::QUERY_METRICS,
            json!({ "dataset": "order_items", "dimensions": ["product"],
                    "period": { "preset": "last_year" } }),
        ),
        MockProvider::answer("There were no sales in that period."),
    ]);
    let app = app(&pool, Arc::new(provider)).await;
    let (_, body) = ask(&app, &org_admin_token(s.org), "products last year").await;

    let block = &body["results"][0];
    assert_eq!(block["row_count"], 0);
    assert!(block["rows"].as_array().unwrap().is_empty());
    // The columns still come back, so the client renders an empty chart with
    // real axes rather than nothing at all.
    assert!(!block["columns"].as_array().unwrap().is_empty());
}

// ── An adversarial model ────────────────────────────────────────────────────

#[sqlx::test]
async fn a_model_cannot_read_another_merchants_data(pool: PgPool) {
    let a = seed(&pool, "a").await;
    let b = seed(&pool, "b").await;
    // The "model" names the other merchant's branch explicitly.
    let provider = MockProvider::scripted(vec![
        MockProvider::call(
            tools::QUERY_METRICS,
            json!({ "dataset": "orders", "dimensions": ["branch"],
                    "branch": "Branch b", "period": { "preset": "all_time" } }),
        ),
        MockProvider::answer("Here you go."),
    ]);
    let app = app(&pool, Arc::new(provider)).await;
    let (_, body) = ask(&app, &org_admin_token(a.org), "revenue at branch b").await;

    let block = &body["results"][0];
    // Only org A's single branch is present, and the unmatched name is flagged.
    let rows = block["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["branch"], "Branch a");
    assert_eq!(block["scope"]["unmatched_branch"], "Branch b");
    let _ = b;
}

#[sqlx::test]
async fn a_model_cannot_inject_sql_through_any_argument(pool: PgPool) {
    let s = seed(&pool, "a").await;
    let provider = MockProvider::scripted(vec![
        MockProvider::call(
            tools::QUERY_METRICS,
            json!({
                "dataset": "orders",
                "dimensions": ["branch'; DROP TABLE orders; --"],
                "filters": { "status": "all'; DELETE FROM orders; --" },
                "period": { "preset": "all_time" }
            }),
        ),
        MockProvider::call(
            tools::QUERY_METRICS,
            json!({ "dataset": "orders", "period": { "preset": "all_time" } }),
        ),
        MockProvider::answer("Fine."),
    ]);
    let app = app(&pool, Arc::new(provider)).await;
    let (status, _) = ask(&app, &org_admin_token(s.org), "revenue").await;
    assert!(status.is_success());

    // The tables are intact: the payload could only ever fail a lookup.
    let orders: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM orders")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(orders, 2, "seeded orders must be untouched");
    let _ = s;
}

#[sqlx::test]
async fn a_model_cannot_exceed_the_row_cap(pool: PgPool) {
    let s = seed(&pool, "a").await;
    let provider = MockProvider::scripted(vec![
        MockProvider::call(
            tools::QUERY_METRICS,
            json!({ "dataset": "orders", "dimensions": ["day"], "limit": 100_000_000,
                    "period": { "preset": "all_time" } }),
        ),
        MockProvider::answer("Fine."),
    ]);
    let app = app(&pool, Arc::new(provider)).await;
    let (status, body) = ask(&app, &org_admin_token(s.org), "daily revenue").await;
    assert!(status.is_success());
    assert!(
        body["results"][0]["rows"].as_array().unwrap().len() <= crate::analytics::execute::MAX_ROWS
    );
}

#[sqlx::test]
async fn calling_a_tool_that_does_not_exist_is_survivable(pool: PgPool) {
    let s = seed(&pool, "a").await;
    let provider = MockProvider::scripted(vec![
        MockProvider::call("run_sql", json!({ "sql": "SELECT * FROM users" })),
        MockProvider::answer("I can only run reports."),
    ]);
    let app = app(&pool, Arc::new(provider)).await;
    let (status, body) = ask(&app, &org_admin_token(s.org), "show me users").await;
    assert_eq!(status, 200);
    assert_eq!(body["kind"], "answer");
    assert!(body["results"].as_array().unwrap().is_empty());
}

// ── Authorization and input validation ──────────────────────────────────────

#[sqlx::test]
async fn a_super_admin_token_is_refused(pool: PgPool) {
    // It has no single org, so its pool is unscoped — the feature must never be
    // able to aggregate across merchants.
    seed(&pool, "a").await;
    let app = app(&pool, Arc::new(MockProvider::router())).await;
    let token = crate::auth::jwt::create_token(
        &secret(),
        Uuid::new_v4(),
        None,
        UserRole::SuperAdmin,
        None,
        24,
    )
    .unwrap();
    let (status, _) = ask(&app, &token, "revenue").await;
    assert_eq!(status, 403);
}

#[sqlx::test]
async fn requests_without_a_token_are_rejected(pool: PgPool) {
    seed(&pool, "a").await;
    let app = app(&pool, Arc::new(MockProvider::router())).await;
    let req = test::TestRequest::post()
        .uri("/ai/chat")
        .set_json(json!({ "question": "revenue" }))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 401);
}

#[sqlx::test]
async fn empty_and_oversized_questions_are_rejected(pool: PgPool) {
    let s = seed(&pool, "a").await;
    let app = app(&pool, Arc::new(MockProvider::router())).await;
    let token = org_admin_token(s.org);
    assert_eq!(ask(&app, &token, "   ").await.0, 400);
    assert_eq!(ask(&app, &token, &"x".repeat(1001)).await.0, 400);
}

#[sqlx::test]
async fn the_feature_reports_itself_unavailable_when_unconfigured(pool: PgPool) {
    let s = seed(&pool, "a").await;
    crate::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();
    // No provider wired at all — the rest of the server is unaffected.
    let state = AiState {
        provider: None,
        cache: moka::future::Cache::builder().max_capacity(10).build(),
    };
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret()))
            .app_data(web::Data::new(state))
            .configure(crate::ai::routes::configure),
    )
    .await;
    assert_eq!(ask(&app, &org_admin_token(s.org), "revenue").await.0, 503);
}

// ── Caching ─────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn a_repeated_question_is_served_without_calling_the_model_again(pool: PgPool) {
    let s = seed(&pool, "a").await;
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(Counting {
        inner: Arc::new(MockProvider::router()),
        calls: calls.clone(),
    });
    let app = app(&pool, provider).await;
    let token = org_admin_token(s.org);

    let (_, first) = ask(&app, &token, "top products").await;
    let after_first = calls.load(Ordering::SeqCst);
    assert!(after_first > 0);

    let (_, second) = ask(&app, &token, "top products").await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        after_first,
        "the second identical question reached the model"
    );
    assert_eq!(first, second);
}

#[sqlx::test]
async fn a_clarifying_question_is_not_cached(pool: PgPool) {
    // Caching it would answer the merchant's follow-up with the same question.
    let s = seed(&pool, "a").await;
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(Counting {
        inner: Arc::new(MockProvider::router()),
        calls: calls.clone(),
    });
    let app = app(&pool, provider).await;
    let token = org_admin_token(s.org);

    let (_, body) = ask(&app, &token, "how is everything going").await;
    assert_eq!(body["kind"], "clarify");
    let after_first = calls.load(Ordering::SeqCst);
    ask(&app, &token, "how is everything going").await;
    assert!(
        calls.load(Ordering::SeqCst) > after_first,
        "a clarification was cached"
    );
}

#[sqlx::test]
async fn two_merchants_asking_the_same_question_never_share_an_answer(pool: PgPool) {
    let a = seed(&pool, "a").await;
    let b = seed(&pool, "b").await;
    let app = app(&pool, Arc::new(MockProvider::router())).await;

    let (_, ra) = ask(&app, &org_admin_token(a.org), "revenue by branch").await;
    let (_, rb) = ask(&app, &org_admin_token(b.org), "revenue by branch").await;
    assert_eq!(ra["results"][0]["rows"][0]["branch"], "Branch a");
    assert_eq!(rb["results"][0]["rows"][0]["branch"], "Branch b");
}

// ── Conversational context ──────────────────────────────────────────────────

/// Answers by reusing the spec it finds replayed from the previous turn,
/// changing only the period. If the transcript does not carry a prior spec it
/// errors, so this provider cannot pass by accident.
struct FollowUp {
    seen_prior_spec: Arc<AtomicUsize>,
}

#[async_trait]
impl LlmProvider for FollowUp {
    async fn complete(&self, req: Completion<'_>) -> Result<Turn, ProviderError> {
        // Answer once a query has run in THIS turn.
        if req
            .messages
            .iter()
            .any(|m| matches!(m, crate::ai::llm::Message::ToolResult { .. }))
        {
            return Ok(MockProvider::answer(
                "And here it is for the earlier period.",
            ));
        }

        // Find the replayed spec from the previous turn.
        let replayed = req.messages.iter().find_map(|m| match m {
            crate::ai::llm::Message::Assistant { text: Some(t), .. } if t.contains("[ran ") => {
                let start = t.find('{')?;
                let end = t.rfind('}')?;
                Some(t[start..=end].to_string())
            }
            _ => None,
        });
        let Some(raw) = replayed else {
            return Err(ProviderError::Parse(
                "no prior spec in the transcript — a follow-up cannot resolve".into(),
            ));
        };
        self.seen_prior_spec.fetch_add(1, Ordering::SeqCst);

        // Reuse it verbatim, changing only the window — which is exactly what
        // "and last month?" means.
        let mut spec: Value = serde_json::from_str(&raw).expect("the replayed spec must parse");
        spec["period"] = json!({ "preset": "all_time" });
        Ok(MockProvider::call(tools::QUERY_METRICS, spec))
    }

    fn name(&self) -> String {
        "follow-up".into()
    }
}

#[sqlx::test]
async fn a_follow_up_resolves_against_the_previous_query(pool: PgPool) {
    // The property this is really testing: a conversation is contextual across
    // messages, not only within one. Replaying prose alone would leave the
    // model reconstructing its own previous query from its own summary.
    let s = seed(&pool, "a").await;
    let seen = Arc::new(AtomicUsize::new(0));
    let app = app(
        &pool,
        Arc::new(FollowUp {
            seen_prior_spec: seen.clone(),
        }),
    )
    .await;

    let prior = json!({
        "dataset": "order_items",
        "dimensions": ["product"],
        "measures": ["units_sold", "item_revenue"],
        "filters": { "status": "sold" },
        "period": { "preset": "last_month" },
        "sort": { "measure": "item_revenue", "dir": "desc" },
        "limit": 5
    });

    let req = test::TestRequest::post()
        .uri("/ai/chat")
        .insert_header((
            "Authorization",
            format!("Bearer {}", org_admin_token(s.org)),
        ))
        .set_json(json!({
            "question": "and for all time?",
            "history": [{
                "question": "top products last month",
                "answer": "Latte led on revenue.",
                "spec": prior,
            }]
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());
    let body: Value = test::read_body_json(resp).await;

    assert_eq!(
        seen.load(Ordering::SeqCst),
        1,
        "the previous turn's spec never reached the transcript"
    );
    assert_eq!(body["kind"], "answer");

    // The follow-up kept every part of the earlier query except the window —
    // dataset, breakdown, measures, filters and sort all carried over.
    let spec = &body["results"][0]["spec"];
    assert_eq!(spec["dataset"], "order_items");
    assert_eq!(spec["dimensions"][0], "product");
    assert_eq!(spec["sort"]["measure"], "item_revenue");
    assert_eq!(spec["period"]["preset"], "all_time");
    // ...and it ran against real data.
    assert_eq!(body["results"][0]["rows"][0]["product"], "Latte");
}

#[sqlx::test]
async fn a_turn_that_ran_no_query_replays_without_one(pool: PgPool) {
    // A clarification carries no spec. Replaying it must not fabricate one or
    // break the transcript.
    let s = seed(&pool, "a").await;
    let app = app(&pool, Arc::new(MockProvider::router())).await;
    let req = test::TestRequest::post()
        .uri("/ai/chat")
        .insert_header((
            "Authorization",
            format!("Bearer {}", org_admin_token(s.org)),
        ))
        .set_json(json!({
            "question": "top products",
            "history": [{ "question": "how is it going", "answer": "Which figure did you mean?" }]
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());
    let body: Value = test::read_body_json(resp).await;
    assert_eq!(body["kind"], "answer");
}

// ── Server-side conversations ───────────────────────────────────────────────

async fn ask_in(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    token: &str,
    conversation: Option<Uuid>,
    question: &str,
) -> Value {
    let mut payload = json!({ "question": question });
    if let Some(id) = conversation {
        payload["conversation_id"] = json!(id);
    }
    let req = test::TestRequest::post()
        .uri("/ai/chat")
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(payload)
        .to_request();
    let resp = test::call_service(app, req).await;
    assert!(resp.status().is_success(), "chat failed: {}", resp.status());
    test::read_body_json(resp).await
}

async fn get_json(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    token: &str,
    uri: &str,
) -> Value {
    let req = test::TestRequest::get()
        .uri(uri)
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let resp = test::call_service(app, req).await;
    assert!(resp.status().is_success(), "GET {uri} → {}", resp.status());
    test::read_body_json(resp).await
}

#[sqlx::test]
async fn a_conversation_is_created_and_can_be_resumed(pool: PgPool) {
    let s = seed(&pool, "a").await;
    let app = app(&pool, Arc::new(MockProvider::router())).await;
    let token = org_admin_token_for(s.org, s.admin);

    let first = ask_in(&app, &token, None, "top products").await;
    let id: Uuid = serde_json::from_value(first["conversation_id"].clone())
        .expect("a turn that answered must be stored");

    // The second message continues it WITHOUT the client re-uploading history.
    let second = ask_in(&app, &token, Some(id), "and by branch?").await;
    assert_eq!(second["conversation_id"], json!(id));

    let detail = get_json(&app, &token, &format!("/ai/conversations/{id}")).await;
    assert_eq!(detail["turn_count"], 2);
    let turns = detail["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0]["seq"], 1);
    assert_eq!(turns[0]["question"], "top products");
    assert_eq!(turns[1]["question"], "and by branch?");
    // The title comes from the first question, so the list is readable.
    assert_eq!(detail["title"], "top products");
}

#[sqlx::test]
async fn a_stored_turn_keeps_the_query_not_the_rows(pool: PgPool) {
    // Rows go stale the moment another order is rung up; the spec re-runs and
    // gives current figures. It is also what a client needs to pin a widget.
    let s = seed(&pool, "a").await;
    let app = app(&pool, Arc::new(MockProvider::router())).await;
    let token = org_admin_token_for(s.org, s.admin);

    let first = ask_in(&app, &token, None, "top products").await;
    let id: Uuid = serde_json::from_value(first["conversation_id"].clone()).unwrap();
    let detail = get_json(&app, &token, &format!("/ai/conversations/{id}")).await;

    let specs = detail["turns"][0]["specs"].as_array().unwrap();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0]["preset_id"], "top_products");
    assert_eq!(specs[0]["spec"]["dataset"], "order_items");
    // No rows anywhere in the stored turn.
    let wire = detail["turns"][0].to_string();
    assert!(
        !wire.contains("\"rows\""),
        "rows must not be stored: {wire}"
    );
}

#[sqlx::test]
async fn a_resumed_conversation_replays_the_previous_query(pool: PgPool) {
    // The same property as the stateless follow-up test, but with the history
    // coming from the SERVER — the client sends only a conversation id.
    let s = seed(&pool, "a").await;
    let token = org_admin_token_for(s.org, s.admin);

    // Turn 1 with the router, so a real spec is stored.
    let app1 = app(&pool, Arc::new(MockProvider::router())).await;
    let first = ask_in(&app1, &token, None, "top products").await;
    let id: Uuid = serde_json::from_value(first["conversation_id"].clone()).unwrap();

    // Turn 2 with a provider that requires the prior spec to be in the
    // transcript, so it cannot pass by accident.
    let seen = Arc::new(AtomicUsize::new(0));
    let app2 = app(
        &pool,
        Arc::new(FollowUp {
            seen_prior_spec: seen.clone(),
        }),
    )
    .await;
    let second = ask_in(&app2, &token, Some(id), "and for all time?").await;

    assert_eq!(
        seen.load(Ordering::SeqCst),
        1,
        "the stored spec never reached the transcript"
    );
    assert_eq!(second["results"][0]["spec"]["dataset"], "order_items");
    assert_eq!(second["results"][0]["spec"]["period"]["preset"], "all_time");
}

#[sqlx::test]
async fn conversations_are_private_to_their_user(pool: PgPool) {
    // RLS fences the org; this is the second fence, which RLS cannot provide
    // because the tenant pool is per-org and not per-user.
    let s = seed(&pool, "a").await;
    let app = app(&pool, Arc::new(MockProvider::router())).await;

    let mine = org_admin_token_for(s.org, s.admin);
    let created = ask_in(&app, &mine, None, "top products").await;
    let id: Uuid = serde_json::from_value(created["conversation_id"].clone()).unwrap();

    // A different user in the SAME org.
    let other = org_admin_token_for(s.org, s.other_admin);
    for (method, uri) in [
        (
            actix_web::http::Method::GET,
            format!("/ai/conversations/{id}"),
        ),
        (
            actix_web::http::Method::DELETE,
            format!("/ai/conversations/{id}"),
        ),
    ] {
        let req = test::TestRequest::default()
            .method(method.clone())
            .uri(&uri)
            .insert_header(("Authorization", format!("Bearer {other}")))
            .to_request();
        assert_eq!(
            test::call_service(&app, req).await.status(),
            404,
            "{method} {uri} leaked another user's conversation"
        );
    }

    // ...and continuing it is refused too, which is the path that would
    // otherwise leak the CONTENT rather than just its existence.
    let req = test::TestRequest::post()
        .uri("/ai/chat")
        .insert_header(("Authorization", format!("Bearer {other}")))
        .set_json(json!({ "question": "and last month?", "conversation_id": id }))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 404);

    // The owner still sees it, so the test cannot be passing because nothing
    // exists.
    let list = get_json(&app, &mine, "/ai/conversations").await;
    assert_eq!(list["conversations"].as_array().unwrap().len(), 1);
    assert_eq!(
        get_json(&app, &other, "/ai/conversations").await["conversations"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

#[sqlx::test]
async fn a_conversation_can_be_renamed_and_deleted(pool: PgPool) {
    let s = seed(&pool, "a").await;
    let app = app(&pool, Arc::new(MockProvider::router())).await;
    let token = org_admin_token_for(s.org, s.admin);
    let created = ask_in(&app, &token, None, "top products").await;
    let id: Uuid = serde_json::from_value(created["conversation_id"].clone()).unwrap();

    let req = test::TestRequest::patch()
        .uri(&format!("/ai/conversations/{id}"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(json!({ "title": "  Menu review  " }))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 204);
    assert_eq!(
        get_json(&app, &token, &format!("/ai/conversations/{id}")).await["title"],
        "Menu review"
    );

    let req = test::TestRequest::patch()
        .uri(&format!("/ai/conversations/{id}"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(json!({ "title": "   " }))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 400);

    let req = test::TestRequest::delete()
        .uri(&format!("/ai/conversations/{id}"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 204);
    // Soft-deleted: gone from the list and from resumption, both as 404.
    assert!(
        get_json(&app, &token, "/ai/conversations").await["conversations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let req = test::TestRequest::get()
        .uri(&format!("/ai/conversations/{id}"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 404);
}

#[sqlx::test]
async fn concurrent_sends_into_one_conversation_do_not_collide(pool: PgPool) {
    // Two devices, or a double-tap. Without the row lock in `append_turn` both
    // would claim the same seq and one would lose to the unique constraint.
    let s = seed(&pool, "a").await;
    let app = app(&pool, Arc::new(MockProvider::router())).await;
    let token = org_admin_token_for(s.org, s.admin);
    let created = ask_in(&app, &token, None, "top products").await;
    let id: Uuid = serde_json::from_value(created["conversation_id"].clone()).unwrap();

    let db = crate::db::Db::for_org(&pool, s.org).await;

    // Drive the store directly: the HTTP layer would serialize these.
    let owner: Uuid = sqlx::query_scalar("SELECT user_id FROM ai_conversations WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let record = |q: &str| crate::ai::store::TurnRecord {
        question: q.to_string(),
        answer: Some("ok".into()),
        kind: "answer".into(),
        specs: json!([]),
        provider: None,
    };
    let (second, third) = (record("second"), record("third"));
    let (a, b) = tokio::join!(
        crate::ai::store::append_turn(&db, id, s.org, owner, &second),
        crate::ai::store::append_turn(&db, id, s.org, owner, &third),
    );
    let mut seqs = vec![
        a.expect("first concurrent append"),
        b.expect("second concurrent append"),
    ];
    seqs.sort_unstable();
    assert_eq!(seqs, vec![2, 3], "concurrent appends collided");
}

#[sqlx::test]
async fn a_clarification_is_stored_as_the_reply_it_was(pool: PgPool) {
    // A stored turn with a blank answer would read to the summarizer as a
    // failure; what the merchant saw was a question back.
    let s = seed(&pool, "a").await;
    let app = app(&pool, Arc::new(MockProvider::router())).await;
    let token = org_admin_token_for(s.org, s.admin);
    let out = ask_in(&app, &token, None, "how is everything going").await;
    assert_eq!(out["kind"], "clarify");
    let id: Uuid = serde_json::from_value(out["conversation_id"].clone()).unwrap();

    let detail = get_json(&app, &token, &format!("/ai/conversations/{id}")).await;
    assert_eq!(detail["turns"][0]["kind"], "clarify");
    assert!(!detail["turns"][0]["answer"].as_str().unwrap().is_empty());
    assert_eq!(detail["turns"][0]["specs"].as_array().unwrap().len(), 0);
}

// ── Rolling compaction ──────────────────────────────────────────────────────

/// A transport that always fails, for proving compaction degrades safely.
struct AlwaysFails;

#[async_trait]
impl LlmProvider for AlwaysFails {
    async fn complete(&self, _req: Completion<'_>) -> Result<Turn, ProviderError> {
        Err(ProviderError::Upstream("upstream is down".into()))
    }
    fn name(&self) -> String {
        "failing".into()
    }
}

/// Append `n` turns straight to the store, bypassing the model.
async fn seed_turns(db: &crate::db::Db, id: Uuid, org: Uuid, user: Uuid, n: i32) {
    for i in 1..=n {
        let record = crate::ai::store::TurnRecord {
            question: format!("question {i}"),
            answer: Some(format!("answer {i}")),
            kind: "answer".into(),
            specs: json!([{ "title": null, "preset_id": "revenue_total",
                            "spec": { "dataset": "orders" } }]),
            provider: Some("mock".into()),
        };
        crate::ai::store::append_turn(db, id, org, user, &record)
            .await
            .unwrap_or_else(|e| panic!("seeding turn {i}: {e}"));
    }
}

async fn new_conversation(db: &crate::db::Db, s: &crate::analytics::tests::Seeded) -> Uuid {
    crate::ai::store::create(db, s.org, s.admin, "en", "first question")
        .await
        .unwrap()
}

#[sqlx::test]
async fn compaction_folds_older_turns_and_leaves_the_window_verbatim(pool: PgPool) {
    use crate::ai::compaction::{VERBATIM_TURNS, compact};
    use crate::ai::store;

    let s = seed(&pool, "a").await;
    let db = crate::db::Db::for_org(&pool, s.org).await;
    let id = new_conversation(&db, &s).await;

    // Nine turns: three should fold, six stay verbatim.
    seed_turns(&db, id, s.org, s.admin, 9).await;

    let provider = MockProvider::scripted(vec![Turn::Text(
        "The merchant reviewed revenue and products for the Marina branch.".into(),
    )]);
    assert!(compact(&db, &provider, id).await.unwrap());

    let ctx = store::replay_context(&db, id, s.admin, 14).await.unwrap();
    assert_eq!(
        ctx.summary.as_deref(),
        Some("The merchant reviewed revenue and products for the Marina branch.")
    );
    // Exactly the window remains verbatim, and it is the NEWEST turns.
    assert_eq!(ctx.turns.len(), VERBATIM_TURNS as usize);
    assert_eq!(ctx.turns.first().unwrap().seq, 4);
    assert_eq!(ctx.turns.last().unwrap().seq, 9);
    // ...and they still carry their specs, so a follow-up still resolves.
    assert!(store::primary_spec(ctx.turns.last().unwrap()).is_some());
}

#[sqlx::test]
async fn a_short_conversation_is_never_compacted(pool: PgPool) {
    use crate::ai::compaction::compact;

    let s = seed(&pool, "a").await;
    let db = crate::db::Db::for_org(&pool, s.org).await;
    let id = new_conversation(&db, &s).await;
    seed_turns(&db, id, s.org, s.admin, 6).await;

    // A model call here would be pure waste — and would also mean every short
    // chat pays for a summary nobody reads.
    let provider = MockProvider::scripted(vec![Turn::Text("should never run".into())]);
    assert!(!compact(&db, &provider, id).await.unwrap());

    let ctx = crate::ai::store::replay_context(&db, id, s.admin, 14)
        .await
        .unwrap();
    assert!(ctx.summary.is_none());
    assert_eq!(ctx.turns.len(), 6);
}

#[sqlx::test]
async fn compaction_is_cumulative_across_rounds(pool: PgPool) {
    use crate::ai::compaction::compact;

    let s = seed(&pool, "a").await;
    let db = crate::db::Db::for_org(&pool, s.org).await;
    let id = new_conversation(&db, &s).await;

    seed_turns(&db, id, s.org, s.admin, 9).await;
    let first = MockProvider::scripted(vec![Turn::Text("Round one summary.".into())]);
    assert!(compact(&db, &first, id).await.unwrap());

    // Five more turns; the window slides again.
    seed_turns(&db, id, s.org, s.admin, 5).await;
    let second = MockProvider::scripted(vec![Turn::Text("Round two summary.".into())]);
    assert!(compact(&db, &second, id).await.unwrap());

    let ctx = crate::ai::store::replay_context(&db, id, s.admin, 14)
        .await
        .unwrap();
    // The second round REPLACES the first — the model was given the old summary
    // and rewrote it, so the summary stays one bounded paragraph however long
    // the conversation runs.
    assert_eq!(ctx.summary.as_deref(), Some("Round two summary."));
    assert_eq!(ctx.turns.len(), 6);
    assert_eq!(ctx.turns.last().unwrap().seq, 14);
}

#[sqlx::test]
async fn the_summarizer_is_shown_the_previous_summary_and_the_new_turns(pool: PgPool) {
    // Without the previous summary each round would only describe its own
    // slice, and everything before it would be silently lost — which is the
    // failure this whole design exists to avoid.
    use crate::ai::compaction::compact;

    let s = seed(&pool, "a").await;
    let db = crate::db::Db::for_org(&pool, s.org).await;
    let id = new_conversation(&db, &s).await;
    seed_turns(&db, id, s.org, s.admin, 9).await;

    let first = MockProvider::scripted(vec![Turn::Text("Earlier: the Marina branch.".into())]);
    compact(&db, &first, id).await.unwrap();
    seed_turns(&db, id, s.org, s.admin, 5).await;

    let seen = Arc::new(std::sync::Mutex::new(String::new()));
    struct Recording(Arc<std::sync::Mutex<String>>);
    #[async_trait]
    impl LlmProvider for Recording {
        async fn complete(&self, req: Completion<'_>) -> Result<Turn, ProviderError> {
            if let Some(crate::ai::llm::Message::User(t)) = req.messages.first() {
                *self.0.lock().unwrap() = t.clone();
            }
            // The summarizing call must offer NO tools, or the model answers
            // with a tool call and no summary is ever written.
            assert!(req.tools.is_empty(), "the summary call must be tool-less");
            assert!(!req.force_tool);
            Ok(Turn::Text("Combined summary.".into()))
        }
        fn name(&self) -> String {
            "recording".into()
        }
    }

    compact(&db, &Recording(seen.clone()), id).await.unwrap();
    let prompt = seen.lock().unwrap().clone();
    // The previous summary is carried in, so round two covers round one's
    // material rather than only its own slice.
    assert!(prompt.contains("Earlier: the Marina branch."), "{prompt}");
    // Round one folded turns 1–3; with 14 turns the window now ends at 8, so
    // this round folds 4–8 and nothing newer.
    assert!(prompt.contains("[8] Merchant: question 8"), "{prompt}");
    assert!(
        !prompt.contains("[9] Merchant:"),
        "a turn still inside the verbatim window was summarized: {prompt}"
    );
}

#[sqlx::test]
async fn a_failed_summary_leaves_the_conversation_intact(pool: PgPool) {
    // Nothing depends on compaction succeeding: the next turn simply replays
    // more verbatim history. An assistant that stopped answering because a
    // SUMMARY could not be written would be a far worse outcome.
    use crate::ai::compaction::compact;

    let s = seed(&pool, "a").await;
    let db = crate::db::Db::for_org(&pool, s.org).await;
    let id = new_conversation(&db, &s).await;
    seed_turns(&db, id, s.org, s.admin, 9).await;

    assert!(compact(&db, &AlwaysFails, id).await.is_err());

    let ctx = crate::ai::store::replay_context(&db, id, s.admin, 14)
        .await
        .unwrap();
    assert!(
        ctx.summary.is_none(),
        "a failed pass must not write a summary"
    );
    // Every turn is still replayed — nothing was dropped on the floor.
    assert_eq!(ctx.turns.len(), 9);

    // ...and a later successful pass still catches up.
    let good = MockProvider::scripted(vec![Turn::Text("Recovered summary.".into())]);
    assert!(compact(&db, &good, id).await.unwrap());
    let ctx = crate::ai::store::replay_context(&db, id, s.admin, 14)
        .await
        .unwrap();
    assert_eq!(ctx.summary.as_deref(), Some("Recovered summary."));
}

#[sqlx::test]
async fn a_model_that_answers_with_no_summary_changes_nothing(pool: PgPool) {
    use crate::ai::compaction::compact;

    let s = seed(&pool, "a").await;
    let db = crate::db::Db::for_org(&pool, s.org).await;
    let id = new_conversation(&db, &s).await;
    seed_turns(&db, id, s.org, s.admin, 9).await;

    // Blank prose is not a summary; storing it would erase context.
    let blank = MockProvider::scripted(vec![Turn::Text("   ".into())]);
    assert!(compact(&db, &blank, id).await.is_err());
    assert!(
        crate::ai::store::replay_context(&db, id, s.admin, 14)
            .await
            .unwrap()
            .summary
            .is_none()
    );
}

#[sqlx::test]
async fn a_losing_concurrent_pass_does_not_rewind_the_winner(pool: PgPool) {
    // Two passes can overlap — a spawned one and a retry. The conditional
    // update means the loser writes nothing rather than replacing a newer
    // summary with an older one and losing the turns it covered.
    use crate::ai::store;

    let s = seed(&pool, "a").await;
    let db = crate::db::Db::for_org(&pool, s.org).await;
    let id = new_conversation(&db, &s).await;
    seed_turns(&db, id, s.org, s.admin, 9).await;

    assert!(
        store::commit_summary(&db, id, "winner", 3, 0)
            .await
            .unwrap(),
        "the first pass must commit"
    );
    assert!(
        !store::commit_summary(&db, id, "loser", 3, 0).await.unwrap(),
        "a pass working from a stale sequence must write nothing"
    );
    assert_eq!(
        store::replay_context(&db, id, s.admin, 14)
            .await
            .unwrap()
            .summary
            .as_deref(),
        Some("winner")
    );
}

#[sqlx::test]
async fn a_long_chat_reaches_the_model_as_a_summary_plus_a_window(pool: PgPool) {
    // The end-to-end property: context is unlimited but the replayed prefix is
    // bounded, and the condensed head is labelled as background rather than
    // replayed as something the merchant just said.
    let s = seed(&pool, "a").await;
    let db = crate::db::Db::for_org(&pool, s.org).await;
    let id = new_conversation(&db, &s).await;
    seed_turns(&db, id, s.org, s.admin, 9).await;

    let summarizer = MockProvider::scripted(vec![Turn::Text(
        "Earlier the merchant compared branches for last month.".into(),
    )]);
    crate::ai::compaction::compact(&db, &summarizer, id)
        .await
        .unwrap();

    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    struct Capture(Arc<std::sync::Mutex<Vec<String>>>);
    #[async_trait]
    impl LlmProvider for Capture {
        async fn complete(&self, req: Completion<'_>) -> Result<Turn, ProviderError> {
            let mut log = self.0.lock().unwrap();
            if log.is_empty() {
                for m in req.messages {
                    if let crate::ai::llm::Message::User(t) = m {
                        log.push(t.clone());
                    }
                }
            }
            drop(log);
            Ok(MockProvider::answer("Here you go."))
        }
        fn name(&self) -> String {
            "capture".into()
        }
    }

    let app = app(&pool, Arc::new(Capture(seen.clone()))).await;
    let token = org_admin_token_for(s.org, s.admin);
    ask_in(&app, &token, Some(id), "and this month?").await;

    let user_messages = seen.lock().unwrap().clone();
    let grounding = &user_messages[0];
    assert!(
        grounding.contains("Earlier in this conversation (condensed"),
        "the summary never reached the model: {grounding}"
    );
    assert!(grounding.contains("compared branches for last month"));
    // Grounding + 6 replayed questions + the new question. The nine older
    // turns are represented by the summary, not by nine more messages.
    assert_eq!(user_messages.len(), 1 + 6 + 1);
    assert!(user_messages.contains(&"question 9".to_string()));
    assert!(!user_messages.contains(&"question 1".to_string()));
}

// ── Pseudonymisation ────────────────────────────────────────────────────────

/// Records every byte sent to the model, so assertions are on what actually
/// crossed the boundary rather than on the code that built it.
///
/// When it sees a result it answers using the `waiter` value it was SHOWN,
/// whatever that turned out to be. Hardcoding a code would make the test
/// depend on user-id ordering, and would pass for the wrong reason the moment
/// the seed changed.
struct Wiretap {
    seen: Arc<std::sync::Mutex<Vec<String>>>,
    /// The reply, with `{code}` replaced by the pseudonym it was shown.
    template: String,
}

#[async_trait]
impl LlmProvider for Wiretap {
    async fn complete(&self, req: Completion<'_>) -> Result<Turn, ProviderError> {
        let mut log = self.seen.lock().unwrap();
        log.push(req.system.to_string());
        for m in req.messages {
            log.push(format!("{m:?}"));
        }
        drop(log);

        let shown_code = req.messages.iter().find_map(|m| match m {
            crate::ai::llm::Message::ToolResult { content, .. } => content
                .get("rows")
                .and_then(Value::as_array)
                .and_then(|rows| rows.first())
                .and_then(|row| row.get("waiter"))
                .and_then(Value::as_str)
                .map(str::to_string),
            _ => None,
        });

        Ok(match shown_code {
            Some(code) => MockProvider::answer(&self.template.replace("{code}", &code)),
            None if req
                .messages
                .iter()
                .any(|m| matches!(m, crate::ai::llm::Message::ToolResult { .. })) =>
            {
                MockProvider::answer(&self.template.replace("{code}", ""))
            }
            None => MockProvider::call(
                tools::QUERY_METRICS,
                json!({
                    "dataset": "orders",
                    "dimensions": ["waiter"],
                    "measures": ["revenue"],
                    "period": { "preset": "all_time" }
                }),
            ),
        })
    }
    fn name(&self) -> String {
        "wiretap".into()
    }
}

/// Seed a waiter and an order they took, so a `waiter` breakdown has a real
/// person's name in it.
async fn seed_waiter(pool: &PgPool, s: &crate::analytics::tests::Seeded, name: &str) -> Uuid {
    let waiter = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, name, role, org_id, pin_hash) VALUES ($1,$2,'waiter',$3,'x')",
    )
    .bind(waiter)
    .bind(name)
    .bind(s.org)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("UPDATE orders SET waiter_id = $1 WHERE branch_id = $2")
        .bind(waiter)
        .bind(s.branch)
        .execute(pool)
        .await
        .unwrap();
    waiter
}

#[sqlx::test]
async fn a_staff_name_never_reaches_the_model(pool: PgPool) {
    // The property, asserted over everything that crossed the wire: the model
    // is given a code, and the merchant is given the name back.
    let s = seed(&pool, "a").await;
    seed_waiter(&pool, &s, "Ahmed Hassan").await;

    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let app = app(
        &pool,
        Arc::new(Wiretap {
            seen: seen.clone(),
            // Replies with whatever code it was actually shown.
            template: "{code} led on revenue.".into(),
        }),
    )
    .await;
    let token = org_admin_token_for(s.org, s.admin);
    let body = ask_in(&app, &token, None, "who sold the most?").await;

    let wire = seen.lock().unwrap().join("\n");
    assert!(
        wire.contains("E-"),
        "the model was never shown a pseudonym: {wire}"
    );
    assert!(
        !wire.contains("Ahmed Hassan"),
        "a staff name reached the model: {wire}"
    );

    // ...and the merchant gets the real name in the prose.
    assert!(
        body["text"].as_str().unwrap().contains("Ahmed Hassan"),
        "the name was not restored: {}",
        body["text"]
    );
    // The rows the client renders were never pseudonymised — they do not pass
    // through the model at all.
    assert_eq!(body["results"][0]["rows"][0]["waiter"], "Ahmed Hassan");
}

#[sqlx::test]
async fn business_names_still_reach_the_model(pool: PgPool) {
    // Over-redaction is its own failure: without product and branch names the
    // model cannot reason or answer at all.
    let s = seed(&pool, "a").await;
    let app = app(&pool, Arc::new(MockProvider::router())).await;
    let token = org_admin_token_for(s.org, s.admin);
    let body = ask_in(&app, &token, None, "top products").await;
    // Product names come back intact through the whole pipeline.
    assert_eq!(body["results"][0]["rows"][0]["product"], "Latte");
}

#[sqlx::test]
async fn a_question_naming_a_colleague_is_pseudonymised_too(pool: PgPool) {
    // The merchant's own words are the easiest way for a name to leak.
    let s = seed(&pool, "a").await;
    seed_waiter(&pool, &s, "Ahmed Hassan").await;

    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let app = app(
        &pool,
        Arc::new(Wiretap {
            seen: seen.clone(),
            template: "{code} took 12 orders.".into(),
        }),
    )
    .await;
    let token = org_admin_token_for(s.org, s.admin);
    ask_in(&app, &token, None, "how did Ahmed Hassan do last week?").await;

    let wire = seen.lock().unwrap().join("\n");
    assert!(
        !wire.contains("Ahmed Hassan"),
        "the question leaked a staff name: {wire}"
    );
}

#[sqlx::test]
async fn a_replayed_answer_does_not_leak_what_this_turn_protects(pool: PgPool) {
    // Prior answers are STORED with real names, because that is what the
    // merchant saw. Replaying them raw would undo the whole mechanism.
    let s = seed(&pool, "a").await;
    seed_waiter(&pool, &s, "Ahmed Hassan").await;
    let db = crate::db::Db::for_org(&pool, s.org).await;
    let id = crate::ai::store::create(&db, s.org, s.admin, "en", "who sold the most")
        .await
        .unwrap();
    crate::ai::store::append_turn(
        &db,
        id,
        s.org,
        s.admin,
        &crate::ai::store::TurnRecord {
            question: "who sold the most".into(),
            answer: Some("Ahmed Hassan led on revenue.".into()),
            kind: "answer".into(),
            specs: json!([{ "title": null, "preset_id": null,
                            "spec": { "dataset": "orders", "dimensions": ["waiter"] } }]),
            provider: Some("mock".into()),
        },
    )
    .await
    .unwrap();

    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let app = app(
        &pool,
        Arc::new(Wiretap {
            seen: seen.clone(),
            template: "Same as before.".into(),
        }),
    )
    .await;
    let token = org_admin_token_for(s.org, s.admin);
    ask_in(&app, &token, Some(id), "and last month?").await;

    let wire = seen.lock().unwrap().join("\n");
    assert!(
        !wire.contains("Ahmed Hassan"),
        "a replayed answer leaked a staff name: {wire}"
    );
}
