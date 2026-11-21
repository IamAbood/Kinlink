#!/bin/sh
# Builds dist/Kinlink.apk. Needs: Android SDK + NDK 26, JDK 17, cargo-apk, rustup target aarch64-linux-android.
set -e
L="$(cygpath -u "$LOCALAPPDATA" 2>/dev/null || echo "$LOCALAPPDATA")"
export ANDROID_HOME="$L/Android/Sdk"
export ANDROID_NDK_ROOT="$ANDROID_HOME/ndk/26.3.11579264"
export JAVA_HOME="$L/android-dev/jdk"
export PATH="$JAVA_HOME/bin:$HOME/.cargo/bin:$PATH"
KS="$L/android-dev/pclone-release.keystore"
# Keystore password: KINLINK_KEYSTORE_PASSWORD, or a local file outside the repository.
PASS="${KINLINK_KEYSTORE_PASSWORD:-$(cat "$L/android-dev/keystore-password.txt" 2>/dev/null)}"
[ -n "$PASS" ] || { echo "set KINLINK_KEYSTORE_PASSWORD (or create $L/android-dev/keystore-password.txt)"; exit 1; }
if [ ! -f "$KS" ]; then
  keytool -genkeypair -keystore "$KS" -storepass "$PASS" -keypass "$PASS" -alias pclone \
    -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=Parsec Clone"
fi
export CARGO_APK_RELEASE_KEYSTORE="$KS" CARGO_APK_RELEASE_KEYSTORE_PASSWORD="$PASS"
cd "$(dirname "$0")/../android"
cargo apk build --release
mkdir -p ../dist && cp target/release/apk/Kinlink.apk ../dist/Kinlink.apk
echo "built dist/Kinlink.apk"
