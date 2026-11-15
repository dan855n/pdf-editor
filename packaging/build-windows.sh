#!/bin/zsh
# Build a self-contained Windows x64 PDF Editor from macOS.
#
# Prerequisites:
#   brew install mingw-w64
#   rustup target add x86_64-pc-windows-gnu
#
# Note: the Homebrew rustc on PATH does not carry the Windows std library, so
# we point RUSTC at the rustup-managed toolchain (which does), exactly like the
# macOS universal build does for its cross-compiled arch.
set -euo pipefail

project_dir="$(cd "$(dirname "$0")/.." && pwd)"
dist_dir="$project_dir/dist"
win_dir="$dist_dir/PDF-Editor-Windows-x64"

# Version metadata is read from Cargo.toml so the archive and the binary agree.
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$project_dir/Cargo.toml" | head -1)"

cd "$project_dir"

# Locate the rustup toolchain's rustc, which has the Windows std installed.
toolchain_dir="$(rustup run stable rustc --print sysroot)"

# Ensure the Windows target and cross toolchain are present.
rustup target add x86_64-pc-windows-gnu
if ! command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1; then
    echo "error: mingw-w64 is not installed (run: brew install mingw-w64)" >&2
    exit 1
fi

# OCR (Tesseract) is not available on this Windows build; it links a native C
# library that cannot be cross-compiled from macOS.
RUSTC="$toolchain_dir/bin/rustc" cargo build --release \
    --target x86_64-pc-windows-gnu --bin pdf-editor

rm -rf "$win_dir"
mkdir -p "$win_dir"

# Strip symbols with the mingw strip (shrinks the 28 MB binary to ~20 MB).
x86_64-w64-mingw32-strip -o "$win_dir/PDF Editor.exe" \
    "$project_dir/target/x86_64-pc-windows-gnu/release/pdf-editor.exe"

# The DLL must sit next to the executable (pdfium_bindings.rs looks there first).
cp "$project_dir/vendor/pdfium/lib/pdfium.dll" "$win_dir/pdfium.dll"
cp "$project_dir/packaging/README-Windows.txt" "$win_dir/README-Windows.txt"

rm -f "$dist_dir/PDF-Editor-Windows-x64.zip"
( cd "$dist_dir" && zip -r -q PDF-Editor-Windows-x64.zip PDF-Editor-Windows-x64 )

echo "Built $win_dir"
echo "Built $dist_dir/PDF-Editor-Windows-x64.zip (v$version)"
