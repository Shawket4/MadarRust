//! The Bundles report (C6, COMBOS_CONTRACT.md §2.6): each combo, and each
//! deal, as its own line. Item sales keep counting a combo's parts as their
//! items (`line_kind <> 'combo'`); only this report reads the header.
//!
//! Revenue is before the order-level discount (the item-sales basis), voided
//! orders are excluded and refunds are netted, exactly as item sales do.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::{
    authz::{Cap, require::require},
    errors::{AppError, AppErrorResponse},
    menu::bases::extract_claims,
};

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BundlesQuery {
    /// Business date, inclusive.
    pub from: NaiveDate,
    /// Business date, inclusive.
    pub to: NaiveDate,
    pub branch_id: Option<Uuid>,
    /// `combo` | `deal`; omitted = both.
    pub kind: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BundlesMixQuery {
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub branch_id: Option<Uuid>,
}

/// One combo or deal over the period.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct BundlesRow {
    /// `combo` | `deal`.
    pub kind: String,
    /// The combo's menu item id, or the deal rule's id.
    pub id: Uuid,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    /// Combo units (Σ header quantity, refunds netted) or deal applications (Σ times).
    pub sold: i64,
    pub orders: i64,
    /// A combo: Σ its parts' line_total + their add-ons. A deal: Σ the
    /// consumed lines' line_total (after the deal).
    pub revenue: i64,
    /// The same lines at their normal prices.
    pub list_value: i64,
    /// `list_value − revenue`.
    pub saving: i64,
    pub cost: i64,
    /// True when any line's cost was unknown (`cost` then counts the known part).
    pub cost_missing: bool,
    /// `(revenue − cost) / revenue` as a fraction string; `null` when revenue is 0.
    pub margin: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema)]
pub struct BundlesTotals {
    pub sold: i64,
    pub revenue: i64,
    pub list_value: i64,
    pub saving: i64,
    pub cost: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct BundlesReport {
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub rows: Vec<BundlesRow>,
    pub totals: BundlesTotals,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct MixPick {
    pub menu_item_id: Option<Uuid>,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub size_label: Option<String>,
    /// Units picked (Σ part quantity, refunds netted).
    pub count: i64,
    pub surcharge_total: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct MixSlot {
    /// `null` for parts whose slot was deleted since.
    pub slot_id: Option<Uuid>,
    pub name: String,
    pub picks: Vec<MixPick>,
}

/// What customers picked in each slot of one combo.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct ComboMix {
    pub combo_id: Uuid,
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub slots: Vec<MixSlot>,
}

#[utoipa::path(
    get,
    path = "/reports/bundles",
    tag = "reports",
    params(BundlesQuery),
    responses((status = 200, description = "Each combo and deal as its own line", body = BundlesReport), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
#[tracing::instrument(skip_all, fields(from = %query.from, to = %query.to))]
pub async fn bundles_report(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<BundlesQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(
        pool.get_ref(),
        &claims,
        Cap::ReportsBundles,
        query.branch_id,
    )
    .await?;
    Err(crate::combos::not_yet())
}

#[utoipa::path(
    get,
    path = "/reports/bundles/combos/{id}/mix",
    tag = "reports",
    params(("id" = Uuid, Path, description = "The combo's menu item id"), BundlesMixQuery),
    responses((status = 200, description = "What customers picked per slot", body = ComboMix), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
#[tracing::instrument(skip_all, fields(combo_id = %*id, from = %query.from, to = %query.to))]
pub async fn combo_mix(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
    query: web::Query<BundlesMixQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(
        pool.get_ref(),
        &claims,
        Cap::ReportsBundles,
        query.branch_id,
    )
    .await?;
    let _ = *id;
    Err(crate::combos::not_yet())
}
