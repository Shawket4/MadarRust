//! Staff routes.
//!
//! Two scopes, both behind `JwtMiddleware`:
//!
//! - `/staff/*` — the ADMIN surface. Every handler checks a permission
//!   (`staff` / `work_shifts` / `attendance` / `leave` / `payroll`).
//! - `/staff/me/*` — SELF-SERVICE. No permission is checked because the scope is
//!   the caller's own rows; the gate is having a `staff_profiles` row at all.
//!
//! `/staff/me/...` is registered on the same scope as `/staff/...`; actix matches
//! the more specific literal segment first, so `me` never shadows a `{user_id}`
//! path — and `me` is not a UUID, so it could not collide anyway.

use actix_web::web;

use crate::auth::middleware::JwtMiddleware;
use crate::staff::{attendance, directory, payroll, requests, schedules};

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/staff")
            .wrap(JwtMiddleware)
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
            .route(
                "/employees/{user_id}",
                web::get().to(directory::get_employee),
            )
            .route(
                "/employees/{user_id}",
                web::put().to(directory::put_employee),
            )
            .route(
                "/employees/{user_id}",
                web::delete().to(directory::delete_employee),
            )
            .route(
                "/employees/{user_id}/documents",
                web::get().to(directory::list_documents),
            )
            .route(
                "/employees/{user_id}/documents",
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
