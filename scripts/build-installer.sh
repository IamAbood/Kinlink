#!/bin/sh
# Builds dist/Kinlink-Setup.exe (needs Inno Setup 6: winget install JRSoftware.InnoSetup).
set -e
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
VER=$(grep -m1 '^version' native/Cargo.toml | cut -d'"' -f2)
(cd native && cargo build --release)
ISCC="$LOCALAPPDATA/Programs/Inno Setup 6/ISCC.exe"
[ -f "$ISCC" ] || ISCC="/c/Program Files (x86)/Inno Setup 6/ISCC.exe"
"$ISCC" //Q "//DAppVersion=$VER" installer/ParsecClone.iss
echo "built dist/Kinlink-Setup.exe ($VER)"
