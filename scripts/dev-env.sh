# Faster Rust builds: a shared compile cache (sccache) and, on Linux, a fast
# linker (mold). Source it in the shell that runs cargo:
#
#   . scripts/dev-env.sh
#
# Each part turns on only when its tool is installed, so on a machine without
# them this does nothing and cargo behaves as before. Nothing here is committed
# into .cargo/config.toml on purpose: a missing wrapper or linker there would
# break every build (CI, Docker, a fresh laptop) instead of just being slower.
#
# Install: `cargo install --locked sccache` (or the release binary), and on
# Linux `apt install mold clang` (or mold's release tarball). macOS needs no
# linker change: Apple's ld is already fast; sccache still helps.

if command -v sccache >/dev/null 2>&1; then
  export RUSTC_WRAPPER="$(command -v sccache)"
  # sccache cannot cache incremental builds; the shared cache wins once more
  # than one worktree or agent builds the same dependencies.
  export CARGO_INCREMENTAL=0
  export SCCACHE_CACHE_SIZE="${SCCACHE_CACHE_SIZE:-40G}"
fi

# Host target only (CARGO_TARGET_<triple>_*), never RUSTFLAGS: Android/iOS
# cross builds (cargokit, the POS) must keep their own linkers.
if [ "$(uname -s)" = Linux ] && command -v mold >/dev/null 2>&1 && command -v clang >/dev/null 2>&1; then
  case "$(uname -m)" in
    x86_64)
      export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=clang
      export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=-fuse-ld=$(command -v mold)"
      ;;
    aarch64)
      export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=clang
      export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=-fuse-ld=$(command -v mold)"
      ;;
  esac
fi
