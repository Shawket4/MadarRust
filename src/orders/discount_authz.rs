//! Who may put a discount on a sale, and how far (PERMISSIONS phase 6).
//!
//! Three capabilities, each with per-person caps (`madar_authz::Limits`):
//!   * `orders.discount.preset`         — a named preset; `max_percent` / `max_amount`
//!   * `orders.discount.manual_amount`  — an amount typed by hand; `max_amount`
//!   * `orders.discount.manual_percent` — a percentage typed by hand; `max_percent`
//!
//! Percent is in basis points (1250 = 12.5%), amount in minor units, the same
//! vocabulary as every other limit. The POS decides offline with the same
//! crate; the live route refuses what `decide` does not allow, and replay
//! accepts the sale and flags it (the money already moved) unless a manager's
//! approval covers it.

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use uuid::Uuid;

use crate::authz::{Cap, Decision, EffectiveSet, Request, decide};
use crate::errors::AppError;
use crate::orders::handlers::CreateOrderRequest;

pub const KIND_PRESET: &str = "preset";
pub const KIND_MANUAL_AMOUNT: &str = "manual_amount";
pub const KIND_MANUAL_PERCENT: &str = "manual_percent";

/// The discount act a sale asks for: which capability, and its figures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscountAsk {
    pub cap: Cap,
    pub kind: &'static str,
    /// Minor units taken off, when known.
    pub amount_minor: Option<i64>,
    /// Basis points, when the discount is a percentage.
    pub percent_bps: Option<i64>,
}

impl DiscountAsk {
    pub fn request(&self) -> Request {
        let mut r = Request::of(self.cap);
        r.amount = self.amount_minor;
        r.percent = self.percent_bps;
        r
    }

    pub fn decide(&self, eff: &EffectiveSet) -> Decision {
        decide(eff, &self.request())
    }
}

/// A stored or sent percentage → basis points. Accepts both spellings: a
/// fraction (`0.125`) and the legacy 0-100 integer (`12`).
pub fn percent_bps_of(value: Decimal) -> i64 {
    let frac = if value > Decimal::ONE {
        value / Decimal::ONE_HUNDRED
    } else {
        value
    };
    (frac * Decimal::from(10_000))
        .round()
        .to_i64()
        .unwrap_or(0)
        .clamp(0, 10_000)
}

/// What discount act `body` performs, if any. A preset's type and value are
/// read from the `discounts` table (of this org) when it still exists, else
/// from what the till sent. `None` when the sale carries no discount.
pub async fn discount_ask(
    pool: &sqlx::PgPool,
    org_id: Uuid,
    body: &CreateOrderRequest,
) -> Result<Option<DiscountAsk>, AppError> {
    let preset: Option<(String, Decimal)> = match body.discount_id {
        Some(id) => {
            sqlx::query_as("SELECT type::text, value FROM discounts WHERE id = $1 AND org_id = $2")
                .bind(id)
                .bind(org_id)
                .fetch_optional(pool)
                .await?
        }
        None => None,
    };
    Ok(ask_from(body, preset))
}

/// The pure half of [`discount_ask`].
pub fn ask_from(body: &CreateOrderRequest, preset: Option<(String, Decimal)>) -> Option<DiscountAsk> {
    let (dtype, value) = match &preset {
        Some((t, v)) => (Some(t.as_str()), *v),
        None => (
            body.discount_type.as_deref(),
            body.discount_value.unwrap_or(Decimal::ZERO),
        ),
    };
    let amount = body.discount_amount.filter(|a| *a > 0).map(i64::from);
    let is_percent = dtype == Some("percentage");
    let is_fixed = dtype == Some("fixed");
    let has_discount = body.discount_id.is_some()
        || amount.is_some()
        || ((is_percent || is_fixed) && value > Decimal::ZERO);
    if !has_discount {
        return None;
    }
    let percent_bps = body
        .discount_percent_bps
        .map(i64::from)
        .or_else(|| is_percent.then(|| percent_bps_of(value)));
    let fixed_amount = || {
        amount.or_else(|| {
            is_fixed
                .then(|| value.round().to_i64())
                .flatten()
                .filter(|v| *v > 0)
        })
    };
    // An explicit kind wins; otherwise a preset id says preset, and an ad-hoc
    // discount is manual of its type (what an older client's ad-hoc one was).
    let kind = match body.discount_kind.as_deref() {
        Some(KIND_PRESET) => KIND_PRESET,
        Some(KIND_MANUAL_AMOUNT) => KIND_MANUAL_AMOUNT,
        Some(KIND_MANUAL_PERCENT) => KIND_MANUAL_PERCENT,
        _ if body.discount_id.is_some() => KIND_PRESET,
        _ if is_percent => KIND_MANUAL_PERCENT,
        _ => KIND_MANUAL_AMOUNT,
    };
    Some(match kind {
        KIND_PRESET => DiscountAsk {
            cap: Cap::OrdersDiscountPreset,
            kind,
            amount_minor: if is_percent { amount } else { fixed_amount() },
            percent_bps: if is_percent { percent_bps } else { None },
        },
        KIND_MANUAL_PERCENT => DiscountAsk {
            cap: Cap::OrdersDiscountManualPercent,
            kind,
            amount_minor: None,
            percent_bps: percent_bps.or(Some(0)),
        },
        _ => DiscountAsk {
            cap: Cap::OrdersDiscountManualAmount,
            kind,
            amount_minor: fixed_amount().or(Some(0)),
            percent_bps: None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz::{CapSet, Limits};
    use rust_decimal_macros::dec;

    fn body() -> CreateOrderRequest {
        CreateOrderRequest::default()
    }

    #[test]
    fn no_discount_is_no_ask() {
        assert_eq!(ask_from(&body(), None), None);
    }

    #[test]
    fn a_preset_percentage_asks_for_its_percent_and_amount() {
        let mut b = body();
        b.discount_id = Some(Uuid::nil());
        b.discount_amount = Some(150);
        let a = ask_from(&b, Some(("percentage".into(), dec!(0.15)))).unwrap();
        assert_eq!(a.cap, Cap::OrdersDiscountPreset);
        assert_eq!(a.percent_bps, Some(1500));
        assert_eq!(a.amount_minor, Some(150));
    }

    #[test]
    fn an_ad_hoc_discount_is_manual_of_its_type_in_either_spelling() {
        let mut b = body();
        b.discount_type = Some("percentage".into());
        b.discount_value = Some(dec!(12));
        let a = ask_from(&b, None).unwrap();
        assert_eq!((a.cap, a.percent_bps), (Cap::OrdersDiscountManualPercent, Some(1200)));

        let mut b = body();
        b.discount_type = Some("fixed".into());
        b.discount_value = Some(dec!(500));
        let a = ask_from(&b, None).unwrap();
        assert_eq!((a.cap, a.amount_minor), (Cap::OrdersDiscountManualAmount, Some(500)));
    }

    #[test]
    fn over_the_cap_needs_a_manager() {
        let mut eff = EffectiveSet {
            caps: CapSet::from_keys(["orders.discount.manual_amount"]),
            ..Default::default()
        };
        eff.limits.insert(
            Cap::OrdersDiscountManualAmount.id(),
            Limits { max_amount: Some(1000), ..Default::default() },
        );
        let mut b = body();
        b.discount_kind = Some("manual_amount".into());
        b.discount_type = Some("fixed".into());
        b.discount_amount = Some(1000);
        assert_eq!(ask_from(&b, None).unwrap().decide(&eff), Decision::Allow);
        b.discount_amount = Some(1001);
        assert!(matches!(
            ask_from(&b, None).unwrap().decide(&eff),
            Decision::NeedsApproval(_)
        ));
    }
}
