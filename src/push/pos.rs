//! POS pushes: a new online order rings the tills that are NOT watching.
//!
//! The realtime stream (`/realtime/stream`) is the primary path: an open POS
//! hears `delivery.created` and alerts on its own. FCM is only the fallback,
//! for a till that is closed, backgrounded with its stream torn down, or
//! offline. So a device is pushed only when it has no live stream carrying the
//! `delivery` topic at the order's branch — checked when the order commits
//! AND again after a short grace window, so a till that is just reconnecting
//! (and will replay the event through `Last-Event-ID`) does not get both.
//!
//! Who: every live `push_devices` row with `app = 'pos'` in the order's org
//! whose person holds `delivery.orders.manage` (what accepting or rejecting
//! the order requires) AT that branch, resolved through the architecture E
//! model (`authz::require::effective` at the branch), and whose role is one
//! the live banner rings for (the core's `role_wants_alert`: not a waiter, not
//! the kitchen). A device row is matched to a stream by install id
//! (`X-Madar-Device`, sent by both the registration and the stream); a row
//! without one cannot be matched, so it is always pushed.
//!
//! The words are the server's own (the client holds no policy), in the
//! device's registered locale. Everything runs after the order commits, in a
//! spawned task; nothing here can slow down or fail an order.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::authz::Cap;
use crate::delivery::staff::DeliveryOrder;
use crate::realtime::hub::BranchEventHub;

/// `push_devices.app` of the POS (the client's `madar_core::push::APP`).
pub const APP: &str = "pos";

/// The Android notification channel: the one the POS already creates for its
/// live alerts (`apps/madar/lib/app/notifications.dart`, high importance, with
/// sound), so a pushed order is heads-up exactly like a live one.
pub const ANDROID_CHANNEL: &str = "madar_realtime";

/// `data.kind` of a new online order push.
pub const KIND_ONLINE_ORDER: &str = "online_order";

/// What accepting or rejecting an online order requires
/// (`delivery_orders:update` on `POST /delivery-orders/{id}/status`).
pub const CAP: Cap = Cap::DeliveryOrdersManage;

/// Roles whose POS never alerts on `delivery.created` (the core's
/// `role_wants_alert`: a waiter's new work is `ticket.ready` and bookings, the
/// kitchen's is `kitchen.fired`). The push follows the live banner.
const QUIET_ROLES: &[&str] = &["waiter", "kitchen"];

/// Default grace window before the second presence check.
const DEFAULT_GRACE_MS: u64 = 3_000;
static GRACE_MS: AtomicU64 = AtomicU64::new(DEFAULT_GRACE_MS);

/// Tests: shorten the grace window. Harmless anywhere (it only delays or
/// hastens the fallback), but nothing outside the tests calls it.
#[doc(hidden)]
pub fn set_grace(d: Duration) {
    GRACE_MS.store(d.as_millis() as u64, Ordering::Relaxed);
}

fn grace() -> Duration {
    Duration::from_millis(GRACE_MS.load(Ordering::Relaxed))
}

/// (key, English, Arabic). The title and channel names are the POS core's own
/// words (`madar-core/src/i18n.rs`: `notif.new_delivery`, `delivery.<channel>`)
/// so a push reads exactly like the live banner it stands in for; the body
/// line is the server's.
pub const WORDS: &[(&str, &str, &str)] = &[
    ("notif.new_delivery", "New delivery order", "طلب توصيل جديد"),
    ("delivery.in_mall", "In-Mall", "داخل المول"),
    ("delivery.outside", "Outside", "خارجي"),
    ("delivery.umbrella", "Umbrella", "المظلات"),
    ("delivery.pickup", "Pickup", "استلام"),
    (
        "pos.n_new_delivery",
        "{ref} · {channel} · {amount}",
        "{ref} · {channel} · {amount}",
    ),
];

/// The live banner's tag for this order (the core's
/// `alert_tag("delivery.created", id)`): a push and the SSE event, replayed or
/// live, replace each other instead of showing twice.
pub fn tag(order_id: Uuid) -> String {
    format!("delivery.created:{order_id}")
}

/// The title and body of a new-order push, in one language: the core's
/// "New delivery order", then the order's reference, its channel and the
/// server's own total (piastres), formatted the way every push formats money.
pub fn new_order_words(reference: &str, channel: &str, total: i32, ar: bool) -> (String, String) {
    let title = super::word("notif.new_delivery", ar)
        .unwrap_or_default()
        .to_string();
    let channel_word = super::word(&format!("delivery.{channel}"), ar).unwrap_or(channel);
    let body = super::render(
        "pos.n_new_delivery",
        &json!({ "ref": reference, "channel": channel_word, "amount": total }),
        ar,
    )
    .unwrap_or_default();
    (title, body)
}

/// The FCM message for one device.
pub fn new_order_message(token: &str, order: &DeliveryOrder, ar: bool) -> Value {
    // Every online order gets a `D-…` reference at intake; the id is only a
    // fallback that is never expected to show.
    let reference = order
        .delivery_ref
        .clone()
        .unwrap_or_else(|| order.id.simple().to_string()[..8].to_uppercase());
    let (title, body) = new_order_words(&reference, &order.channel, order.total, ar);
    let tag = tag(order.id);
    json!({ "message": {
        "token": token,
        "notification": { "title": title, "body": body },
        // Strings only (FCM's rule). title/body repeat here for a client that
        // draws its own banner from a data message.
        "data": {
            "kind": KIND_ONLINE_ORDER,
            "order_id": order.id.to_string(),
            "tag": tag,
            "title": title,
            "body": body,
        },
        "android": {
            "priority": "high",
            "notification": {
                "channel_id": ANDROID_CHANNEL,
                "tag": tag,
                "sound": "default",
            },
        },
        "apns": {
            "headers": {
                "apns-priority": "10",
                "apns-push-type": "alert",
                // APNs caps this at 64 bytes; "delivery.created:" + a uuid is 53.
                "apns-collapse-id": tag,
            },
            "payload": { "aps": { "sound": "default" } },
        },
    }})
}

/// A live POS device registration.
#[derive(sqlx::FromRow)]
struct Device {
    user_id: Uuid,
    token: String,
    locale: String,
    device_id: Option<Uuid>,
}

/// The org's live POS devices whose person may accept an order at `branch`.
async fn recipients(pool: &PgPool, org_id: Uuid, branch: Uuid) -> Vec<Device> {
    let rows: Vec<Device> = match sqlx::query_as(
        "SELECT d.user_id, d.token, d.locale, d.device_id \
           FROM push_devices d JOIN users u ON u.id = d.user_id \
          WHERE d.org_id = $1 AND u.org_id = $1 AND d.app = $2 AND d.revoked_at IS NULL \
            AND u.is_active AND u.deleted_at IS NULL AND NOT (u.role::text = ANY($3))",
    )
    .bind(org_id)
    .bind(APP)
    .bind(QUIET_ROLES)
    .fetch_all(pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "POS push: could not read devices");
            return vec![];
        }
    };
    let mut allowed: Vec<(Uuid, bool)> = Vec::new();
    let mut out = Vec::new();
    for d in rows {
        let ok = match allowed.iter().find(|(u, _)| *u == d.user_id) {
            Some((_, ok)) => *ok,
            None => {
                // Fails closed: an authz error sends nothing to that person.
                let ok = crate::authz::require::effective(pool, d.user_id, Some(branch))
                    .await
                    .map(|e| e.can(CAP))
                    .unwrap_or(false);
                allowed.push((d.user_id, ok));
                ok
            }
        };
        if ok {
            out.push(d);
        }
    }
    out
}

/// A new online order is waiting for the counter: push it to every POS device
/// that may accept it and is not watching the branch's live stream. Returns at
/// once; the work runs in the background after the order has committed.
pub fn new_online_order(pool: &PgPool, hub: &BranchEventHub, order: &DeliveryOrder) {
    let Some(t) = super::transport() else { return };
    let (pool, hub, order) = (pool.clone(), hub.clone(), order.clone());
    tokio::spawn(async move {
        let branch = order.branch_id;
        let watching = |d: &Device| d.device_id.is_some_and(|id| hub.is_connected(branch, id));
        let mut devices = recipients(&pool, order.org_id, branch).await;
        devices.retain(|d| !watching(d));
        if !devices.is_empty() {
            // A till that is reconnecting right now replays `delivery.created`
            // from its `Last-Event-ID`; give it a moment rather than ring it twice.
            tokio::time::sleep(grace()).await;
            devices.retain(|d| !watching(d));
        }
        let batch = devices
            .into_iter()
            .map(|d| {
                let msg = new_order_message(&d.token, &order, d.locale != "en");
                (d.token, msg)
            })
            .collect();
        super::deliver(&pool, t, batch).await;
        #[cfg(debug_assertions)]
        super::fake::mark_done(&tag(order.id));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_order_reads_like_the_live_banner_in_english() {
        assert_eq!(
            new_order_words("D-ARK-260924-0012", "outside", 24500, false),
            (
                "New delivery order".to_string(),
                "D-ARK-260924-0012 · Outside · 245.00 EGP".to_string()
            )
        );
        assert_eq!(
            new_order_words("D-1", "in_mall", 1005, false).1,
            "D-1 · In-Mall · 10.05 EGP"
        );
        assert_eq!(
            new_order_words("D-1", "umbrella", 99, false).1,
            "D-1 · Umbrella · 0.99 EGP"
        );
        assert_eq!(
            new_order_words("D-1", "pickup", 0, false).1,
            "D-1 · Pickup · 0.00 EGP"
        );
    }

    #[test]
    fn a_new_order_reads_like_the_live_banner_in_arabic() {
        assert_eq!(
            new_order_words("D-ARK-260924-0012", "outside", 24500, true),
            (
                "طلب توصيل جديد".to_string(),
                "D-ARK-260924-0012 · خارجي · 245.00 EGP".to_string()
            )
        );
        assert_eq!(
            new_order_words("D-1", "in_mall", 100, true).1,
            "D-1 · داخل المول · 1.00 EGP"
        );
        assert_eq!(
            new_order_words("D-1", "umbrella", 100, true).1,
            "D-1 · المظلات · 1.00 EGP"
        );
        assert_eq!(
            new_order_words("D-1", "pickup", 100, true).1,
            "D-1 · استلام · 1.00 EGP"
        );
    }

    #[test]
    fn an_unknown_channel_shows_as_itself_rather_than_nothing() {
        assert_eq!(
            new_order_words("D-1", "drone", 100, false).1,
            "D-1 · drone · 1.00 EGP"
        );
    }

    #[test]
    fn the_tag_is_the_live_banners() {
        let id = Uuid::nil();
        assert_eq!(
            tag(id),
            "delivery.created:00000000-0000-0000-0000-000000000000"
        );
        assert!(tag(id).len() <= 64, "apns-collapse-id limit");
    }

    #[test]
    fn every_channel_the_database_allows_has_a_word() {
        for c in ["outside", "in_mall", "umbrella", "pickup"] {
            let key = format!("delivery.{c}");
            assert!(super::super::word(&key, false).is_some(), "{key} en");
            assert!(super::super::word(&key, true).is_some(), "{key} ar");
        }
        for (k, en, ar) in WORDS {
            assert!(!en.is_empty() && !ar.is_empty(), "{k}");
            // The shared table and the POS table never define the same key.
            assert!(
                !crate::push::words::WORDS.iter().any(|(w, ..)| w == k),
                "{k} is defined twice"
            );
        }
    }
}
