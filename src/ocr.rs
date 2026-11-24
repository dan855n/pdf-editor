//! OCR support: render a PDF page and recognize its text with Tesseract.
//!
//! The engine is the open-source [Tesseract](https://github.com/tesseract-ocr/tesseract)
//! OCR library, driven through the `leptess` crate. It is not bundled with
//! this crate; the caller must supply a `tessdata` directory containing the
//! language data (e.g. `eng.traineddata`).

use pdfium_render::prelude::*;

use crate::document::{DocumentError, OpenDocument};

/// A recognized word with its bounding box in PDF page space (origin
/// bottom-left, units = points).
#[derive(Debug, Clone, PartialEq)]
pub struct OcrWord {
    pub text: String,
    pub left: f32,
    pub bottom: f32,
    pub right: f32,
    pub top: f32,
}

/// Render `page` at high resolution and OCR it, returning the recognized words
/// in PDF page coordinates.
///
/// On macOS/Linux (with `--features ocr`), this uses Tesseract via `leptess`,
/// and `tessdata` is the directory containing `*.traineddata` files.
#[cfg(feature = "ocr")]
pub fn ocr_page_words(
    doc: &OpenDocument,
    page: usize,
    tessdata: &std::path::Path,
    lang: &str,
) -> Result<Vec<OcrWord>, DocumentError> {
    // ~288 DPI gives Tesseract good accuracy without huge memory use.
    const SCALE: f32 = 4.0;

    let pdf_page = doc.doc.pages().get(page as i32)?;
    let page_w = pdf_page.width().value.max(1.0);
    let page_h = pdf_page.height().value.max(1.0);
    let width_px = (page_w * SCALE).round().max(1.0) as i32;
    let height_px = (page_h * SCALE).round().max(1.0) as i32;

    let config = PdfRenderConfig::new()
        .set_target_width(width_px)
        .set_target_height(height_px);
    let bitmap = pdf_page.render_with_config(&config)?;
    let img = bitmap.as_image()?;
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| DocumentError(format!("could not encode page as PNG: {e}")))?;

    let mut tess = leptess::LepTess::new(tessdata.to_str(), lang)
        .map_err(|e| DocumentError(format!("could not initialize Tesseract: {e}")))?;
    tess.set_image_from_mem(&png)
        .map_err(|e| DocumentError(format!("could not feed image to Tesseract: {e}")))?;
    // Tesseract requires the source resolution to be set after the image.
    tess.set_source_resolution((72.0 * SCALE) as i32);
    tess.recognize();
    let tsv = tess
        .get_tsv_text(0)
        .map_err(|e| DocumentError(format!("could not read OCR output: {e}")))?;

    parse_ocr_tsv(&tsv, page_w, page_h, width_px as f32, height_px as f32)
}

/// Render `page` at high resolution and OCR it using Windows 10/11's built-in
/// `Windows.Media.Ocr` engine, returning recognized words in PDF coordinates.
#[cfg(all(target_os = "windows", not(feature = "ocr")))]
pub fn ocr_page_words(
    doc: &OpenDocument,
    page: usize,
    _tessdata: &std::path::Path,
    _lang: &str,
) -> Result<Vec<OcrWord>, DocumentError> {
    use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap};
    use windows::Media::Ocr::OcrEngine;
    use windows::Storage::Streams::DataWriter;
    use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};

    const SCALE: f32 = 4.0;

    let pdf_page = doc.doc.pages().get(page as i32)?;
    let page_w = pdf_page.width().value.max(1.0);
    let page_h = pdf_page.height().value.max(1.0);
    let width_px = (page_w * SCALE).round().max(1.0) as i32;
    let height_px = (page_h * SCALE).round().max(1.0) as i32;

    let config = PdfRenderConfig::new()
        .set_target_width(width_px)
        .set_target_height(height_px);
    let bitmap = pdf_page.render_with_config(&config)?;
    let rgba = bitmap.as_image()?.to_rgba8();

    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }

    let writer = DataWriter::new()
        .map_err(|e| DocumentError(format!("could not create WinRT DataWriter: {e}")))?;
    writer
        .WriteBytes(rgba.as_raw())
        .map_err(|e| DocumentError(format!("could not write image bytes: {e}")))?;
    let buffer = writer
        .DetachBuffer()
        .map_err(|e| DocumentError(format!("could not detach WinRT buffer: {e}")))?;

    let sw_bitmap = SoftwareBitmap::CreateCopyWithAlphaFromBuffer(
        &buffer,
        BitmapPixelFormat::Rgba8,
        width_px,
        height_px,
        BitmapAlphaMode::Premultiplied,
    )
    .map_err(|e| DocumentError(format!("could not create SoftwareBitmap: {e}")))?;

    let engine = OcrEngine::TryCreateFromUserProfileLanguages().map_err(|e| {
        DocumentError(format!(
            "Windows OCR engine unavailable (check Windows language pack): {e}"
        ))
    })?;

    let result = engine
        .RecognizeAsync(&sw_bitmap)
        .and_then(|op| op.join())
        .map_err(|e| DocumentError(format!("Windows OCR failed: {e}")))?;

    let sx = page_w / (width_px as f32).max(1.0);
    let sy = page_h / (height_px as f32).max(1.0);
    let mut words = Vec::new();

    let lines = result
        .Lines()
        .map_err(|e| DocumentError(format!("could not read OCR lines: {e}")))?;
    for line in lines {
        let Ok(line_words) = line.Words() else {
            continue;
        };
        for word in line_words {
            let Ok(text_h) = word.Text() else { continue };
            let text = text_h.to_string_lossy().trim().to_string();
            if text.is_empty() {
                continue;
            }
            let Ok(rect) = word.BoundingRect() else {
                continue;
            };
            let left = rect.X * sx;
            let right = (rect.X + rect.Width) * sx;
            let top = page_h - rect.Y * sy;
            let bottom = page_h - (rect.Y + rect.Height) * sy;
            words.push(OcrWord {
                text,
                left,
                bottom,
                right,
                top,
            });
        }
    }

    Ok(words)
}

/// Parse Tesseract's TSV output into PDF-space words.
///
/// Tesseract's TSV columns are: level, page, block, par, line, word, left,
/// top, width, height, conf, text. Word rows have `level == 5`; the box is in
/// image pixels with the origin top-left.
#[cfg(any(feature = "ocr", test))]
fn parse_ocr_tsv(
    tsv: &str,
    page_w: f32,
    page_h: f32,
    width_px: f32,
    height_px: f32,
) -> Result<Vec<OcrWord>, DocumentError> {
    let sx = page_w / width_px.max(1.0);
    let sy = page_h / height_px.max(1.0);
    let mut words = Vec::new();

    for line in tsv.lines().skip(1) {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 12 || cols[0] != "5" {
            continue;
        }
        let text = cols[11].trim().to_string();
        if text.is_empty() {
            continue;
        }
        let left_px: f32 = cols[6].trim().parse().unwrap_or(0.0);
        let top_px: f32 = cols[7].trim().parse().unwrap_or(0.0);
        let w_px: f32 = cols[8].trim().parse().unwrap_or(0.0);
        let h_px: f32 = cols[9].trim().parse().unwrap_or(0.0);

        let left = left_px * sx;
        let right = (left_px + w_px) * sx;
        // Image y grows downward; PDF y grows upward.
        let top = page_h - top_px * sy;
        let bottom = page_h - (top_px + h_px) * sy;

        words.push(OcrWord {
            text,
            left,
            bottom,
            right,
            top,
        });
    }

    Ok(words)
}

/// Reconstruct readable text from OCR words in reading order, handling a
/// simple two-column layout: detect a large horizontal gap (the column break),
/// then reconstruct each column top-to-bottom, left-to-right.
pub fn words_to_text(words: &[OcrWord]) -> String {
    if words.is_empty() {
        return String::new();
    }

    // Detect a column break: the largest horizontal gap between sorted left
    // edges, if it is large enough to be a real column separator.
    let separator = {
        let mut lefts: Vec<f32> = words.iter().map(|w| w.left).collect();
        lefts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mut best_gap = 0.0_f32;
        let mut best_mid = None;
        for pair in lefts.windows(2) {
            let gap = pair[1] - pair[0];
            if gap > best_gap {
                best_gap = gap;
                best_mid = Some((pair[0] + pair[1]) / 2.0);
            }
        }
        if best_gap >= 40.0 { best_mid } else { None }
    };

    // Split into columns (left vs right of the separator).
    let mut columns: Vec<Vec<&OcrWord>> = Vec::new();
    for w in words {
        let col = match separator {
            Some(sep) if w.left >= sep => 1,
            _ => 0,
        };
        if columns.len() <= col {
            columns.resize(col + 1, Vec::new());
        }
        columns[col].push(w);
    }

    let mut out = String::new();
    for col in &columns {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&reconstruct_column(col));
    }
    out.trim_end().to_string()
}

/// Reconstruct a single column: group words into lines by their bottom edge,
/// order lines top-to-bottom, and join words within a line with spaces.
fn reconstruct_column(words: &[&OcrWord]) -> String {
    const LINE_TOLERANCE: f32 = 6.0;

    let mut lines: Vec<(f32, Vec<&OcrWord>)> = Vec::new();
    for w in words {
        if let Some((_, ws)) = lines
            .iter_mut()
            .find(|(bottom, _)| (bottom - w.bottom).abs() < LINE_TOLERANCE)
        {
            ws.push(w);
        } else {
            lines.push((w.bottom, vec![w]));
        }
    }
    lines.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut out = String::new();
    for (_, mut ws) in lines {
        ws.sort_by(|a, b| a.left.partial_cmp(&b.left).unwrap_or(std::cmp::Ordering::Equal));
        out.push_str(
            &ws.iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        );
        out.push('\n');
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_word_rows_and_maps_to_pdf_space() {
        let tsv = "\
level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext
5\t1\t1\t1\t1\t1\t100\t200\t50\t20\t95\tHello
5\t1\t1\t1\t1\t2\t160\t200\t40\t20\t90\tWorld
4\t1\t1\t1\t1\t0\t100\t200\t100\t20\t0\t
";

        // Page 612x792 pt rendered at 612x792 px (scale 1) for the test.
        let words = parse_ocr_tsv(tsv, 612.0, 792.0, 612.0, 792.0).unwrap();
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].text, "Hello");
        assert_eq!(words[0].left, 100.0);
        assert_eq!(words[0].right, 150.0);
        // top = 792 - 200 = 592; bottom = 792 - (200+20) = 572.
        assert_eq!(words[0].top, 592.0);
        assert_eq!(words[0].bottom, 572.0);
        assert_eq!(words[1].text, "World");
    }

    #[test]
    fn reconstructs_readable_text_from_words() {
        let words = vec![
            OcrWord { text: "Hello".into(), left: 74.0, bottom: 720.0, right: 100.0, top: 737.0 },
            OcrWord { text: "PDF".into(), left: 108.0, bottom: 720.0, right: 130.0, top: 737.0 },
            OcrWord { text: "Viewer".into(), left: 138.0, bottom: 720.0, right: 175.0, top: 737.0 },
            OcrWord { text: "second".into(), left: 74.0, bottom: 700.0, right: 108.0, top: 717.0 },
            OcrWord { text: "line".into(), left: 116.0, bottom: 700.0, right: 138.0, top: 717.0 },
        ];
        assert_eq!(words_to_text(&words), "Hello PDF Viewer\nsecond line");
    }

    #[test]
    fn reconstructs_two_columns_in_reading_order() {
        let words = vec![
            // Left column.
            OcrWord { text: "Left".into(), left: 36.0, bottom: 700.0, right: 60.0, top: 716.0 },
            OcrWord { text: "one".into(), left: 68.0, bottom: 700.0, right: 90.0, top: 716.0 },
            OcrWord { text: "Left".into(), left: 36.0, bottom: 680.0, right: 60.0, top: 696.0 },
            OcrWord { text: "two".into(), left: 68.0, bottom: 680.0, right: 90.0, top: 696.0 },
            // Right column (large horizontal gap).
            OcrWord { text: "Right".into(), left: 300.0, bottom: 700.0, right: 330.0, top: 716.0 },
            OcrWord { text: "one".into(), left: 338.0, bottom: 700.0, right: 360.0, top: 716.0 },
            OcrWord { text: "Right".into(), left: 300.0, bottom: 680.0, right: 330.0, top: 696.0 },
            OcrWord { text: "two".into(), left: 338.0, bottom: 680.0, right: 360.0, top: 696.0 },
        ];
        assert_eq!(
            words_to_text(&words),
            "Left one\nLeft two\n\nRight one\nRight two"
        );
    }
}
