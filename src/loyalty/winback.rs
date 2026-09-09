//! "We've missed you" — one background task, spawned once from `main`,
//! mirroring `loyalty::birthdays`.
//!
//! Every tick it finds members who have not been in for a while, sends them one
//! message, and — where the shop configured one — puts something on their
//! balance to come back for.
//!
//! ## What "been in" means
//! The later of their last ledger movement and their last order. Both paths
//! write to the member's own row, so it does not matter how they were
//! identified at the till: a card scanned and a phone number typed resolve to
//! the same customer, and the clock is per member, not per channel.
//!
//! A visit nobody attributed to anyone — walk-in, cash, no card, no phone — is
//! invisible here, and always will be. We can only reset on visits we can see.
//!
//! ## How it stops repeating, without a counter
//! `since` is the member's last visit at the moment they were found dormant. It
//! does not move while they stay away, so it names the dormant SPELL; a visit
//! changes it, and every nudge recorded against the old value belongs to a spell
//! that is over. Nothing has to remember to reset anything.
//!
//! ## Why the row is written before the message
//! Copied from the birthday greeting, for the same reason: there is no unsending
//! a WhatsApp. A row we wrote and failed to send is one missed message; a message
//! we sent and failed to record is a customer nudged again on the next tick.

use std::time::Duration;

use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;

/// Default cadence. Hours, not minutes: "a week" is not a moment, and a tighter
/// loop only means more scans finding the same nobody.
const DEFAULT_TICK_SECS: u64 = 3600 * 6;

/// Dormant for this long earns the first nudge.
const DEFAULT_AFTER_DAYS: i64 = 7;
/// And this long earns the second, which is the last.
const DEFAULT_SECOND_AFTER_DAYS: i64 = 14;
/// Past this, silence. "We've missed you" from a shop somebody has forgotten
/// reads as a list being worked through, which is how a business number gets
/// reported.
const DEFAULT_MAX_DAYS: i64 = 60;
/// Messages per shop per tick.
///
/// The day a shop switches this on, everyone who has ever drifted away is
/// dormant at once. Without a cap that is one WhatsApp burst of the entire
/// lapsed list — a bill, a rate limit, and possibly a blocked number. With one,
/// the backlog drains over days and nobody notices but us.
const DEFAULT_PER_ORG_PER_TICK: i64 = 200;

fn days(var: &str, fallback: i64) -> i64 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|d| (1..=3650).contains(d))
        .unwrap_or(fallback)
}

/// Spawn the sweep. No-op when `LOYALTY_WINBACK_SWEEP_ENABLED` is falsy.
pub fn spawn(pool: PgPool) {
    let disabled = std::env::var("LOYALTY_WINBACK_SWEEP_ENABLED")
        .map(|v| matches!(v.as_str(), "0" | "false" | "no" | "off"))
        .unwrap_or(false);
    if disabled {
        tracing::info!("Loyalty win-back sweep disabled (LOYALTY_WINBACK_SWEEP_ENABLED)");
        return;
    }
    let secs = std::env::var("LOYALTY_WINBACK_SWEEP_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_TICK_SECS)
        .max(60);

    tracing::info!("Loyalty win-back sweep started ({secs}s tick)");
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(secs));
        loop {
            ticker.tick().await;
            crate::observability::report::guarded_tick("loyalty_winback", || run_tick(&pool)).await;
        }
    });
}

/// One member who has been away long enough, and which nudge they are due.
#[derive(sqlx::FromRow)]
struct Lapsed {
    id: Uuid,
    org_id: Uuid,
    name: String,
    phone: String,
    locale: String,
    member_token: String,
    /// Their last attributable visit — the spell's name, and the idempotency key.
    since: chrono::DateTime<chrono::Utc>,
    /// 1 or 2. Never 3.
    seq: i16,
}

async fn run_tick(pool: &PgPool) -> Result<(), AppError> {
    let after = days("LOYALTY_WINBACK_AFTER_DAYS", DEFAULT_AFTER_DAYS);
    let second = days(
        "LOYALTY_WINBACK_SECOND_AFTER_DAYS",
        DEFAULT_SECOND_AFTER_DAYS,
    );
    let max = days("LOYALTY_WINBACK_MAX_DAYS", DEFAULT_MAX_DAYS);
    let cap = days(
        "LOYALTY_WINBACK_MAX_PER_ORG_PER_TICK",
        DEFAULT_PER_ORG_PER_TICK,
    );

    // A member with no visit at all has not been missed — they were never had.
    // Someone who signed up and never came back is a different message from
    // this one, and sending them this one would be a lie.
    let due: Vec<Lapsed> = sqlx::query_as(
        "WITH seen AS ( \
             SELECT c.id, c.org_id, c.name, c.phone, c.locale, c.member_token, \
                    GREATEST( \
                      (SELECT max(t.created_at) FROM loyalty_transactions t \
                        WHERE t.customer_id = c.id), \
                      (SELECT max(o.created_at) FROM orders o \
                        WHERE o.loyalty_customer_id = c.id) \
                    ) AS since \
               FROM loyalty_customers c \
               JOIN loyalty_settings s \
                 ON s.org_id = c.org_id AND s.branch_id IS NULL \
              WHERE s.enabled AND s.winback_enabled \
                AND NOT c.marketing_opt_out \
                AND c.deleted_at IS NULL), \
         ranked AS ( \
             SELECT seen.*, \
                    CASE WHEN now() - since >= make_interval(days => $2::int) \
                         THEN 2 ELSE 1 END::smallint AS seq, \
                    row_number() OVER (PARTITION BY org_id ORDER BY since) AS rn \
               FROM seen \
              WHERE since IS NOT NULL \
                AND now() - since >= make_interval(days => $1::int) \
                AND now() - since <= make_interval(days => $3::int)) \
         SELECT id, org_id, name, phone, locale, member_token, since, seq \
           FROM ranked r \
          WHERE rn <= $4 \
            AND NOT EXISTS ( \
                SELECT 1 FROM loyalty_winbacks w \
                 WHERE w.customer_id = r.id AND w.since = r.since AND w.seq = r.seq)",
    )
    .bind(after as i32)
    .bind(second as i32)
    .bind(max as i32)
    .bind(cap)
    .fetch_all(pool)
    .await?;

    for m in due {
        if let Err(e) = nudge(pool, &m).await {
            // One member's failure is not the sweep's. The next tick retries
            // them, because the row is only kept on a claim that succeeded.
            tracing::warn!(customer_id = %m.id, error = %e, "loyalty: win-back failed");
        }
    }
    Ok(())
}

async fn nudge(pool: &PgPool, m: &Lapsed) -> Result<(), AppError> {
    let settings = crate::loyalty::settings::load_scope(pool, m.org_id, None)
        .await?
        .ok_or_else(|| AppError::NotFound("No programme for that org".into()))?;

    // Claim it FIRST. A lost race means someone else is sending this one, and
    // doing nothing is the correct response to that.
    let claimed = sqlx::query(
        "INSERT INTO loyalty_winbacks (customer_id, org_id, since, seq, reward_amount) \
         VALUES ($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING",
    )
    .bind(m.id)
    .bind(m.org_id)
    .bind(m.since)
    .bind(m.seq)
    .bind(settings.winback_reward_amount)
    .execute(pool)
    .await?
    .rows_affected();
    if claimed == 0 {
        return Ok(());
    }

    // Something to come back for, where the shop offers one. Through the same
    // ledger as every other movement, so "why is my balance up" has an answer.
    if let Some(amount) = settings.winback_reward_amount {
        let member = crate::loyalty::model::find_by_id(pool, m.id)
            .await?
            .ok_or_else(|| AppError::NotFound("Member vanished mid-nudge".into()))?;
        let branch: Option<Uuid> = sqlx::query_scalar(
            "SELECT COALESCE(c.joined_branch_id, \
                    (SELECT id FROM branches WHERE org_id = $2 AND deleted_at IS NULL \
                      ORDER BY created_at LIMIT 1)) \
               FROM loyalty_customers c WHERE c.id = $1",
        )
        .bind(m.id)
        .bind(m.org_id)
        .fetch_one(pool)
        .await?;
        if let Some(branch_id) = branch {
            crate::loyalty::model::adjust(
                pool,
                &member,
                branch_id,
                settings.mode(),
                amount,
                Some("We've missed you".to_string()),
                None,
            )
            .await?;
        }
    }

    let text = message_for(&settings, &m.name, &m.locale, &m.member_token);
    crate::delivery::whatsapp::send_message(pool.clone(), m.phone.clone(), text);
    tracing::info!(customer_id = %m.id, seq = m.seq, "loyalty: win-back sent");
    Ok(())
}

/// The nudge, in the member's own language, with the shop's override where
/// there is one — and always with a way out.
///
/// `{name}` is the ONLY substitution in an override. A template language here
/// would be a way for an org admin to put arbitrary text into a message that
/// arrives looking like it came from the shop.
///
/// The link is not decoration. This is marketing, and marketing a customer
/// cannot stop is what gets a business number blocked; the card page carries
/// the switch. It is appended to a shop's own wording too, for the same reason
/// — an override must not be a way to send an unstoppable message.
pub fn message_for(
    settings: &crate::loyalty::settings::LoyaltySettings,
    name: &str,
    locale: &str,
    member_token: &str,
) -> String {
    let arabic = locale.starts_with("ar");
    let program = settings
        .program_name_ar
        .as_deref()
        .filter(|_| arabic)
        .unwrap_or(&settings.program_name);

    let body = match settings.winback_message.as_deref().map(str::trim) {
        Some(custom) if !custom.is_empty() => custom.replace("{name}", name),
        // Written in each language rather than translated into one.
        _ if arabic => format!(
            "{name}، وحشتنا! بطاقة {program} لسه مستنياك، ورصيدك زي ما هو. \
             نستناك قريب 🤍"
        ),
        _ => format!(
            "{name}, we've missed you! Your {program} card is still here and \
             your balance is exactly where you left it. Come see us soon."
        ),
    };

    let reward = settings.winback_reward_amount.map(|amount| {
        let unit = crate::loyalty::wallet::google::balance_label(settings.mode()).to_lowercase();
        if arabic {
            format!("\n\nوعشان ترجع: ضفنالك {amount} {unit}.")
        } else {
            format!("\n\nAnd to make it easier: we've added {amount} {unit} to your card.")
        }
    });

    let mut out = body;
    if let Some(r) = reward {
        out.push_str(&r);
    }
    if let Some(link) = card_link(member_token) {
        out.push_str(&if arabic {
            format!("\n\nبطاقتك: {link}\n(تقدر توقف الرسائل دي من نفس الصفحة.)")
        } else {
            format!("\n\nYour card: {link}\n(You can turn these off on the same page.)")
        });
    }
    out
}

/// Where the customer's own card lives, when there is a public base to build on.
fn card_link(member_token: &str) -> Option<String> {
    let base = std::env::var("PUBLIC_LOYALTY_BASE_URL").ok()?;
    Some(format!(
        "{}/card/{member_token}",
        base.trim_end_matches('/')
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loyalty::settings::LoyaltySettings;

    fn settings() -> LoyaltySettings {
        let mut s = LoyaltySettings::defaults(Uuid::nil(), None);
        s.program_name = "Rue Rewards".into();
        s.winback_enabled = true;
        s
    }

    /// The sweep's SQL, which is where the whole feature actually lives.
    ///
    /// Seeds one member last seen ten days ago and checks the three things that
    /// matter: they are found, the nudge is recorded once, and a member who has
    /// opted out is not found at all.
    #[sqlx::test]
    async fn a_lapsed_member_is_nudged_once_and_an_opted_out_one_never(pool: PgPool) {
        let org: Uuid = sqlx::query_scalar(
            "INSERT INTO organizations (name, slug) VALUES ('Rue','rue') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let branch: Uuid = sqlx::query_scalar(
            "INSERT INTO branches (org_id, name) VALUES ($1,'Maadi') RETURNING id",
        )
        .bind(org)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO loyalty_settings (org_id, enabled, program_name, mode, \
                 earn_piastres_per_point, default_reward_cost, winback_enabled) \
             VALUES ($1, true, 'Rue Rewards', 'points', 1000, 100, true)",
        )
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();

        let member = async |phone: &str, token: &str, opted_out: bool| -> Uuid {
            let id: Uuid = sqlx::query_scalar(
                "INSERT INTO loyalty_customers (org_id, phone, name, member_token, \
                     marketing_opt_out) \
                 VALUES ($1,$2,'Ali',$3,$4) RETURNING id",
            )
            .bind(org)
            .bind(phone)
            .bind(token)
            .bind(opted_out)
            .fetch_one(&pool)
            .await
            .unwrap();
            // Their last visit: ten days ago, so past the seven-day window and
            // well inside the sixty-day one.
            sqlx::query(
                "INSERT INTO loyalty_transactions (org_id, customer_id, branch_id, kind, \
                     currency, points, created_at) \
                 VALUES ($1,$2,$3,'earn','points',1, now() - interval '10 days')",
            )
            .bind(org)
            .bind(id)
            .bind(branch)
            .execute(&pool)
            .await
            .unwrap();
            id
        };

        let away = member("+201000000001", "tok-away", false).await;
        let quiet = member("+201000000002", "tok-quiet", true).await;

        run_tick(&pool).await.unwrap();
        let sent: Vec<(Uuid, i16)> =
            sqlx::query_as("SELECT customer_id, seq FROM loyalty_winbacks ORDER BY customer_id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(sent.len(), 1, "only the one who did not opt out");
        assert_eq!(sent[0].0, away);
        assert_eq!(sent[0].1, 1, "the first nudge, not the second");
        assert!(
            !sent.iter().any(|(id, _)| *id == quiet),
            "an opt-out is an opt-out"
        );

        // A second tick changes nothing: the spell is the same, and the nudge
        // for it is already recorded.
        run_tick(&pool).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM loyalty_winbacks")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "one nudge per spell, however often we look");
    }

    #[test]
    fn each_language_is_written_rather_than_translated() {
        let en = message_for(&settings(), "Sara", "en", "tok");
        let ar = message_for(&settings(), "سارة", "ar", "tok");
        assert!(en.starts_with("Sara, we've missed you!"));
        assert!(ar.starts_with("سارة، وحشتنا!"));
        assert!(en.contains("Rue Rewards"));
    }

    #[test]
    fn a_shop_that_writes_its_own_still_cannot_send_an_unstoppable_message() {
        // SAFETY: single-threaded test process for this variable.
        unsafe { std::env::set_var("PUBLIC_LOYALTY_BASE_URL", "https://loyalty.example") }
        let mut s = settings();
        s.winback_message = Some("{name}! Two for one this week.".into());
        let out = message_for(&s, "Sara", "en", "tok123");
        assert!(out.starts_with("Sara! Two for one this week."));
        assert!(
            out.contains("https://loyalty.example/card/tok123"),
            "the way out is appended to a shop's own wording too: {out}"
        );
        unsafe { std::env::remove_var("PUBLIC_LOYALTY_BASE_URL") }
    }

    #[test]
    fn a_gift_is_mentioned_only_when_there_is_one() {
        let mut s = settings();
        assert!(!message_for(&s, "Sara", "en", "t").contains("added"));
        s.winback_reward_amount = Some(20);
        assert!(message_for(&s, "Sara", "en", "t").contains("added 20 points"));
    }
}
