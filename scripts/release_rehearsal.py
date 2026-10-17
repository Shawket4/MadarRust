#!/usr/bin/env python3
"""HTTP half of scripts/release_rehearsal.sh (stdlib only).

  release_rehearsal.py token   <jwt_secret> <user_id> <org_id> <role>
  release_rehearsal.py capture <base_url> <token> <ctx.json> <out_dir>
  release_rehearsal.py compare <old_dir> <new_dir>
  release_rehearsal.py smoke   <base_url> <token> <ctx.json> <out_dir>

ctx.json (written by the shell script from the clone):
  {"org", "branch", "item", "addon", "user", "teller_till" | null, "sale_items": [...]}

`capture` GETs the 16 old-shape compat routes pinned by
tests/fixtures/compat/old_shapes.json, with the fixture's ids replaced by
real ids of the copy, and stores each raw body. `compare` requires every
status and body to be byte-identical. `smoke` rings a POS sale through
/orders and one through /sync/replay, does a full menu pull, renames an item,
pulls again (the new name must be there: the cache is fresh), and reads a
dashboard report. Prints `OK ...` / `!! ...`; exit 1 on any `!!`.
"""
import base64, hashlib, hmac, json, os, sys, time, urllib.error, urllib.request, uuid

FIXTURE = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                       "..", "tests", "fixtures", "compat", "old_shapes.json")
# The fixture ids (tests/compat_contract_tests.rs) and the ctx key each stands for.
FIXTURE_IDS = {
    "c0de0000-0000-4000-8000-000000000001": "org",
    "c0de0000-0000-4000-8000-000000000002": "branch",
    "c0de0000-0000-4000-8000-000000000005": "item",
    "c0de0000-0000-4000-8000-00000000000e": "addon",
}


def b64(b):
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def token(secret, user, org, role):
    now = int(time.time())
    head = b64(json.dumps({"typ": "JWT", "alg": "HS256"}).encode())
    body = b64(json.dumps({"sub": user, "org_id": org, "role": role, "branch_id": None,
                           "iat": now, "exp": now + 6 * 3600}).encode())
    sig = hmac.new(secret.encode(), f"{head}.{body}".encode(), hashlib.sha256).digest()
    return f"{head}.{body}.{b64(sig)}"


def call(base, tok, method, path, body=None, timeout=120):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(base + path, data=data, method=method)
    req.add_header("Authorization", f"Bearer {tok}")
    if data is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


def routes(ctx):
    with open(FIXTURE) as f:
        paths = sorted(json.load(f))
    out = []
    for p in paths:
        q = p
        for fid, key in FIXTURE_IDS.items():
            q = q.replace(fid, str(ctx[key]))
        out.append((p, q))
    return out


def capture(base, tok, ctx_file, out):
    ctx = json.load(open(ctx_file))
    os.makedirs(out, exist_ok=True)
    index = []
    for i, (pin, path) in enumerate(routes(ctx)):
        st, body = call(base, tok, "GET", path)
        with open(os.path.join(out, f"{i:02d}.body"), "wb") as f:
            f.write(body)
        index.append({"n": i, "pinned": pin, "path": path, "status": st, "bytes": len(body)})
    json.dump(index, open(os.path.join(out, "index.json"), "w"), indent=1)
    print(f"   captured {len(index)} compat routes from {base}")


def pretty(b):
    try:
        return json.dumps(json.loads(b), indent=1, sort_keys=True, ensure_ascii=False).splitlines()
    except Exception:
        return b.decode(errors="replace").splitlines()


# Lists whose ORDER BY ties on both binaries (allowed add-ons: sort_order +
# a view's now(); recipe lines), so the order of tied entries is not defined
# by either and differs run to run. Only these may differ, and only in order.
REORDER_OK = {"allowed_addon_ids", "recipes"}
# Routes whose top-level list has the same tie on both binaries: /addon-items
# orders by (type, created_at) and the add-ons were created in one batch;
# /branch-addon-overrides has no ORDER BY at all.
TOP_REORDER_OK = ("/addon-items?", "/branch-addon-overrides?")


def canon(v, key=None):
    if isinstance(v, dict):
        return {k: canon(x, k) for k, x in v.items()}
    if isinstance(v, list):
        items = [canon(x) for x in v]
        if key in REORDER_OK:
            items.sort(key=lambda x: json.dumps(x, sort_keys=True))
        return items
    return v


def same_up_to_tie_order(ob, nb, path):
    try:
        a, b = canon(json.loads(ob)), canon(json.loads(nb))
    except Exception:
        return False
    if path.startswith(TOP_REORDER_OK) and isinstance(a, list) and isinstance(b, list):
        k = lambda x: json.dumps(x, sort_keys=True)
        a, b = sorted(a, key=k), sorted(b, key=k)
    return a == b


def compare(old, new):
    import difflib
    a = json.load(open(os.path.join(old, "index.json")))
    b = json.load(open(os.path.join(new, "index.json")))
    bad = 0
    tie = []
    for x, y in zip(a, b):
        ob = open(os.path.join(old, f"{x['n']:02d}.body"), "rb").read()
        nb = open(os.path.join(new, f"{y['n']:02d}.body"), "rb").read()
        if x["status"] == y["status"] and ob != nb and same_up_to_tie_order(ob, nb, x["pinned"]):
            tie.append(x["path"])
            continue
        if x["status"] != y["status"] or ob != nb:
            bad += 1
            print(f"!! compat {x['path']}: v1.10.0 {x['status']} ({len(ob)} B) vs release {y['status']} ({len(nb)} B)")
            for line in list(difflib.unified_diff(pretty(ob), pretty(nb), "v1.10.0", "release", n=1))[:30]:
                print("     " + line)
    non200 = [x["path"] for x in a if x["status"] != 200]
    if bad == 0:
        note = f"; non-200 on both (same body): {len(non200)}" if non200 else ""
        ident = len(a) - len(tie)
        tnote = (f"; {len(tie)} identical except the order of tied entries in "
                 f"{'/'.join(sorted(REORDER_OK))} or of a route's tied rows") if tie else ""
        print(f"OK compat: {len(a)} old-shape routes vs v1.10.0: {ident} byte-identical{tnote}{note}")
        for t in tie:
            print(f"     tie order only: {t}")
    return 1 if bad else 0


def smoke(base, tok, ctx_file, out):
    ctx = json.load(open(ctx_file))
    os.makedirs(out, exist_ok=True)
    fail = 0

    def ok(cond, what, detail=""):
        nonlocal fail
        if cond:
            print(f"OK smoke: {what}")
        else:
            fail = 1
            print(f"!! smoke: {what} {detail}")

    branch = ctx["branch"]
    # An open till of the signed-in user (the copy's, at its own branch, or one
    # opened now at the busiest branch). Sales go to the till's branch.
    till = ctx.get("teller_till")
    sale_branch = ctx.get("till_branch") or branch
    if not till:
        st, body = call(base, tok, "POST", f"/tills/branches/{branch}/open", {"opening_cash": 0, "edit_reason": "release rehearsal"})
        ok(st in (200, 201), "open a till", f"{st} {body[:300]!r}")
        j = json.loads(body or b"{}")
        till = j.get("id") or (j.get("till") or {}).get("id")
    # 1. A counter sale through /orders.
    sale = None
    for item in ctx["sale_items"]:
        body = {"branch_id": sale_branch, "till_id": till, "payment_method": "cash",
                "items": [{"menu_item_id": item, "quantity": 2}], "idempotency_key": str(uuid.uuid4())}
        st, resp = call(base, tok, "POST", "/orders", body)
        if st in (200, 201):
            sale = (item, json.loads(resp))
            break
        last = f"{st} {resp[:300]!r}"
    ok(sale is not None, "POST /orders (a counter sale)", "" if sale else last)
    # 2. The same through an offline till's queue.
    if sale:
        req = {"branch_id": sale_branch, "till_id": till, "payment_method": "cash",
               "items": [{"menu_item_id": sale[0], "quantity": 1}], "idempotency_key": str(uuid.uuid4())}
        st, resp = call(base, tok, "POST", "/sync/replay",
                        {"op": "create_order", "teller_id": ctx["user"], "request": req})
        ok(st in (200, 201), "POST /sync/replay create_order", f"{st} {resp[:300]!r}")
        st2, resp2 = call(base, tok, "POST", "/sync/replay",
                          {"op": "create_order", "teller_id": ctx["user"], "request": req})
        same = False
        try:
            same = json.loads(resp).get("id") == json.loads(resp2).get("id") or \
                   (json.loads(resp).get("order") or {}).get("id") == (json.loads(resp2).get("order") or {}).get("id")
        except Exception:
            pass
        ok(st2 in (200, 201) and same, "replaying the same op again is idempotent", f"{st2} {resp2[:200]!r}")
    # 3. Full pull, a catalog write, full pull: the second sees the write.
    st, p1 = call(base, tok, "POST", "/sync/pull", {"branch_id": branch})
    ok(st == 200, f"full menu pull #1 ({len(p1)} B)", f"{st} {p1[:200]!r}")
    marker = f"Rehearsal {uuid.uuid4().hex[:8]}"
    st, resp = call(base, tok, "PATCH", f"/menu-items/{ctx['item']}", {"name": marker})
    ok(st in (200, 204), "PATCH /menu-items/{id} (rename)", f"{st} {resp[:300]!r}")
    st, p2 = call(base, tok, "POST", "/sync/pull", {"branch_id": branch})
    ok(st == 200 and marker.encode() in p2 and marker.encode() not in p1,
       "full menu pull #2 carries the rename (cache fresh)", f"{st}")
    st, m2 = call(base, tok, "GET", f"/menu-items?org_id={ctx['org']}&full=true&branch_id={branch}")
    ok(st == 200 and marker.encode() in m2, "GET /menu-items?full=true carries the rename", f"{st}")
    # 4. A dashboard report over the last 30 days, today's sales included.
    now = time.time()
    frm = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(now - 30 * 86400))
    to = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(now + 60))
    st, rep = call(base, tok, "GET", f"/reports/branches/{branch}/sales?from={frm}&to={to}")
    ok(st == 200 and rep[:1] == b"{", "GET /reports/branches/{id}/sales (30 days)", f"{st} {rep[:200]!r}")
    for name, data in (("pull1.json", p1), ("pull2.json", p2), ("report.json", rep)):
        open(os.path.join(out, name), "wb").write(data)
    return fail


def main():
    cmd = sys.argv[1]
    if cmd == "token":
        print(token(*sys.argv[2:6]))
    elif cmd == "capture":
        capture(*sys.argv[2:6])
    elif cmd == "compare":
        sys.exit(compare(*sys.argv[2:4]))
    elif cmd == "smoke":
        sys.exit(smoke(*sys.argv[2:6]))
    else:
        sys.exit(f"unknown command {cmd}")


if __name__ == "__main__":
    main()
