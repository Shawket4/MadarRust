//! One reporter for failures that never reach the HTTP error funnel.
//!
//! `sentry-actix` captures 5xx responses, which covers every handler that
//! returns `Err(AppError)` — the normal shape here. Three classes of failure
//! escape it entirely, and each of them used to be invisible:
//!
//!   1. **Handled failures.** Caught, logged with `tracing::warn!`, and shown to
//!      the user. The WhatsApp gateway refusing a send, a widget query failing,
//!      a summary call timing out. The request succeeds; the work did not.
//!   2. **Handlers answering 200 with an error in the body.** A batch endpoint
//!      that returns per-item outcomes is invisible to status-based reporting
//!      *and* to any client keying off the status.
//!   3. **Background work.** A task that returns an error dies quietly; a task
//!      that panics takes its loop down for the life of the process and reports
//!      nothing at all.
//!
//! Everything goes through [`report`] rather than a `capture_message` at each
//! site, because a list of call sites drifts the moment someone adds another.
//! Two properties come from having one funnel:
//!
//!   * **Deduplication.** A failure that reaches two paths — logged by the
//!     helper *and* surfaced by the caller — raises one issue, not two.
//!   * **A stable fingerprint.** Grouping is by component and operation, not by
//!     the message, so a hundred variations of "connection refused" stay one
//!     issue instead of a hundred.

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use sentry::protocol::Value;

use super::scrub::sanitize_text;

/// How long an identical failure stays deduplicated. Long enough to collapse a
/// retry storm or a fan-out round into one issue, short enough that a problem
/// still recurring twenty minutes later says so.
const DEDUP_TTL: Duration = Duration::from_secs(300);
/// Cap on distinct fingerprints held. Bounded so a pathological caller cannot
/// grow this without limit; overflow simply means less deduplication.
const DEDUP_MAX: usize = 512;

/// Where a failure happened, and what was being attempted.
///
/// These two form the Sentry fingerprint, so choose them the way you would
/// choose an issue title: `component` is the subsystem ("whatsapp", "metrics",
/// "attendance_sweep") and `operation` is the specific step ("send_message",
/// "widget_query"). Neither may contain a value — an id in the fingerprint is
/// one issue per record, forever.
pub struct Failure<'a> {
    pub component: &'a str,
    pub operation: &'a str,
    /// Structured context. Ids and counts, never personal data — it is scrubbed
    /// on the way out regardless, but do not rely on that.
    pub context: Vec<(&'a str, Value)>,
}

impl<'a> Failure<'a> {
    pub fn new(component: &'a str, operation: &'a str) -> Self {
        Self {
            component,
            operation,
            context: Vec::new(),
        }
    }

    pub fn with(mut self, key: &'a str, value: impl Into<Value>) -> Self {
        self.context.push((key, value.into()));
        self
    }
}

/// Report a handled failure.
///
/// Safe to call when Sentry is not configured — it is then only a `tracing`
/// line, exactly as before. Safe to call from anywhere, including a background
/// task: the event carries no request scope it did not earn.
pub fn report(failure: Failure<'_>, error: &dyn Display) {
    let message = sanitize_text(&error.to_string());

    // Always log, whether or not Sentry is on. An operator reading journalctl
    // must see the same failures the issue stream does.
    tracing::warn!(
        component = %failure.component,
        operation = %failure.operation,
        error = %message,
        "handled failure"
    );

    if !should_report(failure.component, failure.operation, &message) {
        return;
    }

    sentry::with_scope(
        |scope| {
            scope.set_tag("component", failure.component);
            scope.set_tag("operation", failure.operation);
            // Grouping by component+operation, never by the message: a hundred
            // spellings of "connection refused" are one problem.
            scope.set_fingerprint(Some(
                ["handled", failure.component, failure.operation].as_slice(),
            ));
            for (k, v) in &failure.context {
                scope.set_extra(k, v.clone());
            }
        },
        || {
            sentry::capture_message(
                &format!(
                    "{}: {} failed — {message}",
                    failure.component, failure.operation
                ),
                sentry::Level::Error,
            );
        },
    );
}

/// Report a **4xx that this system caused itself**.
///
/// The blanket "do not report 4xx" rule is right for validation, auth and a
/// genuinely missing row — that is ordinary API traffic and reporting it drowns
/// the issue stream. It is too blunt for the cases where a 4xx means *our* data
/// is wrong rather than the caller's: a configuration row that should exist and
/// does not, a reference left dangling by a partial migration, a credential that
/// expired without anyone noticing. Those are faults, and they are invisible
/// precisely because they look like a client error from the outside.
///
/// Use it only where the system is at fault. A merchant asking for an order that
/// was deleted is not a fault; a branch with no `code` is.
pub fn report_data_fault(component: &str, operation: &str, detail: &dyn Display) {
    let mut failure = Failure::new(component, operation);
    failure
        .context
        .push(("fault_class", Value::from("own_data")));
    report(failure, detail);
}

/// Report the outcome of a fan-out round.
///
/// One event for the round, never one per item: a batch of forty widgets where
/// the database is down is one incident, and forty issues is a pager storm that
/// says nothing extra. An event is raised only when **every** item failed —
/// partial failure is normal (one bad widget on a dashboard) and is already
/// visible per item to the caller.
pub fn report_round(component: &str, operation: &str, total: usize, failed: usize) {
    if total == 0 || failed < total {
        return;
    }
    let failure = Failure::new(component, operation)
        .with("items", Value::from(total as u64))
        .with("failed", Value::from(failed as u64));
    report(failure, &format!("all {total} items in this round failed"));
}

/// Deduplicate by (component, operation, message) within [`DEDUP_TTL`].
fn should_report(component: &str, operation: &str, message: &str) -> bool {
    static SEEN: OnceLock<Arc<Mutex<HashMap<String, Instant>>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Arc::new(Mutex::new(HashMap::new())));
    let key = format!("{component}|{operation}|{message}");
    let now = Instant::now();

    // A poisoned lock must not stop reporting — the worst case of proceeding is
    // a duplicate issue, and the worst case of returning early is silence.
    let mut map = match seen.lock() {
        Ok(m) => m,
        Err(poisoned) => poisoned.into_inner(),
    };
    map.retain(|_, seen_at| now.duration_since(*seen_at) < DEDUP_TTL);
    if map.len() >= DEDUP_MAX {
        map.clear();
    }
    match map.get(&key) {
        Some(_) => false,
        None => {
            map.insert(key, now);
            true
        }
    }
}

/// Run one tick of a background job, guarded.
///
/// Two things this fixes, both of which were silent before:
///
///   * **A returned error** was logged and forgotten. It is now reported at the
///     **job boundary** — one place, with the job name as the fingerprint —
///     rather than needing a capture call inside every step.
///   * **A panic** killed the task. `tokio` catches it into a `JoinHandle`
///     nobody holds, so the loop simply stopped ticking for the life of the
///     process, with no event and no log line. It is now caught, reported, and
///     the loop continues.
///
/// The job also gets its **own hub**, so its events carry the job's scope rather
/// than whatever request context happened to be bound to the worker thread the
/// task landed on. Without that, a nightly sweep's failure can arrive attributed
/// to an unrelated merchant's HTTP request.
pub async fn guarded_tick<F, Fut, E>(job: &'static str, tick: F) -> bool
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), E>>,
    E: Display,
{
    use futures::FutureExt;

    // Derived from the CURRENT hub so the client is inherited (a task spawned
    // from `main` sees the main hub), then the scope is cleared outright:
    // `new_from_top` copies the top scope, which on a worker thread that just
    // served a request is that request's context. Anything it left behind is
    // not ours, and an event attributed to an unrelated merchant is worse than
    // no event.
    let hub = std::sync::Arc::new(sentry::Hub::new_from_top(sentry::Hub::current()));
    hub.configure_scope(|scope| {
        scope.clear();
        scope.set_tag("job", job);
    });

    // `AssertUnwindSafe` is sound here because nothing observable is shared
    // across the boundary: on a panic the future is dropped and the next tick
    // starts from scratch.
    let outcome = std::panic::AssertUnwindSafe(tick()).catch_unwind().await;

    sentry::Hub::run(hub, || match outcome {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            report(Failure::new("background", job), &e);
            false
        }
        Err(panic) => {
            let detail = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic with a non-string payload".to_string());
            report(
                Failure::new("background", job).with("panicked", Value::from(true)),
                &format!("panicked: {detail}"),
            );
            false
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn with_client<F: FnOnce()>(f: F) -> Vec<sentry::protocol::Event<'static>> {
        let transport = sentry::test::TestTransport::new();
        let options = sentry::ClientOptions::new()
            .dsn("https://public@example.invalid/1")
            .transport(transport.clone())
            .before_send(super::super::scrub::scrub_event);
        let hub = Arc::new(sentry::Hub::new(
            Some(Arc::new(options.into())),
            Default::default(),
        ));
        sentry::Hub::run(hub, f);
        transport.fetch_and_clear_events()
    }

    /// A unique component per test, so the process-wide dedup cache cannot make
    /// one test's report suppress another's.
    fn unique(prefix: &str) -> String {
        format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
    }

    #[test]
    fn a_handled_failure_becomes_an_event_with_a_stable_fingerprint() {
        let component = unique("whatsapp");
        let events = with_client(|| {
            report(
                Failure::new(&component, "send_message").with("attempt", Value::from(2u64)),
                &"gateway returned 502",
            );
        });
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.tags["component"], component);
        assert_eq!(e.tags["operation"], "send_message");
        assert_eq!(e.extra["attempt"], 2);
        // Fingerprint carries no value, so this is one issue rather than one
        // per message variation.
        let fp = e.fingerprint.as_ref();
        assert!(fp.contains(&"handled".into()));
        assert!(!fp.iter().any(|f| f.contains("502")));
    }

    #[test]
    fn the_same_failure_reaching_two_paths_raises_one_issue() {
        let component = unique("dedup");
        let events = with_client(|| {
            report(Failure::new(&component, "op"), &"connection refused");
            report(Failure::new(&component, "op"), &"connection refused");
        });
        assert_eq!(events.len(), 1, "the duplicate must be suppressed");
    }

    #[test]
    fn different_operations_are_not_deduplicated_into_one() {
        let component = unique("dedup");
        let events = with_client(|| {
            report(Failure::new(&component, "a"), &"boom");
            report(Failure::new(&component, "b"), &"boom");
        });
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn a_reported_message_is_sanitized_before_it_leaves() {
        let component = unique("scrubbed");
        let events = with_client(|| {
            report(
                Failure::new(&component, "notify"),
                &"could not reach customer_phone=+201000000000",
            );
        });
        let wire = serde_json::to_string(&events[0]).unwrap();
        assert!(!wire.contains("201000000000"), "{wire}");
        // ...and still says what failed.
        assert!(wire.contains("notify"));
    }

    #[test]
    fn a_fan_out_round_raises_one_event_only_when_everything_failed() {
        let all = unique("fanout");
        let events = with_client(|| report_round(&all, "batch", 40, 40));
        assert_eq!(events.len(), 1, "a fully failed round is an incident");
        assert_eq!(events[0].extra["failed"], 40);

        let partial = unique("fanout");
        let events = with_client(|| report_round(&partial, "batch", 40, 39));
        assert!(
            events.is_empty(),
            "partial failure is normal and is already visible per item"
        );

        let empty = unique("fanout");
        let events = with_client(|| report_round(&empty, "batch", 0, 0));
        assert!(events.is_empty());
    }

    #[test]
    fn a_data_fault_is_tagged_so_it_can_be_told_from_a_client_error() {
        let component = unique("branches");
        let events = with_client(|| {
            report_data_fault(&component, "resolve_code", &"branch has no code");
        });
        assert_eq!(events[0].extra["fault_class"], "own_data");
    }

    #[tokio::test]
    async fn a_background_tick_that_returns_an_error_is_reported() {
        // Previously this was a `tracing::warn!` and nothing else — the job
        // could fail every tick for a week in silence.
        let events = with_client(|| {
            futures::executor::block_on(async {
                let ok = guarded_tick("test_sweep_err", || async {
                    Err::<(), &str>("tick failed")
                })
                .await;
                assert!(!ok);
            });
        });
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].tags["component"], "background");
        assert_eq!(events[0].tags["operation"], "test_sweep_err");
    }

    #[tokio::test]
    async fn a_background_tick_that_panics_is_reported_and_the_loop_survives() {
        // The important half: without the guard the panic kills the task and
        // the loop never ticks again, reporting nothing.
        let events = with_client(|| {
            futures::executor::block_on(async {
                let ok = guarded_tick("test_sweep_panic", || async {
                    panic!("index out of bounds");
                    #[allow(unreachable_code)]
                    Ok::<(), &str>(())
                })
                .await;
                assert!(!ok, "a panicking tick must report failure, not unwind out");
            });
        });
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].extra["panicked"], true);
        let wire = serde_json::to_string(&events[0]).unwrap();
        assert!(wire.contains("index out of bounds"), "{wire}");
    }

    #[tokio::test]
    async fn a_successful_tick_reports_nothing() {
        let events = with_client(|| {
            futures::executor::block_on(async {
                assert!(guarded_tick("test_sweep_ok", || async { Ok::<(), &str>(()) }).await);
            });
        });
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn a_job_does_not_inherit_request_breadcrumbs() {
        // The trap this guards: a long-lived task lands on a worker thread that
        // served a request, and its events arrive carrying that request's
        // context — attributed to an unrelated merchant.
        let events = with_client(|| {
            sentry::add_breadcrumb(sentry::Breadcrumb {
                message: Some("POST /orders from another request".into()),
                ..Default::default()
            });
            futures::executor::block_on(async {
                guarded_tick("test_sweep_scope", || async { Err::<(), &str>("boom") }).await;
            });
        });
        let wire = serde_json::to_string(&events[0]).unwrap();
        assert!(
            !wire.contains("from another request"),
            "the job picked up request context: {wire}"
        );
        assert_eq!(events[0].tags["job"], "test_sweep_scope");
    }
}
