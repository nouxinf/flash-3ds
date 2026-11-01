#!/usr/bin/env bash
# Canonical build environment for flash3ds.
#
# The Freebuff shell and any terminal opened before the devkitPro install can
# have a stale PATH, so every path is set explicitly here instead of inherited.
# Source this file, then run the build commands.

export DEVKITPRO="${DEVKITPRO:-C:/devkitPro}"

# An inherited DEVKITPRO is not necessarily usable: an editor or launcher can
# export its own bundled copy that has no devkitARM, and every later step then
# fails inside ctru-sys with "the system cannot find the path specified". Fall
# back to the normal install when that happens.
# `-d` needs a path this shell understands: a Windows-style "C:/..." path is not
# a directory here, so the check below silently passed and the stale value won.
dkp_ok() {
  [ -z "$1" ] && return 1
  [ -d "$1" ] && return 0
  [ -d "$(cygpath -u "$1" 2>/dev/null)" ]
}

if ! dkp_ok "$DEVKITPRO/devkitARM" && dkp_ok "C:/devkitPro/devkitARM"; then
  echo "note: ignoring DEVKITPRO=$DEVKITPRO (no devkitARM there), using C:/devkitPro" >&2
  export DEVKITPRO="C:/devkitPro"
fi

# DEVKITARM can be stale for the same reason, and ctru-sys only finds out when it
# tries to run the compiler, so it is re-derived from DEVKITPRO when missing.
if ! dkp_ok "$DEVKITARM"; then
  echo "note: ignoring DEVKITARM=$DEVKITARM (not found), using $DEVKITPRO/devkitARM" >&2
  export DEVKITARM="$DEVKITPRO/devkitARM"
fi
export DEVKITARM="${DEVKITARM:-$DEVKITPRO/devkitARM}"

LLVM_BIN="C:/Program Files/LLVM/bin"

# PATH is colon-separated, so it needs MSYS-style paths. A Windows path such as
# C:/devkitPro contains a colon and would silently break every later entry.
export PATH="/c/devkitPro/devkitARM/bin:/c/devkitPro/tools/bin:/c/Program Files/LLVM/bin:$PATH"

# bindgen and devkitPro's own tools expect Windows-style paths in the env vars.
export LIBCLANG_PATH="$LLVM_BIN"

# Total RAM budget for the app when running under hbmenu / a CIA.
# Kept in one place so the build, the docs and the runtime checks agree.
export FLASH3DS_HEAP_MB="${FLASH3DS_HEAP_MB:-48}"

log_env() {
  echo "DEVKITPRO=$DEVKITPRO"
  echo "DEVKITARM=$DEVKITARM"
  echo "LIBCLANG_PATH=$LIBCLANG_PATH"
  command -v arm-none-eabi-gcc || echo "WARNING: arm-none-eabi-gcc not on PATH"
  command -v 3dsxtool || echo "WARNING: 3dsxtool not on PATH"
}
