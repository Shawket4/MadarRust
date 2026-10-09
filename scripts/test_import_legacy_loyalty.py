"""Tests for import-legacy-loyalty.py: the phone rule, reading exports, dedupe
and grouping. Every person here is invented.

    python3 -m unittest scripts/test_import_legacy_loyalty.py      (or pytest)
"""
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location("legacy_import", HERE / "import-legacy-loyalty.py")
imp = importlib.util.module_from_spec(_spec)
sys.modules[_spec.name] = imp  # dataclasses resolve their module by name
_spec.loader.exec_module(imp)

# The shared rule's vectors live in madar-shared (checked out beside this repo).
VECTORS = HERE.parent.parent / "madar-shared" / "crates" / "madar-ids" / "vectors" / "phone_vectors.json"


def person(pid, phone, name="Test Person", stamps=0, visits=0, updated="2026-01-02T00:00:00.000Z", **kw):
    first, _, last = name.partition(" ")
    rec = {
        "id": pid, "merchantId": "m-1", "email": None, "phone": phone,
        "firstName": first, "lastName": last, "displayName": name,
        "createdAt": "2026-01-01T10:00:00.000Z", "updatedAt": updated,
        "totalVisits": visits, "totalStamps": stamps, "totalPoints": 0,
        "activePasses": 1, "completedPasses": 0, "cardInstalled": True,
        "cardStatus": "in_wallet", "lastVisit": None, "lastVisitLocation": None,
        "programs": [{"id": "p-1", "name": "Invented Rewards"}],
    }
    rec.update(kw)
    return rec


class CanonicalTests(unittest.TestCase):
    @unittest.skipUnless(VECTORS.exists(), "madar-shared checkout not beside this repo")
    def test_the_shared_vectors_hold(self):
        v = json.loads(VECTORS.read_text(encoding="utf-8"))
        self.assertGreaterEqual(len(v["valid"]), 20)
        for raw, want in v["valid"]:
            self.assertEqual(imp.canonical(raw), want, raw)
            self.assertEqual(imp.canonical(want), want, want)
        for raw in v["invalid"]:
            self.assertIsNone(imp.canonical(raw), raw)

    def test_the_rule_by_hand(self):
        self.assertEqual(imp.canonical("+201001234567"), "201001234567")
        self.assertEqual(imp.canonical("0100 123 4567"), "201001234567")
        self.assertEqual(imp.canonical("٠١٠٠١٢٣٤٥٦٧"), "201001234567")
        self.assertIsNone(imp.canonical("+20100123456"))  # a mobile one digit short
        self.assertIsNone(imp.canonical("1" * 33))
        self.assertIsNone(imp.canonical(None))


class CheckPhoneTests(unittest.TestCase):
    def test_egyptian_mobiles_are_keyed_without_the_plus(self):
        for prefix in ("10", "11", "12", "15"):
            self.assertEqual(imp.check_phone(f"+20{prefix}00000001"), (f"20{prefix}00000001", None))

    def test_the_same_number_written_other_ways_gives_one_key(self):
        for raw in ("+20 100 000 0001", "01000000001", "00201000000001", "201000000001",
                    "(0100) 000-0001", "٠١٠٠٠٠٠٠٠٠١"):
            self.assertEqual(imp.check_phone(raw)[0], "201000000001", raw)

    def test_a_short_mobile_is_refused_not_guessed(self):
        key, why = imp.check_phone("+20100000001")  # +20 then 9 digits
        self.assertIsNone(key)
        self.assertIn("too short", why)
        key, why = imp.check_phone("0155000001")  # local, one digit short
        self.assertIsNone(key)
        self.assertIn("too short", why)

    def test_a_long_mobile_is_refused(self):
        key, why = imp.check_phone("+2010000000011")
        self.assertIsNone(key)
        self.assertIn("too long", why)

    def test_other_countries_are_refused(self):
        for raw in ("+17735550100", "+21620000000", "+966500000000"):
            key, why = imp.check_phone(raw)
            self.assertIsNone(key, raw)
            self.assertIn("not an Egyptian number", why)

    def test_a_landline_is_not_a_mobile(self):
        key, why = imp.check_phone("+20223456789")
        self.assertIsNone(key)
        self.assertIn("not an Egyptian mobile", why)

    def test_a_bare_number_says_no_country(self):
        key, why = imp.check_phone("1000000001")
        self.assertIsNone(key)
        self.assertIn("no country code", why)

    def test_empty_and_junk(self):
        self.assertEqual(imp.check_phone(None), (None, "no phone"))
        self.assertEqual(imp.check_phone("  "), (None, "no phone"))
        self.assertEqual(imp.check_phone("call me"), (None, "not a phone number"))

    def test_mask_shows_the_last_four_only(self):
        self.assertEqual(imp.mask("+201000000123"), "...0123")
        self.assertEqual(imp.mask(None), "(none)")


class ExportShapeTests(unittest.TestCase):
    def test_every_shape_yields_the_customers(self):
        a, b = person("a", "+201000000001"), person("b", "+201000000002")
        self.assertEqual(imp.extract_customers({"success": True, "data": {"customers": [a, b]}}), [a, b])
        self.assertEqual(imp.extract_customers({"customers": [a]}), [a])
        self.assertEqual(imp.extract_customers([a, b]), [a, b])
        pages = [{"data": {"customers": [a]}}, {"data": {"customers": [b]}}]
        self.assertEqual(imp.extract_customers(pages), [a, b])
        with self.assertRaises(ValueError):
            imp.extract_customers({"data": {"nothing": []}})

    def test_folders_read_every_json_once(self):
        with tempfile.TemporaryDirectory() as d:
            d = Path(d)
            (d / "page-1.json").write_text(json.dumps({"data": {"customers": [person("a", "+201000000001")]}}))
            (d / "page-2.json").write_text(json.dumps([person("b", "+201000000002")]))
            (d / "notes.txt").write_text("not an export")
            files = imp.input_files([d, d / "page-1.json"])
            self.assertEqual([f.name for f in files], ["page-1.json", "page-2.json"])
            self.assertEqual(len(imp.load_records(files)), 2)

    def test_names(self):
        self.assertEqual(imp.display_name(person("a", "x", name="  Mona   Ali ")), "Mona   Ali")
        rec = person("a", "x", name="Mona Ali", displayName="")
        self.assertEqual(imp.display_name(rec), "Mona Ali")
        self.assertEqual(imp.display_name(person("a", "x", name="Line\nBreak")), "Line Break")
        self.assertEqual(len(imp.display_name(person("a", "x", name="x" * 300))), imp.MAX_NAME)


class DedupeTests(unittest.TestCase):
    def plan(self, *recs):
        return imp.build_plan([(r, f"file-{i}.json") for i, r in enumerate(recs)])

    def test_the_same_id_in_two_files_is_one_customer(self):
        a = person("a", "+201000000001", stamps=3)
        p = self.plan(a, dict(a))
        self.assertEqual(len(p.unique), 1)
        self.assertEqual(len(p.exact_dupes), 1)
        self.assertEqual(len(p.importable), 1)

    def test_differing_copies_keep_the_newest(self):
        old = person("a", "+201000000001", stamps=2, updated="2026-01-01T00:00:00.000Z")
        new = person("a", "+201000000001", stamps=5, updated="2026-02-01T00:00:00.000Z")
        for order in ((old, new), (new, old)):
            p = self.plan(*order)
            self.assertEqual(p.unique["a"]["totalStamps"], 5)
            self.assertEqual(len(p.conflicting_dupes), 1)
            self.assertIn("totalStamps", p.conflicting_dupes[0][3])

    def test_one_phone_under_two_ids_is_one_person_with_both_cards(self):
        a = person("a", "+201000000001", name="Old Name", stamps=2, updated="2026-01-01T00:00:00.000Z")
        b = person("b", "01000000001", name="New Name", stamps=3, updated="2026-03-01T00:00:00.000Z")
        p = self.plan(a, b)
        self.assertEqual(list(p.groups), ["201000000001"])
        self.assertEqual([r["id"] for r in p.groups["201000000001"]], ["b", "a"])  # newest names them
        self.assertEqual(len(p.phone_dupes), 1)
        rows = imp.stage_rows(p)
        self.assertEqual([(r[0], r[1], r[2], r[6]) for r in rows], [("b", 1, True, 3), ("a", 1, False, 2)])
        self.assertEqual([r["id"] for r in p.normalised], ["b"])

    def test_invalid_phones_and_missing_names_are_skipped_with_a_reason(self):
        p = self.plan(person("a", "+20100000001"), person("b", None), person("c", "+201000000003", name=" ",
                      firstName="", lastName=""), person("d", "+201000000004"))
        self.assertEqual(sorted(r["id"] for r, _ in p.invalid), ["a", "b", "c"])
        self.assertEqual([r["id"] for r in p.importable], ["d"])
        reasons = {r["id"]: why for r, why in p.invalid}
        self.assertIn("too short", reasons["a"])
        self.assertEqual(reasons["b"], "no phone")
        self.assertEqual(reasons["c"], "no name")

    def test_a_record_without_an_id_is_not_imported(self):
        p = self.plan(person("", "+201000000001"))
        self.assertEqual(len(p.no_id), 1)
        self.assertEqual(p.importable, [])

    def test_groups_are_numbered_by_phone_so_a_rerun_stages_the_same(self):
        p1 = self.plan(person("x", "+201500000002"), person("y", "+201000000001"))
        p2 = self.plan(person("y", "+201000000001"), person("x", "+201500000002"))
        self.assertEqual(imp.stage_rows(p1), imp.stage_rows(p2))


class CardSizeTests(unittest.TestCase):
    def test_a_ten_stamp_card_is_read_off_the_data(self):
        recs = [person(str(i), "x", visits=v, stamps=v % 10) for i, v in enumerate([1, 4, 9, 12, 25, 53])]
        recs.append(person("odd", "x", visits=1, stamps=6))  # stamped by hand
        fit = imp.infer_card_size(recs)
        self.assertEqual((fit.size, fit.fits, fit.rows), (10, 6, 7))
        self.assertEqual([r["id"] for r in fit.exceptions], ["odd"])

    def test_nobody_went_round_so_the_size_is_unknown(self):
        recs = [person(str(i), "x", visits=v, stamps=v) for i, v in enumerate([0, 1, 2, 3])]
        self.assertIsNone(imp.infer_card_size(recs))


class CopyStreamTests(unittest.TestCase):
    def test_fields(self):
        self.assertEqual(imp.csv_field(None), "")
        self.assertEqual(imp.csv_field(""), '""')  # an empty string is not NULL
        self.assertEqual(imp.csv_field('Say "hi", ok'), '"Say ""hi"", ok"')
        self.assertEqual(imp.csv_field(7), "7")
        self.assertEqual(imp.csv_field(True), "true")

    def test_the_block_ends_the_copy(self):
        p = imp.build_plan([(person("a", "+201000000001", stamps=1), "f.json")])
        block = imp.copy_block(imp.stage_rows(p))
        self.assertTrue(block.startswith("COPY import_rows ("))
        self.assertTrue(block.endswith("\\.\n"))


class GuardTests(unittest.TestCase):
    def test_the_report_never_lands_in_a_git_work_tree(self):
        with tempfile.TemporaryDirectory() as d:
            (Path(d) / ".git").mkdir()
            with self.assertRaises(SystemExit):
                imp.guard_out_dir(Path(d) / "out")
        with tempfile.TemporaryDirectory() as d:
            imp.guard_out_dir(Path(d) / "out")  # no .git: fine

    def test_only_the_drops_org(self):
        with self.assertRaises(SystemExit) as e:
            imp.main(["nowhere.json", "--org", "00000000-0000-0000-0000-000000000001", "--db-url", "x"])
        self.assertIn("refusing org", str(e.exception))

    def test_apply_on_production_needs_the_extra_flag(self):
        with self.assertRaises(SystemExit) as e:
            imp.main(["nowhere.json", "--apply", "--ssh", "root@187.124.33.153", "--db-url", "madar"])
        self.assertIn("--yes-prod", str(e.exception))

    def test_the_sql_still_has_its_copy_marker_and_org_guard(self):
        sql = imp.SQL_FILE.read_text(encoding="utf-8")
        self.assertIn(imp.COPY_MARKER, sql)
        self.assertIn(imp.DROPS_ORG, sql)
        self.assertIn("SET LOCAL ROLE madar_app", sql)


if __name__ == "__main__":
    unittest.main()
