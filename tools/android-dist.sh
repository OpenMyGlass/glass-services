#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Builds glass-services for Android (4.4 and 6.0, armeabi-v7a, API 19) into a directory that
# goes to <aosp>/vendor/openmyglass/glass-services (android/README.md):
#   tools/android-dist.sh <out-dir>
# Needs: Rust with the armv7-linux-androideabi target; ANDROID_NDK_HOME = NDK r25c (the last
# NDK with API 19); ANDROID_HOME with platforms/android-19 and build-tools/$BUILD_TOOLS; a JDK
# (javac, keytool). The APK is signed with a throwaway key made here: AOSP signs it again with
# the platform key (LOCAL_CERTIFICATE := platform).
set -eu

if [ $# -ne 1 ]; then
    echo "usage: $0 <out-dir>" >&2
    exit 2
fi
out=$1
here=$(cd "$(dirname "$0")/.." && pwd)
API=19
BUILD_TOOLS=${BUILD_TOOLS:-34.0.0}
: "${ANDROID_NDK_HOME:?set ANDROID_NDK_HOME to NDK r25c}"
: "${ANDROID_HOME:?set ANDROID_HOME to the Android SDK}"
jar=$ANDROID_HOME/platforms/android-$API/android.jar
bt=$ANDROID_HOME/build-tools/$BUILD_TOOLS
for f in "$jar" "$bt/aapt2" "$bt/d8" "$bt/zipalign" "$bt/apksigner"; do
    [ -e "$f" ] || { echo "error: missing $f" >&2; exit 1; }
done
case "$(sed -n 's/^Pkg.Revision *= *//p' "$ANDROID_NDK_HOME/source.properties")" in
    25.2.*) ;;
    *) echo "error: $ANDROID_NDK_HOME is not NDK r25c (25.2.x)" >&2; exit 1 ;;
esac
host=$(uname -s | tr '[:upper:]' '[:lower:]')-x86_64
llvm=$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/$host/bin
[ -d "$llvm" ] || { echo "error: no NDK toolchain at $llvm" >&2; exit 1; }

# glassd, API 19, so one binary runs on both Android tracks.
export CARGO_TARGET_ARMV7_LINUX_ANDROIDEABI_LINKER="$llvm/armv7a-linux-androideabi$API-clang"
export CC_armv7_linux_androideabi="$llvm/armv7a-linux-androideabi$API-clang"
export AR_armv7_linux_androideabi="$llvm/llvm-ar"
(cd "$here" && cargo build --release --locked --target armv7-linux-androideabi -p glassd)
bin=$here/target/armv7-linux-androideabi/release/glassd

# The glue APK: javac against android-19, d8, aapt2, zipalign, apksigner.
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
glue=$here/android/glue
mkdir -p "$work/classes" "$work/dex"
javac -source 8 -target 8 -Xlint:-options -encoding UTF-8 -bootclasspath "$jar" \
    -d "$work/classes" $(find "$glue/src" -name '*.java')
"$bt/d8" --release --min-api $API --lib "$jar" --output "$work/dex" $(find "$work/classes" -name '*.class')
"$bt/aapt2" link --manifest "$glue/AndroidManifest.xml" -I "$jar" \
    --min-sdk-version $API --target-sdk-version 22 -o "$work/unaligned.apk"
(cd "$work/dex" && zip -q -X "$work/unaligned.apk" classes.dex)
"$bt/zipalign" -f 4 "$work/unaligned.apk" "$work/aligned.apk"
keytool -genkeypair -keystore "$work/test.jks" -storepass android -keypass android \
    -alias test -keyalg RSA -keysize 2048 -validity 1 -dname "CN=glass-services test key" >/dev/null 2>&1
"$bt/apksigner" sign --ks "$work/test.jks" --ks-pass pass:android --min-sdk-version $API \
    --out "$work/GlassServicesGlue.apk" "$work/aligned.apk"

rm -rf "$out"
mkdir -p "$out/bin" "$out/app"
cp -R "$here/android/dist/." "$out/"
"$llvm/llvm-strip" -o "$out/bin/glassd" "$bin"
cp "$work/GlassServicesGlue.apk" "$out/app/"
cp "$here/android/README.md" "$out/README.md"
rev=$(git -C "$here" rev-parse HEAD 2>/dev/null || echo unknown)
dirty=$(git -C "$here" status --porcelain 2>/dev/null | head -c1)
printf 'glass-services %s%s\nNDK r25c, API %s, build-tools %s\n' "$rev" "${dirty:+ (modified)}" "$API" "$BUILD_TOOLS" > "$out/SOURCE"
(cd "$out" && find . -type f ! -name SHA256SUMS | LC_ALL=C sort | sed 's|^\./||' | xargs shasum -a 256 > SHA256SUMS)
echo "glass-services Android dist: $out"
