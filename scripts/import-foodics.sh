#!/usr/bin/env bash
# Import a Foodics menu export (categories, products, modifier groups + options,
# product↔group links) into one Madar org by direct SQL. Idempotent; --dry-run rolls back.
#
#   scripts/import-foodics.sh --org <org-uuid> [--dir drops] [--db URL] [--dry-run]
#                             [--replace-menu | --reset-org | --reset-activity]
#                             [--keep-stock] [--reset-devices] [--reset-tables]
#                             [--reset-loyalty] [--reset-qr] [--yes] [--yes-prod]
#
#   --replace-menu     hard-delete the org's menu (categories, items, modifier groups)
#                      AND every row referencing it — orders, tickets, recipes,
#                      overrides, loyalty — then seed.
#   --reset-org        hard-delete ALL of the org's data (branches, users, orders,
#                      inventory, menu, …), recreate the org row with the same id and
#                      settings, then seed. Nobody can log into the org afterwards.
#   --reset-activity   hard-delete only what the org DID — orders, tickets, tills and
#                      their movements/reconciliations, table occupancy, kitchen
#                      tickets, refunds, the inventory ledger (movements, stocktakes,
#                      receipts, purchase orders, transfers, waste; branch_stock is
#                      rebased to 0), customers + loyalty history, HR activity
#                      (attendance, payroll, requests), approvals, AI chats, menu
#                      decisions, the POS changefeed and the order/ticket counters.
#                      The setup is KEPT: branches, users/roles/permissions, devices,
#                      menu, recipes, the ingredient & supplier catalog, floor
#                      geometry, payment methods, discounts, schedules, settings.
#                      No CSV is read and nothing is imported in this mode.
#     With --reset-activity only (each adds to what is cleared, or changes it):
#   --keep-stock       carry today's stock levels over: after the ledger is cleared,
#                      each non-zero level is recorded as an opening 'stock_count'
#                      movement, so the counts stay and the history starts clean.
#                      Without it, every level goes to 0.
#   --reset-devices    also clear the org's devices: POS/KDS devices, their activation
#                      codes and payment-method bindings, push registrations and staff
#                      phones. Every till and phone has to be activated/signed in again.
#   --reset-tables     also clear the floor: floor sections and their tables.
#   --reset-loyalty    also clear the loyalty setup: settings, reward and earning items,
#                      birthday greetings, win-backs, pass cache and token aliases
#                      (members and their history already go with --reset-activity).
#   --reset-qr         also clear the org's QR short links.
#   All three ask you to type the org name unless --yes or --dry-run.
#
# Expects in --dir (Foodics console exports, renamed):
#   categories.csv, products.csv, product_modifiers.csv, modifier_groups.csv, modifier_options.csv
#   product_optionals.csv (OPTIONAL: product_sku,product_name,name,name_localized,price)
#
# A group whose `reference` is set is keyed by that reference, not its name, and
# the reference also picks its addon family: size | milk_type | coffee_type |
# extra. `size` is not a modifier group at all — its options become the item's
# menu_item_sizes, which is how Madar models sizes.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
dir="drops"
db="${DATABASE_URL:-}"
org=""
final="COMMIT"
yes_prod=0
replace_menu=off
reset_org=off
reset_activity=off
keep_stock=off
reset_devices=off
reset_tables=off
reset_loyalty=off
reset_qr=off
do_import=on
yes=0

usage() { sed -n '2,28p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-1}"; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --org) org="$2"; shift 2 ;;
    --dir) dir="$2"; shift 2 ;;
    --db) db="$2"; shift 2 ;;
    --dry-run) final="ROLLBACK"; shift ;;
    --yes-prod) yes_prod=1; shift ;;
    --replace-menu) replace_menu=on; shift ;;
    --reset-org) reset_org=on; shift ;;
    --reset-activity) reset_activity=on; do_import=off; shift ;;
    --keep-stock) keep_stock=on; shift ;;
    --reset-devices) reset_devices=on; shift ;;
    --reset-tables) reset_tables=on; shift ;;
    --reset-loyalty) reset_loyalty=on; shift ;;
    --reset-qr) reset_qr=on; shift ;;
    --yes) yes=1; shift ;;
    -h|--help) usage 0 ;;
    *) echo "unknown argument: $1" >&2; usage ;;
  esac
done

die() { echo "error: $*" >&2; exit 1; }

command -v psql >/dev/null || die "psql not found"
[[ -n "$db" ]] || die "set DATABASE_URL or pass --db"
[[ "$org" =~ ^[0-9a-fA-F-]{36}$ ]] || die "--org <uuid> is required"
modes=0
for m in "$replace_menu" "$reset_org" "$reset_activity"; do [[ "$m" == on ]] && modes=$((modes + 1)); done
[[ $modes -gt 1 ]] && die "use only one of --replace-menu, --reset-org, --reset-activity"
for m in "$keep_stock" "$reset_devices" "$reset_tables" "$reset_loyalty" "$reset_qr"; do
  [[ "$m" == on && "$reset_activity" != on ]] && die "--keep-stock/--reset-devices/--reset-tables/--reset-loyalty/--reset-qr need --reset-activity"
done
if [[ "$db" == *prod* && $yes_prod -eq 0 ]]; then
  die "database URL looks like production; re-run with --yes-prod if intended"
fi

# Header check (strips the UTF-8 BOM Foodics writes).
check_header() {
  local file="$dir/$1" want="$2" got
  [[ -f "$file" ]] || die "missing $file"
  got="$(head -1 "$file" | sed $'s/^\xEF\xBB\xBF//' | tr -d '\r')"
  [[ "$got" == "$want" ]] || die "$file header mismatch
  want: $want
  got:  $got"
}

cat_cols="id,name,name_localized,reference,image"
item_cols="id,name,sku,category_reference,tax_group_reference,is_sold_by_weight,is_active,is_stock_product,price,barcode,description,preparation_time,calories,walking_minutes_to_burn_calories,is_high_salt,image,name_localized,description_localized,ereceipt_item_type,ereceipt_item_code,ereceipt_unit_type"
addon_cols="product_name,product_name_localized,product_sku,modifier_name,modifier_name_localized,modifier_reference,minimum_options,maximum_options,free_options,default_options,unique_options"

group_cols="id,reference,name,name_localized"
option_cols="id,modifier_reference,modifier_name,modifier_name_localized,tax_group_reference,name,sku,price,calories,name_localized,is_active"
# Optional 4th file: item-private optionals (menu_item_optional_fields), the way
# the matcha drinks carry Honey / Condensed Milk / Vanilla. Absent → skipped.
optional_cols="product_sku,product_name,name,name_localized,price"

have_optionals=0
if [[ "$do_import" == on ]]; then
  check_header categories.csv "$cat_cols"
  check_header products.csv "$item_cols"
  check_header product_modifiers.csv "$addon_cols"
  check_header modifier_groups.csv "$group_cols"
  check_header modifier_options.csv "$option_cols"
  if [[ -f "$dir/product_optionals.csv" ]]; then
    check_header product_optionals.csv "$optional_cols"; have_optionals=1
  fi
fi

org_name="$(psql "$db" -Atc "SELECT name FROM organizations WHERE id = '$org'")" \
  || die "cannot query database"
[[ -n "$org_name" ]] || die "organization $org not found"

abs() { (cd "$(dirname "$1")" && printf '%s/%s' "$(pwd)" "$(basename "$1")"); }
ddl() { # table, comma-separated columns → CREATE TEMP TABLE with text columns
  local t="$1" cols; cols="$(sed 's/,/ text, /g' <<<"$2") text"
  printf 'CREATE TEMP TABLE %s (%s);\n' "$t" "$cols"
}
sq() { sed "s/'/''/g" <<<"$1"; }

mode="add"
[[ "$replace_menu" == on ]] && mode="replace-menu: the whole menu and every order/ticket that uses it"
[[ "$reset_org" == on ]] && mode="reset-org: ALL data of the org (branches, users, orders, inventory, menu)"
if [[ "$reset_activity" == on ]]; then
  mode="reset-activity: everything the org DID (orders, tickets, tills, inventory ledger, customers, HR activity)"
  extras=""
  [[ "$reset_devices" == on ]] && extras+=", devices"
  [[ "$reset_tables" == on ]] && extras+=", floor sections + tables"
  [[ "$reset_loyalty" == on ]] && extras+=", the loyalty setup"
  [[ "$reset_qr" == on ]] && extras+=", QR short links"
  [[ -n "$extras" ]] && mode+=" AND${extras#,}"
  if [[ "$keep_stock" == on ]]; then mode+=" — stock levels carried over"; else mode+=" — stock levels go to 0"; fi
fi

if [[ "$mode" != add && "$final" == COMMIT && $yes -eq 0 ]]; then
  echo "This permanently DELETES $mode" >&2
  read -r -p "Type the org name \"$org_name\" to continue: " answer
  [[ "$answer" == "$org_name" ]] || die "aborted"
fi

if [[ "$do_import" == on ]]; then
  echo "Importing $(abs "$dir") into org \"$org_name\" ($org) — ${final}, mode ${mode%%:*}"
else
  echo "Org \"$org_name\" ($org) — ${final}, mode ${mode%%:*} (no import)"
fi

{
  echo '\set ON_ERROR_STOP on'
  echo 'BEGIN;'
  if [[ "$do_import" == on ]]; then
    ddl stg_categories "$cat_cols"
    ddl stg_items "$item_cols"
    ddl stg_addons "$addon_cols"
    ddl stg_groups "$group_cols"
    ddl stg_options "$option_cols"
    ddl stg_optionals "$optional_cols"
    printf "\\\\copy stg_categories FROM '%s' WITH (FORMAT csv, HEADER true)\n" "$(sq "$(abs "$dir/categories.csv")")"
    printf "\\\\copy stg_items FROM '%s' WITH (FORMAT csv, HEADER true)\n" "$(sq "$(abs "$dir/products.csv")")"
    printf "\\\\copy stg_addons FROM '%s' WITH (FORMAT csv, HEADER true)\n" "$(sq "$(abs "$dir/product_modifiers.csv")")"
    printf "\\\\copy stg_groups FROM '%s' WITH (FORMAT csv, HEADER true)\n" "$(sq "$(abs "$dir/modifier_groups.csv")")"
    printf "\\\\copy stg_options FROM '%s' WITH (FORMAT csv, HEADER true)\n" "$(sq "$(abs "$dir/modifier_options.csv")")"
    if [[ $have_optionals -eq 1 ]]; then
      printf "\\\\copy stg_optionals FROM '%s' WITH (FORMAT csv, HEADER true)\n" "$(sq "$(abs "$dir/product_optionals.csv")")"
    fi
    # Keep file order so modifier and option sort follow the export.
    echo 'ALTER TABLE stg_addons ADD COLUMN ordinal bigserial;'
    echo 'ALTER TABLE stg_options ADD COLUMN ordinal bigserial;'
  fi
  cat "$here/import-foodics.sql"
} | psql "$db" -X -q -v ON_ERROR_STOP=1 -v org="$org" -v final="$final" \
    -v replace_menu="$replace_menu" -v reset_org="$reset_org" \
    -v reset_activity="$reset_activity" -v do_import="$do_import" \
    -v keep_stock="$keep_stock" -v reset_devices="$reset_devices" -v reset_tables="$reset_tables" \
    -v reset_loyalty="$reset_loyalty" -v reset_qr="$reset_qr"

if [[ "$final" == "ROLLBACK" ]]; then
  echo "Dry run: nothing was written."
fi
