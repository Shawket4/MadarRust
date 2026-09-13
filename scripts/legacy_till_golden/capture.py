#!/usr/bin/env python3
"""Capture golden JSON of the OLD (pre tills-rename) backend by running
scenario.json on a database seeded with seed.sql. Driven by capture.sh.
See tests/fixtures/legacy_till_api/README.md.

Env: BASE (http://127.0.0.1:8090), JWT_SECRET, OUT (fixture dir).
"""
import base64, hashlib, hmac, json, os, re, sys, time, urllib.request, urllib.error

BASE = os.environ.get("BASE", "http://127.0.0.1:8090")
SECRET = os.environ["JWT_SECRET"].encode()
OUT = os.environ["OUT"]
HERE = os.path.dirname(os.path.abspath(__file__))

# The ONLY values the strict test does not compare: wall-clock timestamps.
# Scrubbed to a fixed value (nulls stay null) so fixtures diff cleanly.
VOLATILE_TS_KEYS = sorted({
    "created_at", "updated_at", "opened_at", "closed_at", "issued_at", "settled_at",
    "voided_at", "finalized_at", "force_closed_at", "generated_at", "server_time",
    "last_activity_at", "bumped_at", "fired_at", "seated_at", "ended_at", "started_at",
    "printed_at", "moved_at", "status_changed_at", "accepted_at", "received_at",
    "delivered_at", "round_fired_at",
})
FIXED_TS = "2026-01-01T00:00:00Z"
# Human refs embed the business date (<CODE>-YYMMDD-...): only that segment is
# normalised, the rest of the ref is compared.
DATED_REF_KEYS = sorted({"order_ref", "delivery_ref", "ticket_ref"})
REF_DATE_RE = re.compile(r"-\d{6}-")
TS_RE = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}")
BOTH = ["v0.5.1", "v0.6.0"]


def b64(b):
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def jwt(claims):
    h = b64(json.dumps({"alg": "HS256", "typ": "JWT"}).encode())
    p = b64(json.dumps(claims).encode())
    sig = b64(hmac.new(SECRET, f"{h}.{p}".encode(), hashlib.sha256).digest())
    return f"{h}.{p}.{sig}"


def token(user_id, org_id, role):
    now = int(time.time())
    return jwt({"sub": user_id, "org_id": org_id, "role": role, "branch_id": None,
                "iat": now, "exp": now + 3600 * 4})


def call(method, path, tok, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(BASE + path, data=data, method=method)
    if tok:
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
    if isinstance(v, str) and key in VOLATILE_TS_KEYS and TS_RE.match(v):
        return FIXED_TS
    if isinstance(v, str) and key in DATED_REF_KEYS:
        return REF_DATE_RE.sub("-YYMMDD-", v, count=1)
    return v


def subst(v, vars):
    if isinstance(v, dict):
        return {k: subst(x, vars) for k, x in v.items()}
    if isinstance(v, list):
        return [subst(x, vars) for x in v]
    if isinstance(v, str):
        return re.sub(r"\{\{(\w+)\}\}", lambda m: str(vars[m.group(1)]), v)
    return v


def dig(v, path):
    for part in path.split("."):
        v = v[int(part)] if isinstance(v, list) else v[part]
    return v


def main():
    scenario = json.load(open(os.path.join(HERE, "scenario.json")))
    vars = dict(scenario["vars"])
    now = int(time.time())
    vars["device_token"] = jwt({"sub": "201000000000", "kind": "delivery_device", "exp": now + 86400 * 90})
    roles = {"admin": "org_admin", "teller_a": "teller", "teller_b": "teller", "waiter": "waiter"}
    toks = {who: token(vars[who], vars["org"], role) for who, role in roles.items()}
    os.makedirs(OUT, exist_ok=True)
    manifest = []
    for step in scenario["steps"]:
        path = subst(step["path"], vars)
        body = subst(step.get("body"), vars)
        status, resp = call(step["method"], path, toks.get(step.get("as")), body)
        for var, p in (step.get("bind") or {}).items():
            try:
                vars[var] = dig(resp, p)
            except (KeyError, IndexError, TypeError):
                print(f"{status} {step['method']} {path}: cannot bind {var} from {p}: {json.dumps(resp)[:400]}")
                return 1
        name = step.get("save")
        want = step.get("expect")
        flag = "" if want is None or status == want else f"  (EXPECTED {want}: {json.dumps(resp)[:300]})"
        print(f"{status} {step['method']} {path} -> {name or '-'}{flag}")
        if want is not None and status != want:
            return 1
        if not name:
            continue
        doc = {"request": {"method": step["method"], "path": path, "body": body}, "status": status, "body": scrub(resp)}
        with open(os.path.join(OUT, f"{name}.json"), "w") as f:
            json.dump(doc, f, indent=2, sort_keys=True, ensure_ascii=False)
            f.write("\n")
        ok = 200 <= status < 300
        model = step.get("model")
        manifest.append({"file": f"{name}.json", "status": status, "model": model if ok else None,
                         "clients": step.get("clients", BOTH) if (ok and model) else []})
    with open(os.path.join(OUT, "manifest.json"), "w") as f:
        json.dump({"volatile_timestamp_keys": VOLATILE_TS_KEYS, "dated_ref_keys": DATED_REF_KEYS, "pii_keys": [],
                   "fixed_timestamp": FIXED_TS, "files": manifest}, f, indent=2)
        f.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
