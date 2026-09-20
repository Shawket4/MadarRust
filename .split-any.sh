#!/bin/zsh
# Lift any src test file into its own integration binary.
# $1 = path under src (e.g. tills/followup_tests.rs, e2e_tests.rs)
set -e
REL=$1
SRC="src/$REL"
[ -f "$SRC" ] || { echo "no $SRC"; exit 1; }
DIR=$(dirname "$REL"); BASE=$(basename "$REL" .rs)
if [ "$DIR" = "." ]; then MODPATH=""; PARENT="src/lib.rs"; NAME="$BASE";
else MODPATH=$(echo "$DIR" | sed 's#/#::#g'); PARENT="src/$DIR/mod.rs"; NAME=$(echo "${DIR}_${BASE}" | sed 's#/#_#g'); fi
DST="tests/$NAME.rs"
git mv "$SRC" "$DST"
if [ -n "$MODPATH" ]; then perl -pi -e "s/\bsuper::/madar_rust::${MODPATH}::/g" "$DST"; fi
perl -pi -e "s/\bcrate::/madar_rust::/g" "$DST"
# drop the module declaration (with any cfg(test) above it)
perl -0pi -e "s/#\[cfg\(test\)\]\n(pub )?mod ${BASE};\n//" "$PARENT"
echo "$SRC -> $DST"
