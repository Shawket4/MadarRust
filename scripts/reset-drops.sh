#!/usr/bin/env bash
# Reset the Drops org on production back to its setup, in one command.
#
#   scripts/reset-drops.sh            dry run on production: prints exactly what
#                                     would be deleted, then rolls back
#   scripts/reset-drops.sh --apply    full-database backup on the server, then the
#                                     real reset (you type the org name to confirm)
#
# It runs scripts/import-foodics.sh --reset-activity with the extras the owner
# chose (2026-09-25/26): --keep-stock --reset-devices --reset-tables
# --reset-loyalty --reset-qr. So:
#   DELETED  orders, tickets, tills, refunds, bookings, the inventory ledger
#            (today's stock levels carried over as opening counts), customers
#            and loyalty members + history, loyalty setup, QR short links,
#            devices (every till and staff phone signs in again), floor
#            sections + tables, and all Dawam activity: attendance, requests,
#            leave balances, payroll + payslips, advances, the rota.
#   KEPT     branches, users/roles/permissions, the menu, recipes, ingredients,
#            suppliers, payment methods, discounts, settings, and the Dawam
#            setup: employees (salary, pay method, app access), their salary
#            history, branches and documents, departments, leave types,
#            holidays, shift templates.
#
# Env overrides: MADAR_PROD_SSH (root@187.124.33.153), DROPS_ORG (the Drops id).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
host="${MADAR_PROD_SSH:-root@187.124.33.153}"
org="${DROPS_ORG:-27b8f8db-fec2-4909-b9f6-9fffbd860a1a}"
apply=0

case "${1:-}" in
  "") ;;
  --apply) apply=1 ;;
  -h|--help) sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
  *) echo "unknown argument: $1 (use --apply, or nothing for a dry run)" >&2; exit 1 ;;
esac

flags=(--reset-activity --keep-stock --reset-devices --reset-tables --reset-loyalty --reset-qr)
stamp="$(date -u +%Y%m%dT%H%M%SZ)"
dir="/tmp/reset-drops-$stamp"

echo "Drops ($org) on $host: $([[ $apply -eq 1 ]] && echo 'REAL RESET' || echo 'dry run')"
ssh -o ConnectTimeout=15 "$host" "mkdir -p '$dir'"
scp -q "$here/import-foodics.sh" "$here/import-foodics.sql" "$host:$dir/"
cleanup() { ssh -o ConnectTimeout=15 "$host" "rm -rf '$dir'" </dev/null || true; }
trap cleanup EXIT

if [[ $apply -eq 0 ]]; then
  ssh -o ConnectTimeout=15 "$host" \
    "chmod 755 '$dir'/import-foodics.sh && cd '$dir' && sudo -u postgres ./import-foodics.sh --org $org --db postgresql:///madar ${flags[*]} --dry-run" </dev/null
  echo
  echo "Dry run only: nothing changed. Run with --apply to reset for real."
  exit 0
fi

backup="/root/backups/madar-pre-drops-reset-$stamp.dump"
echo "Backing up the whole database to $backup ..."
ssh -o ConnectTimeout=15 "$host" \
  "mkdir -p /root/backups && sudo -u postgres pg_dump -Fc madar > '$backup' && ls -lh '$backup'" </dev/null
# -t: import-foodics.sh asks for the org name to be typed before it deletes.
ssh -t -o ConnectTimeout=15 "$host" \
  "chmod 755 '$dir'/import-foodics.sh && cd '$dir' && sudo -u postgres ./import-foodics.sh --org $org --db postgresql:///madar ${flags[*]}"
echo
echo "Done. Restore point on the server: $backup"
