//! Prompts, shared by every provider.
//!
//! The system instruction is assembled **once** and cached, and it embeds the
//! whole semantic-layer digest ([`crate::analytics::registry::schema_digest`]).
//! Two consequences worth stating:
//!
//!   * It is byte-stable across every request, so it sits in the cacheable
//!     prefix that Gemini's implicit cache and OpenAI-style prefix caching both
//!     key on. All per-request variation goes in the trailing user turn.
//!   * It cannot drift from what actually executes, because it is generated from
//!     the same registry the compiler reads. A measure that exists is described;
//!     one that does not, is not.

use crate::analytics::registry::schema_digest;

/// Instructions the model gets before anything else.
const INSTRUCTIONS: &str = "\
You are the analytics assistant inside a restaurant point-of-sale system. A merchant \
asks about THEIR OWN business data in plain language, in English or Arabic (including \
Egyptian dialect). You answer with real figures pulled from their data.

How you work:
- You never write SQL. You call tools that run pre-approved, parameterized queries.
- You do not choose which branches to include. Branch access is enforced by the backend. \
If the merchant names a branch, pass its name as the `branch` argument and the backend \
matches it within what they are allowed to see.
- Money in tool results is ALREADY IN POUNDS (EGP). Quote the figures exactly as given — never divide, multiply or otherwise convert them. Doing arithmetic on them is how a stated figure ends up disagreeing with the chart shown beside it.
- Resolve relative dates with a `period.preset` such as `yesterday` or `last_month`. \
The backend resolves these in the merchant's own timezone. Only use explicit from/to \
dates for a window no preset covers, and never compute a date yourself when a preset fits.

Your loop:
1. Pick the dataset whose single row IS the thing being counted. Revenue and ticket size \
are order-grain; product and category questions are line-item grain; payment mix is \
tender grain. Getting this wrong gives a plausible number that is simply false.
2. Call `run_preset` when a curated metric clearly matches, otherwise `query_metrics`.
3. If a call is rejected, READ THE ERROR — it lists the valid options — and retry. Do not \
give up after one rejection, and do not answer from a query that failed.
4. Call `answer` with the finding. Lead with the number that answers the question.

Rules that matter:
- If a query returns no rows, say there were none. Never present an empty result as a \
zero, and never invent figures. Every number you state must come from a tool result.
- If a measure comes back null because cost data is missing, say the cost is not \
recorded rather than treating it as zero.
- Answer in the same language as the question.
- Two or three sentences. No markdown, no bullet lists, no preamble.
- The merchant sees the table or chart alongside your words, so do not read the whole \
table back to them — state the takeaway.";

/// The full system instruction: fixed guidance plus the generated schema digest.
pub fn system() -> &'static str {
    static PROMPT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PROMPT.get_or_init(|| format!("{INSTRUCTIONS}\n\n---\n\n{}", schema_digest()))
}

/// Per-request grounding, sent as the leading user turn: everything that varies
/// per merchant and per moment, kept out of the cacheable prefix.
pub fn grounding(
    today: &str,
    timezone: &str,
    locale: &str,
    branches: &[String],
    condensed: Option<&str>,
) -> String {
    let branch_list = if branches.is_empty() {
        "none".to_string()
    } else {
        branches.join(", ")
    };
    let mut out = format!(
        "Context for this conversation.\n\
         Today is {today} in timezone {timezone}.\n\
         Answer language: {locale}.\n\
         Branches this user can see: {branch_list}."
    );
    // The condensed head of a long conversation. It is stated as a summary
    // rather than replayed as turns so the model treats it as background it
    // already knows, not as something the merchant just said.
    if let Some(summary) = condensed.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str(&format!(
            "\n\nEarlier in this conversation (condensed — the messages below are \
             the most recent ones in full):\n{summary}"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_system_prompt_carries_the_live_registry() {
        let p = system();
        // Not a hand-maintained copy: these come from the registry itself.
        assert!(p.contains("order_items"));
        assert!(p.contains("avg_order_value"));
        assert!(p.contains("top_products"));
        assert!(p.contains("last_month"));
    }

    #[test]
    fn the_system_prompt_is_stable_across_calls() {
        // Byte-stability is what makes upstream prefix caching hit.
        assert_eq!(system().as_ptr(), system().as_ptr());
    }

    #[test]
    fn grounding_holds_the_per_request_variation() {
        let g = grounding(
            "2026-09-03",
            "Africa/Cairo",
            "ar",
            &["Sidi Henish".into()],
            None,
        );
        assert!(
            g.contains("2026-09-03") && g.contains("Africa/Cairo") && g.contains("Sidi Henish")
        );
        // ...and none of it leaks into the cached prefix.
        assert!(!system().contains("2026-09-03"));
    }

    #[test]
    fn no_branches_is_stated_not_left_blank() {
        let g = grounding("2026-09-03", "UTC", "en", &[], None);
        assert!(g.contains("none"));
    }

    #[test]
    fn a_condensed_head_is_labelled_as_background_not_as_a_new_message() {
        // Replayed as a turn it would read as something the merchant just
        // said; labelled as a summary it reads as context already known.
        let g = grounding(
            "2026-09-03",
            "UTC",
            "en",
            &[],
            Some("The merchant is reviewing the Marina branch."),
        );
        assert!(g.contains("Earlier in this conversation (condensed"));
        assert!(g.contains("Marina branch"));

        // An empty summary adds nothing rather than an empty heading.
        let bare = grounding("2026-09-03", "UTC", "en", &[], Some("   "));
        assert!(!bare.contains("Earlier in this conversation"));
    }
}
