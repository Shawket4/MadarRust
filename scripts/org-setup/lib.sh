# Sourced by the org-setup scripts: the questions a script asks when it is run
# with no arguments. Every answer lands in the variable named first.

# The MadarRust checkout: menu and data folders (drops/, pachamama/) live here,
# so a folder name answered from any directory is found.
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
prod_ssh="${MADAR_PROD_SSH:-root@187.124.33.153}"
# Production Postgres over Tailscale; its pg_hba lets the owner's Mac in as madar.
prod_db="${MADAR_PROD_DB:-postgresql://madar@100.101.100.57:5432/madar}"

# ask VAR "Question" [default]: a blank answer takes the default; no default = required.
ask() {
  local _var=$1 _q=$2 _def=${3-} _a
  while :; do
    read -r -p "$_q${_def:+ [$_def]}: " _a || exit 1
    _a=${_a:-$_def}
    [[ -n $_a ]] && break
    echo "  (required)" >&2
  done
  printf -v "$_var" '%s' "$_a"
}

# ask_optional VAR "Question": a blank answer is kept blank.
ask_optional() {
  local _a
  read -r -p "$2 (Enter to skip): " _a || exit 1
  printf -v "$1" '%s' "$_a"
}

# ask_secret VAR "Question": not echoed; required.
ask_secret() {
  local _a
  while :; do
    read -r -s -p "$2: " _a || exit 1
    echo >&2
    [[ -n $_a ]] && break
  done
  printf -v "$1" '%s' "$_a"
}

# confirm "Question": y/N, true on yes.
confirm() {
  local _a
  read -r -p "$1 [y/N]: " _a || exit 1
  [[ $_a == [yY]* ]]
}

# choose VAR "Question" "option" ...: VAR gets the number picked (1-based).
choose() {
  local _var=$1 _q=$2 _i=1 _o _a
  shift 2
  echo "$_q" >&2
  for _o in "$@"; do echo "  $_i) $_o" >&2; _i=$((_i + 1)); done
  while :; do
    read -r -p "Pick 1-$#: " _a || exit 1
    [[ $_a =~ ^[0-9]+$ ]] && ((_a >= 1 && _a <= $#)) && break
  done
  printf -v "$_var" '%s' "$_a"
}

# pick_db: sets db (a psql connection string) and is_prod. For production the
# madar password is read from the box's .env (URL-encoded there) into PGPASSWORD.
pick_db() {
  local n pw
  choose n "Which database?" "Production" "Local (\$DATABASE_URL, else madar on localhost)" "Another connection string"
  is_prod=0
  case $n in
    1)
      db=$prod_db is_prod=1
      pw="$(ssh -o ConnectTimeout=15 "$prod_ssh" 'grep -h "^DATABASE_URL" /opt/madar-rust/.env' </dev/null |
        sed -E 's#^[^:]+://[^:]+:([^@]+)@.*#\1#' |
        python3 -I -c 'import sys, urllib.parse; print(urllib.parse.unquote(sys.stdin.read().strip()))')"
      [[ -n $pw ]] || { echo "error: could not read the database password from $prod_ssh" >&2; exit 1; }
      export PGPASSWORD=$pw
      ;;
    2) db=${DATABASE_URL:-postgresql://localhost:5432/madar} ;;
    3) ask db "Connection string" ;;
  esac
  psql "$db" -Atqc 'SELECT 1' >/dev/null || { echo "error: cannot connect to $db" >&2; exit 1; }
}

# pick_org: an organization by name, short name or id; sets org and org_name.
pick_org() {
  local q rows
  while :; do
    ask q "Organization (name, short name or id)"
    rows="$(psql "$db" -Atq -F $'\t' -v q="$q" <<'SQL'
SELECT id, name FROM organizations
WHERE deleted_at IS NULL AND (id::text = :'q' OR slug = lower(:'q') OR name ILIKE :'q');
SQL
)"
    case "$(grep -c . <<<"$rows")" in
      1) org=${rows%%$'\t'*} org_name=${rows#*$'\t'}; echo "  → $org_name ($org)" >&2; return ;;
      0) echo "  nothing matches \"$q\"" >&2 ;;
      *) echo "  more than one matches; type the id:" >&2; sed 's/^/    /' <<<"$rows" >&2 ;;
    esac
  done
}

# backup_prod LABEL: a full pg_dump on the server, before a real write to production.
backup_prod() {
  local f
  f="/root/backups/madar-pre-$1-$(date -u +%Y%m%dT%H%M%SZ).dump"
  echo "Backing up production to $f ..." >&2
  ssh -o ConnectTimeout=15 "$prod_ssh" \
    "mkdir -p /root/backups && sudo -u postgres pg_dump -Fc madar > '$f' && ls -lh '$f'" </dev/null
}
