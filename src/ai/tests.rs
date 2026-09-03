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
use crate::analytics::tests::{org_admin_token, secret, seed};
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
