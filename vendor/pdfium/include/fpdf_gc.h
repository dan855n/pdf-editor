#ifndef PUBLIC_FPDF_GC_H_
#define PUBLIC_FPDF_GC_H_

// NOLINTNEXTLINE(build/include)
#include "fpdfview.h"

#ifdef __cplusplus
extern "C" {
#endif  // __cplusplus

// Experimental API.
//
// Garbage-collect the document: permanently delete every indirect object that
// is not reachable from the trailer (/Root, /Info, /Encrypt).
//
// After a redaction pass removes text/images, the font subsets that rendered
// them become unreferenced (CPDF_PageContentGenerator drops the /Resources
// entries), but CPDF_Creator still writes those now-dead font objects on save,
// leaving the redacted text's glyph outlines, ToUnicode maps and widths
// recoverable from the file. Calling this before FPDF_SaveAsCopy() physically
// removes them.
//
// Safe to call when nothing is orphaned.
//
//   document - handle to a document.
//
// Returns the number of objects removed, or -1 if |document| is invalid.
FPDF_EXPORT int FPDF_CALLCONV
FPDF_RemoveOrphanObjects(FPDF_DOCUMENT document);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  // PUBLIC_FPDF_GC_H_
