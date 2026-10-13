//! Kitchen pushes: a fired round reaches a kitchen screen that is NOT watching.
//!
//! A kitchen screen hears new work three ways, in order (owner, 2026-10-11):
//! the branch LAN (a till on the same Wi-Fi relays it at once, no internet),
//! the realtime stream (`kitchen.fired` on the `kitchen` topic), and this push,
//! the fallback for a screen whose stream is down — closed, backgrounded, or
//! between reconnects. So, as for tills (`push::pos`), a screen is pushed only
//! when it holds no live stream at the round's branch, checked when the round
//! commits and again after the grace window, so a screen that is reconnecting
//! replays the event instead of being rung twice.
//!
//! Who: every live `push_devices` row with `app = 'kds'` whose install
//! (`device_id`) is a live device of the round's branch. A kitchen screen
//! registers under its own principal (`POST /auth/device-session`), so there
//! is no person to check: the device being in the branch is the whole rule.

use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::kitchen::KitchenTicketView;
use crate::realtime::hub::BranchEventHub;

/// `push_devices.app` of Madar Kitchen.
pub const APP: &str = "kds";

/// The Android notification channel the kitchen app creates for new work.
pub const ANDROID_CHANNEL: &str = "kitchen_orders";

/// `data.kind` of a new-round push.
pub const KIND_KITCHEN_FIRED: &str = "kitchen_fired";

/// (key, English, Arabic). The title is the POS core's own word for the live
/// kitchen alert (`notif.new_kitchen`), so a push reads like the banner.
pub const WORDS: &[(&str, &str, &str)] = &[
    ("notif.new_kitchen", "New kitchen order", "طلب مطبخ جديد"),
    ("kitchen.n_round", "{ref}", "{ref}"),
    (
        "kitchen.n_round_table",
        "{ref} · {table}",
        "{ref} · {table}",
    ),
];

/// The live alert's tag for this round: the push and the SSE event replace
/// each other instead of showing twice.
pub fn tag(ticket_id: Uuid) -> String {
    format!("kitchen.fired:{ticket_id}")
}

/// The title and body of a new-round push, in one language.
pub fn fired_words(kitchen_ref: Option<&str>, table: Option<&str>, ar: bool) -> (String, String) {
    let title = super::word("notif.new_kitchen", ar)
        .unwrap_or_default()
        .to_string();
    let reference = kitchen_ref.unwrap_or_default();
    let body = match table.filter(|t| !t.is_empty()) {
        Some(t) => super::render(
            "kitchen.n_round_table",
            &json!({ "ref": reference, "table": t }),
            ar,
        ),
        None => super::render("kitchen.n_round", &json!({ "ref": reference }), ar),
    }
    .unwrap_or_default();
    (title, body)
}

/// The FCM message for one kitchen screen.
pub fn fired_message(token: &str, view: &KitchenTicketView, ar: bool) -> Value {
    let (title, body) = fired_words(view.kitchen_ref.as_deref(), view.table_label.as_deref(), ar);
    let tag = tag(view.id);
    json!({ "message": {
        "token": token,
        "notification": { "title": title, "body": body },
        "data": {
            "kind": KIND_KITCHEN_FIRED,
            "ticket_id": view.id.to_string(),
            "branch_id": view.branch_id.to_string(),
            "tag": tag,
            "title": title,
            "body": body,
        },
        "android": {
            "priority": "high",
            "notification": { "channel_id": ANDROID_CHANNEL, "tag": tag, "sound": "default" },
        },
        "apns": {
            "headers": {
                "apns-priority": "10",
                "apns-push-type": "alert",
                // "kitchen.fired:" + a uuid is 50 bytes, under APNs' 64.
                "apns-collapse-id": tag,
            },
            "payload": { "aps": { "sound": "default" } },
        },
    }})
}

#[derive(sqlx::FromRow)]
struct Screen {
    token: String,
    locale: String,
    device_id: Uuid,
}

/// The branch's live kitchen screens with a push token.
async fn screens(pool: &PgPool, branch: Uuid) -> Vec<Screen> {
    sqlx::query_as(
        "SELECT p.token, p.locale, p.device_id
           FROM push_devices p JOIN devices d ON d.id = p.device_id
          WHERE p.app = $1 AND p.revoked_at IS NULL
            AND d.branch_id = $2 AND d.kind = 'kds' AND d.retired_at IS NULL",
    )
    .bind(APP)
    .bind(branch)
    .fetch_all(pool)
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(error = %e, "kitchen push: could not read screens");
        vec![]
    })
}

/// A round was fired: push it to the branch's kitchen screens that are not
/// watching the live stream. Returns at once; runs after the commit.
pub fn fired(pool: &PgPool, hub: &BranchEventHub, view: &KitchenTicketView) {
    let Some(t) = super::transport() else { return };
    let (pool, hub, view) = (pool.clone(), hub.clone(), view.clone());
    crate::db::spawn(async move {
        let branch = view.branch_id;
        let watching = |s: &Screen| {
            hub.is_connected(branch, s.device_id, crate::realtime::event::Topic::Kitchen)
        };
        let mut targets = screens(&pool, branch).await;
        targets.retain(|s| !watching(s));
        if !targets.is_empty() {
            tokio::time::sleep(super::pos::grace()).await;
            targets.retain(|s| !watching(s));
        }
        tracing::info!(branch = %branch, ticket = %view.id, screens = targets.len(), "kitchen push");
        let batch = targets
            .into_iter()
            .map(|s| {
                let msg = fired_message(&s.token, &view, s.locale != "en");
                (s.token, msg)
            })
            .collect();
        super::deliver(&pool, t, batch).await;
        #[cfg(debug_assertions)]
        super::fake::mark_done(&tag(view.id));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_round_reads_like_the_live_banner() {
        assert_eq!(
            fired_words(Some("K-12"), Some("T4"), false),
            ("New kitchen order".to_string(), "K-12 · T4".to_string())
        );
        assert_eq!(fired_words(Some("K-12"), None, false).1, "K-12");
        assert_eq!(
            fired_words(Some("K-12"), Some(""), true),
            ("طلب مطبخ جديد".to_string(), "K-12".to_string())
        );
    }

    #[test]
    fn the_tag_fits_apns() {
        assert!(tag(Uuid::nil()).len() <= 64);
    }
}
