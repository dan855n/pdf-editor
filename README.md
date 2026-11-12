# PDF Editor

A native macOS PDF editor written in **Rust** — view, annotate, edit the original
text, rearrange pages, and OCR scanned documents. Built on **egui/eframe** for
the UI and **PDFium** (via `pdfium-render`) for rendering and editing.

## Features

### Viewing
- Continuous-scroll reading with zoom and fit-to-width
- Thumbnail sidebar, page-jump box, and full keyboard navigation
- Full-document text search with hit highlighting (debounced, ⌘F)
- Selectable/copyable text layer
- Dark / light / system theme, persisted
- Session restore (reopens your last document at the last page)
- Rotation-aware rendering

### Original-text editing
The standout feature: it rewrites the *actual* text in the PDF, not just an
overlay. With the **🔤 Edit text** tool you can:

- **Select** a word, **rewrite** it (double-click), or edit a whole line
- **Move** a line by dragging (with a live ghost preview)
- **Resize** and **bold** text via the toolbar
- **Delete** a word (Delete / Backspace)

All text edits are undoable (⌘Z) and saved as real, standards-compliant PDF
changes that any viewer sees.

### Annotations
- Highlight, ink, rectangle, ellipse, text note, and image stamp
- Select, move, resize, and reorder marks
- Colour, fill, and stroke-width controls
- Saved as real PDF annotations

### Pages & images
- Insert, delete, and rotate pages
- Move, rotate, crop, replace, and extract embedded images

### OCR
Recognize text in a scanned page and make it searchable, selectable, and
copyable — see [OCR](#ocr) below.

### Reliability
- Undo/redo across edits
- Atomic saves (temp file + rename, so a failed save never corrupts your file)

## Building

Prerequisites: a recent stable **Rust** toolchain, and (for OCR) **Tesseract**.

```sh
# Run the editor (viewer/annotation/editing features)
cargo run --bin pdf-editor -- some.pdf

# With OCR enabled (requires `brew install tesseract`)
cargo run --features ocr --bin pdf-editor -- some.pdf

# Tests
cargo test              # no OCR
cargo test --features ocr
```

There is also a read-only viewer binary:

```sh
cargo run --bin pdf-viewer -- some.pdf
```

## Universal macOS app

Build self-contained apps for Apple Silicon and Intel Macs:

```sh
chmod +x packaging/build-universal-app.sh
packaging/build-universal-app.sh
```

The apps and thumb-drive-ready ZIPs land in `dist/`. They are ad-hoc signed but
not notarized, so macOS may require a right-click → **Open** the first time.

To produce a distributable, notarized build, sign with a Developer ID and
notarize using a keychain profile:

```sh
CODESIGN_IDENTITY="Developer ID Application: Name (TEAMID)" \
NOTARY_PROFILE="my-profile" packaging/build-universal-app.sh
```

Create the profile once with `xcrun notarytool store-credentials`.

## OCR

OCR is powered by the open-source [Tesseract](https://github.com/tesseract-ocr/tesseract)
engine via the `leptess` crate, behind the `ocr` cargo feature:

```sh
brew install tesseract          # provides the library + eng.traineddata
cargo run --features ocr --bin pdf-editor -- file.pdf
```

The **🔍 OCR page** toolbar button recognizes the current page, adds an
invisible text layer (so it becomes searchable/copyable), and opens an
**OCR result** panel showing the recognized text — editable, with a **📋 Copy**
button.

> **Note:** the build script enables OCR for the native (arm64) build only,
> because Tesseract's C library cannot be cross-compiled for x86_64 from an
> arm64 host. The Apple Silicon app bundles Tesseract/Leptonica and
> `eng.traineddata`; the Intel build has no OCR.

## Architecture

```
src/
├── lib.rs             crate root + shared re-exports
├── engine.rs          shared rendering and search (used by both binaries)
├── document.rs        PDFium bridge: open, annotations, text, images, pages, save
├── annotations.rs     annotation model
├── text_model.rs      original-text line/word model
├── ocr.rs             Tesseract OCR + word-box parsing (feature-gated)
├── main.rs            the read-only viewer binary
└── editor/
    ├── main.rs        the pdf-editor binary entry point
    ├── app.rs         app state, canvas interaction, editing, undo/redo
    ├── app/ui.rs      toolbar, panels, and windows
    ├── app/search.rs  debounced full-document search
    ├── app/image_tools.rs  image move/rotate/crop/replace/extract
    ├── tools.rs       tool and interaction enums
    ├── undo.rs        undo/redo edit model
    └── persist.rs     last-file/page and theme persistence
```

Rendering and text search live in `engine.rs` so both binaries share one
implementation.

## How pdfium is loaded

The prebuilt `libpdfium.dylib` lives in `vendor/pdfium/lib/` (from
[bblanchon/pdfium-binaries](https://github.com/bblanchon/pdfium-binaries),
build 8125 — `pdfium-mac-univ.tgz`, Chromium 158.0.8125.0). The crate feature
`pdfium_8125` matches that build's API level; if you swap the dylib for a newer
one, bump the feature to match its release number.

## Known limitations

- **OCR is Apple Silicon only** (see above).
- **Text editing is ASCII-only** — non-ASCII replacement needs font
  re-encoding, which is not yet implemented.
- **No paragraph reflow** — a text object spanning multiple lines is edited
  word-by-word; whole-line edits on such objects are rejected.
- **Double-click a PDF in Finder** launches the editor but does not open the
  file (⌘O works). Receiving the file needs an `NSApplicationDelegate`
  (`objc2`) open-files handler.

## License

[MIT](LICENSE)
