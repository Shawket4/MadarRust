#!/usr/bin/env python3
"""Import a legacy loyalty provider's customers into Madar's loyalty programme.

Built for Drops' move off its old stamp-card provider ("Drops Specialty
Rewards"). Each old customer becomes (or is matched to) ONE Madar customer, keyed
by canonical phone, is enrolled in the org's loyalty programme, and has their
CURRENT old stamp balance carried over as one ledger credit.

    scripts/import-legacy-loyalty.py INPUT [INPUT ...]
        [--org UUID] [--db-url URL] [--ssh HOST [--sudo-user USER]]
        [--dry-run | --apply] [--yes-prod] [--out DIR]

  INPUT       export files (*.json), or folders whose *.json are all read. Any
              number of page files and/or one merged file: records are deduped
              by the old `id`, then grouped by canonical phone.
  --org       the Madar org (default: Drops). Any other org is refused, here
              and again inside the SQL.
  --db-url    psql connection string or database name (default: $DATABASE_URL).
  --ssh HOST  run psql on HOST over ssh (e.g. root@server); --sudo-user runs it
              as that OS user (e.g. postgres, for peer authentication).
  --dry-run   DEFAULT. Connects, does everything in one transaction, prints the
              report and ROLLS BACK.
  --apply     Commits. Asks you to type the org name first; against production
              it also needs --yes-prod.
  --out DIR   where the per-customer CSV goes (default: an `out/` folder beside
              the first input's folder). Refused inside a git work tree: the CSV
              holds personal data and the repos are public.

What is written, and why it looks like what the app writes, is described in
import-legacy-loyalty.sql. Nothing is sent to anybody: no pass is pushed from
here and no message goes out. Cards already in a Madar wallet are marked for
the backend's own refresh sweep.
"""
from __future__ import annotations

import argparse
import csv
import datetime as dt
import json
import os
import re
import shlex
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path

HERE = Path(__file__).resolve().parent
SQL_FILE = HERE / "import-legacy-loyalty.sql"
COPY_MARKER = "-- @@COPY_IMPORT_ROWS@@"

# The only org this script may write to. The SQL re-checks it.
DROPS_ORG = "27b8f8db-fec2-4909-b9f6-9fffbd860a1a"
ALLOWED_ORGS = {DROPS_ORG: "Drops"}
PROD_MARKERS = ("187.124.33.153",)

MAX_NAME = 120  # customers::handlers::MAX_NAME
MAX_PHONE_RAW_LEN = 32  # madar_ids::phone::MAX_PHONE_RAW_LEN
EG_MOBILE = re.compile(r"^20(10|11|12|15)\d{8}$")
ARABIC_DIGITS = str.maketrans("٠١٢٣٤٥٦٧٨٩۰۱۲۳۴۵۶۷۸۹", "01234567890123456789")
PHONE_CHARS = re.compile(r"^\+?[0-9\s\-().]+$")
CONTROL = re.compile(r"[\x00-\x1f\x7f]")


# ── Phones ───────────────────────────────────────────────────────────────────

def digits(raw: str) -> str:
    """ASCII digits of `raw`, Arabic-Indic numerals folded (madar_ids::phone::digits)."""
    return "".join(ch for ch in raw.translate(ARABIC_DIGITS) if ch.isascii() and ch.isdigit())


def canonical(raw: str | None) -> str | None:
    """Madar's stored phone key: E.164 digits without `+`.

    A line-for-line mirror of madar_ids::phone::canonical (MadarRust
    `crate::phone`, SQL `phone_canonical`), pinned by the shared
    phone_vectors.json in the tests and re-checked by the database per row.
    """
    if raw is None or len(raw) > MAX_PHONE_RAW_LEN:
        return None
    d = digits(raw)
    if d.startswith("00"):
        n = d[2:]
    elif d.startswith("20"):
        n = d
    elif d.startswith("0"):
        n = "20" + d[1:]
    elif len(d) == 10 and d.startswith("1"):
        n = "20" + d
    else:
        n = d
    if len(n) < 10 or len(n) > 15:
        return None
    if n[:4] in ("2010", "2011", "2012", "2015") and len(n) != 12:
        return None
    return n


def check_phone(raw) -> tuple[str | None, str | None]:
    """(key, None) for an Egyptian mobile, else (None, why it was refused).

    Stricter than `canonical`: the number must SAY it is Egyptian (+20, 0020,
    20…, or the local 0…), then be a mobile — 20 + 10 digits starting 10, 11,
    12 or 15. Nothing is guessed: a bare number or another country's is refused.
    """
    if raw is None or not str(raw).strip():
        return None, "no phone"
    s = str(raw).strip().translate(ARABIC_DIGITS)
    if not PHONE_CHARS.match(s):
        return None, "not a phone number"
    d = digits(s)
    if s.startswith("+"):
        if not d.startswith("20"):
            return None, f"not an Egyptian number (+{d[:3]}...)"
        n = d
    elif d.startswith("0020"):
        n = d[2:]
    elif d.startswith("20") and len(d) == 12:
        n = d
    elif d.startswith("0"):
        n = "20" + d[1:]
    else:
        return None, "no country code; cannot tell which country it is"
    national = n[2:]
    if national[:2] not in ("10", "11", "12", "15"):
        return None, f"not an Egyptian mobile (+20 {national[:2]}...)"
    if len(national) < 10:
        return None, f"too short: +20 then {len(national)} digits (a mobile has 10)"
    if len(national) > 10 or not EG_MOBILE.match(n):
        return None, f"too long: +20 then {len(national)} digits (a mobile has 10)"
    if canonical(str(raw)) != n:  # the shared rule must agree, or refuse
        return None, "Madar's phone rule reads this number differently"
    return n, None


def mask(phone) -> str:
    d = digits(str(phone or ""))
    return "..." + d[-4:] if d else "(none)"


# ── Input ────────────────────────────────────────────────────────────────────

def input_files(paths) -> list[Path]:
    files: list[Path] = []
    for p in map(Path, paths):
        if p.is_dir():
            files.extend(sorted(x for x in p.glob("*.json") if x.is_file()))
        elif p.is_file():
            files.append(p)
        else:
            raise SystemExit(f"input not found: {p}")
    seen, out = set(), []
    for f in files:
        key = f.resolve()
        if key not in seen:
            seen.add(key)
            out.append(f)
    if not out:
        raise SystemExit("no input files (*.json) found")
    return out


def extract_customers(obj) -> list[dict]:
    """The customers in one export: a page ({data:{customers}}), {customers},
    a bare list of customers, or a list of pages (a merged file)."""
    if isinstance(obj, dict):
        if isinstance(obj.get("data"), dict) and "customers" in obj["data"]:
            return list(obj["data"]["customers"])
        if "customers" in obj:
            return list(obj["customers"])
        if "id" in obj and "phone" in obj:
            return [obj]
        raise ValueError("unrecognised export shape (no customers list)")
    if isinstance(obj, list):
        out: list[dict] = []
        for item in obj:
            if isinstance(item, dict) and "id" in item and "phone" in item:
                out.append(item)
            else:
                out.extend(extract_customers(item))
        return out
    raise ValueError("unrecognised export shape")


def load_records(files) -> list[tuple[dict, str]]:
    records = []
    for f in files:
        with open(f, encoding="utf-8") as fh:
            data = json.load(fh)
        for rec in extract_customers(data):
            records.append((rec, Path(f).name))
    return records


def clean_text(s) -> str:
    return CONTROL.sub(" ", str(s or "")).strip()


def display_name(rec: dict) -> str:
    """The name the person goes by: displayName, else first + last. Trimmed and
    capped like `customers::handlers::clean_name`."""
    name = clean_text(rec.get("displayName"))
    if not name:
        name = " ".join(x for x in (clean_text(rec.get("firstName")), clean_text(rec.get("lastName"))) if x)
    return name[:MAX_NAME]


def as_int(v) -> int:
    if v is None or v == "":
        return 0
    if isinstance(v, bool):
        raise ValueError(f"not a count: {v!r}")
    n = int(v)
    if n != v and not isinstance(v, str):
        raise ValueError(f"not a whole count: {v!r}")
    return n


# ── Plan: dedupe and group ───────────────────────────────────────────────────

@dataclass
class Plan:
    files: list[str] = field(default_factory=list)
    records_read: int = 0
    unique: dict = field(default_factory=dict)            # old id -> record
    source: dict = field(default_factory=dict)            # old id -> file name
    exact_dupes: list = field(default_factory=list)       # (old id, file)
    conflicting_dupes: list = field(default_factory=list)  # (old id, kept file, other file, fields)
    invalid: list = field(default_factory=list)           # (record, reason)
    no_id: list = field(default_factory=list)             # records without an id
    normalised: list = field(default_factory=list)        # records not written as +20…
    groups: dict = field(default_factory=dict)            # phone key -> [records], primary first
    merchants: set = field(default_factory=set)
    programs: set = field(default_factory=set)

    @property
    def phone_dupes(self) -> dict:
        return {k: v for k, v in self.groups.items() if len(v) > 1}

    @property
    def importable(self) -> list[dict]:
        return [r for recs in self.groups.values() for r in recs]


def _newest(a: dict, b: dict) -> dict:
    return a if (a.get("updatedAt") or "") >= (b.get("updatedAt") or "") else b


def build_plan(records) -> Plan:
    plan = Plan()
    plan.records_read = len(records)
    plan.files = sorted({f for _, f in records})
    for rec, fname in records:
        oid = str(rec.get("id") or "").strip()
        if not oid:
            plan.no_id.append(rec)
            continue
        if rec.get("merchantId"):
            plan.merchants.add(rec["merchantId"])
        for p in rec.get("programs") or []:
            if p.get("name"):
                plan.programs.add(p["name"])
        prev = plan.unique.get(oid)
        if prev is None:
            plan.unique[oid], plan.source[oid] = rec, fname
        elif prev == rec:
            plan.exact_dupes.append((oid, fname))
        else:
            keep = _newest(prev, rec)
            diff = sorted(k for k in set(prev) | set(rec) if prev.get(k) != rec.get(k))
            kept_file = plan.source[oid] if keep is prev else fname
            other_file = fname if keep is prev else plan.source[oid]
            plan.conflicting_dupes.append((oid, kept_file, other_file, diff))
            plan.unique[oid], plan.source[oid] = keep, kept_file
    for oid, rec in plan.unique.items():
        key, why = check_phone(rec.get("phone"))
        if key is None:
            plan.invalid.append((rec, why))
            continue
        rec = dict(rec, _key=key, _name=display_name(rec))
        if not rec["_name"]:
            plan.invalid.append((rec, "no name"))
            continue
        if not clean_text(rec.get("phone")).startswith("+20"):
            plan.normalised.append(rec)
        plan.groups.setdefault(key, []).append(rec)
    # The most recently updated record names the person; ties by id, stable.
    for key, recs in plan.groups.items():
        recs.sort(key=lambda r: (r.get("updatedAt") or "", r["id"]), reverse=True)
    return plan


@dataclass
class CardFit:
    size: int                 # stamps per reward on the old card
    fits: int                 # rows with stamps == visits mod size
    rows: int
    exceptions: list          # records that do not follow the rule


def infer_card_size(records, lo: int = 2, hi: int = 50) -> CardFit | None:
    """The old card's size, read off the data rather than assumed.

    A stamp card that resets on a reward leaves stamps == visits mod k. The k
    that most rows follow wins, among those some row actually went round (a
    size nobody reached explains nothing). Rows that break the rule (stamps
    added or taken by hand in the old system) are returned for the report.
    Their CURRENT stamps are what is carried; nothing is derived from visits.
    """
    recs = list(records)
    best = None
    for k in range(lo, hi + 1):
        pairs = [(as_int(r.get("totalVisits")), as_int(r.get("totalStamps"))) for r in recs]
        if not any(v >= k and s == v % k for v, s in pairs):
            continue
        fits = sum(1 for v, s in pairs if s == v % k)
        if best is None or fits > best.fits:
            best = CardFit(k, fits, len(recs), [])
    if best is None or best.fits * 2 <= best.rows:
        return None
    best.exceptions = [r for r in recs
                       if as_int(r.get("totalStamps")) != as_int(r.get("totalVisits")) % best.size]
    return best


# ── Staging CSV (the COPY stream) ────────────────────────────────────────────

STAGE_COLS = ["old_id", "grp", "primary_in_group", "phone_raw", "phone_key", "name", "stamps",
              "visits", "old_created_at", "last_visit", "card_status", "program"]


def csv_field(v) -> str:
    """One CSV field for COPY: None is NULL (unquoted empty), text is always
    quoted (so an empty string stays a string), numbers and booleans bare."""
    if v is None:
        return ""
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, int):
        return str(v)
    return '"' + str(v).replace('"', '""') + '"'


def stage_rows(plan: Plan) -> list[list]:
    rows = []
    for grp, (key, recs) in enumerate(sorted(plan.groups.items()), start=1):
        for i, r in enumerate(recs):
            programs = [p.get("name") for p in r.get("programs") or [] if p.get("name")]
            rows.append([
                r["id"], grp, i == 0, clean_text(r["phone"]), key, r["_name"],
                as_int(r.get("totalStamps")), as_int(r.get("totalVisits")),
                r.get("createdAt"), r.get("lastVisit") or None, r.get("cardStatus"),
                programs[0] if programs else "the old programme",
            ])
    for row in rows:
        if not row[8]:
            raise SystemExit(f"record {row[0]} has no createdAt")
    return rows


def copy_block(rows) -> str:
    out = [f"COPY import_rows ({', '.join(STAGE_COLS)}) FROM STDIN WITH (FORMAT csv, HEADER true);",
           ",".join(STAGE_COLS)]
    out += [",".join(csv_field(v) for v in row) for row in rows]
    out.append("\\.")
    return "\n".join(out) + "\n"


# ── Database ─────────────────────────────────────────────────────────────────

def psql_cmd(args, extra: list[str]) -> list[str]:
    inner = ["psql", "-X", "-q", "-v", "ON_ERROR_STOP=1", "-P", "pager=off"] + extra
    if args.db_url:
        inner += ["-d", args.db_url]
    if args.ssh:
        remote = shlex.join((["sudo", "-u", args.sudo_user] if args.sudo_user else []) + inner)
        return ["ssh", "-o", "BatchMode=yes", args.ssh, f"cd /tmp && {remote}"]
    if args.sudo_user:
        return ["sudo", "-u", args.sudo_user] + inner
    return inner


def run_psql(args, sql: str, extra: list[str]) -> str:
    proc = subprocess.run(psql_cmd(args, extra), input=sql, capture_output=True, text=True)
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr)
        raise SystemExit(f"psql failed (exit {proc.returncode}); nothing was committed")
    return proc.stdout


def parse_sections(out: str) -> dict:
    sections, name, buf = {}, None, []
    for line in out.splitlines():
        if line.startswith("@@"):
            if name:
                sections[name] = buf
            name, buf = line[2:].strip(), []
        elif name:
            buf.append(line)
    if name:
        sections[name] = buf
    return {k: list(csv.DictReader(v)) if v else [] for k, v in sections.items()}


def looks_like_prod(args) -> bool:
    target = " ".join(x for x in (args.ssh, args.db_url) if x)
    return any(m in target for m in PROD_MARKERS)


def guard_out_dir(out: Path) -> None:
    for p in [out.resolve(), *out.resolve().parents]:
        if (p / ".git").exists():
            raise SystemExit(f"refusing to write the report into a git work tree ({p}): "
                             "it holds personal data. Pick an --out outside the repos.")


# ── Report ───────────────────────────────────────────────────────────────────

def truthy(v) -> bool:
    return str(v).lower() in ("t", "true")


def report(plan: Plan, program: dict, rows: list[dict], checks: dict, committed: bool,
           card: CardFit | None) -> tuple[list[str], list[dict]]:
    by_id = {r["id"]: r for r in plan.importable}
    lines: list[str] = []
    say = lines.append
    reward = int(program.get("cheapest_reward") or program.get("default_reward_cost") or 0)
    cap = None
    if truthy(program.get("balance_cap_enabled")):
        cap = int(program.get("balance_cap") or program.get("dearest_reward") or reward or 0)

    say(f"Legacy loyalty import into \"{program['org_name']}\" — "
        + ("APPLIED (committed)" if committed else "DRY RUN (rolled back, nothing written)"))
    say(f"Madar programme: \"{program['program_name']}\", {program['mode']} mode, a reward at "
        f"{reward} stamps, one stamp per {'item' if truthy(program['stamp_per_line_item']) else 'sale'}, "
        f"cap {cap if cap is not None else 'none'}, max rewards/order "
        f"{program.get('max_rewards_per_order') or 'unlimited'}; ledger branch "
        f"{program.get('ledger_branch_name')}")
    old = ", ".join(sorted(plan.programs)) or "unknown"
    if card:
        say(f"Old programme ({old}): the data fits a {card.size}-stamp card, one stamp per visit "
            f"(stamps == visits mod {card.size} on {card.fits} of {card.rows} rows). Madar's reward is at "
            f"{reward}: " + ("the SAME threshold." if card.size == reward else
                             "a DIFFERENT threshold. Balances are carried 1:1, no conversion applied."))
        for r in card.exceptions:
            say(f"  - does not follow it: {display_name(r)} ({mask(r.get('phone'))}): "
                f"{as_int(r.get('totalVisits'))} visits, {as_int(r.get('totalStamps'))} stamps "
                "(changed by hand in the old system?) — carried as the export states")
    else:
        say(f"Old programme ({old}): the data does not show the card size.")

    say("")
    say(f"Input: {len(plan.files)} file(s) {', '.join(plan.files)}; {plan.records_read} records, "
        f"{len(plan.unique)} unique old ids")
    if len(plan.merchants) > 1:
        say(f"  WARNING: records from {len(plan.merchants)} different old merchants")

    say(f"Duplicates within the input: {len(plan.exact_dupes)} identical copies of an id"
        f"{' (' + ', '.join(sorted({f for _, f in plan.exact_dupes})) + ')' if plan.exact_dupes else ''}; "
        f"{len(plan.conflicting_dupes)} ids with differing copies; "
        f"{len(plan.phone_dupes)} phones under more than one old id")
    for oid, kept, other, diff in plan.conflicting_dupes:
        say(f"  - id {oid[:8]}: kept the newer copy from {kept} over {other} (differs in {', '.join(diff)})")
    for key, recs in plan.phone_dupes.items():
        say(f"  - phone {mask(key)}: {len(recs)} old ids ({', '.join(r['id'][:8] for r in recs)}) "
            f"-> one Madar customer \"{recs[0]['_name']}\", each old card credited separately")

    left = sum(as_int(rec.get("totalStamps")) for rec, _ in plan.invalid)
    say(f"Skipped, invalid phone: {len(plan.invalid) + len(plan.no_id)} "
        f"({left} stamps on them are NOT carried)")
    for rec, why in plan.invalid:
        say(f"  - {display_name(rec) or '(no name)'} ({mask(rec.get('phone'))}): {why}; "
            f"old balance {as_int(rec.get('totalStamps'))} stamps, card {rec.get('cardStatus')}")
    for rec in plan.no_id:
        say(f"  - a record with no id ({mask(rec.get('phone'))})")
    if plan.normalised:
        say(f"Normalised to +20 from another written form (e.g. local 01...): {len(plan.normalised)}")
        for rec in plan.normalised:
            say(f"  - {rec['_name']} ({mask(rec['_key'])})")

    people: dict = {}
    for r in rows:
        people.setdefault(r["grp"], []).append(r)
    created = [g for g in people.values() if not truthy(g[0]["existed"])]
    matched = [g for g in people.values() if truthy(g[0]["existed"])]

    def stamps_of(g):
        return sum(int(r["stamps_credited"]) for r in g)

    say("")
    say(f"To create: {len(created)} customers, each enrolled; "
        f"{sum(stamps_of(g) for g in created)} stamps credited "
        f"({sum(1 for g in created if stamps_of(g) > 0)} with a balance, "
        f"{sum(1 for g in created if stamps_of(g) == 0)} with 0)")
    say(f"To match (phone already a Madar customer of this org): {len(matched)}")
    for g in matched:
        r0 = g[0]
        rec = by_id[r0["old_id"]]
        member = ("enrolled now" if truthy(r0["enrolled_now"]) else
                  "already a member" if r0["member_state"] == "live" else "membership ENDED, left alone")
        say(f"  - Madar \"{r0['existing_name']}\" / old \"{rec['_name']}\" ({mask(rec['_key'])}), {member}: "
            f"existing {r0['balance_before']} + old {stamps_of(g)} = {r0['balance_after']}"
            + ("; empty name filled from the old record" if truthy(r0["name_filled"]) else "")
            + ("; Madar pass marked for refresh" if truthy(r0["pass_marked_stale"]) else ""))
    already = [r for r in rows if r["credit_outcome"] == "already_imported"]
    say(f"Already imported earlier (not credited again): {len(already)}")
    for r in already:
        rec = by_id[r["old_id"]]
        now_says = as_int(rec.get("totalStamps"))
        say(f"  - {rec['_name']} ({mask(rec['_key'])}): {r['done_points']} stamps credited {r['done_at'][:16]}"
            + (f"; the export now says {now_says} (NOT topped up)" if str(now_says) != r["done_points"] else "")
            + ("; that credit sits on another customer now" if truthy(r["done_elsewhere"]) else ""))
    ended = [r for r in rows if r["credit_outcome"] == "membership_ended"]
    if ended:
        say(f"Not credited, membership ended in Madar: {len(ended)}")

    if reward:
        ready = [(g, int(g[0]["balance_after"])) for g in people.values()
                 if int(g[0]["balance_after"]) >= reward and stamps_of(g) > 0]
        say(f"Balances at or over the {reward}-stamp reward after import: {len(ready)} "
            "(a reward is claimable on their next visit)")
        for g, b in ready:
            rec = by_id[g[0]["old_id"]]
            say(f"  - {rec['_name']} ({mask(rec['_key'])}): {b}"
                + (f" — ABOVE the cap of {cap}: earning stops until they redeem" if cap and b > cap else ""))

    if card:
        k = card.size
        owed = [r for r in plan.importable if as_int(r.get("totalVisits")) >= k]
        say(f"Went round the old {k}-stamp card at least once: {len(owed)} customers, "
            f"{sum(as_int(r.get('totalVisits')) // k for r in owed)} rewards earned there. The export has no "
            "redemption record (completedPasses is 0 everywhere): check the old provider for unclaimed rewards.")
        for r in owed:
            say(f"  - {r['_name']} ({mask(r['_key'])}): {as_int(r.get('totalVisits'))} visits, "
                f"{as_int(r.get('totalStamps'))} stamps now, {as_int(r.get('totalVisits')) // k} reward(s) earned")

    imp = plan.importable
    allrecs = list(plan.unique.values())
    say("")
    say(f"Wallet: {sum(1 for r in allrecs if r.get('cardStatus') == 'in_wallet')} of {len(allrecs)} "
        f"have the OLD card in a wallet ({sum(1 for r in imp if r.get('cardStatus') == 'in_wallet')} of the "
        f"{len(imp)} imported). It cannot be moved: they need the new Madar card (nothing was sent).")
    say(f"Not stored (Madar has no field): emails ({sum(1 for r in allrecs if r.get('email'))} in the export); "
        f"visits (total {sum(as_int(r.get('totalVisits')) for r in imp)} across the imported) and last visit "
        "— both are in the CSV only.")
    say("")
    say(f"Totals: {len(created)} created + {len(matched)} matched = {len(people)} people from "
        f"{len(rows)} old ids; {sum(int(r['stamps_credited']) for r in rows)} stamps credited in "
        f"{sum(1 for r in rows if int(r['stamps_credited']) > 0)} ledger rows")
    say(f"  customers {program['customers_before']} -> {checks['customers_after']}, members "
        f"{program['members_before']} -> {checks['members_after']}, stamps outstanding "
        f"{program['stamps_before']} -> {checks['stamps_after']}")
    say(f"  balance vs ledger mismatches: {checks['ledger_mismatches']}; people not visible as members: "
        f"{checks['people_not_visible']}; import-marked ledger rows in the org: {checks['marker_rows']}")

    csv_rows = []
    for r in rows:
        rec = by_id[r["old_id"]]
        csv_rows.append({
            "old_id": r["old_id"], "old_name": rec["_name"], "phone": "+" + rec["_key"],
            "action": ("match" if truthy(r["existed"]) else "create"),
            "member": ("enrolled" if truthy(r["enrolled_now"]) else r["member_state"]),
            "madar_customer_id": r["customer_id"], "madar_name_before": r["existing_name"],
            "credit": r["credit_outcome"], "stamps_credited": r["stamps_credited"],
            "balance_before": r["balance_before"], "balance_after": r["balance_after"],
            "old_stamps": as_int(rec.get("totalStamps")), "old_visits": as_int(rec.get("totalVisits")),
            "old_last_visit": rec.get("lastVisit") or "", "old_created_at": rec.get("createdAt") or "",
            "old_card_status": rec.get("cardStatus") or "", "email_in_export": bool(rec.get("email")),
            "reason": "", "source_file": plan.source.get(r["old_id"], ""),
        })
    for rec, why in plan.invalid:
        csv_rows.append({
            "old_id": rec.get("id"), "old_name": display_name(rec), "phone": clean_text(rec.get("phone")),
            "action": "skipped", "member": "", "madar_customer_id": "", "madar_name_before": "",
            "credit": "", "stamps_credited": 0, "balance_before": "", "balance_after": "",
            "old_stamps": as_int(rec.get("totalStamps")), "old_visits": as_int(rec.get("totalVisits")),
            "old_last_visit": rec.get("lastVisit") or "", "old_created_at": rec.get("createdAt") or "",
            "old_card_status": rec.get("cardStatus") or "", "email_in_export": bool(rec.get("email")),
            "reason": why, "source_file": plan.source.get(rec.get("id"), ""),
        })
    return lines, csv_rows


# ── Main ─────────────────────────────────────────────────────────────────────

def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("inputs", nargs="+")
    ap.add_argument("--org", default=DROPS_ORG)
    ap.add_argument("--db-url", default=os.environ.get("DATABASE_URL"))
    ap.add_argument("--ssh")
    ap.add_argument("--sudo-user")
    mode = ap.add_mutually_exclusive_group()
    mode.add_argument("--dry-run", action="store_true")
    mode.add_argument("--apply", action="store_true")
    ap.add_argument("--yes-prod", action="store_true")
    ap.add_argument("--out")
    args = ap.parse_args(argv)

    if args.org not in ALLOWED_ORGS:
        raise SystemExit(f"refusing org {args.org}: this import is only for {', '.join(ALLOWED_ORGS.values())}")
    if not args.db_url and not args.ssh:
        raise SystemExit("no database: pass --db-url (or set DATABASE_URL)")
    if args.apply and looks_like_prod(args) and not args.yes_prod:
        raise SystemExit("this looks like production; --apply there also needs --yes-prod")

    files = input_files(args.inputs)
    out_dir = Path(args.out) if args.out else files[0].resolve().parent.parent / "out"
    guard_out_dir(out_dir)

    plan = build_plan(load_records(files))
    if len(plan.merchants) > 1:
        raise SystemExit(f"the input mixes {len(plan.merchants)} old merchants; import one shop at a time")
    rows = stage_rows(plan)
    card = infer_card_size(list(plan.unique.values()))

    org_q = run_psql(args, f"SELECT name FROM organizations WHERE id = '{args.org}';\n", ["-At"]).strip()
    if not org_q:
        raise SystemExit(f"organisation {args.org} not found in that database")
    if args.apply:
        print(f"This COMMITS {len(rows)} old customers into \"{org_q}\" ({args.org}).", file=sys.stderr)
        answer = input(f"Type the org name \"{org_q}\" to continue: ")
        if answer.strip() != org_q:
            raise SystemExit("aborted")

    sql = SQL_FILE.read_text(encoding="utf-8")
    if COPY_MARKER not in sql:
        raise SystemExit("the SQL file has lost its COPY marker")
    sql = sql.replace(COPY_MARKER, copy_block(rows), 1)
    out = run_psql(args, sql, ["-v", f"org={args.org}", "-v", f"apply={'true' if args.apply else 'false'}"])
    sections = parse_sections(out)
    committed = "COMMITTED" in sections
    if args.apply and not committed or not args.apply and "ROLLED_BACK" not in sections:
        sys.stderr.write(out)
        raise SystemExit("the transaction did not end the way it should have; check the output above")
    program, checks = sections["PROGRAM"][0], sections["CHECKS"][0]
    if int(checks["ledger_mismatches"]) or int(checks["people_not_visible"]):
        print("WARNING: a check failed (see Totals)", file=sys.stderr)

    lines, csv_rows = report(plan, program, sections["ROWS"], checks, committed, card)
    out_dir.mkdir(parents=True, exist_ok=True)
    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    csv_path = out_dir / f"legacy-loyalty-{'apply' if committed else 'dry-run'}-{stamp}.csv"
    with open(csv_path, "w", newline="", encoding="utf-8") as fh:
        w = csv.DictWriter(fh, fieldnames=list(csv_rows[0].keys()) if csv_rows else ["old_id"])
        w.writeheader()
        w.writerows(csv_rows)
    print("\n".join(lines))
    print(f"\nPer-customer CSV: {csv_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
