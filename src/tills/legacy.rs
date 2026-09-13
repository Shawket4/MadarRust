//! Wire shapes POS v0.5.1 / v0.6.0 decode (TILLS_CONTRACT §2.6). Mounted until
//! cutover. Every shape here is the new data under the old names, plus the
//! fields their generated models require.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::handlers::{Till, TillReportFigures};

/// Legacy `Shift` = `Till` + `till_id`/`till_name` (always null: the drawer
/// entity is gone).
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct Shift {
    #[serde(flatten)]
    pub till: Till,
    pub till_id: Option<Uuid>,
    pub till_name: Option<String>,
}

impl std::ops::Deref for Shift {
    type Target = Till;
    fn deref(&self) -> &Till {
        &self.till
    }
}

impl From<Till> for Shift {
    fn from(till: Till) -> Self {
        Self { till, till_id: None, till_name: None }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct ShiftPreFill {
    pub has_open_shift: bool,
    pub open_shift: Option<Shift>,
    pub suggested_opening_cash: i32,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct PaginatedShifts {
    pub data: Vec<Shift>,
    pub total: i64,
    pub page: i64,
    pub per_page: i64,
    pub total_pages: i64,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct ShiftReportResponse {
    pub shift: Shift,
    #[serde(flatten)]
    pub figures: TillReportFigures,
}

impl std::ops::Deref for ShiftReportResponse {
    type Target = TillReportFigures;
    fn deref(&self) -> &TillReportFigures {
        &self.figures
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct CloseShiftResponse {
    pub shift: Shift,
}

/// Request of the legacy open (`till_id` is the removed entity and ignored).
#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct OpenShiftRequest {
    #[serde(default)]
    pub id: Option<Uuid>,
    #[serde(default)]
    pub till_id: Option<Uuid>,
    pub opening_cash: i32,
    #[serde(default)]
    pub opening_cash_edited: Option<bool>,
    #[serde(default)]
    pub edit_reason: Option<String>,
    #[serde(default)]
    pub opened_at: Option<DateTime<Utc>>,
}

impl From<OpenShiftRequest> for super::handlers::OpenTillRequest {
    fn from(r: OpenShiftRequest) -> Self {
        Self {
            id: r.id,
            opening_cash: r.opening_cash,
            opening_cash_edited: r.opening_cash_edited,
            edit_reason: r.edit_reason,
            opened_at: r.opened_at,
            device_id: None,
            verification: None,
        }
    }
}

/// The removed drawer entity, synthesized one per branch for old settings screens.
#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct LegacyTill {
    pub id: Uuid,
    pub org_id: Uuid,
    pub branch_id: Uuid,
    pub name: String,
    pub is_default: bool,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub standard_float: Option<i32>,
}

/// `uuid v5(NAMESPACE_OID, "madar-legacy-till:"+branch_id)`.
pub fn synthesized_till_id(branch_id: Uuid) -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_OID, format!("madar-legacy-till:{branch_id}").as_bytes())
}
