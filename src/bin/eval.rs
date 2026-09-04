//! Run the golden evaluation set against a live model.
//!
//! ```text
//!   cargo run --bin eval                    # run every case
//!   cargo run --bin eval -- --category period_resolution
//!   cargo run --bin eval -- --limit 20      # a cheap smoke run
//!   cargo run --bin eval -- --threshold 0.8 # gate CI on accuracy
//!   cargo run --bin eval -- --delay-ms 0     # no pacing (paid key)
//!   cargo run --bin eval -- --export-jsonl out.jsonl   # fine-tuning data, no API calls
//! ```
//!
//! # What it measures, and what it deliberately does not
//!
//! **Tool selection and argument accuracy only.** It sends the real system
//! prompt and the real tool declarations, then compares the tool call that comes
//! back against the case's expectation. It does NOT execute the query, touch the
//! database, or judge the prose — those have their own tests, and mixing them in
//! would mean an eval failure could mean six different things.
//!
//! That also keeps it runnable with nothing but an API key: no database, no
//! seeded tenant, no auth.
//!
//! # Why this is a binary and not a test
//!
//! Every case is a paid API call, so a 200-case run costs real money and takes
//! minutes. `cargo test` must stay free and fast. The *validation* of the case
//! file against the registry does run in the suite — see `ai::evals` — so a
//! renamed preset still breaks the build immediately.
//!
//! # Reading the output
//!
//! Accuracy is reported per category AND split by confidence. `high` cases
//! follow mechanically from the schema; `review` cases encode a judgement about
//! intent. Averaging the two hides which one regressed, and only the first is a
//! fair gate.

use std::collections::BTreeMap;

use madar_rust::ai::{
    evals::{self, Case},
    llm::{Completion, LlmProvider, Message, Turn},
    prompt, tools,
};

#[derive(Default)]
struct Tally {
    total: usize,
    correct: usize,
}

impl Tally {
    fn record(&mut self, ok: bool) {
        self.total += 1;
        self.correct += ok as usize;
    }
    fn pct(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        self.correct as f64 / self.total as f64
    }
}

/// How a single case came out.
enum Outcome {
    Correct,
    /// The model chose a different tool or arguments than expected.
    Wrong(String),
    /// The provider itself failed — a transport error, not a model mistake.
    /// Counted separately so a flaky network does not read as a quality drop.
    Failed(String),
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };

    let file = evals::load();

    // Export needs no provider and no key — it is a pure transform of the case
    // file, so it works offline and in CI.
    if let Some(path) = flag("--export-jsonl") {
        export_jsonl(&file.cases, &path);
        return;
    }

    let category = flag("--category");
    let limit: usize = flag("--limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(usize::MAX);
    let threshold: f64 = flag("--threshold")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);
    // Pacing. Gemini's free tier allows 15 requests/minute, so an unpaced
    // 200-case run spends most of itself being rate-limited and reports the
    // 429s as excluded cases — a run that looks like it worked while covering a
    // fraction of the set. 4.2s keeps just under the free-tier ceiling; set it
    // to 0 on a paid key.
    let delay_ms: u64 = flag("--delay-ms")
        .and_then(|v| v.parse().ok())
        .unwrap_or(4_200);

    let state = madar_rust::ai::AiState::from_env();
    let Some(provider) = state.provider else {
        eprintln!(
            "No AI provider configured. Set GEMINI_API_KEY or GROQ_API_KEY \
             (and optionally AI_PROVIDER) and try again."
        );
        std::process::exit(2);
    };

    let selected: Vec<&Case> = file
        .cases
        .iter()
        .filter(|c| category.as_deref().is_none_or(|cat| c.category == cat))
        .take(limit)
        .collect();

    println!(
        "Running {} case(s) against {} — frozen now = {}\n",
        selected.len(),
        provider.name(),
        file.now
    );

    let mut by_category: BTreeMap<String, Tally> = BTreeMap::new();
    let mut by_confidence: BTreeMap<String, Tally> = BTreeMap::new();
    let mut by_lang: BTreeMap<String, Tally> = BTreeMap::new();
    let mut transport_failures = 0usize;
    let mut misses: Vec<(String, String)> = Vec::new();

    for (i, case) in selected.iter().enumerate() {
        if i > 0 && delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
        let outcome = run_case(provider.as_ref(), case).await;
        match outcome {
            Outcome::Failed(why) => {
                // Not the model's fault; excluded from accuracy entirely.
                transport_failures += 1;
                eprintln!("  ⚠ {} — provider error: {why}", case.id);
                continue;
            }
            Outcome::Correct => {
                for (map, key) in [
                    (&mut by_category, &case.category),
                    (&mut by_confidence, &case.confidence),
                    (&mut by_lang, &case.lang),
                ] {
                    map.entry(key.clone()).or_default().record(true);
                }
            }
            Outcome::Wrong(detail) => {
                for (map, key) in [
                    (&mut by_category, &case.category),
                    (&mut by_confidence, &case.confidence),
                    (&mut by_lang, &case.lang),
                ] {
                    map.entry(key.clone()).or_default().record(false);
                }
                misses.push((case.id.clone(), detail));
            }
        }
    }

    let report = |title: &str, map: &BTreeMap<String, Tally>| {
        println!("\n{title}");
        for (k, t) in map {
            println!(
                "  {:<20} {:>5.1}%  ({}/{})",
                k,
                t.pct() * 100.0,
                t.correct,
                t.total
            );
        }
    };
    report("By category", &by_category);
    report("By confidence", &by_confidence);
    report("By language", &by_lang);

    if !misses.is_empty() {
        println!("\nMisses ({}):", misses.len());
        for (id, detail) in misses.iter().take(40) {
            println!("  {id}: {detail}");
        }
        if misses.len() > 40 {
            println!("  … and {} more", misses.len() - 40);
        }
    }

    let overall: Tally = Tally {
        total: by_category.values().map(|t| t.total).sum(),
        correct: by_category.values().map(|t| t.correct).sum(),
    };
    // Gate on `high`-confidence cases only. `review` cases encode a judgement
    // about intent that has not been confirmed, so failing a build on them would
    // be gating on an opinion.
    let gated = by_confidence.get("high").map(Tally::pct).unwrap_or(0.0);

    println!(
        "\nOverall {:.1}% ({}/{}), high-confidence {:.1}%{}",
        overall.pct() * 100.0,
        overall.correct,
        overall.total,
        gated * 100.0,
        if transport_failures > 0 {
            format!(", {transport_failures} provider error(s) excluded")
        } else {
            String::new()
        }
    );

    if threshold > 0.0 && gated < threshold {
        eprintln!(
            "\n::error::high-confidence accuracy {:.1}% is below the {:.1}% threshold",
            gated * 100.0,
            threshold * 100.0
        );
        std::process::exit(1);
    }
}

/// Ask the model to route ONE question, and compare what it chose.
async fn run_case(provider: &dyn LlmProvider, case: &Case) -> Outcome {
    // Grounding mirrors what a real turn sends, minus anything tenant-specific:
    // the eval is about routing, and branch names would make results depend on
    // whichever database happened to be around.
    let grounding = prompt::grounding(
        &case.now[..10],
        "Africa/Cairo",
        if case.lang == "ar" { "ar" } else { "en" },
        &["Marina".into(), "Sidi Henish".into(), "Maadi".into()],
        None,
    );
    let messages = [
        Message::User(grounding),
        Message::User(case.question.clone()),
    ];

    let turn = provider
        .complete(Completion {
            system: prompt::system(),
            messages: &messages,
            tools: tools::tool_defs(),
            max_tokens: 700,
            force_tool: true,
        })
        .await;

    let calls = match turn {
        Ok(Turn::Calls(c)) if !c.is_empty() => c,
        Ok(Turn::Text(t)) => {
            return judge_non_call(case, "answered in prose", &t);
        }
        Ok(Turn::Calls(_)) => return Outcome::Wrong("no tool call".into()),
        Err(e) => return Outcome::Failed(e.to_string()),
    };

    let call = &calls[0];
    let e = &case.expect;

    // A negative case is correct when the model declines or asks, rather than
    // routing confidently to something plausible.
    if let Some(expected_outcome) = &e.outcome {
        let asked = call.name == tools::CLARIFY;
        return match expected_outcome.as_str() {
            "clarify" | "no_tool" | "refused" | "unknown_entity" | "invalid_period" => {
                if asked {
                    Outcome::Correct
                } else {
                    Outcome::Wrong(format!("expected a clarification, got {}", call.name))
                }
            }
            // `review` cases have no single right answer; record what happened.
            _ => Outcome::Correct,
        };
    }

    if let Some(tool) = &e.tool {
        // Composing the same query by hand is not a routing error when the case
        // says so — both paths reach the same numbers through the same compiler.
        let acceptable =
            &call.name == tool || (e.accept_custom_query && call.name == tools::QUERY_METRICS);
        if !acceptable {
            return Outcome::Wrong(format!("tool {} (expected {tool})", call.name));
        }
    }

    let args = &call.args;
    let got = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("");

    if let Some(p) = &e.preset {
        // Skip when the model legitimately composed a custom query instead.
        if call.name == tools::RUN_PRESET {
            let actual = got("preset");
            let ok = actual == p || e.accept_presets.iter().any(|a| a == actual);
            if !ok {
                return Outcome::Wrong(format!("preset '{actual}' (expected '{p}')"));
            }
        }
    }
    if let Some(d) = &e.dataset {
        if got("dataset") != d {
            return Outcome::Wrong(format!("dataset '{}' (expected '{d}')", got("dataset")));
        }
    }
    if let Some(period) = &e.period {
        let actual = args
            .get("period")
            .and_then(|p| p.get("preset"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        // OMITTING the period is correct when the question named none and the
        // expectation is simply the preset's own default — the server applies
        // it. Demanding the model restate a default it was never asked for
        // measures obedience, not accuracy.
        let default_for_preset = e
            .preset
            .as_deref()
            .and_then(madar_rust::analytics::presets::preset)
            .map(|p| serde_json::to_value(p.default_period).unwrap_or_default())
            .and_then(|v| v.as_str().map(str::to_string));
        let omitted_but_defaulted =
            actual.is_empty() && default_for_preset.as_deref() == Some(period.as_str());
        if actual != period && !omitted_but_defaulted {
            return Outcome::Wrong(format!("period '{actual}' (expected '{period}')"));
        }
    }
    if let Some(dims) = &e.dimensions {
        let actual: Vec<String> = args
            .get("dimensions")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if &actual != dims {
            return Outcome::Wrong(format!("dimensions {actual:?} (expected {dims:?})"));
        }
    }

    Outcome::Correct
}

fn judge_non_call(case: &Case, what: &str, text: &str) -> Outcome {
    // Prose is the right answer for a case whose expectation is a refusal.
    if case.expect.outcome.is_some() {
        Outcome::Correct
    } else {
        Outcome::Wrong(format!(
            "{what}: {}",
            text.chars().take(80).collect::<String>()
        ))
    }
}

/// Write the set as JSONL for fine-tuning.
///
/// One line per case: the system prompt, the question, and the ideal tool call.
/// Negative and `review` cases are EXCLUDED — training on a judgement call
/// teaches the model one opinion as fact, and training on "should have refused"
/// needs a different target format than a tool call.
fn export_jsonl(cases: &[Case], path: &str) {
    use std::io::Write;
    let mut out = std::fs::File::create(path).expect("could not create the export file");
    let mut written = 0usize;
    let mut skipped = 0usize;

    for case in cases {
        if case.confidence != "high" || case.expect.outcome.is_some() {
            skipped += 1;
            continue;
        }
        let mut args = serde_json::Map::new();
        let e = &case.expect;
        if let Some(v) = &e.preset {
            args.insert("preset".into(), serde_json::json!(v));
        }
        if let Some(v) = &e.dataset {
            args.insert("dataset".into(), serde_json::json!(v));
        }
        if let Some(v) = &e.dimensions {
            args.insert("dimensions".into(), serde_json::json!(v));
        }
        if let Some(v) = &e.measures {
            args.insert("measures".into(), serde_json::json!(v));
        }
        if let Some(v) = &e.filters {
            args.insert("filters".into(), serde_json::json!(v));
        }
        if let Some(v) = &e.period {
            args.insert("period".into(), serde_json::json!({ "preset": v }));
        }
        if let Some(v) = &e.compare {
            args.insert("compare".into(), serde_json::json!(v));
        }

        let line = serde_json::json!({
            "messages": [
                { "role": "system", "content": prompt::system() },
                { "role": "user", "content": case.question },
                { "role": "assistant", "tool_calls": [{
                    "type": "function",
                    "function": {
                        "name": e.tool.clone().unwrap_or_else(|| "query_metrics".into()),
                        "arguments": serde_json::Value::Object(args).to_string(),
                    }
                }]}
            ],
            "meta": { "id": case.id, "lang": case.lang, "category": case.category }
        });
        writeln!(out, "{line}").expect("write");
        written += 1;
    }
    println!(
        "wrote {written} training examples to {path} \
         ({skipped} skipped: review-tagged or negative — a judgement call must \
         not be trained in as fact)"
    );
}
