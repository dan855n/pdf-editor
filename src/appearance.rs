//! Regenerate AcroForm text-field appearance streams so a filled PDF shows the
//! entered values in any viewer, even when the document does not set
//! `/NeedAppearances`.
//!
//! pdfium-render 0.9 writes `/V` (the field value) but never regenerates the
//! field's appearance stream (`/AP`). Our own renderer draws fields through
//! `FPDF_FFLDraw`, which honors `/NeedAppearances`, so it looks right in-app —
//! but a saved file can carry a stale `/AP`. This module re-serializes the
//! saved bytes and writes a fresh single-line appearance for every text field,
//! using the same vendored PDFium the rest of the crate already loaded.

use std::ffi::{c_int, c_ulong, c_void};

use pdfium_render::prelude::{
    FPDF_ANNOTATION, FPDF_DWORD, FPDF_FILEWRITE, FPDF_WCHAR, FS_RECTF, PdfiumLibraryBindings,
};

/// The appearance-mode constant `FPDF_ANNOT_APPEARANCEMODE_NORMAL`.
const AP_NORMAL: c_int = 0;

/// Re-serialize `bytes` with regenerated text-field appearance streams, using
/// the given (already-initialized) PDFium bindings.
///
/// Returns the new PDF bytes, or the original bytes unchanged if the document
/// cannot be processed. The input is left untouched on any failure.
pub fn regenerate_form_appearances(
    bindings: &dyn PdfiumLibraryBindings,
    bytes: &[u8],
) -> Vec<u8> {
    unsafe {
        let doc = bindings.FPDF_LoadMemDocument(bytes, None);
        if doc.is_null() {
            return bytes.to_vec();
        }

        let page_count = bindings.FPDF_GetPageCount(doc);
        for page_index in 0..page_count {
            let page = bindings.FPDF_LoadPage(doc, page_index);
            if page.is_null() {
                continue;
            }
            let annot_count = bindings.FPDFPage_GetAnnotCount(page);
            for annot_index in 0..annot_count {
                let annot = bindings.FPDFPage_GetAnnot(page, annot_index);
                if annot.is_null() {
                    continue;
                }
                if let Some(ap) = build_text_appearance(&*bindings, annot) {
                    bindings.FPDFAnnot_SetAP_str(annot, AP_NORMAL, &ap);
                }            }
            bindings.FPDF_ClosePage(page);
        }

        let mut fw = FileWrite {
            version: 1,
            write_block: Some(write_block),
            data: Vec::new(),
        };
        let ok = bindings.FPDF_SaveAsCopy(
            doc,
            &mut fw as *mut FileWrite as *mut FPDF_FILEWRITE,
            0 as FPDF_DWORD,
        );
        bindings.FPDF_CloseDocument(doc);

        if ok == 0 {
            bytes.to_vec()
        } else {
            fw.data
        }
    }
}

/// A `FPDF_FILEWRITE` implementation that collects the written bytes.
#[repr(C)]
struct FileWrite {
    version: c_int,
    write_block: Option<
        unsafe extern "C" fn(*mut FPDF_FILEWRITE, *const c_void, c_ulong) -> c_int,
    >,
    data: Vec<u8>,
}

unsafe extern "C" fn write_block(
    self_: *mut FPDF_FILEWRITE,
    data: *const c_void,
    size: c_ulong,
) -> c_int {
    unsafe {
        let fw = self_ as *mut FileWrite;
        let slice = std::slice::from_raw_parts(data as *const u8, size as usize);
        (*fw).data.extend_from_slice(slice);
    }
    1
}

/// Build a text-field appearance stream for `annot`, single- or multi-line,
/// from its `/DA` default-appearance string. Returns `None` for non-text fields
/// (checkboxes, radios, signatures, …) whose appearances are already defined
/// by the document.
fn build_text_appearance(
    bindings: &dyn PdfiumLibraryBindings,
    annot: FPDF_ANNOTATION,
) -> Option<String> {
    let da = get_string(bindings, annot, "DA")?;
    if !da.split_whitespace().any(|t| t == "Tf") {
        return None;
    }
    let (font, size) = parse_da(&da);

    let value = get_string(bindings, annot, "V").unwrap_or_default();
    let mut rect = FS_RECTF {
        left: 0.0,
        top: 0.0,
        right: 0.0,
        bottom: 0.0,
    };
    if unsafe { bindings.FPDFAnnot_GetRect(annot, &mut rect) } == 0 {
        return None;
    }

    // `FPDF_FORMFLAG_TEXT_MULTILINE` is bit 12 of the field's `/Ff` flags.
    let mut flags = 0.0f32;
    let multiline = unsafe { bindings.FPDFAnnot_GetNumberValue(annot, "Ff", &mut flags) != 0 }
        && (flags as i64 & (1 << 12)) != 0;

    let left = rect.left;
    let top = rect.top;
    let bottom = rect.bottom;
    let width = rect.right - rect.left;
    let height = top - bottom;

    let escaped = |s: &str| {
        s.replace('\\', r"\\")
            .replace('(', r"\(")
            .replace(')', r"\)")
    };

    Some(if multiline {
        build_multiline_appearance(font, size, left, top, bottom, width, height, &value, &escaped)
    } else {
        let x = left + 2.0;
        let baseline = bottom + (height - size) / 2.0 + size * 0.25;
        format!(
            "/Tx BMC q BT {font} {size:.0} Tf 0 g 1 0 0 1 {x:.1} {baseline:.1} Tm ({}) Tj ET Q EMC",
            escaped(&value)
        )
    })
}

/// Multi-line appearance: clip to the widget rect and draw one line per
/// explicit newline, top-aligned. Automatic word-wrapping is not applied
/// (that needs real font metrics).
fn build_multiline_appearance(
    font: &str,
    size: f32,
    left: f32,
    top: f32,
    bottom: f32,
    width: f32,
    height: f32,
    value: &str,
    escaped: &impl Fn(&str) -> String,
) -> String {
    let line_height = size * 1.2;
    let x = left + 2.0;
    let first_baseline = top - size * 0.8 - 2.0;

    let normalized = value.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();

    let mut body = String::new();
    body.push_str(&format!(
        "/Tx BMC q {left:.1} {bottom:.1} {width:.1} {height:.1} re W n BT {font} {size:.0} Tf 0 g 1 0 0 1 {x:.1} {first_baseline:.1} Tm"
    ));
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            body.push_str(&format!(" 0 -{line_height:.1} Td"));
        }
        body.push_str(&format!(" ({}) Tj", escaped(line)));
    }
    body.push_str(" ET Q EMC");
    body
}

fn get_string(
    bindings: &dyn PdfiumLibraryBindings,
    annot: FPDF_ANNOTATION,
    key: &str,
) -> Option<String> {
    let mut buf = vec![0u16; 1024];
    let len = unsafe {
        bindings.FPDFAnnot_GetStringValue(
            annot,
            key,
            buf.as_mut_ptr() as *mut FPDF_WCHAR,
            (buf.len() * 2) as c_ulong,
        )
    };
    if len == 0 {
        return Some(String::new());
    }
    let chars = (len as usize / 2).min(buf.len());
    Some(String::from_utf16_lossy(&buf[..chars]).trim_end_matches('\0').to_string())
}

fn parse_da(da: &str) -> (&str, f32) {
    let tokens: Vec<&str> = da.split_whitespace().collect();
    let tf_pos = tokens.iter().position(|t| *t == "Tf").unwrap_or(0);
    let size = tokens
        .get(tf_pos.wrapping_sub(1))
        .and_then(|s| s.parse::<f32>().ok())
        .unwrap_or(12.0);
    let font = tokens
        .get(tf_pos.wrapping_sub(2))
        .copied()
        .unwrap_or("/Helv");
    (font, size)
}
