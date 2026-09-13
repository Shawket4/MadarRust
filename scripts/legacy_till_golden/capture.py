#!/usr/bin/env python3
"""Capture golden JSON of the CURRENT (pre tills-rename) backend for every
shifts/tills-related endpoint POS v0.5.1 / v0.6.0 call, plus the /sync/replay
responses for their queued ops. See tests/fixtures/legacy_till_api/README.md.

Runs against a backend booted on a DISPOSABLE copy of madar_prodcopy (driven by
capture.sh). It MUTATES that copy (replays, a refund, closes/force-closes).

Env: BASE (http://127.0.0.1:8090), DB (madar_golden), JWT_SECRET, OUT (fixture dir).
"""
import base64, hashlib, hmac, json, os, re, subprocess, sys, time, urllib.request, urllib.error, uuid

BASE = os.environ.get("BASE", "http://127.0.0.1:8090")
DB = os.environ.get("DB", "madar_golden")
SECRET = os.environ["JWT_SECRET"].encode()
OUT = os.environ["OUT"]

# Timestamps and server-minted values that change on every capture. Values are
# replaced (type-preserving) so fixtures diff cleanly; the key stays present.
VOLATILE_TS_KEYS = {
    "created_at", "updated_at", "opened_at", "closed_at", "issued_at", "settled_at",
    "voided_at", "finalized_at", "force_closed_at", "generated_at", "server_time",
    "last_activity_at", "bumped_at", "fired_at", "seated_at", "ended_at", "started_at",
}
# PII from the prod copy — scrubbed (strings only).
PII_KEYS = {"customer_name", "customer_phone", "phone", "email", "address", "address_line",
            "customer_address", "notes", "cash_note", "note"}
FIXED_TS = "2026-01-01T00:00:00Z"
TS_RE = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}")


def b64(b):
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def token(user_id, org_id, role, branch_id=None):
    now = int(time.time())
    claims = {"sub": user_id, "org_id": org_id, "role": role, "branch_id": branch_id,
              "iat": now, "exp": now + 3600 * 4}
    h = b64(json.dumps({"alg": "HS256", "typ": "JWT"}).encode())
    p = b64(json.dumps(claims).encode())
    sig = b64(hmac.new(SECRET, f"{h}.{p}".encode(), hashlib.sha256).digest())
    return f"{h}.{p}.{sig}"


def sql(q):
    out = subprocess.check_output(["psql", "-U", os.environ.get("PGUSER", "shawket"), "-d", DB, "-Atc", q]).decode()
    return [line.split("|") for line in out.strip().splitlines() if line]


def call(method, path, tok, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(BASE + path, data=data, method=method)
    req.add_header("Authorization", f"Bearer {tok}")
    if data is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req) as r:
            status, raw = r.status, r.read().decode()
    except urllib.error.HTTPError as e:
        status, raw = e.code, e.read().decode()
    try:
        parsed = json.loads(raw) if raw else None
    except json.JSONDecodeError:
        parsed = raw
    return status, parsed


def scrub(v, key=None):
    if isinstance(v, dict):
        return {k: scrub(x, k) for k, x in v.items()}
    if isinstance(v, list):
        return [scrub(x, key) for x in v]
    if isinstance(v, str):
        if key in VOLATILE_TS_KEYS and TS_RE.match(v):
            return FIXED_TS
        if key in PII_KEYS and v:
            return "REDACTED"
    return v


MANIFEST = []
BOTH = ["v0.5.1", "v0.6.0"]


def save(name, method, path, tok, body=None, model=None, clients=BOTH, expect=None):
    status, resp = call(method, path, tok, body)
    doc = {"request": {"method": method, "path": path, "body": body}, "status": status, "body": scrub(resp)}
    with open(os.path.join(OUT, f"{name}.json"), "w") as f:
        json.dump(doc, f, indent=2, sort_keys=True, ensure_ascii=False)
        f.write("\n")
    ok = 200 <= status < 300
    # Only 2xx bodies are parse-checked against the old models.
    MANIFEST.append({"file": f"{name}.json", "status": status,
                     "model": model if ok else None, "clients": clients if (ok and model) else []})
    flag = "" if (expect is None or status == expect) else f"  (expected {expect})"
    print(f"{status} {method} {path} -> {name}.json{flag}")
    return status, resp


def main():
    os.makedirs(OUT, exist_ok=True)
    # Scenario rows (stable ids in the prod copy; picked, not hardcoded, where possible).
    org = sql("select b.org_id from shifts s join branches b on b.id=s.branch_id where s.status='open' "
              "group by 1 order by count(*) desc limit 1")[0][0]
    admin = sql(f"select id from users where role='org_admin' and org_id='{org}' and is_active and deleted_at is null limit 1")[0][0]
    # Branch A: an open shift with orders + cash movements + a settled ticket.
    s_a, branch_a, teller_a = sql(
        f"select s.id, s.branch_id, s.teller_id from shifts s join branches b on b.id=s.branch_id "
        f"where s.status='open' and b.org_id='{org}' order by (select count(*) from shift_cash_movements m where m.shift_id=s.id) desc limit 1")[0]
    # Branch B: an open shift in a branch with OPEN tickets (settle) — may equal A.
    rows = sql(f"select s.id, s.branch_id, s.teller_id, t.id from shifts s join open_tickets t on t.branch_id=s.branch_id "
               f"and t.status='open' where s.status='open' and s.id<>'{s_a}' limit 1")
    s_b, branch_b, teller_b, ticket_open = rows[0]
    s_force = sql(f"select s.id from shifts s join branches b on b.id=s.branch_id where s.status='open' and b.org_id='{org}' and b.deleted_at is null "
                  f"and s.id not in ('{s_a}','{s_b}') limit 1")[0][0]
    s_closed = sql(f"select s.id from shifts s where s.branch_id='{branch_a}' and s.status='closed' order by closed_at desc limit 1")[0][0]
    settled_ticket = sql(f"select id from open_tickets where settled_shift_id is not null and branch_id='{branch_a}' order by settled_at desc limit 1")[0][0]
    order_a = sql(f"select id from orders where shift_id='{s_a}' order by created_at desc limit 1")[0][0]
    items = [{"menu_item_id": r[0], "quantity": int(r[1]), "unit_price": int(r[2]), "addons": [], "optional_field_ids": [],
              "size_label": r[3] or None}
             for r in sql(f"select menu_item_id, quantity, unit_price, size_label from order_items where order_id='{order_a}' and menu_item_id is not null")]
    delivery = sql(f"select id from delivery_orders where branch_id='{branch_b}' and status='received' limit 1")

    t_admin = token(admin, org, "org_admin")
    t_a = token(teller_a, org, "teller", branch_a)
    t_b = token(teller_b, org, "teller", branch_b)

    # ── reads the old POS core makes ────────────────────────────────────────
    save("shifts_current", "GET", f"/shifts/branches/{branch_a}/current", t_a, model="ShiftPreFill")
    save("shifts_list_branch", "GET", f"/shifts/branches/{branch_a}?page=1&per_page=3", t_a, model="PaginatedShifts")
    save("shifts_get_open", "GET", f"/shifts/{s_a}", t_admin, model="Shift", clients=[])
    save("shifts_report_open", "GET", f"/shifts/{s_a}/report", t_a, model="ShiftReportResponse")
    save("shifts_report_closed", "GET", f"/shifts/{s_closed}/report", t_admin, model="ShiftReportResponse")
    save("shifts_cash_movements", "GET", f"/shifts/{s_a}/cash-movements", t_a, model="Vec<CashMovement>")
    save("tills_list", "GET", f"/tills?branch_id={branch_a}", t_a, model="Vec<Till>")
    save("orders_list_by_shift", "GET", f"/orders?branch_id={branch_a}&shift_id={s_a}&page=1&per_page=3", t_a, model="PaginatedOrders")
    save("orders_get", "GET", f"/orders/{order_a}", t_a, model="OrderFull")
    save("refunds_by_shift_empty", "GET", f"/refunds/shift/{s_a}", t_a, model="ShiftRefunds", clients=["v0.6.0"])
    save("reports_shift_summary", "GET", f"/reports/shifts/{s_a}/summary", t_admin, model="ShiftSummary")
    save("reports_shift_deductions", "GET", f"/reports/shifts/{s_a}/deductions", t_admin)
    save("open_tickets_list", "GET", f"/open-tickets?branch_id={branch_b}", t_b, model="Vec<OpenTicketView>")
    save("open_tickets_get_settled", "GET", f"/open-tickets/{settled_ticket}", t_a, model="OpenTicketView")
    save("delivery_orders_list", "GET", f"/delivery-orders?branch_id={branch_b}&limit=5", t_b, model="Vec<DeliveryOrder>")

    # ── /sync/replay responses (mutates the disposable copy) ───────────────
    cm_ref = str(uuid.uuid4())
    save("replay_cash_movement", "POST", "/sync/replay", t_a, model="CashMovement", body={
        "op": "cash_movement", "teller_id": teller_a, "shift_id": s_a,
        "request": {"amount": -1500, "note": "golden pay out", "client_ref": cm_ref, "kind": "pay_out"}})
    save("replay_cash_movement_dedup", "POST", "/sync/replay", t_a, model="CashMovement", body={
        "op": "cash_movement", "teller_id": teller_a, "shift_id": s_a,
        "request": {"amount": -1500, "note": "golden pay out", "client_ref": cm_ref, "kind": "pay_out"}})
    sub = sum(i["unit_price"] * i["quantity"] for i in items)
    order_key = str(uuid.uuid4())
    status, order = save("replay_create_order", "POST", "/sync/replay", t_a, model="ReplayCreateOrderAck", body={
        "op": "create_order", "teller_id": teller_a, "request": {
            "branch_id": branch_a, "shift_id": s_a, "payment_method": "cash", "items": items,
            "idempotency_key": order_key, "amount_tendered": sub, "change_given": 0,
            "subtotal": sub, "tax_amount": 0, "total_amount": sub}})
    new_order = order.get("id") if isinstance(order, dict) else None
    if new_order:
        save("orders_get_created", "GET", f"/orders/{new_order}", t_a, model="OrderFull")
        save("replay_refund_order", "POST", "/sync/replay", t_a, model="RefundIssued", clients=["v0.6.0"], body={
            "op": "refund_order", "teller_id": teller_a, "request": {
                "order_id": new_order, "amount": min(1000, sub), "method": "cash", "reason": "quality_issue",
                "client_ref": str(uuid.uuid4()), "shift_id": s_a}})
        save("refunds_by_order", "GET", f"/refunds/order/{new_order}", t_a, model="OrderRefunds", clients=["v0.6.0"])
        save("refunds_by_shift", "GET", f"/refunds/shift/{s_a}", t_a, model="ShiftRefunds", clients=["v0.6.0"])
        save("orders_list_by_shift_after", "GET", f"/orders?branch_id={branch_a}&shift_id={s_a}&page=1&per_page=2", t_a, model="PaginatedOrders")
    save("replay_settle_open_ticket", "POST", "/sync/replay", t_b, model="Order", body={
        "op": "settle_open_ticket", "teller_id": teller_b, "ticket_id": ticket_open,
        "request": {"payment_method": "card", "shift_id": s_b}})
    save("open_tickets_get_just_settled", "GET", f"/open-tickets/{ticket_open}", t_b, model="OpenTicketView")
    if delivery:
        save("delivery_finalize", "POST", f"/delivery-orders/{delivery[0][0]}/finalize", t_b, model="FinalizeResponse",
             body={"payment_method": "cash", "shift_id": s_b})
    save("shifts_report_after_mutations", "GET", f"/shifts/{s_a}/report", t_a, model="ShiftReportResponse")
    save("replay_close_shift", "POST", "/sync/replay", t_a, model="CloseShiftResponse", body={
        "op": "close_shift", "teller_id": teller_a, "shift_id": s_a,
        "request": {"closing_cash_declared": 10000, "cash_note": None}})
    save("replay_close_shift_again", "POST", "/sync/replay", t_a, model="CloseShiftResponse", body={
        "op": "close_shift", "teller_id": teller_a, "shift_id": s_a,
        "request": {"closing_cash_declared": 10000, "cash_note": None}})
    save("shifts_current_after_close", "GET", f"/shifts/branches/{branch_a}/current", t_a, model="ShiftPreFill")
    new_shift = str(uuid.uuid4())
    save("replay_open_shift", "POST", "/sync/replay", t_a, model="Shift", body={
        "op": "open_shift", "teller_id": teller_a, "branch_id": branch_a,
        "request": {"id": new_shift, "opening_cash": 10000}})
    save("replay_open_shift_dedup", "POST", "/sync/replay", t_a, model="Shift", body={
        "op": "open_shift", "teller_id": teller_a, "branch_id": branch_a,
        "request": {"id": new_shift, "opening_cash": 10000}})
    save("shifts_force_close", "POST", f"/shifts/{s_force}/force-close", t_admin, model="Shift", clients=["v0.6.0"],
         body={"reason": "golden capture"})

    # Stable placeholders for ids minted during THIS capture (volatile).
    minted = {v: f"00000000-0000-0000-0000-{i:012d}" for i, v in enumerate(
        [x for x in [cm_ref, order_key, new_order, new_shift] if x], start=1)}
    for fn in os.listdir(OUT):
        if fn.endswith(".json") and fn != "manifest.json":
            p = os.path.join(OUT, fn)
            s = open(p).read()
            for real, ph in minted.items():
                s = s.replace(real, ph)
            open(p, "w").write(s)

    with open(os.path.join(OUT, "manifest.json"), "w") as f:
        json.dump({"volatile_timestamp_keys": sorted(VOLATILE_TS_KEYS), "pii_keys": sorted(PII_KEYS),
                   "fixed_timestamp": FIXED_TS, "files": MANIFEST}, f, indent=2)
        f.write("\n")


if __name__ == "__main__":
    sys.exit(main())
