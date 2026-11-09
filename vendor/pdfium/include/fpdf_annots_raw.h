#ifndef PUBLIC_FPDF_ANNOTS_RAW_H_
#define PUBLIC_FPDF_ANNOTS_RAW_H_

// NOLINTNEXTLINE(build/include)
#include "fpdfview.h"

#ifdef __cplusplus
extern "C" {
#endif  // __cplusplus

// Experimental API.
//
// Count the annotations on a page without loading the page.
//
// FPDFPage_GetAnnotCount() requires an FPDF_PAGE, so callers must first call
// FPDF_LoadPage(), which constructs a CPDF_Page: it resolves the inherited
// /Resources dictionary, computes the page dimensions and loads transparency
// info -- none of which annotation reading needs. On documents with many pages
// and few annotations that dominates the runtime.
//
// This reads the /Annots array straight out of the page dictionary, so pages
// without annotations can be skipped for the cost of a dictionary lookup.
//
//   document   - handle to a document.
//   page_index - zero-based page index.
//
// Returns the number of annotations, or 0 if the page has none, the index is
// out of range, or |document| is invalid.
FPDF_EXPORT int FPDF_CALLCONV
FPDFPage_GetAnnotCountRaw(FPDF_DOCUMENT document, int page_index);

// Experimental API.
//
// Get the value of a form field annotation, without falling back to the
// field's default value.
//
// FPDFAnnot_GetFormFieldValue() calls CPDF_FormField::GetValue(), which for
// every field type except text substitutes /DV when /V is absent. A field that
// carries a default value but has never been filled in therefore reports that
// default as if it were the current value, contradicting the widget's own
// appearance stream, which renders blank.
//
// This reads /V only, still resolving inheritance up the /Parent chain, and
// reports nothing when /V is absent anywhere in that chain.
//
//   hHandle - handle to the form fill module, returned by
//             FPDFDOC_InitFormFillEnvironment().
//   annot   - handle to an annotation.
//   buffer  - buffer for holding the value string, encoded in UTF-16LE.
//   buflen  - length of the buffer in bytes.
//
// Returns the length of the string value in bytes, including the terminating
// NUL, or 0 if the annotation is not a form field or has no /V.
FPDF_EXPORT unsigned long FPDF_CALLCONV
FPDFAnnot_GetFormFieldValueRaw(FPDF_FORMHANDLE hHandle,
                               FPDF_ANNOTATION annot,
                               FPDF_WCHAR* buffer,
                               unsigned long buflen);

// Experimental API.
//
// Get whether the document's interactive form sets /NeedAppearances.
//
// pdfium acts on this flag internally -- CPDFSDK_PageView::NewAnnot() calls
// ResetAppearance() whenever CPDF_InteractiveForm::NeedConstructAP() is true,
// discarding each widget's authored appearance stream and regenerating it --
// but never reports it, so callers cannot tell whether a rendered form shows
// the appearances its producer wrote or the ones pdfium synthesised.
//
// This reads /Root/AcroForm/NeedAppearances directly, without constructing a
// CPDF_InteractiveForm.
//
//   document - handle to a document.
//
// Returns true if the flag is present and set, false otherwise.
FPDF_EXPORT FPDF_BOOL FPDF_CALLCONV
FPDF_GetNeedAppearances(FPDF_DOCUMENT document);

// Experimental API.
//
// Get whether a check box or radio button annotation is in its on state,
// without constructing a page view.
//
// FPDFAnnot_IsChecked() resolves the annotation to a CPDFSDK_Widget, and
// CPDFSDK_InteractiveForm::GetWidget() builds the page's CPDFSDK_PageView as a
// side effect of that lookup -- which runs FORM_OnAfterLoadPage() and, on a
// document with /AcroForm/NeedAppearances set, rewrites every widget's stored
// /AP. Reading a check box therefore destroys the authored appearances before
// anything else can use them. The answer itself needs none of that:
// CPDF_FormControl::IsChecked() compares /AS against the control's on-state.
//
//   hHandle - handle to the form fill module, returned by
//             FPDFDOC_InitFormFillEnvironment().
//   annot   - handle to an annotation.
//
// Returns true if the annotation is a checked check box or radio button.
FPDF_EXPORT FPDF_BOOL FPDF_CALLCONV
FPDFAnnot_IsCheckedRaw(FPDF_FORMHANDLE hHandle, FPDF_ANNOTATION annot);

// Experimental API.
//
// Get the export value of a check box or radio button annotation, without
// constructing a page view. Same rationale as FPDFAnnot_IsCheckedRaw().
//
//   hHandle - handle to the form fill module.
//   annot   - handle to an annotation.
//   buffer  - buffer for holding the value string, encoded in UTF-16LE.
//   buflen  - length of the buffer in bytes.
//
// Returns the length of the string value in bytes, including the terminating
// NUL, or 0 if the annotation is not a check box or radio button.
FPDF_EXPORT unsigned long FPDF_CALLCONV
FPDFAnnot_GetFormFieldExportValueRaw(FPDF_FORMHANDLE hHandle,
                                     FPDF_ANNOTATION annot,
                                     FPDF_WCHAR* buffer,
                                     unsigned long buflen);

// Experimental API.
//
// Load any widget annotations on |page| into the document's interactive form,
// without creating a page view.
//
// A document whose /AcroForm /Fields tree does not reference every widget
// leaves CPDF_InteractiveForm with an incomplete control list: the affected
// annotations report a control index of -1, an undercounted control count and
// an empty export value. pdfium repairs this in the CPDFSDK_PageView
// constructor, which calls CPDF_InteractiveForm::FixPageFields() -- but
// obtaining a page view also runs FORM_OnAfterLoadPage(), and that regenerates
// every widget's appearance stream when /NeedAppearances is set. Callers that
// only want to read the form should not have to rewrite it to do so.
//
//   hHandle - handle to the form fill module, returned by
//             FPDFDOC_InitFormFillEnvironment().
//   page    - handle to the page.
FPDF_EXPORT void FPDF_CALLCONV FPDFPage_FixFormFields(FPDF_FORMHANDLE hHandle,
                                                      FPDF_PAGE page);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  // PUBLIC_FPDF_ANNOTS_RAW_H_
