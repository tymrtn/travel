#!/bin/bash
set -euo pipefail
travel_root="$(cd "$(dirname "$0")/.." && pwd)"
travel_output="${TRAVEL_PACKAGE_DIR:-$travel_root/dist/Travel.app}"
for tool in cargo npm swift codesign otool install_name_tool; do
  command -v "$tool" >/dev/null || { echo "Missing build tool: $tool" >&2; exit 1; }
done
if [ -e "$travel_output" ]; then
  echo "Output already exists. Choose a fresh TRAVEL_PACKAGE_DIR; existing packages are never overwritten." >&2
  exit 1
fi
cd "$travel_root/crates/dashboard/web"
npm ci
npm run check
npm run build
cd "$travel_root"
cargo build --locked --release -p travel
cd "$travel_root/mac"
swift build -c release
travel_swift_output="$(swift build -c release --show-bin-path)"
mkdir -p "$travel_output/Contents/MacOS" "$travel_output/Contents/Resources"
cp "$travel_root/mac/Info.plist" "$travel_output/Contents/Info.plist"
travel_icon_work="$(mktemp -d /tmp/travel-icon.XXXXXX)"
swift "$travel_root/scripts/make-icon.swift" "$travel_icon_work/Travel.iconset"
iconutil -c icns "$travel_icon_work/Travel.iconset" -o "$travel_output/Contents/Resources/Travel.icns"
cp "$travel_swift_output/TravelCompanion" "$travel_output/Contents/MacOS/TravelCompanion"
cp "$travel_root/target/release/travel" "$travel_output/Contents/Resources/travel"
mkdir -p "$travel_output/Contents/Frameworks"
# Web Push encryption links OpenSSL. Bundle those libraries so the installed
# app does not depend on the build machine's Homebrew paths.
while IFS= read -r travel_dependency; do
  case "$travel_dependency" in
    /usr/lib/*|/System/*) continue ;;
    */libssl.*.dylib|*/libcrypto.*.dylib)
      travel_library="$(basename "$travel_dependency")"
      travel_openssl_license="$(dirname "$(dirname "$travel_dependency")")/LICENSE.txt"
      [ -f "$travel_openssl_license" ] || { echo "OpenSSL license not found beside build libraries" >&2; exit 1; }
      cp "$travel_openssl_license" "$travel_output/Contents/Resources/OpenSSL-LICENSE.txt"
      cp "$travel_dependency" "$travel_output/Contents/Frameworks/$travel_library"
      install_name_tool -change "$travel_dependency" "@loader_path/../Frameworks/$travel_library" "$travel_output/Contents/Resources/travel"
      install_name_tool -id "@rpath/$travel_library" "$travel_output/Contents/Frameworks/$travel_library"
      ;;
    *) echo "Unbundled dependency: $travel_dependency" >&2; exit 1 ;;
  esac
done < <(otool -L "$travel_output/Contents/Resources/travel" | awk 'NR>1 {print $1}')
for travel_library_path in "$travel_output"/Contents/Frameworks/*.dylib; do
  [ -f "$travel_library_path" ] || continue
  while IFS= read -r travel_dependency; do
    case "$travel_dependency" in
      /usr/lib/*|/System/*|@rpath/*) continue ;;
      */libssl.*.dylib|*/libcrypto.*.dylib)
        travel_library="$(basename "$travel_dependency")"
        [ -f "$travel_output/Contents/Frameworks/$travel_library" ] || { echo "Missing bundled library: $travel_library" >&2; exit 1; }
        install_name_tool -change "$travel_dependency" "@loader_path/$travel_library" "$travel_library_path"
        ;;
      *) echo "Unbundled library dependency: $travel_dependency" >&2; exit 1 ;;
    esac
  done < <(otool -L "$travel_library_path" | awk 'NR>1 {print $1}')
  codesign --force --sign "${TRAVEL_SIGNING_IDENTITY:--}" "$travel_library_path"
done
codesign --force --sign "${TRAVEL_SIGNING_IDENTITY:--}" "$travel_output/Contents/Resources/travel"
codesign --force --sign "${TRAVEL_SIGNING_IDENTITY:--}" "$travel_output"
codesign --verify --strict "$travel_output"
"$travel_output/Contents/Resources/travel" --version
echo "Built $travel_output (not notarized)."
