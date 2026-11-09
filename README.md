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
[docusealco/pdfium-binaries](https://github.com/docusealco/pdfium-binaries),
commit 34cb5701b, 2026-09-20). The crate feature `pdfium_7763` matches that
build's API level; if you swap the dylib for a newer one, bump the feature to
match its release number.

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
