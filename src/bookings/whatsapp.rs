//! Guest-facing WhatsApp messages for bookings. Bilingual by the booking's
//! locale; every send is best-effort through the delivery gateway helper.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use sqlx::PgPool;
use uuid::Uuid;

use super::model::BookingView;
use crate::delivery::whatsapp::send_message;

/// The guest's manage link, or `None` when `PUBLIC_RESERVATIONS_BASE_URL` is
/// unset (the message goes out without a link — same degrade as delivery).
pub fn manage_url(token: &str) -> Option<String> {
    std::env::var("PUBLIC_RESERVATIONS_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|base| format!("{}/manage/{}", base.trim_end_matches('/'), token))
}

/// "Thu 10 Sep, 19:30" in the branch's zone (Arabic uses the same digits and
/// a localized weekday/month via chrono's English names — kept simple).
pub fn format_when(at: DateTime<Utc>, tz: Tz, locale: &str) -> String {
    let local = at.with_timezone(&tz);
    if locale == "ar" {
        let days = [
            "الاثنين",
            "الثلاثاء",
            "الأربعاء",
            "الخميس",
            "الجمعة",
            "السبت",
            "الأحد",
        ];
        let months = [
            "يناير",
            "فبراير",
            "مارس",
            "أبريل",
            "مايو",
            "يونيو",
            "يوليو",
            "أغسطس",
            "سبتمبر",
            "أكتوبر",
            "نوفمبر",
            "ديسمبر",
        ];
        use chrono::Datelike;
        let d = days[local.weekday().num_days_from_monday() as usize];
        let m = months[(local.month0()) as usize];
        format!("{d} {} {m}، {}", local.day(), local.format("%H:%M"))
    } else {
        local.format("%a %-d %b, %H:%M").to_string()
    }
}

pub enum Kind {
    Confirmed,
    Changed,
    Reminder,
    Cancelled,
}

pub fn build(
    kind: &Kind,
    locale: &str,
    name: &str,
    branch: &str,
    when: &str,
    party: i32,
    url: Option<&str>,
) -> String {
    let link = |en: &str, ar: &str| match url {
        Some(u) if locale == "ar" => format!("\n{ar}: {u}"),
        Some(u) => format!("\n{en}: {u}"),
        None => String::new(),
    };
    match (kind, locale) {
        (Kind::Confirmed, "ar") => format!(
            "أهلاً {name}، تم تأكيد حجزك في {branch} يوم {when} لعدد {party} أشخاص.{}",
            link("Manage your booking", "لتعديل أو إلغاء الحجز")
        ),
        (Kind::Confirmed, _) => format!(
            "Hi {name}, your table for {party} at {branch} is confirmed for {when}.{}",
            link("Manage your booking", "لتعديل أو إلغاء الحجز")
        ),
        (Kind::Changed, "ar") => format!(
            "أهلاً {name}، تم تحديث حجزك في {branch}: {when} لعدد {party} أشخاص.{}",
            link("Manage your booking", "لتعديل أو إلغاء الحجز")
        ),
        (Kind::Changed, _) => format!(
            "Hi {name}, your booking at {branch} was updated: {when} for {party}.{}",
            link("Manage your booking", "لتعديل أو إلغاء الحجز")
        ),
        (Kind::Reminder, "ar") => format!(
            "تذكير يا {name}: حجزك في {branch} الساعة {when} لعدد {party} أشخاص. نراك قريباً!{}",
            link(
                "Running late or can't make it?",
                "تأخرت أو لن تتمكن من الحضور؟"
            )
        ),
        (Kind::Reminder, _) => format!(
            "Reminder: your table for {party} at {branch} is at {when}. See you soon!{}",
            link(
                "Running late or can't make it?",
                "تأخرت أو لن تتمكن من الحضور؟"
            )
        ),
        (Kind::Cancelled, "ar") => {
            format!("أهلاً {name}، تم إلغاء حجزك في {branch} ({when}). نتمنى أن نراك في وقت آخر.")
        }
        (Kind::Cancelled, _) => format!(
            "Hi {name}, your booking at {branch} ({when}) has been cancelled. We hope to see you another time."
        ),
    }
}

/// Send the message for `kind` to the booking's guest. Loads the branch name
/// and zone; never fails the caller (a booking exists whether or not the
/// message goes out — the gateway helper reports failures on its own).
pub async fn notify(pool: &PgPool, view: &BookingView, kind: Kind) {
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT b.name, COALESCE(b.timezone, o.timezone)::text FROM branches b \
         JOIN organizations o ON o.id = b.org_id WHERE b.id = $1",
    )
    .bind(view.branch_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    let Some((branch, tz_name)) = row else { return };
    let tz: Tz = tz_name.parse().unwrap_or(chrono_tz::Africa::Cairo);
    let when = format_when(view.starts_at, tz, &view.locale);
    let url = match kind {
        Kind::Cancelled => None,
        _ => manage_url_for(pool, view.id).await,
    };
    let text = build(
        &kind,
        &view.locale,
        &view.guest_name,
        &branch,
        &when,
        view.party_size,
        url.as_deref(),
    );
    send_message(pool.clone(), view.guest_phone.clone(), text);
}

async fn manage_url_for(pool: &PgPool, id: Uuid) -> Option<String> {
    let token: Option<String> =
        sqlx::query_scalar("SELECT manage_token FROM bookings WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    token.as_deref().and_then(manage_url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_carry_the_link_only_when_configured() {
        let en = build(
            &Kind::Confirmed,
            "en",
            "Ahmed",
            "Maadi",
            "Thu 10 Sep, 19:30",
            4,
            Some("https://r/x"),
        );
        assert!(en.contains("Ahmed") && en.contains("19:30") && en.contains("https://r/x"));
        let ar = build(&Kind::Reminder, "ar", "أحمد", "المعادي", "19:30", 2, None);
        assert!(ar.contains("أحمد") && !ar.contains("http"));
        let c = build(&Kind::Cancelled, "en", "A", "B", "W", 1, Some("u"));
        assert!(!c.contains("u:"), "cancellations never link");
    }

    #[test]
    fn when_is_rendered_in_the_branch_zone() {
        let tz: Tz = "Africa/Cairo".parse().unwrap();
        // Egypt observes summer time (UTC+3) in September.
        let at = Utc.with_ymd_and_hms(2026, 9, 10, 16, 30, 0).unwrap(); // 19:30 Cairo
        assert_eq!(format_when(at, tz, "en"), "Thu 10 Sep, 19:30");
        assert!(format_when(at, tz, "ar").contains("19:30"));
    }

    use chrono::TimeZone;
}
