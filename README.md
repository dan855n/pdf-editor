# pdf-viewer — a Rust PDF viewer and editor

## PDF editor

The `pdf-editor` binary extends the viewer with annotation and page-editing
tools:

- Highlight, ink, rectangle, ellipse, text, and image annotations
- Selection, movement, and corner-handle resizing
- Annotation undo/redo backed by snapshots
- Save and Save As while preserving editable PDF annotations
- Page insertion, deletion, and rotation
- Continuous scrolling and background-rendered page thumbnails

This is an annotation editor: it does not rewrite text already embedded in the
PDF. Highlight, rectangle, and ellipse tools accept either a click (creating a
default-sized mark) or a click-and-drag. Use the Select tool afterward to move
or resize a mark, and use Delete or the toolbar button to remove it.

Run it with:

```sh
cargo run --bin pdf-editor -- test.pdf
```

The editor targets `eframe`/`egui` 0.37. PDFium does not expose a public circle
annotation constructor, so ellipses are written as stamp annotations containing
vector ellipse paths. A contents marker preserves their ellipse identity when
the document is reopened.

Verification:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

### Universal macOS app

Build a self-contained Universal app for both Apple Silicon and Intel Macs:

```sh
chmod +x packaging/build-universal-app.sh
packaging/build-universal-app.sh
```

The resulting apps and thumb-drive-ready ZIPs are written to `dist/`. The apps
are ad-hoc signed but not notarized, so macOS may require the recipient to
right-click the app and choose **Open** the first time.

The bundle includes an app icon (`packaging/AppIcon.icns`), registers PDF as a
document type (so "Open With" lists PDF Editor), and stamps its version and
build number into `Info.plist` from `Cargo.toml`.

To produce a distributable, notarized build, sign with a Developer ID and
notarize using a keychain profile:

```sh
CODESIGN_IDENTITY="Developer ID Application: Name (TEAMID)" \
NOTARY_PROFILE="my-profile" packaging/build-universal-app.sh
```

Create the profile once with `xcrun notarytool store-credentials`. Without
`CODESIGN_IDENTITY` the script falls back to ad-hoc signing, and without
`NOTARY_PROFILE` it skips notarization.

The package contains Universal executables for both `arm64` and `x86_64`, with
a matching Universal PDFium library. It targets macOS 11 or newer. Both slices
have been launched successfully; the Intel slice was tested through Rosetta 2
on the build machine.

For macOS 28 compatibility on Apple Silicon, distribute
`PDF-Editor-Apple-Silicon.zip`. The Universal package includes an Intel slice,
and macOS may warn that this component will not run in macOS 28. Use
`PDF-Editor-Intel.zip` for Intel Macs.

> **Note:** the document association registers PDF Editor as a handler, but the
> app does not yet respond to the macOS "open document" Apple event, so
> double-clicking a PDF in Finder launches the editor without opening the file.
> Opening via **File → Open** (⌘O) works normally. Receiving the file on launch
> requires an `NSApplicationDelegate` (`objc2`) open-files handler.

### OCR (make a scanned page searchable)

The editor can recognize text in a scanned (image-only) page and add an
invisible text layer so it becomes searchable, selectable, and copyable. The
**🔍 OCR page** toolbar button recognizes the current page and opens an
**OCR result** panel showing the recognized text — editable, with a **Copy**
button.

OCR is powered by the open-source [Tesseract](https://github.com/tesseract-ocr/tesseract)
engine via the `leptess` crate, behind the `ocr` cargo feature:

```sh
brew install tesseract          # provides the library + eng.traineddata
cargo run --features ocr --bin pdf-editor -- file.pdf
```

The build script enables OCR for the native (arm64) build only, because
Tesseract's C library cannot be cross-compiled for x86_64 from an arm64 host.
The Apple Silicon app bundles Tesseract/Leptonica and `eng.traineddata`; the
Intel build has no OCR.

## Stage 4 (code complete — awaiting your manual test): search

Adds to Stage 3:
- **Search field** in the toolbar (always visible when a document is open)
- **Full-document search** through every page's text layer via pdfium
  (`FPDFText_FindStart`), case-insensitive, capped at 500 pages
- **Live hit counter** — the field's placeholder shows `3/17` style position as you
  navigate; typing triggers a re-search automatically (debounced against the last
  committed query)
- **Highlighted hits** — all matches painted translucent yellow over the pages,
  the selected one bright yellow; highlights scroll with their pages
- **Navigation** — Enter / ▼ next match, Shift+Enter / ▲ previous; each jump
  centers the viewport on the hit; Escape clears
- **⌘F** focuses the search field
- Extra launch arg for scripted testing: `pdf-viewer file.pdf fox` opens with "fox" searched

### How to test it manually
1. `cargo run -- test-search.pdf` (or any real PDF)
2. Click the Search box (top right) or press ⌘F, type e.g. `fox`, wait a beat —
   yellow highlights appear and the view jumps to hit 1/3
3. Press Enter repeatedly → cycles through hits across pages
4. Shift+Enter goes backwards; Escape clears everything

Verified headlessly already: the search engine finds 3 hits for "fox" in
test-search.pdf and 17 for "I2C" in a 28-page datasheet. The egui wiring was not
confirmable through synthetic clicks, hence this manual pass.

## Stage 3: look & feel

Adds to Stage 2:
- **Thumbnail sidebar** — background-rendered page thumbnails, click any one to jump;
  current page highlighted; panel is resizable; toggle with the "Thumbnails" button
- **Status bar** — author · page count · file size on the left; zoom %, page dimensions
  in points, and PDF producer on the right
- **Theme cycle button** — 🌙 dark → ☀️ light → 🌗 system, persisted in the state file
- Document title from PDF metadata shown in the toolbar when present

## Stage 2: reading comfort

Adds to Stage 1:
- **Continuous scrolling** — all pages in one scroll strip, wheel/trackpad scrolls across page boundaries
- **Page jump box** — type a number + Enter in the toolbar
- **Number-key shortcuts** — press `1`–`9` to jump straight to that page
- **Up/Down/PageUp/PageDown/Home/End** keys turn pages
- **Multi-page texture cache** with neighbour prefetching (renders up to 3 pages concurrently off-thread)
- **Session restore** — reopening the app without arguments reopens your last file at your last page (`~/.local/share/pdf-viewer/state.txt`)
- Placeholders show page numbers while far pages render

## Stage 1: basic viewing

A native desktop app: **Rust + egui/eframe** for UI, **PDFium** (via `pdfium-render`) for rendering.

Features:
- Open a PDF via the toolbar button, ⌘O, or `cargo run -- some.pdf`
- Page navigation: Prev/Next buttons, ← → keys
- Zoom: ➕/➖ buttons, +/− keys, ⌘/Ctrl + scroll wheel
- Fit-to-width toggle
- Off-thread page rendering (UI never blocks on big pages)

Run it:
```sh
cargo run                     # opens with file picker
cargo run -- path/to/file.pdf # opens directly
```

### How pdfium is loaded
The prebuilt `libpdfium.dylib` lives in `vendor/pdfium/lib/` (from
[bblanchon/pdfium-binaries](https://github.com/bblanchon/pdfium-binaries),
build 8125 — `pdfium-mac-univ.tgz`, Chromium 158.0.8125.0). The crate feature
`pdfium_8125` matches that build's API level; if you swap the dylib for a newer
one, bump the feature to match its release number. The previous 7763 build is
kept at `vendor/pdfium/lib/libpdfium.dylib.bak-7763` for rollback.

## Roadmap

| Stage | Theme | Ideas |
|-------|-------|-------|
| 2 | Reading comfort | continuous scroll, mouse-wheel page turn, page-number entry box, remember last file & position |
| 3 | Look & feel | sidebar with page thumbnails, dark/light visuals, status bar, keyboard `g`+number jump |
| 4 | Text | text-layer selection & copy (⌘C), search with hit highlighting (`FPDFText_*`) |
| 5 | Annotations-lite | bookmarks/outline panel, document properties, link following |
| 6 | Polish | print/export, password-protected docs, recent-files menu, .app bundle packaging |

Verified: click the page box → type a number → Enter jumps there. The box also
shows the current page while you scroll or turn pages with the keyboard.

Suggested next step: **Stage 3 — thumbnail sidebar** (small renders of every page, click to jump).
