#!/bin/zsh
set -euo pipefail

project_dir="$(cd "$(dirname "$0")/.." && pwd)"
dist_dir="$project_dir/dist"
app_dir="$dist_dir/PDF Editor.app"
contents_dir="$app_dir/Contents"
macos_dir="$contents_dir/MacOS"
resources_dir="$contents_dir/Resources"
toolchain_dir="$(rustup run stable rustc --print sysroot)"

cd "$project_dir"
rustup target add x86_64-apple-darwin

RUSTC="$toolchain_dir/bin/rustc" cargo build --release --bin pdf-editor
RUSTC="$toolchain_dir/bin/rustc" cargo build --release \
    --target x86_64-apple-darwin --bin pdf-editor

rm -rf "$app_dir"
mkdir -p "$macos_dir" "$resources_dir"

lipo -create \
    "$project_dir/target/release/pdf-editor" \
    "$project_dir/target/x86_64-apple-darwin/release/pdf-editor" \
    -output "$macos_dir/pdf-editor"
chmod +x "$macos_dir/pdf-editor"

cp "$project_dir/vendor/pdfium/lib/libpdfium.dylib" "$macos_dir/libpdfium.dylib"
cp "$project_dir/packaging/Info.plist" "$contents_dir/Info.plist"
cp "$project_dir/README.md" "$resources_dir/README.md"

codesign --force --sign - "$macos_dir/libpdfium.dylib"
codesign --force --sign - "$app_dir"

rm -f "$dist_dir/PDF-Editor-macOS-universal.zip"
ditto -c -k --sequesterRsrc --keepParent \
    "$app_dir" "$dist_dir/PDF-Editor-macOS-universal.zip"

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
    cp "$project_dir/README.md" "$arch_contents/Resources/README.md"
    plutil -replace CFBundleDisplayName -string "PDF Editor ($label)" \
        "$arch_contents/Info.plist"
    plutil -replace CFBundleName -string "PDF Editor ($label)" \
        "$arch_contents/Info.plist"
    plutil -replace CFBundleIdentifier -string "$identifier" \
        "$arch_contents/Info.plist"

    codesign --force --sign - "$arch_macos/libpdfium.dylib"
    codesign --force --sign - "$arch_app"

    rm -f "$dist_dir/$zip_name"
    ditto -c -k --sequesterRsrc --keepParent "$arch_app" "$dist_dir/$zip_name"
    echo "Built $arch_app"
    echo "Built $dist_dir/$zip_name"
}

build_arch_app \
    "Apple Silicon" arm64 local.danieledwards.pdf-editor.arm64 \
    "PDF-Editor-Apple-Silicon.zip"
build_arch_app \
    "Intel" x86_64 local.danieledwards.pdf-editor.intel \
    "PDF-Editor-Intel.zip"
