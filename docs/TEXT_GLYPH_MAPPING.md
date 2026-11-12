> NOTE: Reverted by user request. Kept for reference only.

# Glyph-Mapping Layer for Unicode Text Edits

## Goal

Allow non-ASCII Unicode text edits by encoding new text as explicit glyph IDs
with `FPDFText_SetCharcodes`, using `ttf-parser` to map Unicode -> glyph ID
against the embedded fallback font.

## Blocker

`pdfium-render` 0.9.5 does **not** expose:

- `PdfPageTextObject::set_charcodes()`
- the raw `FPDF_PAGEOBJECT` / `FPDF_DOCUMENT` / `FPDF_FONT` handles
  (they are `pub(crate)`)

The underlying binding exists in `PdfiumLibraryBindings::FPDFText_SetCharcodes`,
but we cannot call it from outside the crate because we cannot obtain the
object handle. That is the only missing link.

## Options

### Option A — Fork / vendor `pdfium-render`

Add one public method to `PdfPageTextObject`:

```rust
pub fn set_charcodes(&mut self, charcodes: &[u32]) -> Result<(), PdfiumError> {
    if self.bindings().is_true(unsafe {
        self.bindings().FPDFText_SetCharcodes(
            self.object_handle(),
            charcodes.as_ptr(),
            charcodes.len(),
        )
    }) {
        Ok(())
    } else {
        Err(PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::Unknown))
    }
}
```

Then our fallback path can:

1. Load Noto Sans as a CID TrueType font.
2. Create a text object with that font.
3. For each `char` in the new text, use `ttf_parser` to get `face.glyph_index(c)`.
4. Call `set_charcodes(&glyph_ids)`.

Pros: small, clean, uses the existing wrapper.
Cons: requires maintaining a fork or submitting an upstream PR.

### Option B — Raw FFI layer

Add a small `src/pdfium_raw.rs` that links directly to `libpdfium.dylib` and
declares only the handful of functions we need:

- `FPDF_LoadPage`
- `FPDFPageObj_CreateTextObj`
- `FPDFText_SetCharcodes`
- `FPDFPage_InsertObject` / `FPDFPage_RemoveObject`
- `FPDFPage_GenerateContent`

This avoids forking, but duplicates some of `pdfium-render` and needs care
around handle lifetimes.

### Option C — `lopdf` content-stream surgery

Ignore PDFium for text editing and rewrite `BT…ET` text-showing operators with
`lopdf`, creating a CID font resource and ToUnicode CMap ourselves.

Pros: full control.
Cons: the largest effort; Stage 1 already works via PDFium.

## Recommendation

**Option A implemented.** `pdfium-render` is vendored under
`vendor/pdfium-render`, and `PdfPageTextObject::set_charcodes()` has been
added. The fallback path now maps Unicode -> glyph IDs with `ttf-parser` and
writes them via `FPDFText_SetCharcodes`.

## Acceptance test

- [x] Edit a line to `"Héllo"` on a PDF whose original font lacks those glyphs.
- [x] Save, reopen, and verify `text_runs` reads back `"Héllo"` exactly.
- [x] Undo/redo restores the original text exactly.
- [x] `"Héllo ✓"`, Arabic, Hebrew, Devanagari, and Thai work via the
      data-driven fallback chain (`FALLBACK_FONTS`).
