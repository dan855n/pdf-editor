#ifndef PUBLIC_FPDF_SIGNATURE_RAW_H_
#define PUBLIC_FPDF_SIGNATURE_RAW_H_

// NOLINTNEXTLINE(build/include)
#include "fpdfview.h"

#ifdef __cplusplus
extern "C" {
#endif  // __cplusplus

// Experimental API.
//
// Check whether |document| differs from |signed_document| (the same file
// truncated at the end of a signature's /ByteRange) only by LTV data.
//
// Every object whose cross-reference entry changed must be unreachable from
// the trailer once the catalog's /DSS and /Metadata are detached, in either
// document. The catalog may only change /DSS, /Extensions and /Perms, the
// encryption dictionary only /Perms, the document information dictionary only
// /ModDate and /Producer, and neither document may have had its
// cross-reference table rebuilt.
//
// Returns true if the changes are LTV-only, false otherwise.
FPDF_EXPORT FPDF_BOOL FPDF_CALLCONV
FPDF_HasOnlyDSSChanges(FPDF_DOCUMENT document, FPDF_DOCUMENT signed_document);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  // PUBLIC_FPDF_SIGNATURE_RAW_H_
