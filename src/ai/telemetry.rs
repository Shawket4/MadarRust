//! Per-turn telemetry.
//!
//! Without this you cannot answer the only question that matters about an LLM
//! feature: *is it getting better or worse?* A prompt edit, a model swap, a new
//! measure in the registry — each of them changes routing quality, and none of
//! them is observable from error rates alone, because a confidently wrong answer
//! returns 200 like a right one.
//!
//! So every turn emits one structured record: what was asked, which tools ran,
//! how many were rejected, how many rows came back, where the time went, and how
//! it ended. That is enough to watch the rejection rate, spot a dataset the
//! model never picks, and catch a model change that quietly doubled latency.
//!
//! Nothing here records the merchant's data. The question is recorded because it
//! is what the merchant chose to type and is needed to diagnose routing; result
//! rows and figures never are.

use std::time::Duration;

/// Accumulates one turn's telemetry, then emits it.
#[derive(Debug, Default)]
pub struct TurnLog {
    pub provider: String,
    pub locale: String,
    pub question_len: usize,
    /// Model round trips.
    pub model_calls: u32,
    /// Total time waiting on the model.
    pub model_time: Duration,
    /// Tool names in call order — the routing trace.
    pub tools: Vec<String>,
    /// Tool calls the backend rejected. A rising rate means the schema digest or
    /// the prompt has stopped matching what the model believes.
    pub tool_errors: u32,
    /// First rejection, for triage.
    pub first_error: Option<String>,
    /// Rows returned across every query in the turn.
    pub rows: usize,
    /// Terminal state: answered, clarified, exhausted, …
    pub outcome: String,
    /// True when the answer came from cache and no model call was made.
    pub cached: bool,
}

impl TurnLog {
    pub fn new(provider: &str, locale: &str, question: &str) -> Self {
        Self {
            provider: provider.to_string(),
            locale: locale.to_string(),
            question_len: question.chars().count(),
            outcome: "incomplete".into(),
            ..Default::default()
        }
    }

    pub fn record_model_call(&mut self, elapsed: Duration) {
        self.model_calls += 1;
        self.model_time += elapsed;
    }

    pub fn record_tool(&mut self, name: &str) {
        self.tools.push(name.to_string());
    }

    pub fn record_tool_error(&mut self, message: &str) {
        self.tool_errors += 1;
        if self.first_error.is_none() {
            // Truncated: this is a routing signal, not a place to accumulate
            // whatever text an error happens to carry.
            self.first_error = Some(message.chars().take(200).collect());
        }
    }

    pub fn record_rows(&mut self, rows: usize) {
        self.rows += rows;
    }

    pub fn finished(&mut self, outcome: &str) {
        self.outcome = outcome.to_string();
    }

    pub fn served_from_cache(&mut self) {
        self.cached = true;
        self.outcome = "cache_hit".into();
    }

    /// Emit the record. Called once per turn, whatever the outcome — including
    /// failures, which are exactly the turns worth counting.
    pub fn emit(&self, total: Duration, question: &str) {
        tracing::info!(
            target: "ai_turn",
            provider = %self.provider,
            locale = %self.locale,
            outcome = %self.outcome,
            cached = self.cached,
            model_calls = self.model_calls,
            model_ms = self.model_time.as_millis() as u64,
            total_ms = total.as_millis() as u64,
            tools = %self.tools.join(","),
            tool_errors = self.tool_errors,
            first_error = self.first_error.as_deref().unwrap_or(""),
            rows = self.rows,
            question_len = self.question_len,
            question = %question,
            "ai turn"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_accumulates_its_routing_trace() {
        let mut log = TurnLog::new("mock", "en", "top products last month");
        log.record_model_call(Duration::from_millis(300));
        log.record_tool("run_preset");
        log.record_rows(10);
        log.record_model_call(Duration::from_millis(200));
        log.record_tool("answer");
        log.finished("answered");

        assert_eq!(log.model_calls, 2);
        assert_eq!(log.model_time, Duration::from_millis(500));
        assert_eq!(log.tools, vec!["run_preset", "answer"]);
        assert_eq!(log.rows, 10);
        assert_eq!(log.outcome, "answered");
        assert_eq!(log.question_len, 23);
    }

    #[test]
    fn the_first_rejection_is_kept_and_bounded() {
        let mut log = TurnLog::new("mock", "en", "q");
        log.record_tool_error(&"x".repeat(1000));
        log.record_tool_error("second");
        assert_eq!(log.tool_errors, 2);
        // The first is what diagnoses the turn; it is capped so a verbose error
        // cannot bloat the log line.
        assert_eq!(log.first_error.as_ref().unwrap().len(), 200);
    }

    #[test]
    fn question_length_counts_characters_not_bytes() {
        // Arabic questions are the common case here; a byte count would report
        // roughly double and make any length threshold meaningless.
        let log = TurnLog::new("mock", "ar", "مبيعات امبارح");
        assert_eq!(log.question_len, 13);
    }

    #[test]
    fn a_cache_hit_is_its_own_outcome() {
        let mut log = TurnLog::new("mock", "en", "q");
        log.served_from_cache();
        assert!(log.cached);
        assert_eq!(log.outcome, "cache_hit");
        assert_eq!(log.model_calls, 0);
    }

    #[test]
    fn an_unfinished_turn_is_visible_as_such() {
        // A turn that panics or returns early must not look like a success.
        let log = TurnLog::new("mock", "en", "q");
        assert_eq!(log.outcome, "incomplete");
    }
}
