// Copyright 2026 The PDFium Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef PUBLIC_FPDF_FORM_RAW_H_
#define PUBLIC_FPDF_FORM_RAW_H_

#include "fpdfview.h"

#ifdef __cplusplus
extern "C" {
#endif

// Registers the interactive form fields of pages already imported from
// |src_doc| into |dest_doc|'s AcroForm.
//
// FPDF_ImportPages() copies page dictionaries and everything they reference,
// but the interactive form lives in the document catalog at /Root/AcroForm and
// is therefore never copied. The widget annotations survive on their pages, yet
// nothing ties them to a form, so FPDF_FFLDraw() has no fields to draw and
// FPDFAnnot_GetFormFieldType() reports every widget as unknown.
//
// Call this after FPDF_ImportPages(). It walks the widget annotations already
// present in |dest_doc|, collects the root of each field tree they belong to,
// and appends those roots to |dest_doc|'s /AcroForm /Fields, creating the
// AcroForm when absent. /DR, /DA, /Q and /NeedAppearances are taken from
// |src_doc| when |dest_doc| does not already define them.
//
// Field names are preserved as-is unless a root field's name is already
// registered in |dest_doc|. On collision that field's /T gains a numeric
// suffix -- "addr" becomes "addr_1", or "addr_2" if "addr_1" is taken too --
// so fully qualified names stay unique while remaining recognisable. A renamed
// field keeps its widgets, value and appearance; only the name a caller would
// look it up by changes. Fields that do not collide are never renamed.
//
// This follows Apache PDFBox's PDFMergerUtility; see the licence notice in
// fpdfsdk/fpdf_form_raw.cpp.
//
// Only fields whose widgets were actually imported are registered; widgets
// belonging to pages that were not imported are ignored.
//
//   dest_doc - the destination document, must not be NULL.
//   src_doc  - the source document the pages came from, must not be NULL.
//
// Returns the number of top-level field entries appended to /Fields, 0 when
// there was nothing to register, or -1 on error.
FPDF_EXPORT int FPDF_CALLCONV FPDF_ImportAcroForm(FPDF_DOCUMENT dest_doc,
                                                  FPDF_DOCUMENT src_doc);

#ifdef __cplusplus
}  // extern "C"
#endif

#endif  // PUBLIC_FPDF_FORM_RAW_H_
