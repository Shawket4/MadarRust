//! Wire shapes POS v0.5.1 / v0.6.0 decode (TILLS_CONTRACT §2.6). Mounted until
//! cutover. Every shape here is the new data under the old names, plus the
//! fields their generated models require.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::handlers::{Till, TillReportFigures};

/// Legacy `Shift` = `Till` + `till_id`/`till_name`: the branch's legacy drawer
/// entity (the one `GET /tills` synthesizes) and its name, exactly as the
/// pre-rename backend reported them. Build it with [`legacy_shift`].
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

/// The id old clients know as the branch's drawer: the archived default entity
/// when one exists, else [`synthesized_till_id`]. Same rule as `GET /tills`.
pub async fn legacy_till_entity_id(pool: &sqlx::PgPool, branch_id: Uuid) -> Uuid {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT e.id FROM archive.till_entities e WHERE e.branch_id = $1 AND e.is_default \
           AND e.deleted_at IS NULL ORDER BY e.created_at LIMIT 1",
    )
    .bind(branch_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .unwrap_or_else(|| synthesized_till_id(branch_id))
}

/// Which joins the pre-rename query behind a response made. The old backend
/// built single-shift reads with the drawer join but no branch join
/// (`branch_name: null`), the branch list with both, and the close /
/// force-close `RETURNING` rows with neither (`till_id`/`till_name: null`).
#[derive(Clone, Copy)]
pub enum LegacyJoins {
    /// `GET /shifts/{id}`, report, current, open.
    Till,
    /// `GET /shifts/branches/{b}`.
    TillAndBranch,
    /// close / force-close.
    None,
}

/// The legacy `Shift` for a till, with exactly the joins the old response had.
pub async fn legacy_shift(
    pool: &sqlx::PgPool,
    mut till: Till,
    joins: LegacyJoins,
) -> Result<Shift, crate::errors::AppError> {
    if !matches!(joins, LegacyJoins::TillAndBranch) {
        till.branch_name = None;
    }
    if matches!(joins, LegacyJoins::None) {
        return Ok(Shift { till, till_id: None, till_name: None });
    }
    let till_id = legacy_till_entity_id(pool, till.branch_id).await;
    Ok(Shift { till, till_id: Some(till_id), till_name: Some("Till 1".into()) })
}

/// `uuid v5(NAMESPACE_OID, "madar-legacy-till:"+branch_id)`.
pub fn synthesized_till_id(branch_id: Uuid) -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_OID, format!("madar-legacy-till:{branch_id}").as_bytes())
}
