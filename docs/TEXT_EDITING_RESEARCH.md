# Phase 0 Research — Text Edit Strategy

**Date:** 2026-11-10
**Conclusion:** use **Path A — PDFium object API** for Stage 1. It works.

## What we tested

Created `examples/text_edit_spike.rs`. The spike:

1. Opens a PDF page.
2. Iterates page objects and prints each `PdfPageTextObject` with its text.
3. Picks the first non-empty text object.
4. Calls `PdfPageTextObject::set_text("...")`.
5. Calls `PdfPage::regenerate_content()`.
6. Saves the document.
7. Reopens the saved file in a fresh process to verify the edit persisted.

## Result

On `test.pdf`:

```
object 0: text="Hello PDF Viewer - Page 1"
replacing object text -> "Hello Mars"
after set_text, object.text() = "Hello Mars"
regenerated page content
saved test_edited.pdf
```

Fresh process reopening `test_edited.pdf` prints:

```
object 0: text="Hello Mars"
```

So a single text object can be replaced and persisted with PDFium's object API.

## Key PDFium / pdfium-render APIs

| Task | API |
|---|---|
| Open text layer | `PdfPage::text() -> PdfPageText` |
| Iterate chars | `PdfPageText::chars()` |
| Map char -> object | `PdfPageTextChar::text_object()` |
| Read object text | `PdfPageTextObject::text()` |
| Replace object text | `PdfPageTextObject::set_text()` |
| Commit page content | `PdfPage::regenerate_content()` |
| Persist | `PdfDocument::save_to_file()` |

## Critical detail

`set_text()` alone changes the in-memory object but **does not write
the content stream**. You must call:

```rust
page.regenerate_content()?;
```

before saving, otherwise the saved PDF still contains the original text.

Also note: `page.text()` borrows the page, so in Rust drop the
`PdfPageText` handle before calling `page.regenerate_content()`.

## Bulletin PDF check

Ran the spike on a copy of the 2-page bulletin. Each visible line is
generally its own `PdfPageTextObject` (e.g. `object 42: "It is a covenant
more generous than anyone could"`). That means Stage 1 can treat
"replace a line" as "replace one text object" for the common case.

## Caveats / next-stage work

- **Multi-line objects:** if one text object spans multiple visual lines,
  replacing its text replaces all of those lines. Stage 1 should either
  reject such objects or split them (deferred).
- **Font coverage:** `set_text` reuses the original font. If the new text
  has glyphs the font lacks, PDFium may render `.notdef`. Phase 4 will add
  a fallback font.
- **Empty strings:** pdfium-render already substitutes a single space for
  empty strings, so clearing a line is safe at the API level.
- **Line detection:** use character origins/baselines, not loose bounds,
  to decide whether an object is a single visual line.
- **lopdf:** not needed for Stage 1. We should revisit `lopdf`/`pdf-writer`
  only for multi-line object splitting and font subsetting later.

## Decision

**Stage 1 will use Path A (PDFium object API).**
Fallback to content-stream surgery (`lopdf`) is deferred until a real
Stage 1 limitation demands it.
