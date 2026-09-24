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
use uuid::Uuid;

use crate::errors::AppError;
use crate::orders::handlers::CreateOrderRequest;

// The rule is madar-shared's (`madar_money::discount`), the one copy the till
// asks the same question with. Pinned by its `discount_vectors.json`.
pub use madar_money::discount::{
    DiscountAsk, DiscountFields, KIND_MANUAL_AMOUNT, KIND_MANUAL_PERCENT, KIND_PRESET, ask_from,
    percent_bps_of,
};

impl CreateOrderRequest {
    pub fn discount_fields(&self) -> DiscountFields<'_> {
        DiscountFields {
            has_preset: self.discount_id.is_some(),
            discount_type: self.discount_type.as_deref(),
            discount_value: self.discount_value,
            discount_amount: self.discount_amount,
            discount_kind: self.discount_kind.as_deref(),
            discount_percent_bps: self.discount_percent_bps,
        }
    }
}

/// What discount act `fields` performs, if any. A preset's type and value are
/// read from the `discounts` table (of this org) when it still exists, else
/// from what the till sent. `None` when the sale carries no discount.
///
/// The lookup deliberately does NOT filter on `is_active`: a preset switched
/// off after a bill was rung must still be JUDGED on the figures it really had,
/// not silently demoted to a manual discount with different caps.
pub async fn discount_ask(
    pool: &sqlx::PgPool,
    org_id: Uuid,
    discount_id: Option<Uuid>,
    fields: &DiscountFields<'_>,
) -> Result<Option<DiscountAsk>, AppError> {
    let preset: Option<(String, Decimal)> = match discount_id {
        Some(id) => {
            sqlx::query_as("SELECT type::text, value FROM discounts WHERE id = $1 AND org_id = $2")
                .bind(id)
                .bind(org_id)
                .fetch_optional(pool)
                .await?
        }
        None => None,
    };
    Ok(ask_from(
        fields,
        preset.as_ref().map(|(t, v)| (t.as_str(), *v)),
    ))
}
