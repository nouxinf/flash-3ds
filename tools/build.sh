#!/usr/bin/env bash
# Builds flash3ds: .3dsx always, .cia when makerom and bannertool are present.
set -euo pipefail
cd "$(dirname "$0")/.."
source tools/env.sh

python tools/make_assets.py
[ -f assets/sample.swf ] || python tools/gen_swf.py assets/sample.swf

cargo 3ds build --release

ELF="target/armv6k-nintendo-3ds/release/flash3ds.elf"
mkdir -p dist
SMDH="dist/flash3ds.smdh"
smdhtool --create "flash3ds" "Flash (SWF) player for 3DS" "nouxinf" assets/icon.png "$SMDH"
3dsxtool "$ELF" dist/flash3ds.3dsx --smdh="$SMDH"
cp assets/sample.swf dist/sample.swf

# The D-pad test movie ships with the build so input can be checked on a console
# without running the generator by hand.
python tools/gen_avm1.py .tmp/avm1 > /dev/null
cp .tmp/avm1/keys.swf dist/keys.swf

MAKEROM="${MAKEROM:-/e/Installers/makerom-v0.19.0-win_x86_64/makerom.exe}"
BANNERTOOL="${BANNERTOOL:-/e/Installers/bannertool-1.2.3-windows/bannertool-1.2.3-windows/bannertool.exe}"

if [ -f "$MAKEROM" ] && [ -f "$BANNERTOOL" ]; then
  "$BANNERTOOL" makebanner -i assets/banner.png -a assets/banner.wav -o dist/banner.bin
  "$MAKEROM" -f cia -o dist/flash3ds.cia -rsf tools/flash3ds.rsf -target t \
    -exefslogo -elf "$ELF" -icon "$SMDH" -banner dist/banner.bin -desc app:4
  echo "dist/flash3ds.cia ready (install with FBI)"
else
  echo "makerom/bannertool not found - skipped the CIA, dist/flash3ds.3dsx is ready"
fi

ls -l dist
