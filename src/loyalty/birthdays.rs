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
    crate::db::spawn(async move {
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
pub struct Greetable {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub locale: String,
    pub year: i32,
    /// The birthday being greeted, in the org's own calendar.
    pub day: chrono::NaiveDate,
}

/// Members to greet on `on` (default: today in each ORG's timezone), at most 500.
///
/// Today in the ORG's timezone, not the server's. A shop in Cairo greeting its
/// customers on UTC's calendar would send some of them a day early.
///
/// The 29th of February is greeted on the 28th in a common year. Matching it
/// exactly would mean a leap-day customer hears from the shop once every four
/// years, which is not a birthday programme — and the alternative, storing them
/// as the 1st of March, would be us quietly changing when their birthday is.
///
/// The SQL is one plain string literal with no `\` line continuations and no
/// `--` comments: a continuation joins the lines, so a SQL comment used to
/// swallow the rest of the query and the sweep failed on every tick. (Behind
/// that it would have failed anyway: `organizations.timezone` is the
/// `timezone_name` domain, which `AT TIME ZONE` only takes as `text`.)
pub async fn due_greetings(
    pool: &PgPool,
    on: Option<chrono::NaiveDate>,
) -> Result<Vec<Greetable>, AppError> {
    Ok(sqlx::query_as(DUE_SQL).bind(on).fetch_all(pool).await?)
}

const DUE_SQL: &str = "
WITH today AS (
    SELECT o.id AS org_id,
           COALESCE($1::date, (now() AT TIME ZONE o.timezone::text)::date) AS d
      FROM organizations o WHERE o.deleted_at IS NULL
       -- Loyalty is the paid `loyalty` feature (or the Rewards by Madar add-on): a Free org
       -- without it gets no greetings.
       AND (o.plan IS DISTINCT FROM 'free' OR 'loyalty' = ANY(org_active_addons(o.id))))
SELECT c.id, c.org_id, c.name, c.locale,
       EXTRACT(YEAR FROM t.d)::int AS year, t.d AS day
  FROM loyalty_members_v c
  JOIN today t ON t.org_id = c.org_id
  JOIN loyalty_settings s ON s.org_id = c.org_id AND s.branch_id IS NULL
 WHERE c.birth_month IS NOT NULL
   AND c.deleted_at IS NULL
   AND s.enabled AND s.birthday_enabled
   AND (
       (c.birth_month = EXTRACT(MONTH FROM t.d)::smallint
        AND c.birth_day = EXTRACT(DAY FROM t.d)::smallint)
       OR (c.birth_month = 2 AND c.birth_day = 29
           AND EXTRACT(MONTH FROM t.d) = 2 AND EXTRACT(DAY FROM t.d) = 28
           AND NOT (EXTRACT(DAY FROM (date_trunc('month', t.d)
                    + interval '1 month - 1 day')) = 29))
   )
   AND NOT EXISTS (
       SELECT 1 FROM loyalty_birthday_greetings g
        WHERE g.customer_id = c.id
          AND g.year = EXTRACT(YEAR FROM t.d)::int)
 LIMIT 500";

pub async fn run_tick(pool: &PgPool) -> Result<(), AppError> {
    crate::loyalty::gifts::expire_due(pool).await?;
    let due = due_greetings(pool, None).await?;

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

    let kind = settings.birthday_gift_kind();
    let amount = settings.birthday_reward_amount.filter(|_| kind == "amount");
    // Claim the greeting FIRST. A lost race here means someone else is sending
    // it, and doing nothing is the correct response to that.
    let claimed = sqlx::query(
        "INSERT INTO loyalty_birthday_greetings (customer_id, org_id, year, reward_amount) \
         VALUES ($1,$2,$3,$4) ON CONFLICT DO NOTHING",
    )
    .bind(m.id)
    .bind(m.org_id)
    .bind(m.year)
    .bind(amount)
    .execute(pool)
    .await?
    .rows_affected();
    if claimed == 0 {
        return Ok(());
    }

    // The gift, where there is one. Written through the same ledger as every
    // other movement, so it shows up in the member's history with a reason
    // rather than as a balance that changed by itself.
    if let Some(amount) = amount {
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
               FROM loyalty_memberships c WHERE c.id = $1",
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
                crate::loyalty::model::Source::Birthday,
                Some("Birthday gift".to_string()),
                None,
            )
            .await?;
        }
    }

    // The free item (`birthday_gift = 'item'`): granted, never credited. A
    // rerun finds this year's gift already there.
    let until = if kind == "item" {
        crate::loyalty::gifts::grant(
            pool,
            m.org_id,
            m.id,
            m.year,
            m.day,
            settings.birthday_gift_days.unwrap_or(30),
        )
        .await?
    } else {
        None
    };

    // A member who asked the shop to stop messaging them still gets the gift
    // (it is theirs, not marketing: owner, 2026-10-23) but no greeting, on the
    // card or by WhatsApp. Win-back skips such members entirely; a birthday
    // gift is a benefit, not a nudge, so only the message is dropped.
    let opted_out: bool =
        sqlx::query_scalar("SELECT marketing_opt_out FROM loyalty_members_v WHERE id = $1")
            .bind(m.id)
            .fetch_optional(pool)
            .await?
            .unwrap_or(false);
    if opted_out {
        tracing::info!(customer_id = %m.id, "loyalty: birthday gift given, greeting withheld (opted out)");
        return Ok(());
    }

    let text = message_with(&settings, &m.name, &m.locale, until);
    // The card first. A greeting on the lock screen from the shop's own pass is
    // better than a WhatsApp among a hundred others, and it costs nothing —
    // WhatsApp is the fallback for someone with no card to reach.
    let member = crate::loyalty::model::find_by_id(pool, m.id)
        .await?
        .ok_or_else(|| AppError::NotFound("Member vanished mid-greeting".into()))?;
    let ar = m.locale.starts_with("ar");
    let line = match (until, ar) {
        (Some(d), false) => format!(
            "Happy birthday! A free item is waiting until {}",
            crate::loyalty::gifts::day_label(d, false)
        ),
        (Some(d), true) => format!(
            "كل سنة وأنت طيب! منتج مجاني بانتظارك حتى {}",
            crate::loyalty::gifts::day_label(d, true)
        ),
        (None, true) => format!("كل سنة وأنت طيب يا {} 🎂", m.name),
        (None, false) => format!("Happy birthday, {} 🎂", m.name),
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
    message_with(settings, name, locale, None)
}

/// [`message_for`], with the free item's last day when one was granted
/// (`None` in a preview: the item gift then reads "valid for N days").
pub fn message_with(
    settings: &crate::loyalty::settings::LoyaltySettings,
    name: &str,
    locale: &str,
    until: Option<chrono::NaiveDate>,
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
    if settings.birthday_gift_kind() == "item" {
        let days = settings.birthday_gift_days.unwrap_or(30);
        let label = |ar| until.map(|d| crate::loyalty::gifts::day_label(d, ar));
        return match (arabic, label(arabic)) {
            (false, Some(d)) => format!(
                "Happy birthday {name}! 🎉 A free item on us at {program}, valid until {d}."
            ),
            (false, None) => format!(
                "Happy birthday {name}! 🎉 A free item on us at {program}, valid for {days} days."
            ),
            (true, Some(d)) => {
                format!("كل سنة وأنت طيب يا {name}! 🎉 منتج مجاني على حسابنا في {program}، صالح حتى {d}.")
            }
            (true, None) => format!(
                "كل سنة وأنت طيب يا {name}! 🎉 منتج مجاني على حسابنا في {program}، صالح لمدة {days} يوم."
            ),
        };
    }
    let gift = settings
        .birthday_reward_amount
        .filter(|_| settings.birthday_gift_kind() == "amount");
    // `{n} من النقاط` reads right at every number; `{n} نقطة` did not (3-10
    // take the plural). See `wallet::i18n::unit_in_a_sentence`.
    let unit = |a: i32| crate::loyalty::wallet::i18n::unit_in_a_sentence(settings.mode(), a);
    match (arabic, gift) {
        (false, Some(a)) => format!(
            "Happy birthday, {name}! 🎉 We've put {} on your {program} card — \
             come and enjoy something on us.",
            unit(a).en
        ),
        (false, None) => {
            format!("Happy birthday, {name}! 🎉 Everyone at {program} is thinking of you today.")
        }
        (true, Some(a)) => format!(
            "كل سنة وأنت طيب يا {name}! 🎉 أضفنا {} إلى بطاقتك في {program} — \
             تعال واستمتع بشيء على حسابنا.",
            unit(a).ar
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
        // Arabic in the form that agrees with any number (5 أختام was wrong).
        settings.birthday_reward_amount = Some(5);
        let ar = message_for(&settings, "علي", "ar");
        assert!(ar.contains("أضفنا 5 من الأختام"), "{ar}");
    }

    #[test]
    fn a_free_item_programme_promises_the_item_and_its_last_day() {
        let mut settings = s();
        settings.birthday_gift = Some("item".into());
        settings.birthday_reward_amount = Some(50);
        let d = chrono::NaiveDate::from_ymd_opt(2026, 11, 21);
        let en = message_with(&settings, "Ali", "en", d);
        assert_eq!(
            en,
            "Happy birthday Ali! 🎉 A free item on us at Bean Club, valid until 21 Nov."
        );
        let ar = message_with(&settings, "علي", "ar", d);
        assert!(ar.contains("منتج مجاني") && ar.contains("21 نوفمبر"), "{ar}");
        // The preview has no date yet: the window instead.
        assert!(message_for(&settings, "Ali", "en").contains("valid for 30 days"));
        // 'none' greets and promises nothing, whatever amount is stored.
        settings.birthday_gift = Some("none".into());
        assert!(!message_for(&settings, "Ali", "en").contains("points"));
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
