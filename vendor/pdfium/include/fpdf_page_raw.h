#ifndef PUBLIC_FPDF_PAGE_RAW_H_
#define PUBLIC_FPDF_PAGE_RAW_H_

// NOLINTNEXTLINE(build/include)
#include "fpdfview.h"

#ifdef __cplusplus
extern "C" {
#endif  // __cplusplus

// Experimental API.
//
// Get the rotation of a page without loading the page.
//
// FPDFPage_GetRotation() requires an FPDF_PAGE, so callers must first call
// FPDF_LoadPage(), which constructs a CPDF_Page: it resolves the inherited
// /Resources dictionary, computes the page dimensions and loads transparency
// info -- none of which reading /Rotate needs. On documents with many pages
// that dominates the runtime.
//
// This walks the page dictionary's inherited attribute chain directly, exactly
// as CPDF_Page::GetPageRotation() does, so the result matches
// FPDFPage_GetRotation() for the same page.
//
//   document   - handle to a document.
//   page_index - zero-based page index.
//
// Returns one of the following indicating the page rotation:
//   0 - No rotation.
//   1 - Rotated 90 degrees clockwise.
//   2 - Rotated 180 degrees clockwise.
//   3 - Rotated 270 degrees clockwise.
//
// Or returns -1 if |document| is invalid, |page_index| is out of range, or the
// page dictionary cannot be resolved.
FPDF_EXPORT int FPDF_CALLCONV
FPDFPage_GetRotationRaw(FPDF_DOCUMENT document, int page_index);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  // PUBLIC_FPDF_PAGE_RAW_H_
