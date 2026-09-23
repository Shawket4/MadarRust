//! Staff routes.
//!
//! One scope behind `StaffAuth` (see `staff::principal`), which accepts a
//! Madar user's session (dashboard, POS) or the staff app's staff token — and
//! is the ONLY place a staff token is accepted:
//!
//! - `/staff/*` — the ADMIN surface. Every handler checks an `hr.*` capability
//!   at the right branch (`staff::access`).
//! - `/staff/me/*` — SELF-SERVICE for the staff app's employee. No capability
//!   is checked because the scope is the caller's own rows; the gate is a live
//!   staff session (device, active employee, active org, Dawam on).
//!
//! `/staff/me/...` is registered on the same scope as `/staff/...`; actix matches
//! the more specific literal segment first, so `me` never shadows an
//! `{employee_id}` path — and `me` is not a UUID, so it could not collide anyway.

use actix_web::web;

use crate::staff::dawam::{context, pay, presence, roster};
use crate::staff::principal::StaffAuth;
use crate::staff::{attendance, directory, discipline, payroll, requests, schedules};

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/staff")
            .wrap(StaffAuth)
            // ── Self-service ─────────────────────────────────────
            .route("/me/today", web::get().to(attendance::my_today))
            .route("/me/check-in", web::post().to(attendance::check_in))
            .route("/me/check-out", web::post().to(attendance::check_out))
            .route("/me/attendance", web::get().to(attendance::my_attendance))
            .route("/me/schedule", web::get().to(schedules::my_schedule))
            .route("/me/requests", web::get().to(requests::my_requests))
            .route("/me/requests", web::post().to(requests::create_my_request))
            .route(
                "/me/leave-balances",
                web::get().to(requests::my_leave_balances),
            )
            .route("/me/advances", web::get().to(payroll::my_advances))
            .route("/me/advances", web::post().to(payroll::create_my_advance))
            .route("/me/payslips", web::get().to(payroll::my_payslips))
            // ── Dawam self-service ───────────────────────────────
            .route("/me/context", web::get().to(context::my_context))
            .route(
                "/me/push-token",
                web::put().to(crate::staff::dawam::set_push_token),
            )
            .route("/me/pings", web::post().to(presence::ping))
            .route("/me/coverable", web::get().to(presence::my_coverable))
            .route("/me/cover", web::post().to(presence::open_cover))
            .route("/me/roster", web::get().to(roster::my_roster))
            .route("/me/swaps", web::post().to(roster::ask_swap))
            .route("/me/swaps/{id}", web::patch().to(roster::answer_swap))
            .route("/me/preferences", web::put().to(roster::put_preferences))
            .route("/me/pay/estimate", web::get().to(pay::my_estimate))
            .route("/me/adjustments", web::get().to(pay::my_adjustments))
            .route(
                "/me/expense-advances",
                web::get().to(pay::my_expense_advances),
            )
            .route("/me/notifications", web::get().to(pay::my_notifications))
            .route(
                "/me/notifications/read",
                web::post().to(pay::read_notifications),
            )
            // ── Dawam management ─────────────────────────────────
            .route("/flags", web::get().to(presence::list_flags))
            .route("/flags/{id}", web::patch().to(presence::resolve_flag))
            .route("/attendance/punch", web::post().to(presence::punch_for))
            .route(
                "/attendance/till-punch",
                web::post().to(presence::till_punch),
            )
            .route(
                "/attendance/{id}/cover",
                web::patch().to(presence::decide_cover),
            )
            .route(
                "/attendance/{id}/overtime",
                web::patch().to(presence::decide_overtime),
            )
            .route(
                "/employees/{employee_id}/device",
                web::delete().to(presence::revoke_device),
            )
            .route("/roster", web::get().to(roster::roster))
            .route("/roster/publish", web::post().to(roster::publish))
            .route("/roster/suggestions", web::get().to(roster::suggestions))
            .route("/roster/coverage", web::get().to(roster::get_coverage))
            .route("/roster/coverage", web::put().to(roster::put_coverage))
            .route("/roster/fairness", web::get().to(roster::fairness))
            .route(
                "/reports/labour-vs-sales",
                web::get().to(super::dawam::reports::labour_vs_sales),
            )
            .route(
                "/reports/payroll-history",
                web::get().to(super::dawam::reports::payroll_history),
            )
            .route(
                "/reports/advances",
                web::get().to(super::dawam::reports::advances),
            )
            .route(
                "/roster/suggestions/decide",
                web::post().to(roster::decide_suggestion),
            )
            .route("/open-shifts", web::post().to(roster::post_open_shift))
            .route("/open-shifts", web::get().to(roster::list_open_shifts))
            .route(
                "/open-shifts/{id}/claim",
                web::post().to(roster::claim_open_shift),
            )
            .route(
                "/open-shifts/{id}/decision",
                web::patch().to(roster::decide_claim),
            )
            .route("/swaps", web::get().to(roster::list_swaps))
            .route("/swaps/{id}/decision", web::patch().to(roster::decide_swap))
            .route("/holidays/{date}", web::put().to(roster::decide_holiday))
            .route("/payroll/current", web::get().to(pay::current))
            .route(
                "/payroll/periods/{id}/payslips/{employee_id}/paid",
                web::patch().to(pay::mark_paid),
            )
            .route("/adjustments", web::get().to(pay::list_adjustments))
            .route("/adjustments", web::post().to(pay::create_adjustment))
            .route(
                "/adjustments/{kind}/{id}/decision",
                web::patch().to(pay::decide_adjustment),
            )
            .route(
                "/adjustments/{kind}/{id}/stop",
                web::post().to(pay::stop_adjustment),
            )
            .route(
                "/advances/{id}/review",
                web::patch().to(pay::review_advance),
            )
            .route(
                "/expense-advances",
                web::get().to(pay::list_expense_advances),
            )
            .route(
                "/expense-advances",
                web::post().to(pay::log_expense_advance),
            )
            // ── Directory ────────────────────────────────────────
            .route("/departments", web::get().to(directory::list_departments))
            .route("/departments", web::post().to(directory::create_department))
            .route(
                "/departments/{id}",
                web::patch().to(directory::update_department),
            )
            .route(
                "/departments/{id}",
                web::delete().to(directory::delete_department),
            )
            .route("/employees", web::get().to(directory::list_employees))
            .route("/employees", web::post().to(directory::create_employee))
            .route(
                "/employees/linkable",
                web::get().to(directory::linkable_users),
            )
            .route(
                "/branches/{branch_id}/people",
                web::get().to(directory::branch_people),
            )
            .route(
                "/employees/{employee_id}",
                web::get().to(directory::get_employee),
            )
            .route(
                "/employees/{employee_id}",
                web::put().to(directory::put_employee),
            )
            .route(
                "/employees/{employee_id}",
                web::delete().to(directory::delete_employee),
            )
            .route(
                "/employees/{employee_id}/documents",
                web::get().to(directory::list_documents),
            )
            .route(
                "/employees/{employee_id}/documents",
                web::post().to(directory::create_document),
            )
            .route(
                "/documents/{id}",
                web::delete().to(directory::delete_document),
            )
            // ── Work shifts + roster ─────────────────────────────
            .route("/work-shifts", web::get().to(schedules::list_work_shifts))
            .route("/work-shifts", web::post().to(schedules::create_work_shift))
            .route(
                "/work-shifts/{id}",
                web::patch().to(schedules::update_work_shift),
            )
            .route(
                "/work-shifts/{id}",
                web::delete().to(schedules::delete_work_shift),
            )
            // Literal sub-paths before `{id}` so `overrides` and `day` are not
            // swallowed by the parameterised delete/patch routes.
            .route(
                "/schedules/overrides",
                web::put().to(schedules::put_override),
            )
            .route(
                "/schedules/overrides/{id}",
                web::delete().to(schedules::delete_override),
            )
            .route(
                "/schedules/day",
                web::get().to(schedules::get_scheduled_day),
            )
            .route("/schedules", web::get().to(schedules::list_assignments))
            .route("/schedules", web::post().to(schedules::create_assignment))
            .route(
                "/schedules/{id}",
                web::delete().to(schedules::delete_assignment),
            )
            // ── Attendance ───────────────────────────────────────
            .route(
                "/attendance/settings",
                web::get().to(attendance::get_attendance_settings),
            )
            .route(
                "/attendance/settings",
                web::put().to(attendance::put_attendance_settings),
            )
            .route(
                "/attendance/summary",
                web::get().to(attendance::attendance_summary),
            )
            .route(
                "/discipline-report",
                web::get().to(discipline::discipline_report),
            )
            .route("/team/presence", web::get().to(attendance::team_presence))
            .route("/attendance", web::get().to(attendance::list_attendance))
            .route(
                "/attendance",
                web::post().to(attendance::create_manual_record),
            )
            .route(
                "/attendance/{id}",
                web::patch().to(attendance::correct_record),
            )
            .route(
                "/attendance/{id}",
                web::delete().to(attendance::delete_record),
            )
            // ── Requests (leave, late arrival, early departure, excuse,
            //    mission) + leave types and balances ─────────────────
            .route("/requests", web::get().to(requests::list_requests))
            .route("/requests", web::post().to(requests::create_request_admin))
            .route(
                "/requests/{id}/decision",
                web::patch().to(requests::decide_request),
            )
            .route("/leave/types", web::get().to(requests::list_leave_types))
            .route("/leave/types", web::post().to(requests::create_leave_type))
            .route(
                "/leave/types/{id}",
                web::patch().to(requests::update_leave_type),
            )
            .route(
                "/leave/types/{id}",
                web::delete().to(requests::delete_leave_type),
            )
            .route("/leave/balances", web::get().to(requests::list_balances))
            .route("/leave/balances", web::put().to(requests::put_balance))
            // ── Payroll ──────────────────────────────────────────
            .route(
                "/payroll/deductions",
                web::get().to(payroll::list_deductions),
            )
            .route(
                "/payroll/deductions",
                web::post().to(payroll::create_deduction),
            )
            .route(
                "/payroll/deductions/{id}",
                web::delete().to(payroll::delete_deduction),
            )
            .route(
                "/payroll/deductions/{id}/override",
                web::patch().to(payroll::override_deduction),
            )
            .route(
                "/payroll/deductions/{id}/waive",
                web::patch().to(payroll::waive_deduction),
            )
            .route("/payroll/bonuses", web::get().to(payroll::list_bonuses))
            .route("/payroll/bonuses", web::post().to(payroll::create_bonus))
            .route(
                "/payroll/bonuses/{id}",
                web::delete().to(payroll::delete_bonus),
            )
            .route("/payroll/advances", web::get().to(payroll::list_advances))
            .route(
                "/payroll/advances",
                web::post().to(payroll::create_advance_admin),
            )
            .route(
                "/payroll/advances/{id}/decision",
                web::patch().to(payroll::decide_advance),
            )
            .route("/payroll/periods", web::get().to(payroll::list_periods))
            .route("/payroll/periods", web::post().to(payroll::create_period))
            .route(
                "/payroll/periods/{id}/generate",
                web::post().to(payroll::generate_period),
            )
            .route(
                "/payroll/periods/{id}/export.csv",
                web::get().to(payroll::export_period_csv),
            )
            .route(
                "/payroll/periods/{id}/preview",
                web::get().to(payroll::preview_period),
            )
            .route(
                "/payroll/periods/{id}/payslips",
                web::get().to(payroll::list_payslips),
            )
            .route(
                "/payroll/periods/{id}/status",
                web::patch().to(payroll::set_period_status),
            )
            .route(
                "/payroll/periods/{id}",
                web::delete().to(payroll::delete_period),
            ),
    );
}
