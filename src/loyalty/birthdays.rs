//! The birthday greeting — one background task spawned once from `main`,
//! mirroring `staff::jobs` and `bookings::jobs`.
//!
//! Every tick it finds members whose birthday is today in their own
//! organisation's timezone, sends them a WhatsApp greeting, and — only where the
//! shop configured one — puts a gift on their balance.
//!
//! ## Why the greeting is recorded before it is sent
//! There is no unsending a WhatsApp. The tick runs every few hours and would run
//! on every instance if this were ever scaled out, so "did we already greet
//! this person" cannot live in the job's memory. It is a row, written first,
//! under a primary key of `(customer_id, year)` — a second attempt loses the
//! insert and returns before the message goes anywhere. A greeting we recorded
//! but failed to send is a customer who misses one message; a greeting we sent
//! but failed to record is a customer messaged twice a day until midnight.
//!
//! Runs on the OWNER pool, which bypasses RLS — the sanctioned path for
//! cross-tenant background work. Every query is keyed by `org_id` regardless.

use std::time::Duration;

use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;
use crate::loyalty::earn::Mode;

/// Default cadence. Hours rather than minutes: a birthday is a day, not a
/// moment, and a tighter loop only means more scans finding nothing.
const DEFAULT_TICK_SECS: u64 = 3600 * 4;

/// Spawn the sweep. No-op when `LOYALTY_BIRTHDAY_SWEEP_ENABLED` is falsy.
pub fn spawn(pool: PgPool) {
    let disabled = std::env::var("LOYALTY_BIRTHDAY_SWEEP_ENABLED")
        .map(|v| matches!(v.as_str(), "0" | "false" | "no" | "off"))
        .unwrap_or(false);
    if disabled {
        tracing::info!("Loyalty birthday sweep disabled (LOYALTY_BIRTHDAY_SWEEP_ENABLED)");
        return;
    }
    let secs = std::env::var("LOYALTY_BIRTHDAY_SWEEP_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_TICK_SECS)
        .max(60);

    tracing::info!("Loyalty birthday sweep started ({secs}s tick)");
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(secs));
        loop {
            ticker.tick().await;
            crate::observability::report::guarded_tick("loyalty_birthdays", || run_tick(&pool))
                .await;
        }
    });
}

/// One member with a birthday today, and the programme that greets them.
#[derive(sqlx::FromRow)]
struct Greetable {
    id: Uuid,
    org_id: Uuid,
    name: String,
    locale: String,
    year: i32,
}

async fn run_tick(pool: &PgPool) -> Result<(), AppError> {
    // Today in the ORG's timezone, not the server's. A shop in Cairo greeting
    // its customers on UTC's calendar would send some of them a day early.
    // Today in the ORG's timezone, not the server's. A shop in Cairo greeting
    // its customers on UTC's calendar would send some of them a day early.
    //
    // The 29th of February is greeted on the 28th in a common year. Matching it
    // exactly would mean a leap-day customer hears from the shop once every
    // four years, which is not a birthday programme — and the alternative,
    // storing them as the 1st of March, would be us quietly changing when their
    // birthday is.
    let due: Vec<Greetable> = sqlx::query_as(
        "WITH today AS ( \
             SELECT o.id AS org_id, \
                    (now() AT TIME ZONE o.timezone)::date AS d \
               FROM organizations o WHERE o.deleted_at IS NULL) \
         SELECT c.id, c.org_id, c.name, c.locale, \
                EXTRACT(YEAR FROM t.d)::int AS year \
           FROM loyalty_customers c \
           JOIN today t ON t.org_id = c.org_id \
           JOIN loyalty_settings s ON s.org_id = c.org_id AND s.branch_id IS NULL \
          WHERE c.birth_month IS NOT NULL \
            AND s.enabled AND s.birthday_enabled \
            AND ( \
                (c.birth_month = EXTRACT(MONTH FROM t.d)::smallint \
                 AND c.birth_day = EXTRACT(DAY FROM t.d)::smallint) \
                -- A leap-day birthday, in a year that has no leap day. \
                OR (c.birth_month = 2 AND c.birth_day = 29 \
                    AND EXTRACT(MONTH FROM t.d) = 2 AND EXTRACT(DAY FROM t.d) = 28 \
                    AND NOT (EXTRACT(DAY FROM (date_trunc('month', t.d) \
                             + interval '1 month - 1 day')) = 29)) \
            ) \
            AND NOT EXISTS ( \
                SELECT 1 FROM loyalty_birthday_greetings g \
                 WHERE g.customer_id = c.id \
                   AND g.year = EXTRACT(YEAR FROM t.d)::int) \
          LIMIT 500",
    )
    .fetch_all(pool)
    .await?;

    for m in due {
        if let Err(e) = greet(pool, &m).await {
            // One member's failure is not the sweep's. The next tick retries
            // them, because the greeting row is only written on success.
            tracing::warn!(customer_id = %m.id, error = %e, "loyalty: birthday greeting failed");
        }
    }
    Ok(())
}

async fn greet(pool: &PgPool, m: &Greetable) -> Result<(), AppError> {
    let settings = crate::loyalty::settings::load_scope(pool, m.org_id, None)
        .await?
        .ok_or_else(|| AppError::NotFound("No programme for that org".into()))?;

    // Claim the greeting FIRST. A lost race here means someone else is sending
    // it, and doing nothing is the correct response to that.
    let claimed = sqlx::query(
        "INSERT INTO loyalty_birthday_greetings (customer_id, org_id, year, reward_amount) \
         VALUES ($1,$2,$3,$4) ON CONFLICT DO NOTHING",
    )
    .bind(m.id)
    .bind(m.org_id)
    .bind(m.year)
    .bind(settings.birthday_reward_amount)
    .execute(pool)
    .await?
    .rows_affected();
    if claimed == 0 {
        return Ok(());
    }

    // The gift, where there is one. Written through the same ledger as every
    // other movement, so it shows up in the member's history with a reason
    // rather than as a balance that changed by itself.
    if let Some(amount) = settings.birthday_reward_amount {
        let member = crate::loyalty::model::find_by_id(pool, m.id)
            .await?
            .ok_or_else(|| AppError::NotFound("Member vanished mid-greeting".into()))?;
        let mode = settings.mode();
        // `adjust` needs a branch for the ledger row; the branch they joined at
        // is the honest one, and any branch of the org can honour it.
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
                mode,
                amount,
                Some("Birthday gift".to_string()),
                None,
            )
            .await?;
        }
    }

    let text = message_for(&settings, &m.name, &m.locale);
    // The card first. A greeting on the lock screen from the shop's own pass is
    // better than a WhatsApp among a hundred others, and it costs nothing —
    // WhatsApp is the fallback for someone with no card to reach.
    let member = crate::loyalty::model::find_by_id(pool, m.id)
        .await?
        .ok_or_else(|| AppError::NotFound("Member vanished mid-greeting".into()))?;
    let line = if m.locale.starts_with("ar") {
        format!("كل سنة وأنت طيب يا {} 🎂", m.name)
    } else {
        format!("Happy birthday, {} 🎂", m.name)
    };
    crate::loyalty::wallet::notices::announce(pool, &member, &line, &text).await?;
    tracing::info!(customer_id = %m.id, "loyalty: birthday greeting sent");
    Ok(())
}

/// The greeting, in the member's own language, with the shop's override where
/// there is one.
///
/// `{name}` is the ONLY substitution. A template language here would be a way
/// for an org admin to put arbitrary text into a message that arrives looking
/// like it came from the shop, and one placeholder is all anybody has asked for.
pub fn message_for(
    settings: &crate::loyalty::settings::LoyaltySettings,
    name: &str,
    locale: &str,
) -> String {
    let arabic = locale.starts_with("ar");
    let custom = if arabic {
        settings
            .birthday_message_ar
            .as_deref()
            .or(settings.birthday_message.as_deref())
    } else {
        settings.birthday_message.as_deref()
    };
    if let Some(t) = custom.map(str::trim).filter(|t| !t.is_empty()) {
        return t.replace("{name}", name);
    }

    let program = &settings.program_name;
    let gift = settings.birthday_reward_amount;
    let unit = match settings.mode() {
        Mode::Points => ("points", "نقطة"),
        Mode::Visits => ("stamps", "أختام"),
    };
    match (arabic, gift) {
        (false, Some(a)) => format!(
            "Happy birthday, {name}! 🎉 We've put {a} {} on your {program} card — \
             come and enjoy something on us.",
            unit.0
        ),
        (false, None) => {
            format!("Happy birthday, {name}! 🎉 Everyone at {program} is thinking of you today.")
        }
        (true, Some(a)) => format!(
            "كل سنة وأنت طيب يا {name}! 🎉 أضفنا {a} {} إلى بطاقتك في {program} — \
             تعال واستمتع بشيء على حسابنا.",
            unit.1
        ),
        (true, None) => format!("كل سنة وأنت طيب يا {name}! 🎉 نتمنى لك يوماً سعيداً من {program}."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loyalty::settings::LoyaltySettings;

    fn s() -> LoyaltySettings {
        let mut s = LoyaltySettings::defaults(Uuid::nil(), None);
        s.program_name = "Bean Club".into();
        s
    }

    #[test]
    fn the_greeting_says_what_was_given_only_when_something_was() {
        let mut settings = s();
        // No gift: a greeting, and no promise of one.
        let plain = message_for(&settings, "Ali", "en");
        assert!(plain.contains("Happy birthday, Ali"));
        assert!(plain.contains("Bean Club"));
        assert!(!plain.contains("points"), "{plain}");

        settings.birthday_reward_amount = Some(50);
        let gift = message_for(&settings, "Ali", "en");
        assert!(gift.contains("50 points"), "{gift}");
    }

    #[test]
    fn a_stamp_card_says_stamps_rather_than_points() {
        let mut settings = s();
        settings.mode = "visits".into();
        settings.birthday_reward_amount = Some(1);
        let m = message_for(&settings, "Ali", "en");
        assert!(m.contains("1 stamps"), "{m}");
        assert!(!m.contains("points"), "{m}");
    }

    #[test]
    fn arabic_members_are_greeted_in_arabic() {
        let settings = s();
        let ar = message_for(&settings, "علي", "ar");
        assert!(ar.contains("كل سنة وأنت طيب"), "{ar}");
        assert!(ar.contains("علي"));
    }

    #[test]
    fn a_shops_own_words_win_and_carry_the_name() {
        let mut settings = s();
        settings.birthday_message = Some("Happy birthday {name}, from all of us.".into());
        assert_eq!(
            message_for(&settings, "Ali", "en"),
            "Happy birthday Ali, from all of us."
        );
        // An Arabic reader falls back to the shop's English line rather than to
        // a default in a voice the shop did not choose.
        assert_eq!(
            message_for(&settings, "Ali", "ar"),
            "Happy birthday Ali, from all of us."
        );
        settings.birthday_message_ar = Some("كل سنة وأنت طيب يا {name}".into());
        assert_eq!(
            message_for(&settings, "علي", "ar"),
            "كل سنة وأنت طيب يا علي"
        );

        // Whitespace is not a message.
        settings.birthday_message = Some("   ".into());
        settings.birthday_message_ar = None;
        assert!(message_for(&settings, "Ali", "en").contains("Bean Club"));
    }

    #[test]
    fn only_the_name_is_substituted() {
        let mut settings = s();
        settings.birthday_message = Some("Hi {name}, {program_name} {balance} {{}}".into());
        assert_eq!(
            message_for(&settings, "Ali", "en"),
            "Hi Ali, {program_name} {balance} {{}}",
            "a template language here is a way to send arbitrary text as the shop"
        );
    }
}
