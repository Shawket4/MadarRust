//! Saying something to a customer through the card they already carry.
//!
//! WhatsApp costs money per message and arrives in a thread full of other
//! people. A wallet notification is free, lands on the lock screen, and comes
//! from the card the shop gave them. So: the card first, WhatsApp only when
//! there is no card to reach.
//!
//! ## Apple cannot be sent a message
//!
//! There is no "push this text" call. iOS raises a pass notification when a
//! FIELD's value changes and that field's definition carries a `changeMessage`.
//! So a message has to BE a field: we write it onto the pass, push a
//! content-free APNs, the device fetches, and iOS shows the field's new value.
//!
//! Two consequences worth knowing rather than discovering. The message is also
//! printed on the card, not merely announced — so it is dropped on the next
//! update, and removing a field notifies nobody. And the customer can turn pass
//! notifications off per pass, which we are never told about.
//!
//! ## Only Apple tells us it arrived
//!
//! When we push, the device calls our pass web service for the new pass, and we
//! serve that request — see `web_service::latest_pass`. That is a real delivery
//! signal, and it is what lets the WhatsApp fallback be evidence rather than a
//! guess: pushed, and hours later never fetched, means it did not land.
//!
//! Google has no equivalent. A card saved there therefore gets no fallback at
//! all, because the alternatives are messaging everyone twice or messaging
//! nobody, and a customer who has muted their wallet missing a nicety is the
//! smaller harm. That was a deliberate call, not an oversight.

use sqlx::PgPool;
use uuid::Uuid;

use super::MemberRow;
use crate::errors::AppError;

/// How long a pushed notice has to be fetched before we assume it never was.
///
/// Long enough for a phone that spent the evening face down, short enough that
/// a birthday message does not arrive on the 3rd.
const DEFAULT_FALLBACK_HOURS: i64 = 8;

fn fallback_hours() -> i64 {
    std::env::var("LOYALTY_PASS_NOTICE_FALLBACK_HOURS")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|h| (1..=168).contains(h))
        .unwrap_or(DEFAULT_FALLBACK_HOURS)
}

/// Which wallets we can actually reach this member through.
async fn wallets_for(pool: &PgPool, member: &MemberRow) -> (bool, bool) {
    // Apple: a device registered for this pass, which happens only if they
    // added it AND the device called our web service. Both halves matter — a
    // pass sitting in a wallet that never registered cannot be pushed to.
    let apple = if super::apple::is_configured() && super::apns::is_configured() {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM loyalty_pass_devices WHERE customer_id = $1",
        )
        .bind(member.id)
        .fetch_one(pool)
        .await
        .unwrap_or(0)
            > 0
    } else {
        false
    };
    // Google: we know we CREATED an object. Whether anyone saved it is a
    // further question Google will answer, at the cost of a read per member —
    // worth doing if this proves too generous, and not worth it before.
    let google = member.google_object_id.is_some();
    (apple, google)
}

/// Tell a member something, through their card where they have one.
///
/// `line` is what the pass carries — one line, because it is a field on a card.
/// `message` is the WhatsApp we owe them if the card never gets it, which is a
/// different text: it carries the link and the way to stop.
pub async fn announce(
    pool: &PgPool,
    member: &MemberRow,
    line: &str,
    message: &str,
) -> Result<(), AppError> {
    let (apple, google) = wallets_for(pool, member).await;

    if !apple && !google {
        // No card anywhere. This is the whole reason WhatsApp is still here.
        crate::delivery::whatsapp::send_message(
            pool.clone(),
            member.phone.clone(),
            message.to_string(),
        );
        return Ok(());
    }

    let mut reached: Vec<&str> = Vec::new();

    if apple {
        // Write the line onto the pass, then push. The order matters: the
        // device fetches whatever is there when it asks, and asking first would
        // hand it the pass without the message on it.
        sqlx::query(
            "UPDATE loyalty_customers \
                SET pass_notice = $2, pass_notice_at = now(), pass_notice_seen_at = NULL, \
                    pass_notice_fallback = $3, pass_updated_at = now() \
              WHERE id = $1",
        )
        .bind(member.id)
        .bind(line)
        .bind(message)
        .execute(pool)
        .await?;
        super::apple::notify_devices(pool, member).await?;
        reached.push("apple");
    }

    if google {
        match super::google::add_message(member, line).await {
            Ok(()) => reached.push("google"),
            Err(e) => tracing::warn!(
                customer_id = %member.id, error = %e,
                "loyalty: Google would not take the message; the card is unchanged"
            ),
        }
    }

    // Nothing landed after all — Google refused and there was no Apple card.
    if reached.is_empty() {
        crate::delivery::whatsapp::send_message(
            pool.clone(),
            member.phone.clone(),
            message.to_string(),
        );
        return Ok(());
    }

    sqlx::query("UPDATE loyalty_customers SET pass_notice_wallets = $2 WHERE id = $1")
        .bind(member.id)
        .bind(reached.join(","))
        .execute(pool)
        .await?;
    tracing::info!(customer_id = %member.id, wallets = %reached.join(","), "loyalty: notice on the card");
    Ok(())
}

/// Send the WhatsApp we owe anyone whose card never came back for the message.
///
/// Apple only: a Google card gives no delivery signal, so following one up
/// would either double-message everyone or nobody. Clearing the notice is part
/// of the same pass — the field goes off the card on its next build either way,
/// and leaving the row would have us reconsider it every tick forever.
pub async fn sweep_undelivered(pool: &PgPool) -> Result<(), AppError> {
    let stale: Vec<(Uuid, String, String)> = sqlx::query_as(
        "SELECT id, phone, pass_notice_fallback FROM loyalty_customers \
          WHERE pass_notice_at IS NOT NULL \
            AND pass_notice_seen_at IS NULL \
            AND pass_notice_fallback IS NOT NULL \
            AND COALESCE(pass_notice_wallets, '') NOT LIKE '%google%' \
            AND pass_notice_at < now() - make_interval(hours => $1::int) \
          LIMIT 200",
    )
    .bind(fallback_hours() as i32)
    .fetch_all(pool)
    .await?;

    for (id, phone, text) in stale {
        // Clear FIRST, like every other message this system sends: a notice we
        // cleared and failed to send is one missed message; one we sent and
        // failed to clear is a customer messaged every tick.
        let cleared = sqlx::query(
            "UPDATE loyalty_customers \
                SET pass_notice = NULL, pass_notice_at = NULL, pass_notice_fallback = NULL, \
                    pass_notice_wallets = NULL \
              WHERE id = $1 AND pass_notice_at IS NOT NULL",
        )
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
        if cleared == 0 {
            continue;
        }
        crate::delivery::whatsapp::send_message(pool.clone(), phone, text);
        tracing::info!(customer_id = %id, "loyalty: card never fetched the notice — sent WhatsApp");
    }
    Ok(())
}

/// The device came back for the pass, so the message was delivered.
///
/// Called from the pass web service, which is the only place either wallet
/// tells us anything about delivery.
pub async fn mark_seen(pool: &PgPool, member: &MemberRow) {
    if member.pass_notice.is_none() {
        return;
    }
    let _ = sqlx::query(
        "UPDATE loyalty_customers SET pass_notice_seen_at = now() \
          WHERE id = $1 AND pass_notice_seen_at IS NULL",
    )
    .bind(member.id)
    .execute(pool)
    .await;
}
