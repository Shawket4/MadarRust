#!/bin/zsh
# Lift src/<path>/tests.rs into tests/<name>.rs as its own binary.
# $1 = module path under src (e.g. orders, menu/modifiers)  $2 = target name
set -e
M=$1; N=$2
SRC="src/$M/tests.rs"; DST="tests/$N.rs"
[ -f "$SRC" ] || { echo "no $SRC"; exit 1; }
git mv "$SRC" "$DST"
MODPATH=$(echo "$M" | sed 's#/#::#g')
perl -pi -e "s/\bsuper::/madar_rust::${MODPATH}::/g; s/\bcrate::/madar_rust::/g" "$DST"
perl -0pi -e "s/#\[cfg\(test\)\]\nmod tests;\n\n?//" "src/$M/mod.rs"
echo "moved $SRC -> $DST (module $MODPATH)"
