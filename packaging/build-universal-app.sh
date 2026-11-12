#!/bin/zsh
set -euo pipefail

project_dir="$(cd "$(dirname "$0")/.." && pwd)"
dist_dir="$project_dir/dist"
app_dir="$dist_dir/PDF Editor.app"
contents_dir="$app_dir/Contents"
macos_dir="$contents_dir/MacOS"
resources_dir="$contents_dir/Resources"
toolchain_dir="$(rustup run stable rustc --print sysroot)"

# Version metadata is read from Cargo.toml so the bundle and the binary agree.
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$project_dir/Cargo.toml" | head -1)"
build="$(git -C "$project_dir" rev-list --count HEAD 2>/dev/null || echo 1)"

# Signing identity: default to ad-hoc ("-"). Set CODESIGN_IDENTITY to a
# Developer ID Application identity (e.g. "Developer ID Application: Name
# (TEAMID)") to produce a distributable, notarizable app with hardened runtime.
SIGN_IDENTITY="${CODESIGN_IDENTITY:--}"
if [[ "$SIGN_IDENTITY" == "-" ]]; then
    SIGN_FLAGS=(--force --sign -)
else
    SIGN_FLAGS=(--force --options runtime --timestamp --sign "$SIGN_IDENTITY")
fi

cd "$project_dir"
rustup target add x86_64-apple-darwin

# The native (arm64) build enables OCR; the x86_64 cross-build does not, since
# Tesseract's C library cannot be cross-compiled from an arm64 host.
RUSTC="$toolchain_dir/bin/rustc" cargo build --release --features ocr --bin pdf-editor
RUSTC="$toolchain_dir/bin/rustc" cargo build --release \
    --target x86_64-apple-darwin --bin pdf-editor

rm -rf "$app_dir"
mkdir -p "$macos_dir" "$resources_dir"

lipo -create \
    "$project_dir/target/release/pdf-editor" \
    "$project_dir/target/x86_64-apple-darwin/release/pdf-editor" \
    -output "$macos_dir/pdf-editor"
chmod +x "$macos_dir/pdf-editor"

# Strip debug symbols with the system strip (rust-objcopy is unreliable here).
strip "$macos_dir/pdf-editor"

# Bundle Tesseract + Leptonica and their transitive dylibs (via dylibbundler),
# plus the English language data, so OCR works without a Homebrew install.
# dylibbundler cannot write into the binary's own directory, so bundle into a
# temp dir, then move the dylibs next to the executable so @loader_path
# resolves for both the binary and the dylibs' transitive dependencies.
# NOTE: these dylibs are the host's architecture (arm64 on Apple Silicon);
# x86_64 OCR bundling is not yet supported.
TESSDATA_SRC="/opt/homebrew/share/tessdata/eng.traineddata"
ocr_tmp="$(mktemp -d)"
dylibbundler -od -b -x "$macos_dir/pdf-editor" \
    -d "$ocr_tmp" -p @loader_path >/dev/null 2>&1
mv "$ocr_tmp"/*.dylib "$macos_dir/"
rmdir "$ocr_tmp"
mkdir -p "$resources_dir/tessdata"
cp "$TESSDATA_SRC" "$resources_dir/tessdata/eng.traineddata"

cp "$project_dir/vendor/pdfium/lib/libpdfium.dylib" "$macos_dir/libpdfium.dylib"
cp "$project_dir/packaging/Info.plist" "$contents_dir/Info.plist"
cp "$project_dir/packaging/AppIcon.icns" "$resources_dir/AppIcon.icns"
cp "$project_dir/README.md" "$resources_dir/README.md"

plutil -replace CFBundleShortVersionString -string "$version" \
    "$contents_dir/Info.plist"
plutil -replace CFBundleVersion -string "$build" \
    "$contents_dir/Info.plist"

codesign "${SIGN_FLAGS[@]}" "$macos_dir/libpdfium.dylib"
# Sign the bundled OCR dylibs (required for hardened runtime library validation).
for dylib in "$macos_dir"/*.dylib; do
    [[ "$(basename "$dylib")" == "libpdfium.dylib" ]] && continue
    codesign "${SIGN_FLAGS[@]}" "$dylib"
done
codesign "${SIGN_FLAGS[@]}" "$app_dir"

rm -f "$dist_dir/PDF-Editor-macOS-universal.zip"
ditto -c -k --sequesterRsrc --keepParent \
    "$app_dir" "$dist_dir/PDF-Editor-macOS-universal.zip"

# Optional notarization. Requires a Developer ID signing identity and a
# keychain profile created once with:
#   xcrun notarytool store-credentials "$NOTARY_PROFILE" \
#       --apple-id "$APPLE_ID" --team-id "$TEAM_ID" --password "$APP_PASSWORD"
if [[ -n "${NOTARY_PROFILE:-}" ]]; then
    echo "Submitting for notarization…"
    xcrun notarytool submit "$dist_dir/PDF-Editor-macOS-universal.zip" \
        --keychain-profile "$NOTARY_PROFILE" --wait
    xcrun stapler staple "$app_dir"
    rm -f "$dist_dir/PDF-Editor-macOS-universal.zip"
    ditto -c -k --sequesterRsrc --keepParent \
        "$app_dir" "$dist_dir/PDF-Editor-macOS-universal.zip"
    echo "Notarized and stapled."
fi

echo "Built $app_dir"
echo "Built $dist_dir/PDF-Editor-macOS-universal.zip"

build_arch_app() {
    local label="$1"
    local arch="$2"
    local identifier="$3"
    local zip_name="$4"
    local arch_app="$dist_dir/PDF Editor ($label).app"
    local arch_contents="$arch_app/Contents"
    local arch_macos="$arch_contents/MacOS"

    rm -rf "$arch_app"
    mkdir -p "$arch_macos" "$arch_contents/Resources"
    lipo -thin "$arch" "$macos_dir/pdf-editor" -output "$arch_macos/pdf-editor"
    lipo -thin "$arch" "$macos_dir/libpdfium.dylib" -output "$arch_macos/libpdfium.dylib"
    chmod +x "$arch_macos/pdf-editor"
    cp "$project_dir/packaging/Info.plist" "$arch_contents/Info.plist"
    cp "$project_dir/packaging/AppIcon.icns" "$arch_contents/Resources/AppIcon.icns"
    cp "$project_dir/README.md" "$arch_contents/Resources/README.md"
    # Carry the OCR dylibs and language data into the arch-specific bundle.
    for f in "$macos_dir"/*.dylib; do
        [[ "$(basename "$f")" == "libpdfium.dylib" ]] && continue
        cp "$f" "$arch_macos/"
    done
    mkdir -p "$arch_contents/Resources/tessdata"
    cp "$resources_dir/tessdata/eng.traineddata" \
        "$arch_contents/Resources/tessdata/eng.traineddata"
    plutil -replace CFBundleDisplayName -string "PDF Editor ($label)" \
        "$arch_contents/Info.plist"
    plutil -replace CFBundleName -string "PDF Editor ($label)" \
        "$arch_contents/Info.plist"
    plutil -replace CFBundleIdentifier -string "$identifier" \
        "$arch_contents/Info.plist"
    plutil -replace CFBundleShortVersionString -string "$version" \
        "$arch_contents/Info.plist"
    plutil -replace CFBundleVersion -string "$build" \
        "$arch_contents/Info.plist"

    codesign "${SIGN_FLAGS[@]}" "$arch_macos/libpdfium.dylib"
    for dylib in "$arch_macos"/*.dylib; do
        [[ "$(basename "$dylib")" == "libpdfium.dylib" ]] && continue
        codesign "${SIGN_FLAGS[@]}" "$dylib"
    done
    codesign "${SIGN_FLAGS[@]}" "$arch_app"

    rm -f "$dist_dir/$zip_name"
    ditto -c -k --sequesterRsrc --keepParent "$arch_app" "$dist_dir/$zip_name"
    echo "Built $arch_app"
    echo "Built $dist_dir/$zip_name"
}

build_arch_app \
    "Apple Silicon" arm64 com.example.pdf-editor.arm64 \
    "PDF-Editor-Apple-Silicon.zip"
build_arch_app \
    "Intel" x86_64 com.example.pdf-editor.intel \
    "PDF-Editor-Intel.zip"
