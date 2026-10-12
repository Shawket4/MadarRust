#!/usr/bin/env bash
# Seed an org's INGREDIENT CATALOG (no stock levels) from a cleaned CSV, and
# wire the milk / coffee-bean swap options to their ingredients.
#
#   scripts/org-setup/import-foodics-ingredients.sh --org <org-uuid> [--dir drops_clean] [--db URL] [--dry-run] [--yes-prod]
#
# Expects <dir>/ingredients.csv: sku,name,name_ar,unit,category,swap_key
# (generate it with scripts/org-setup/clean-foodics-inventory-export.py).
#
# Run it with NO arguments to be asked for everything: it dry-runs, then offers
# the real run (backup first on prod).
#
# Catalog only: no branch_stock, no movements, so every ingredient shows up in
# the first stocktake at book stock 0 and the count sets reality.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
dir="drops_clean"; db="${DATABASE_URL:-}"; org=""; final="COMMIT"; yes_prod=0
wizard=0
[[ $# -eq 0 ]] && wizard=1
while [[ $# -gt 0 ]]; do
  case "$1" in
    --org) org="$2"; shift 2 ;;
    --dir) dir="$2"; shift 2 ;;
    --db) db="$2"; shift 2 ;;
    --dry-run) final="ROLLBACK"; shift ;;
    --yes-prod) yes_prod=1; shift ;;
    -h|--help) sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 1 ;;
  esac
done
die() { echo "error: $*" >&2; exit 1; }
command -v psql >/dev/null || die "psql not found"
if [[ $wizard -eq 1 ]]; then
  . "$here/lib.sh"
  pick_db
  pick_org
  ask dir "Folder with ingredients.csv" "$dir"
  [[ -d $dir ]] || dir="$repo/$dir"
  final=ROLLBACK yes_prod=1
fi
[[ -n "$db" ]] || die "set DATABASE_URL or pass --db"
[[ "$org" =~ ^[0-9a-fA-F-]{36}$ ]] || die "--org <uuid> is required"
if [[ "$db" == *prod* && $yes_prod -eq 0 ]]; then
  die "database URL looks like production; re-run with --yes-prod if intended"
fi
file="$dir/ingredients.csv"
[[ -f "$file" ]] || die "missing $file"
want="sku,name,name_ar,unit,category,swap_key"
got="$(head -1 "$file" | sed $'s/^\xEF\xBB\xBF//' | tr -d '\r')"
[[ "$got" == "$want" ]] || die "$file header mismatch
  want: $want
  got:  $got"
org_name="$(psql "$db" -Atc "SELECT name FROM organizations WHERE id = '$org'")" || die "cannot query database"
[[ -n "$org_name" ]] || die "organization $org not found"
abs() { (cd "$(dirname "$1")" && printf '%s/%s' "$(pwd)" "$(basename "$1")"); }
run() {
  echo "Seeding ingredients from $(abs "$file") into org \"$org_name\" ($org) — $final"
  {
    echo '\set ON_ERROR_STOP on'
    echo 'BEGIN;'
    echo 'CREATE TEMP TABLE stg_ing (sku text, name text, name_ar text, unit text, category text, swap_key text);'
    printf "\\\\copy stg_ing FROM '%s' WITH (FORMAT csv, HEADER true)\n" "$(abs "$file" | sed "s/'/''/g")"
    cat "$here/import-foodics-ingredients.sql"
  } | psql "$db" -X -q -v ON_ERROR_STOP=1 -v org="$org" -v final="$final"
  [[ "$final" == "ROLLBACK" ]] && echo "Dry run: nothing was written."
  return 0
}

run
if [[ $wizard -eq 1 ]]; then
  echo
  confirm "Do it for real on \"$org_name\"?" || { echo "Nothing changed."; exit 0; }
  [[ $is_prod -eq 1 ]] && backup_prod "import-ingredients"
  final=COMMIT
  run
fi
