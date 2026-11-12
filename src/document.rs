//! Open-document model plus the bridge to PDFium's annotation and page APIs.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pdfium_render::prelude::*;

use crate::annotations::{Annotation, AnnotationKind, StrokeStyle};

const ELLIPSE_CONTENTS_MARKER: &str = "__pdf_editor_ellipse__";
const IMAGE_CONTENTS_MARKER: &str = "__pdf_editor_image__";
/// A page's geometry.
#[derive(Debug, Clone, Copy)]
pub struct PageInfo {
    pub width_pts: f32,
    pub height_pts: f32,
    pub rotation: PdfPageRenderRotation,
}

/// Errors surfaced to the UI.
#[derive(Debug, Clone)]
pub struct DocumentError(pub String);

impl std::fmt::Display for DocumentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<PdfiumError> for DocumentError {
    fn from(e: PdfiumError) -> Self {
        DocumentError(e.to_string())
    }
}

/// The result of applying an original-text edit to a page.
pub struct TextEditApplication {
    pub object_original: String,
    pub object_new: String,
    /// Index of the original PDF text object in the page's object collection.
    pub object_index: usize,
}

/// An open PDF document.
///
/// The PDFium instance is leaked for the process lifetime so the document can
/// be stored as `'static` without threading a lifetime parameter through the
/// whole UI. Documents are still closed properly when replaced (their `Drop`
/// calls `FPDF_CloseDocument`).
pub struct OpenDocument {
    pub path: PathBuf,
    pub page_count: usize,
    /// The document. Not behind an `Arc`: editing needs mutable access, so the
    /// UI owns it directly.
    pub doc: PdfDocument<'static>,
    /// Shareable read-only handles for background render threads. Each thread
    /// gets its own `PdfDocument` loaded from the same path, which keeps the
    /// UI's mutable handle free for editing.
    pub render_docs: Vec<Arc<PdfDocument<'static>>>,
    pub pages: Vec<PageInfo>,
    pub title: String,
    pub author: String,
    pub producer: String,
    pub file_size: u64,
    /// Editor annotations per page, keyed by page index.
    pub annotations: Vec<Vec<Annotation>>,
    /// Next annotation id to hand out.
    pub next_id: u64,
    /// Temp files created while extracting reloaded stamp images; removed on
    /// [`Drop`] so reopening documents does not leak files into the temp dir.
    pub temp_stamp_files: Vec<PathBuf>,
}

impl Drop for OpenDocument {
    fn drop(&mut self) {
        for path in &self.temp_stamp_files {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl OpenDocument {
    /// Load a document from disk, reading any existing annotations.
    pub fn load(pdfium: &'static Pdfium, path: &Path) -> Result<Self, DocumentError> {
        let doc = pdfium.load_pdf_from_file(path, None)?;
        // SAFETY: the Pdfium this document borrows is leaked and lives for the
        // whole process, so extending 'a to 'static is sound.
        let doc: PdfDocument<'static> = unsafe { std::mem::transmute(doc) };

        let page_count = doc.pages().len().max(0) as usize;
        let mut pages = Vec::with_capacity(page_count);
        for i in 0..page_count {
            let page = doc.pages().get(i as i32)?;
            pages.push(PageInfo {
                width_pts: page.width().value,
                height_pts: page.height().value,
                rotation: page.rotation()?,
            });
        }

        let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);

        // Build a pool of read-only documents for background render threads.
        // Three handles matches MAX_CONCURRENT_RENDERS in the editor.
        let mut render_docs = Vec::new();
        for _ in 0..3 {
            if let Ok(d) = pdfium.load_pdf_from_file(path, None) {
                // SAFETY: same reasoning as the main handle above.
                let d: PdfDocument<'static> = unsafe { std::mem::transmute(d) };
                render_docs.push(Arc::new(d));
            }
        }

        let mut open = Self {
            path: path.to_path_buf(),
            page_count,
            doc,
            render_docs,
            pages,
            title: String::new(),
            author: String::new(),
            producer: String::new(),
            file_size,
            annotations: vec![Vec::new(); page_count],
            next_id: 1,
            temp_stamp_files: Vec::new(),
        };

        // Metadata reads need a fully-built struct, so fill it in now.
        open.title = meta(&open.doc, PdfDocumentMetadataTagType::Title);
        open.author = meta(&open.doc, PdfDocumentMetadataTagType::Author);
        open.producer = meta(&open.doc, PdfDocumentMetadataTagType::Producer);

        open.read_annotations();
        Ok(open)
    }

    /// Extract the original text lines for a page as editable runs.
    ///
    /// This uses PDFium's text page to group characters into visual lines.
    /// The result is UI-friendly plain data; the actual PDF text is not
    /// modified by calling this method.
    pub fn text_runs(&self, page: usize) -> Result<Vec<crate::text_model::TextRun>, DocumentError> {
        let pdf_page = self.doc.pages().get(page as i32)?;
        let text_page = pdf_page.text()?;
        crate::text_model::group_chars_into_runs(page, &text_page.chars())
            .map_err(DocumentError::from)
    }

    /// Replace the text of the PDF text object that contains the character at
    /// `first_char_index`, then regenerate the page content stream.
    pub fn set_text_for_run(
        &mut self,
        page: usize,
        first_char_index: usize,
        object_text: &str,
    ) -> Result<(), DocumentError> {
        let mut pdf_page = self.doc.pages_mut().get(page as i32)?;

        {
            let text_page = pdf_page.text()?;
            let chars = text_page.chars();
            let first_char = chars
                .iter()
                .find(|c| c.index() == first_char_index)
                .ok_or_else(|| DocumentError("could not find the text run's characters".into()))?;
            let mut text_obj = first_char.text_object()?;
            text_obj.set_text(object_text)?;
        }

        pdf_page.regenerate_content()?;
        Ok(())
    }

    /// Replace the text of the page text object at `object_index`, then
    /// regenerate the page content stream.
    pub fn set_text_at_object_index(
        &mut self,
        page: usize,
        object_index: usize,
        text: &str,
    ) -> Result<(), DocumentError> {
        let mut pdf_page = self.doc.pages_mut().get(page as i32)?;

        {
            let objects = pdf_page.objects_mut();
            let object = objects.get(object_index)?;
            let PdfPageObject::Text(mut text_obj) = object else {
                return Err(DocumentError("object is not a text object".into()));
            };
            text_obj.set_text(text)?;
        }

        pdf_page.regenerate_content()?;
        Ok(())
    }

    /// Replace a single visual line's text in the underlying PDF.
    ///
    /// Stage 1 keeps the original font and size. Only ASCII edits are
    /// accepted: PDFium's simple-font text objects cannot safely encode
    /// non-ASCII Unicode with `FPDFText_SetText_str`.
    pub fn apply_text_edit(
        &mut self,
        page: usize,
        run: &crate::text_model::TextRun,
        new_run_text: &str,
    ) -> Result<TextEditApplication, DocumentError> {
        if !new_run_text.is_ascii() {
            return Err(DocumentError(
                "Stage 1 text editing supports ASCII only so the original font and size stay intact."
                    .into(),
            ));
        }

        let (object_original, object_index) = {
            let pdf_page = self.doc.pages().get(page as i32)?;
            let text_page = pdf_page.text()?;
            let chars = text_page.chars();
            let first_char = chars
                .iter()
                .find(|c| c.index() == run.first_char_index)
                .ok_or_else(|| DocumentError("could not find the text run's characters".into()))?;
            let text_obj = first_char.text_object()?;
            let object_original = text_obj.text();

            if object_original.trim() != run.text.trim() {
                return Err(DocumentError(
                    "This line shares a text object with other lines; Stage 1 only supports whole-line text objects."
                        .into(),
                ));
            }

            let object_index = pdf_page
                .objects()
                .iter()
                .position(
                    |obj| matches!(obj, PdfPageObject::Text(t) if t.text() == object_original),
                )
                .ok_or_else(|| {
                    DocumentError("could not locate the text object on the page".into())
                })?;

            (object_original, object_index)
        };

        // Preserve any leading/trailing whitespace the PDF kept in the
        // text object, replacing only the visual line's text.
        let leading = object_original.len() - object_original.trim_start().len();
        let trailing = object_original.len() - object_original.trim_end().len();
        let object_new = format!(
            "{}{}{}",
            &object_original[..leading],
            new_run_text,
            &object_original[object_original.len() - trailing..]
        );

        self.set_text_at_object_index(page, object_index, &object_new)?;

        Ok(TextEditApplication {
            object_original,
            object_new,
            object_index,
        })
    }

    /// Replace a single word's text within its PDF text object, leaving the
    /// rest of the object's text intact.
    ///
    /// Only ASCII replacements are accepted (as with [`Self::apply_text_edit`]);
    /// the original font and size are preserved. The word is located by its
    /// first character index and matched by its text within the object.
    pub fn apply_word_edit(
        &mut self,
        page: usize,
        word: &crate::text_model::Word,
        new_text: &str,
    ) -> Result<TextEditApplication, DocumentError> {
        if !new_text.is_ascii() {
            return Err(DocumentError(
                "Stage 1 text editing supports ASCII only so the original font and size stay intact."
                    .into(),
            ));
        }

        let (object_original, object_index) = {
            let pdf_page = self.doc.pages().get(page as i32)?;
            let text_page = pdf_page.text()?;
            let chars = text_page.chars();
            let first_char = chars
                .iter()
                .find(|c| c.index() == word.first_char_index)
                .ok_or_else(|| DocumentError("could not find the word's characters".into()))?;
            let text_obj = first_char.text_object()?;
            let object_original = text_obj.text();
            let object_index = pdf_page
                .objects()
                .iter()
                .position(|obj| matches!(obj, PdfPageObject::Text(t) if t.text() == object_original))
                .ok_or_else(|| {
                    DocumentError("could not locate the text object on the page".into())
                })?;
            (object_original, object_index)
        };

        // Replace the first occurrence of the word's text within the object.
        let word_pos = object_original
            .find(&word.text)
            .ok_or_else(|| {
                DocumentError("could not locate the word in its text object".into())
            })?;
        let object_new = format!(
            "{}{}{}",
            &object_original[..word_pos],
            new_text,
            &object_original[word_pos + word.text.len()..]
        );

        self.set_text_at_object_index(page, object_index, &object_new)?;

        Ok(TextEditApplication {
            object_original,
            object_new,
            object_index,
        })
    }

    /// Return the index of the PDF text object backing `run`.
    pub fn text_object_index_for_run(
        &self,
        page: usize,
        run: &crate::text_model::TextRun,
    ) -> Result<usize, DocumentError> {
        let pdf_page = self.doc.pages().get(page as i32)?;
        let text_page = pdf_page.text()?;
        let chars = text_page.chars();
        let first_char = chars
            .iter()
            .find(|c| c.index() == run.first_char_index)
            .ok_or_else(|| DocumentError("could not find the text run's characters".into()))?;
        let object_text = first_char.text_object()?.text();
        pdf_page
            .objects()
            .iter()
            .position(|obj| matches!(obj, PdfPageObject::Text(t) if t.text() == object_text))
            .ok_or_else(|| DocumentError("could not locate the text object on the page".into()))
    }

    /// Translate the text object at `object_index` by the given PDF-point deltas.
    pub fn translate_text_object(
        &mut self,
        page: usize,
        object_index: usize,
        dx_pt: f32,
        dy_pt: f32,
    ) -> Result<(), DocumentError> {
        let mut pdf_page = self.doc.pages_mut().get(page as i32)?;

        {
            let objects = pdf_page.objects_mut();
            let mut object = objects.get(object_index)?;
            if !matches!(object, PdfPageObject::Text(_)) {
                return Err(DocumentError("object is not a text object".into()));
            }
            object.translate(PdfPoints::new(dx_pt), PdfPoints::new(dy_pt))?;
        }

        pdf_page.regenerate_content()?;
        Ok(())
    }

    /// Return the transformation matrix of the text object at `object_index`.
    pub fn text_object_matrix(
        &self,
        page: usize,
        object_index: usize,
    ) -> Result<PdfMatrix, DocumentError> {
        let pdf_page = self.doc.pages().get(page as i32)?;
        let object = pdf_page.objects().get(object_index)?;
        let PdfPageObject::Text(text_obj) = object else {
            return Err(DocumentError("object is not a text object".into()));
        };
        Ok(text_obj.matrix()?)
    }

    /// Set the transformation matrix of the text object at `object_index`.
    pub fn set_text_object_matrix(
        &mut self,
        page: usize,
        object_index: usize,
        matrix: PdfMatrix,
    ) -> Result<(), DocumentError> {
        let mut pdf_page = self.doc.pages_mut().get(page as i32)?;
        {
            let objects = pdf_page.objects_mut();
            let object = objects.get(object_index)?;
            let PdfPageObject::Text(mut text_obj) = object else {
                return Err(DocumentError("object is not a text object".into()));
            };
            text_obj.reset_matrix(matrix)?;
        }
        pdf_page.regenerate_content()?;
        Ok(())
    }

    /// Return the unscaled font size of the text object at `object_index`.
    pub fn text_object_font_size(
        &self,
        page: usize,
        object_index: usize,
    ) -> Result<f32, DocumentError> {
        let pdf_page = self.doc.pages().get(page as i32)?;
        let object = pdf_page.objects().get(object_index)?;
        let PdfPageObject::Text(text_obj) = object else {
            return Err(DocumentError("object is not a text object".into()));
        };
        Ok(text_obj.unscaled_font_size().value)
    }

    /// Set the unscaled font size of the text object at `object_index`.
    ///
    /// Note this applies to the whole text object (usually one line), not to an
    /// individual word within it.
    pub fn set_text_object_font_size(
        &mut self,
        page: usize,
        object_index: usize,
        size: f32,
    ) -> Result<(), DocumentError> {
        let mut pdf_page = self.doc.pages_mut().get(page as i32)?;
        {
            let objects = pdf_page.objects_mut();
            let object = objects.get(object_index)?;
            let PdfPageObject::Text(text_obj) = object else {
                return Err(DocumentError("object is not a text object".into()));
            };
            text_obj.set_unscaled_font_size(PdfPoints::new(size))?;
        }
        pdf_page.regenerate_content()?;
        Ok(())
    }

    /// Return the render mode of the text object at `object_index`.
    pub fn text_object_render_mode(
        &self,
        page: usize,
        object_index: usize,
    ) -> Result<PdfPageTextRenderMode, DocumentError> {
        let pdf_page = self.doc.pages().get(page as i32)?;
        let object = pdf_page.objects().get(object_index)?;
        let PdfPageObject::Text(text_obj) = object else {
            return Err(DocumentError("object is not a text object".into()));
        };
        Ok(text_obj.render_mode())
    }

    /// Set the render mode of the text object at `object_index`.
    pub fn set_text_object_render_mode(
        &mut self,
        page: usize,
        object_index: usize,
        mode: PdfPageTextRenderMode,
    ) -> Result<(), DocumentError> {
        let mut pdf_page = self.doc.pages_mut().get(page as i32)?;
        {
            let objects = pdf_page.objects_mut();
            let object = objects.get(object_index)?;
            let PdfPageObject::Text(mut text_obj) = object else {
                return Err(DocumentError("object is not a text object".into()));
            };
            text_obj.set_render_mode(mode)?;
        }
        pdf_page.regenerate_content()?;
        Ok(())
    }

    /// Add an invisible text layer for the given OCR words, making a scanned
    /// page searchable and selectable without changing its appearance.
    ///
    /// Each word becomes an invisible text object (render mode 3) positioned
    /// over its recognized location.
    #[cfg(feature = "ocr")]
    pub fn add_invisible_text_layer(
        &mut self,
        page: usize,
        words: &[crate::ocr::OcrWord],
    ) -> Result<(), DocumentError> {
        let font = self.doc.fonts_mut().new_built_in(PdfFontBuiltin::Helvetica);
        let mut pdf_page = self.doc.pages_mut().get(page as i32)?;

        for w in words {
            let font_size = (w.top - w.bottom).max(2.0);
            // Append a trailing space so copy/paste yields word-separated text
            // (pdfium does not otherwise insert spaces between text objects).
            let text = format!("{} ", w.text);
            let mut obj = PdfPageTextObject::new(
                &self.doc,
                text,
                font,
                PdfPoints::new(font_size),
            )?;
            obj.set_render_mode(PdfPageTextRenderMode::Invisible)?;
            obj.translate(PdfPoints::new(w.left), PdfPoints::new(w.bottom))?;
            pdf_page.objects_mut().add_text_object(obj)?;
        }

        pdf_page.regenerate_content()?;
        Ok(())
    }

    /// Translate an existing image object by the given PDF-point deltas.
    pub fn translate_image_object(
        &mut self,
        page: usize,
        object_index: usize,
        dx_pt: f32,
        dy_pt: f32,
    ) -> Result<(), DocumentError> {
        let mut pdf_page = self.doc.pages_mut().get(page as i32)?;

        {
            let objects = pdf_page.objects_mut();
            let mut object = objects.get(object_index)?;
            object.translate(PdfPoints::new(dx_pt), PdfPoints::new(dy_pt))?;
        }

        pdf_page.regenerate_content()?;
        Ok(())
    }

    /// Rotate an existing image object clockwise by `degrees_cw` around the
    /// center of its visible bounding box.
    ///
    /// The object's transformation matrix is updated in place, so the page
    /// content stream is regenerated once after the edit. This mirrors the
    /// matrix-only rotation used by PDF Expert's
    /// `RDPDFPageContentEditingManager rotateImage:degree:`.
    pub fn rotate_image_object(
        &mut self,
        page: usize,
        object_index: usize,
        degrees_cw: f32,
    ) -> Result<(), DocumentError> {
        let mut pdf_page = self.doc.pages_mut().get(page as i32)?;

        {
            let objects = pdf_page.objects_mut();
            let mut object = objects.get(object_index)?;

            let bounds = object.bounds()?;
            let rect = bounds.to_rect();
            let center_x = (rect.left().value + rect.right().value) / 2.0;
            let center_y = (rect.bottom().value + rect.top().value) / 2.0;

            // T(-center) x R(degrees) x T(center): with row-vector PDF
            // matrices the rightmost matrix applies first, so this rotates
            // around the object's on-page center without translating it away.
            let rotation = PdfMatrix::identity()
                .translate(PdfPoints::new(-center_x), PdfPoints::new(-center_y))?
                .rotate_clockwise_degrees(degrees_cw)?
                .translate(PdfPoints::new(center_x), PdfPoints::new(center_y))?;

            object.apply_matrix(rotation)?;
        }

        pdf_page.regenerate_content()?;
        Ok(())
    }

    /// Replace the image data in the page image object at `object_index`.
    ///
    /// The object's on-page rectangle and transformation matrix are preserved,
    /// so the replacement image is stretched to the existing image frame. If
    /// the replacement has a different aspect ratio, crop or scale it before
    /// calling this function.
    pub fn replace_image_object(
        &mut self,
        page: usize,
        object_index: usize,
        new_image: &image::DynamicImage,
    ) -> Result<(), DocumentError> {
        let mut pdf_page = self.doc.pages_mut().get(page as i32)?;

        {
            let objects = pdf_page.objects_mut();
            let object = objects.get(object_index)?;
            let PdfPageObject::Image(mut image_obj) = object else {
                return Err(DocumentError("object is not an image object".into()));
            };
            image_obj.set_image(new_image)?;
        }

        pdf_page.regenerate_content()?;
        Ok(())
    }

    /// Decode an image from bytes and replace the page image object at
    /// `object_index` with it.
    pub fn replace_image_object_from_bytes(
        &mut self,
        page: usize,
        object_index: usize,
        image_bytes: &[u8],
    ) -> Result<(), DocumentError> {
        let new_image = image::load_from_memory(image_bytes)
            .map_err(|e| DocumentError(format!("could not decode replacement image: {e}")))?;
        self.replace_image_object(page, object_index, &new_image)
    }

    /// Crop the page image object at `object_index` to the given PDF-point
    /// rectangle, replacing its bitmap with the cropped page rendering and
    /// resetting its matrix so it occupies exactly the crop rectangle.
    ///
    /// This mirrors PDF Expert's crop flow: render the page, extract the crop
    /// region, then swap the image XObject and set the new on-page rectangle.
    pub fn crop_image_object(
        &mut self,
        page: usize,
        object_index: usize,
        crop_rect: PdfRect,
    ) -> Result<(), DocumentError> {
        let left = crop_rect.left().value.min(crop_rect.right().value);
        let right = crop_rect.left().value.max(crop_rect.right().value);
        let bottom = crop_rect.bottom().value.min(crop_rect.top().value);
        let top = crop_rect.bottom().value.max(crop_rect.top().value);
        let crop_w = (right - left).abs().max(1.0);
        let crop_h = (top - bottom).abs().max(1.0);

        // Render the whole page at a scale that gives the crop region at least
        // 512 px on its longest side while keeping the full-page bitmap within
        // 4000 px to bound memory use.
        let pdf_page = self.doc.pages().get(page as i32)?;
        let page_w = pdf_page.width().value.max(1.0);
        let page_h = pdf_page.height().value.max(1.0);
        let scale = (512.0 / crop_w.max(crop_h)).clamp(0.5, 4.0);
        let scale = scale.min(4000.0 / page_w.max(page_h).max(1.0)).max(0.05);

        let width_px = (page_w * scale).max(1.0) as i32;
        let height_px = (page_h * scale).max(1.0) as i32;
        let config = PdfRenderConfig::new()
            .set_target_width(width_px)
            .set_target_height(height_px);
        let bitmap = pdf_page.render_with_config(&config)?;
        let page_image = bitmap.as_image()?;
        let rgba = page_image.to_rgba8();

        // The bitmap origin is top-left; PDF y grows upward from the page bottom.
        let px_x = (left * scale).floor().clamp(0.0, (width_px - 1) as f32) as u32;
        let px_y = ((page_h - top) * scale)
            .floor()
            .clamp(0.0, (height_px - 1) as f32) as u32;
        let px_w = (crop_w * scale).ceil().max(1.0) as u32;
        let px_h = (crop_h * scale).ceil().max(1.0) as u32;
        let px_w = px_w.min(width_px as u32 - px_x);
        let px_h = px_h.min(height_px as u32 - px_y);
        let cropped = image::imageops::crop_imm(&rgba, px_x, px_y, px_w, px_h).to_image();
        let cropped = image::DynamicImage::ImageRgba8(cropped);

        let mut pdf_page = self.doc.pages_mut().get(page as i32)?;
        {
            let objects = pdf_page.objects_mut();
            let object = objects.get(object_index)?;
            let PdfPageObject::Image(mut image_obj) = object else {
                return Err(DocumentError("object is not an image object".into()));
            };

            image_obj.set_image(&cropped)?;
            image_obj.reset_matrix(PdfMatrix::IDENTITY)?;
            image_obj.scale(crop_w, crop_h)?;
            image_obj.translate(PdfPoints::new(left), PdfPoints::new(bottom))?;
        }

        pdf_page.regenerate_content()?;
        Ok(())
    }

    /// Return the transformation matrix for the page image object at
    /// `object_index`.
    pub fn image_object_matrix(
        &self,
        page: usize,
        object_index: usize,
    ) -> Result<PdfMatrix, DocumentError> {
        let pdf_page = self.doc.pages().get(page as i32)?;
        let object = pdf_page.objects().get(object_index)?;
        let PdfPageObject::Image(image_obj) = object else {
            return Err(DocumentError("object is not an image object".into()));
        };
        Ok(image_obj.matrix()?)
    }

    /// Return the raw image data for the page image object at `object_index`,
    /// encoded as PNG bytes. This is a snapshot for undo/redo.
    pub fn image_object_png_bytes(
        &self,
        page: usize,
        object_index: usize,
    ) -> Result<Vec<u8>, DocumentError> {
        let pdf_page = self.doc.pages().get(page as i32)?;
        let object = pdf_page.objects().get(object_index)?;
        let Some(image_obj) = object.as_image_object() else {
            return Err(DocumentError("object is not an image object".into()));
        };
        let raw_image = image_obj.get_raw_image()?;
        let mut bytes = Vec::new();
        raw_image
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .map_err(|e| DocumentError(format!("could not encode image snapshot: {e}")))?;
        Ok(bytes)
    }

    /// Restore both the bitmap and transformation matrix of the page image
    /// object at `object_index`. Used by undo/redo for crop and replace edits.
    pub fn restore_image_object(
        &mut self,
        page: usize,
        object_index: usize,
        image_bytes: &[u8],
        matrix: PdfMatrix,
    ) -> Result<(), DocumentError> {
        let new_image = image::load_from_memory(image_bytes)
            .map_err(|e| DocumentError(format!("could not decode image snapshot: {e}")))?;

        let mut pdf_page = self.doc.pages_mut().get(page as i32)?;
        {
            let objects = pdf_page.objects_mut();
            let object = objects.get(object_index)?;
            let PdfPageObject::Image(mut image_obj) = object else {
                return Err(DocumentError("object is not an image object".into()));
            };
            image_obj.set_image(&new_image)?;
            image_obj.reset_matrix(matrix)?;
        }

        pdf_page.regenerate_content()?;
        Ok(())
    }

    /// Set the transformation matrix of the page image object at
    /// `object_index`. Used by image-move undo/redo.
    pub fn set_image_object_matrix(
        &mut self,
        page: usize,
        object_index: usize,
        matrix: PdfMatrix,
    ) -> Result<(), DocumentError> {
        let mut pdf_page = self.doc.pages_mut().get(page as i32)?;

        {
            let objects = pdf_page.objects_mut();
            let object = objects.get(object_index)?;
            let PdfPageObject::Image(mut image_obj) = object else {
                return Err(DocumentError("object is not an image object".into()));
            };
            image_obj.reset_matrix(matrix)?;
        }

        pdf_page.regenerate_content()?;
        Ok(())
    }

    /// Remove the page text object at `object_index`, then regenerate content.
    pub fn remove_text_object_at_index(
        &mut self,
        page: usize,
        object_index: usize,
    ) -> Result<(), DocumentError> {
        let mut pdf_page = self.doc.pages_mut().get(page as i32)?;
        pdf_page
            .objects_mut()
            .remove_object_at_index(object_index)?;
        pdf_page.regenerate_content()?;
        Ok(())
    }

    /// Rebuild the read-only render document pool from the current in-memory
    /// document. Call this after editing page content so background page
    /// renders reflect the latest changes.
    pub fn refresh_render_docs(&mut self, pdfium: &'static Pdfium) {
        let Ok(bytes) = self.doc.save_to_bytes() else {
            return;
        };

        let mut render_docs = Vec::new();
        for _ in 0..3 {
            if let Ok(d) = pdfium.load_pdf_from_byte_vec(bytes.clone(), None) {
                // SAFETY: the Pdfium instance is leaked and lives for the
                // whole process, so extending 'a to 'static is sound.
                let d: PdfDocument<'static> = unsafe { std::mem::transmute(d) };
                render_docs.push(Arc::new(d));
            }
        }

        if !render_docs.is_empty() {
            self.render_docs = render_docs;
        }
    }

    /// Re-read page geometry after pages are added, deleted or rotated.
    pub fn refresh_pages(&mut self) -> Result<(), DocumentError> {
        let count = self.doc.pages().len().max(0) as usize;
        let mut pages = Vec::with_capacity(count);
        for i in 0..count {
            let page = self.doc.pages().get(i as i32)?;
            pages.push(PageInfo {
                width_pts: page.width().value,
                height_pts: page.height().value,
                rotation: page.rotation()?,
            });
        }
        self.pages = pages;
        self.page_count = count;
        Ok(())
    }

    /// Read annotations from every page into the editor model.
    pub fn read_annotations(&mut self) {
        self.annotations = vec![Vec::new(); self.page_count];
        self.next_id = 1;

        for p in 0..self.page_count {
            let Ok(page) = self.doc.pages().get(p as i32) else {
                continue;
            };
            let annots: Vec<PdfPageAnnotation> = page.annotations().iter().collect();

            let mut list = Vec::new();
            for a in &annots {
                if let Some(model) = annotation_to_model(a, self.next_id) {
                    self.next_id = self.next_id.max(model.id + 1);
                    list.push(model);
                }
            }
            self.annotations[p] = list;
        }

        self.attach_stamp_images();
    }

    /// Attach image data to reloaded stamp annotations.
    ///
    /// Images are written as page objects plus a hidden marker annotation, so on
    /// reload the model has the marker's bounds but no image. This extracts each
    /// stamp's image into a temp file and records its path and size, so the
    /// editor can render and edit it again.
    fn attach_stamp_images(&mut self) {
        for page in 0..self.page_count {
            // Collect image page objects first (the page handle is 'static, but
            // the bitmap work is best scoped before we mutate the model).
            let mut images: Vec<(f32, f32, f32, f32, image::DynamicImage)> = Vec::new();
            {
                let Ok(pdf_page) = self.doc.pages().get(page as i32) else {
                    continue;
                };
                for obj in pdf_page.objects().iter() {
                    let Some(img_obj) = obj.as_image_object() else {
                        continue;
                    };
                    let Ok(bounds) = img_obj.bounds() else {
                        continue;
                    };
                    let rect = bounds.to_rect();
                    // Use the raw image: `get_processed_image` mutates the
                    // object's transform matrix (pdfium-render issue #52
                    // workaround), which would corrupt on-page placement.
                    let Ok(image) = img_obj.get_raw_image() else {
                        continue;
                    };
                    images.push((
                        rect.left().value,
                        rect.bottom().value,
                        rect.right().value,
                        rect.top().value,
                        image,
                    ));
                }
            }

            // Match each pathless stamp to its image by bounds.
            let mut updates: Vec<(usize, String, (f32, f32))> = Vec::new();
            for (idx, a) in self.annotations[page].iter().enumerate() {
                if a.kind != AnnotationKind::Stamp || a.image_path.is_some() {
                    continue;
                }
                let (l, b, r, t) = a.bounds_f32();
                let Some((_, _, _, _, image)) = images.iter().find(|(il, ib, ir, it, _)| {
                    (il - l).abs() < 1.5
                        && (ib - b).abs() < 1.5
                        && (ir - r).abs() < 1.5
                        && (it - t).abs() < 1.5
                }) else {
                    continue;
                };
                let nonce = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                let path = std::env::temp_dir().join(format!(
                    "pdf-editor-stamp-{}-{idx}-{nonce}.png",
                    std::process::id()
                ));
                if image.save(&path).is_ok() {
                    let path_str = path.to_string_lossy().into_owned();
                    self.temp_stamp_files.push(path);
                    updates.push((idx, path_str, (r - l, t - b)));
                }
            }

            for (idx, path, size) in updates {
                let a = &mut self.annotations[page][idx];
                a.image_path = Some(path);
                a.image_size = Some(size);
            }
        }
    }

    /// Create a new annotation on a page and apply it to the PDF.
    pub fn add_annotation(
        &mut self,
        page: usize,
        mut annotation: Annotation,
    ) -> Result<(), DocumentError> {
        annotation.id = self.next_id;
        self.next_id += 1;

        self.apply_annotation(page, &annotation)?;
        self.annotations
            .get_mut(page)
            .map(|list| list.push(annotation))
            .ok_or_else(|| DocumentError(format!("no such page: {page}")))?;
        Ok(())
    }

    /// Replace an existing annotation (matched by id) with new geometry/style.
    pub fn update_annotation(
        &mut self,
        page: usize,
        annotation: Annotation,
    ) -> Result<(), DocumentError> {
        // Remove the old PDF annotation, then create the updated one.
        let id = annotation.id;
        if let Some(old) = self
            .annotations
            .get(page)
            .and_then(|list| list.iter().find(|a| a.id == annotation.id))
            .cloned()
        {
            self.remove_annotation_from_pdf(page, &old)?;
        }

        self.apply_annotation(page, &annotation)?;
        if let Some(list) = self.annotations.get_mut(page) {
            if let Some(existing) = list.iter_mut().find(|a| a.id == id) {
                *existing = annotation;
            } else {
                list.push(annotation);
            }
        }
        Ok(())
    }

    /// Delete an annotation by id.
    pub fn delete_annotation(&mut self, page: usize, id: u64) -> Result<(), DocumentError> {
        let Some(list) = self.annotations.get(page) else {
            return Err(DocumentError(format!("no such page: {page}")));
        };
        let Some(annotation) = list.iter().find(|a| a.id == id).cloned() else {
            return Err(DocumentError(format!("no such annotation: {id}")));
        };
        self.remove_annotation_from_pdf(page, &annotation)?;
        if let Some(list) = self.annotations.get_mut(page) {
            list.retain(|a| a.id != id);
        }
        Ok(())
    }

    /// Write the PDF annotation objects for `annotation` onto the page.
    /// Public so the editor's undo/redo can re-apply a snapshot.
    pub fn apply_annotation_public(
        &mut self,
        page: usize,
        annotation: &Annotation,
    ) -> Result<(), DocumentError> {
        self.apply_annotation(page, annotation)
    }

    fn apply_annotation(
        &mut self,
        page: usize,
        annotation: &Annotation,
    ) -> Result<(), DocumentError> {
        let mut pdf_page = self.doc.pages_mut().get(page as i32)?;
        let (l, b, r, t) = annotation.bounds_f32();

        match annotation.kind {
            AnnotationKind::Highlight => {
                let mut a = pdf_page.annotations_mut().create_highlight_annotation()?;
                a.set_bounds(PdfRect::new_from_values(b, l, t, r))?;
                if let Some(fill) = annotation.style.fill {
                    a.set_fill_color(crate::annotations::rgba_to_pdf_color(fill))?;
                }
                // Highlight needs at least one attachment point to render.
                a.attachment_points_mut().create_attachment_point_at_end(
                    PdfQuadPoints::new_from_values(l, b, r, b, r, t, l, t),
                )?;
            }

            AnnotationKind::Ink => {
                let mut a = pdf_page.annotations_mut().create_ink_annotation()?;
                a.set_bounds(PdfRect::new_from_values(b, l, t, r))?;
                if let Some(stroke) = annotation.style.stroke {
                    a.set_stroke_color(crate::annotations::rgba_to_pdf_color(stroke))?;
                }
                if !annotation.points.is_empty() {
                    // `create_ink_annotation` returns the concrete ink type,
                    // which exposes `objects_mut()` directly.
                    let mut path = PdfPagePathObject::new(
                        &self.doc,
                        PdfPoints::new(annotation.points[0].0),
                        PdfPoints::new(annotation.points[0].1),
                        Some(crate::annotations::rgba_to_pdf_color(
                            annotation.style.stroke.unwrap_or([0, 0, 0, 255]),
                        )),
                        Some(PdfPoints::new(annotation.style.width.max(0.5))),
                        None,
                    )?;
                    for (x, y) in annotation.points.iter().skip(1) {
                        path.line_to(PdfPoints::new(*x), PdfPoints::new(*y))?;
                    }
                    a.objects_mut().add_path_object(path)?;
                }
            }

            AnnotationKind::Rectangle => {
                let mut a = pdf_page.annotations_mut().create_square_annotation()?;
                a.set_bounds(PdfRect::new_from_values(b, l, t, r))?;
                if let Some(fill) = annotation.style.fill {
                    a.set_fill_color(crate::annotations::rgba_to_pdf_color(fill))?;
                }
                if let Some(stroke) = annotation.style.stroke {
                    a.set_stroke_color(crate::annotations::rgba_to_pdf_color(stroke))?;
                }
            }

            AnnotationKind::Ellipse => {
                // pdfium-render has no public circle-annotation creator, so an
                // ellipse is a stamp annotation carrying a vector ellipse path.
                let mut a = pdf_page.annotations_mut().create_stamp_annotation()?;
                a.set_bounds(PdfRect::new_from_values(b, l, t, r))?;
                a.set_contents(ELLIPSE_CONTENTS_MARKER)?;
                let cx = (l + r) / 2.0;
                let cy = (b + t) / 2.0;
                let path = PdfPagePathObject::new_ellipse_at(
                    &self.doc,
                    PdfPoints::new(cx),
                    PdfPoints::new(cy),
                    PdfPoints::new(((r - l) / 2.0).abs()),
                    PdfPoints::new(((t - b) / 2.0).abs()),
                    Some(crate::annotations::rgba_to_pdf_color(
                        annotation.style.stroke.unwrap_or([0, 0, 0, 255]),
                    )),
                    Some(PdfPoints::new(annotation.style.width.max(0.5))),
                    annotation
                        .style
                        .fill
                        .map(crate::annotations::rgba_to_pdf_color),
                )?;
                a.objects_mut().add_object(PdfPageObject::Path(path))?;
            }

            AnnotationKind::FreeText => {
                let mut a = pdf_page
                    .annotations_mut()
                    .create_free_text_annotation(annotation.text.as_str())?;
                a.set_bounds(PdfRect::new_from_values(b, l, t, r))?;
                if let Some(stroke) = annotation.style.stroke {
                    a.set_stroke_color(crate::annotations::rgba_to_pdf_color(stroke))?;
                }
                if let Some(fill) = annotation.style.fill {
                    a.set_fill_color(crate::annotations::rgba_to_pdf_color(fill))?;
                }
            }

            AnnotationKind::Stamp => {
                let Some(image_path) = annotation.image_path.as_deref() else {
                    return Ok(());
                };
                let Some((w, h)) = annotation.image_size else {
                    return Ok(());
                };
                let img = image::open(image_path).map_err(|e| {
                    DocumentError(format!("could not load image {image_path}: {e}"))
                })?;
                let mut obj = PdfPageImageObject::new_with_size(
                    &self.doc,
                    &img,
                    PdfPoints::new(w),
                    PdfPoints::new(h),
                )?;
                obj.translate(PdfPoints::new(l), PdfPoints::new(b))?;
                pdf_page.objects_mut().add_image_object(obj)?;

                // Keep a hidden marker annotation so the editor can recover
                // selection and deletion metadata after the PDF is reopened.
                let mut a = pdf_page.annotations_mut().create_stamp_annotation()?;
                a.set_bounds(PdfRect::new_from_values(b, l, t, r))?;
                a.set_contents(IMAGE_CONTENTS_MARKER)?;
                a.set_is_hidden(true)?;
                a.set_is_invisible_if_unsupported(true)?;
            }
        }

        Ok(())
    }

    /// Remove the PDF objects backing `annotation` from the page.
    /// Public so the editor's undo/redo can strip a snapshot's objects without
    /// touching foreign annotations.
    pub fn remove_annotation_from_pdf(
        &mut self,
        page: usize,
        annotation: &Annotation,
    ) -> Result<(), DocumentError> {
        // Match by bounds and type — PDFium doesn't expose a stable annotation
        // id, so geometry plus subtype is the most reliable handle we have.
        let wanted = annotation.kind;
        let (l, b, r, t) = annotation.bounds_f32();

        if wanted == AnnotationKind::Stamp {
            let mut pdf_page = self.doc.pages_mut().get(page as i32)?;
            let mut object_index = None;
            for (index, object) in pdf_page.objects().iter().enumerate() {
                let Some(image) = object.as_image_object() else {
                    continue;
                };
                let Ok(bounds) = image.bounds() else {
                    continue;
                };
                if (bounds.left().value - l).abs() < 1.5
                    && (bounds.bottom().value - b).abs() < 1.5
                    && (bounds.right().value - r).abs() < 1.5
                    && (bounds.top().value - t).abs() < 1.5
                {
                    object_index = Some(index);
                    break;
                }
            }
            if let Some(index) = object_index {
                pdf_page.objects_mut().remove_object_at_index(index)?;
            }
        }

        // Collect the match first so we're not holding a borrow on the page
        // while deleting. Annotation handles borrow their page, so match by
        // index rather than keeping a handle around.
        let mut match_index = None;
        {
            let pdf_page = self.doc.pages().get(page as i32)?;
            let annots = pdf_page.annotations();
            for (index, a) in annots.iter().enumerate() {
                if kind_of(&a) != Some(wanted) {
                    continue;
                }
                let Ok(bounds) = a.bounds() else { continue };
                let (bl, bb, br, bt) = (
                    bounds.left().value,
                    bounds.bottom().value,
                    bounds.right().value,
                    bounds.top().value,
                );
                let same = (bl - l).abs() < 1.5
                    && (bb - b).abs() < 1.5
                    && (br - r).abs() < 1.5
                    && (bt - t).abs() < 1.5;
                if same {
                    match_index = Some(index);
                    break;
                }
            }
        }

        if let Some(index) = match_index {
            // Get and delete through the same mutable annotations collection.
            // `get()` returns a handle tied to the document lifetime, not to
            // the immutable borrow of the collection, so it can be consumed by
            // `delete_annotation()` immediately afterwards.
            let mut pdf_page = self.doc.pages_mut().get(page as i32)?;
            let annotations = pdf_page.annotations_mut();
            let annotation = annotations.get(index)?;
            annotations.delete_annotation(annotation)?;
        }
        Ok(())
    }

    /// Delete a page from the document.
    pub fn delete_page(&mut self, page: usize) -> Result<(), DocumentError> {
        if self.page_count <= 1 {
            return Err(DocumentError("cannot delete the last page".into()));
        }
        let pdf_page = self.doc.pages().get(page as i32)?;
        pdf_page.delete()?;
        self.annotations.remove(page);
        self.refresh_pages()?;
        Ok(())
    }

    /// Insert a blank page after `page` (use `page_count - 1` to append at end).
    pub fn insert_blank_page(&mut self, after: usize) -> Result<(), DocumentError> {
        let size = self
            .pages
            .get(after.min(self.page_count.saturating_sub(1)))
            .copied()
            .map(|p| {
                PdfPagePaperSize::new_custom(
                    PdfPoints::new(p.width_pts),
                    PdfPoints::new(p.height_pts),
                )
            })
            .unwrap_or_else(PdfPagePaperSize::a4);
        let index = (after + 1).min(self.page_count);
        self.doc
            .pages_mut()
            .create_page_at_index(size, index as i32)?;
        self.annotations.insert(index, Vec::new());
        self.refresh_pages()?;
        Ok(())
    }

    /// Rotate a page 90° clockwise.
    pub fn rotate_page(&mut self, page: usize) -> Result<(), DocumentError> {
        let mut pdf_page = self.doc.pages_mut().get(page as i32)?;
        let next = match pdf_page.rotation()? {
            PdfPageRenderRotation::None => PdfPageRenderRotation::Degrees90,
            PdfPageRenderRotation::Degrees90 => PdfPageRenderRotation::Degrees180,
            PdfPageRenderRotation::Degrees180 => PdfPageRenderRotation::Degrees270,
            PdfPageRenderRotation::Degrees270 => PdfPageRenderRotation::None,
        };
        pdf_page.set_rotation(next);
        self.refresh_pages()?;
        Ok(())
    }

    /// Flush pending page-content edits so the written file reflects them.
    fn flush_pending_content(&mut self) -> Result<(), DocumentError> {
        for i in 0..self.doc.pages().len() {
            let mut page = self.doc.pages_mut().get(i)?;
            page.regenerate_content()?;
        }
        Ok(())
    }

    /// Serialize and write the document to `path` atomically: the bytes are
    /// written to a temporary file in the same directory, then renamed over
    /// the target. A failed or interrupted save can therefore never truncate
    /// or corrupt an existing file.
    fn save_atomic(&self, path: &Path) -> Result<(), DocumentError> {
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let file_name = path
            .file_name()
            .ok_or_else(|| DocumentError("save path has no file name".into()))?;

        let mut tmp_name = std::ffi::OsString::from(".");
        tmp_name.push(file_name);
        tmp_name.push(format!(".tmp-{}", std::process::id()));
        let tmp_path = dir.join(tmp_name);

        // Serialize first: any failure here leaves the target file untouched.
        let bytes = self.doc.save_to_bytes()?;
        if let Err(e) = std::fs::write(&tmp_path, &bytes) {
            return Err(DocumentError(format!(
                "could not write {}: {e}",
                tmp_path.display()
            )));
        }
        if let Err(e) = std::fs::rename(&tmp_path, path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(DocumentError(format!(
                "could not replace {}: {e}",
                path.display()
            )));
        }
        Ok(())
    }

    /// Save the document to disk (a full rewrite, written atomically).
    pub fn save(&mut self) -> Result<(), DocumentError> {
        self.flush_pending_content()?;
        self.save_atomic(self.path.as_path())
    }

    /// Save to a new path (Save As), written atomically.
    pub fn save_as(&mut self, path: &Path) -> Result<(), DocumentError> {
        self.flush_pending_content()?;
        self.save_atomic(path)
    }
}

/// Map a PDFium annotation to the editor's [`AnnotationKind`], if supported.
fn kind_of(annotation: &PdfPageAnnotation<'_>) -> Option<AnnotationKind> {
    match annotation.annotation_type() {
        PdfPageAnnotationType::Highlight => Some(AnnotationKind::Highlight),
        PdfPageAnnotationType::Ink => Some(AnnotationKind::Ink),
        PdfPageAnnotationType::Square => Some(AnnotationKind::Rectangle),
        PdfPageAnnotationType::Circle => Some(AnnotationKind::Ellipse),
        PdfPageAnnotationType::FreeText => Some(AnnotationKind::FreeText),
        PdfPageAnnotationType::Stamp
            if annotation.contents().as_deref() == Some(ELLIPSE_CONTENTS_MARKER) =>
        {
            Some(AnnotationKind::Ellipse)
        }
        PdfPageAnnotationType::Stamp => Some(AnnotationKind::Stamp),
        _ => None,
    }
}

fn meta(doc: &PdfDocument<'_>, tag: PdfDocumentMetadataTagType) -> String {
    doc.metadata()
        .get(tag)
        .map(|t| t.value().to_owned())
        .unwrap_or_default()
}

/// Convert a PDFium annotation into the editor's plain-data model.
fn annotation_to_model(annotation: &PdfPageAnnotation<'_>, id: u64) -> Option<Annotation> {
    let contents = annotation.contents().unwrap_or_default();
    let kind = match annotation.annotation_type() {
        PdfPageAnnotationType::Highlight => AnnotationKind::Highlight,
        PdfPageAnnotationType::Ink => AnnotationKind::Ink,
        PdfPageAnnotationType::Square => AnnotationKind::Rectangle,
        PdfPageAnnotationType::Circle => AnnotationKind::Ellipse,
        PdfPageAnnotationType::FreeText => AnnotationKind::FreeText,
        PdfPageAnnotationType::Stamp if contents == ELLIPSE_CONTENTS_MARKER => {
            AnnotationKind::Ellipse
        }
        PdfPageAnnotationType::Stamp => AnnotationKind::Stamp,
        _ => return None,
    };

    let bounds = annotation.bounds().ok()?;
    let (l, b, r, t) = (
        bounds.left().value,
        bounds.bottom().value,
        bounds.right().value,
        bounds.top().value,
    );

    let mut model = Annotation::new(id, kind);
    model.rect = PdfRect::new_from_values(b, l, t, r);
    if kind != AnnotationKind::Ellipse {
        model.text = contents;
    }

    let style = StrokeStyle {
        fill: annotation
            .fill_color()
            .ok()
            .map(crate::annotations::pdf_color_to_rgba),
        stroke: annotation
            .stroke_color()
            .ok()
            .map(crate::annotations::pdf_color_to_rgba),
        width: 2.0,
    };
    model.style = style;

    if kind == AnnotationKind::Highlight {
        model.style = StrokeStyle::highlight();
    }

    // Recover ink points from the annotation's path objects.
    if kind == AnnotationKind::Ink {
        let mut points = Vec::new();
        for obj in annotation.objects().iter() {
            if let Some(path) = obj.as_path_object() {
                for s in path.segments().iter() {
                    match s.segment_type() {
                        PdfPathSegmentType::MoveTo | PdfPathSegmentType::LineTo => {
                            points.push((s.x().value, s.y().value));
                        }
                        _ => {}
                    }
                }
            }
        }
        model.points = points;
    }

    Some(model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_pdf_path(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "pdf-viewer-{label}-{}-{nonce}.pdf",
            std::process::id()
        ))
    }

    #[test]
    fn annotations_round_trip_and_delete() {
        let pdfium: &'static Pdfium =
            Box::leak(Box::new(Pdfium::new(crate::load_pdfium_bindings())));
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("test.pdf");
        let first_path = unique_pdf_path("round-trip");
        let second_path = unique_pdf_path("deleted");
        let image_path = unique_pdf_path("stamp").with_extension("png");

        let mut document = OpenDocument::load(pdfium, &source).unwrap();
        let original_count = document.annotations[0].len();
        let original_image_count = document
            .doc
            .pages()
            .get(0)
            .unwrap()
            .objects()
            .iter()
            .filter(|object| object.as_image_object().is_some())
            .count();

        let mut highlight = Annotation::new(0, AnnotationKind::Highlight);
        highlight.rect = PdfRect::new_from_values(40.0, 40.0, 220.0, 90.0);
        highlight.style = StrokeStyle::highlight();
        document.add_annotation(0, highlight).unwrap();

        let mut ellipse = Annotation::new(0, AnnotationKind::Ellipse);
        ellipse.rect = PdfRect::new_from_values(80.0, 120.0, 260.0, 210.0);
        ellipse.style = StrokeStyle {
            fill: Some([255, 220, 0, 80]),
            stroke: Some([220, 40, 40, 255]),
            width: 2.0,
        };
        document.add_annotation(0, ellipse).unwrap();

        image::RgbaImage::from_pixel(2, 2, image::Rgba([220, 30, 30, 255]))
            .save(&image_path)
            .unwrap();
        let mut stamp = Annotation::new(0, AnnotationKind::Stamp);
        stamp.rect = PdfRect::new_from_values(300.0, 300.0, 360.0, 360.0);
        stamp.image_path = Some(image_path.to_string_lossy().into_owned());
        stamp.image_size = Some((60.0, 60.0));
        document.add_annotation(0, stamp).unwrap();
        document.save_as(&first_path).unwrap();

        let mut reloaded = OpenDocument::load(pdfium, &first_path).unwrap();
        assert_eq!(reloaded.annotations[0].len(), original_count + 3);
        assert!(
            reloaded.annotations[0]
                .iter()
                .any(|annotation| annotation.kind == AnnotationKind::Highlight)
        );
        assert!(
            reloaded.annotations[0]
                .iter()
                .any(|annotation| annotation.kind == AnnotationKind::Ellipse)
        );
        assert!(
            reloaded.annotations[0]
                .iter()
                .any(|annotation| annotation.kind == AnnotationKind::Stamp)
        );
        // The stamp's image must be recoverable so the editor can render and
        // edit it again after a save/reload cycle.
        let reloaded_stamp = reloaded.annotations[0]
            .iter()
            .find(|annotation| annotation.kind == AnnotationKind::Stamp)
            .unwrap();
        assert!(reloaded_stamp.image_path.is_some());
        assert!(reloaded_stamp.image_size.is_some());
        assert_eq!(
            reloaded
                .doc
                .pages()
                .get(0)
                .unwrap()
                .objects()
                .iter()
                .filter(|object| object.as_image_object().is_some())
                .count(),
            original_image_count + 1
        );

        let mut moved_highlight = reloaded.annotations[0]
            .iter()
            .find(|annotation| annotation.kind == AnnotationKind::Highlight)
            .cloned()
            .unwrap();
        let highlight_id = moved_highlight.id;
        moved_highlight.rect = PdfRect::new_from_values(55.0, 50.0, 235.0, 100.0);
        reloaded.update_annotation(0, moved_highlight).unwrap();
        assert_eq!(
            reloaded.annotations[0]
                .iter()
                .filter(|annotation| annotation.id == highlight_id)
                .count(),
            1
        );

        let ids: Vec<u64> = reloaded.annotations[0]
            .iter()
            .filter(|annotation| {
                matches!(
                    annotation.kind,
                    AnnotationKind::Highlight | AnnotationKind::Ellipse | AnnotationKind::Stamp
                )
            })
            .map(|annotation| annotation.id)
            .collect();
        for id in ids {
            reloaded.delete_annotation(0, id).unwrap();
        }
        reloaded.save_as(&second_path).unwrap();

        let final_document = OpenDocument::load(pdfium, &second_path).unwrap();
        assert_eq!(final_document.annotations[0].len(), original_count);
        assert_eq!(
            final_document
                .doc
                .pages()
                .get(0)
                .unwrap()
                .objects()
                .iter()
                .filter(|object| object.as_image_object().is_some())
                .count(),
            original_image_count
        );

        let _ = std::fs::remove_file(first_path);
        let _ = std::fs::remove_file(second_path);
        let _ = std::fs::remove_file(image_path);
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temp_file() {
        let pdfium: &'static Pdfium =
            Box::leak(Box::new(Pdfium::new(crate::load_pdfium_bindings())));
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("test.pdf");
        let out = unique_pdf_path("atomic");
        let dir = out.parent().unwrap().to_path_buf();

        let mut document = OpenDocument::load(pdfium, &source).unwrap();
        document.path = out.clone();

        let mut highlight = Annotation::new(0, AnnotationKind::Highlight);
        highlight.rect = PdfRect::new_from_values(40.0, 40.0, 220.0, 90.0);
        highlight.style = StrokeStyle::highlight();
        document.add_annotation(0, highlight).unwrap();

        document.save().unwrap();

        assert!(out.exists(), "saved file should exist");
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".pdf-viewer-atomic-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "atomic save should not leave temp files behind: {leftovers:?}"
        );

        let reloaded = OpenDocument::load(pdfium, &out).unwrap();
        assert!(
            reloaded.annotations[0]
                .iter()
                .any(|a| a.kind == AnnotationKind::Highlight),
            "annotation should survive an atomic save/reload"
        );

        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn text_move_translates_and_restores() {
        let pdfium: &'static Pdfium =
            Box::leak(Box::new(Pdfium::new(crate::load_pdfium_bindings())));
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-search.pdf");

        let mut document = OpenDocument::load(pdfium, &source).unwrap();

        let (page, run) = (0..document.page_count)
            .find_map(|p| {
                document
                    .text_runs(p)
                    .ok()
                    .and_then(|runs| runs.into_iter().next().map(|r| (p, r)))
            })
            .expect("test-search.pdf should contain text");

        let object_index = document.text_object_index_for_run(page, &run).unwrap();
        let before = document.text_object_matrix(page, object_index).unwrap();

        document
            .translate_text_object(page, object_index, 50.0, 40.0)
            .unwrap();
        let after = document.text_object_matrix(page, object_index).unwrap();
        assert_ne!(before, after, "translate should change the matrix");

        document
            .set_text_object_matrix(page, object_index, before)
            .unwrap();
        let restored = document.text_object_matrix(page, object_index).unwrap();
        assert_eq!(before, restored, "restore should return the original matrix");
    }

    #[test]
    fn word_edit_replaces_a_word() {
        let pdfium: &'static Pdfium =
            Box::leak(Box::new(Pdfium::new(crate::load_pdfium_bindings())));
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-search.pdf");

        let mut document = OpenDocument::load(pdfium, &source).unwrap();

        let (page, run) = (0..document.page_count)
            .find_map(|p| {
                document.text_runs(p).ok().and_then(|runs| {
                    runs.into_iter()
                        .find(|r| !r.words.is_empty())
                        .map(|r| (p, r))
                })
            })
            .expect("test-search.pdf should contain text with words");

        let word = run.words.first().cloned().unwrap();
        let original_word = word.text.clone();

        let applied = document.apply_word_edit(page, &word, "CHANGED").unwrap();
        assert_ne!(applied.object_original, applied.object_new);
        assert!(applied.object_new.contains("CHANGED"));
        assert!(
            !applied.object_new.contains(&original_word),
            "the original word should be gone: {original_word:?} -> {:?}",
            applied.object_new
        );
    }

    #[test]
    fn text_render_mode_toggles_bold() {
        let pdfium: &'static Pdfium =
            Box::leak(Box::new(Pdfium::new(crate::load_pdfium_bindings())));
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-search.pdf");

        let mut document = OpenDocument::load(pdfium, &source).unwrap();

        let (page, run) = (0..document.page_count)
            .find_map(|p| {
                document
                    .text_runs(p)
                    .ok()
                    .and_then(|runs| runs.into_iter().next().map(|r| (p, r)))
            })
            .expect("test-search.pdf should contain text");

        let object_index = document.text_object_index_for_run(page, &run).unwrap();
        let before = document.text_object_render_mode(page, object_index).unwrap();

        document
            .set_text_object_render_mode(page, object_index, PdfPageTextRenderMode::FilledThenStroked)
            .unwrap();
        assert_eq!(
            document.text_object_render_mode(page, object_index).unwrap(),
            PdfPageTextRenderMode::FilledThenStroked
        );

        document
            .set_text_object_render_mode(page, object_index, before)
            .unwrap();
        assert_eq!(
            document.text_object_render_mode(page, object_index).unwrap(),
            before
        );
    }

    #[test]
    fn word_delete_removes_a_word() {
        let pdfium: &'static Pdfium =
            Box::leak(Box::new(Pdfium::new(crate::load_pdfium_bindings())));
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-search.pdf");

        let mut document = OpenDocument::load(pdfium, &source).unwrap();

        let (page, run) = (0..document.page_count)
            .find_map(|p| {
                document.text_runs(p).ok().and_then(|runs| {
                    runs.into_iter()
                        .find(|r| !r.words.is_empty())
                        .map(|r| (p, r))
                })
            })
            .expect("test-search.pdf should contain text with words");

        let word = run.words.first().cloned().unwrap();
        let original_word = word.text.clone();

        let applied = document.apply_word_edit(page, &word, "").unwrap();
        assert_ne!(applied.object_original, applied.object_new);
        assert!(
            !applied.object_new.contains(&original_word),
            "the deleted word should be gone: {original_word:?} -> {:?}",
            applied.object_new
        );
    }

    #[test]
    fn text_font_size_changes() {
        let pdfium: &'static Pdfium =
            Box::leak(Box::new(Pdfium::new(crate::load_pdfium_bindings())));
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-search.pdf");

        let mut document = OpenDocument::load(pdfium, &source).unwrap();

        let (page, run) = (0..document.page_count)
            .find_map(|p| {
                document
                    .text_runs(p)
                    .ok()
                    .and_then(|runs| runs.into_iter().next().map(|r| (p, r)))
            })
            .expect("test-search.pdf should contain text");

        let object_index = document.text_object_index_for_run(page, &run).unwrap();
        let before = document.text_object_font_size(page, object_index).unwrap();

        document
            .set_text_object_font_size(page, object_index, before + 10.0)
            .unwrap();
        let after = document.text_object_font_size(page, object_index).unwrap();
        assert!(
            (after - (before + 10.0)).abs() < 0.5,
            "font size should change by +10: {before} -> {after}"
        );

        document
            .set_text_object_font_size(page, object_index, before)
            .unwrap();
        let restored = document.text_object_font_size(page, object_index).unwrap();
        assert!(
            (restored - before).abs() < 0.5,
            "font size should restore: {before} -> {restored}"
        );
    }

    #[test]
    fn word_edit_round_trips_through_save() {
        let pdfium: &'static Pdfium =
            Box::leak(Box::new(Pdfium::new(crate::load_pdfium_bindings())));
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-search.pdf");
        let out = unique_pdf_path("word-roundtrip");

        let mut document = OpenDocument::load(pdfium, &source).unwrap();

        let (page, run) = (0..document.page_count)
            .find_map(|p| {
                document.text_runs(p).ok().and_then(|runs| {
                    runs.into_iter()
                        .find(|r| !r.words.is_empty())
                        .map(|r| (p, r))
                })
            })
            .expect("test-search.pdf should contain text with words");

        let word = run.words.first().cloned().unwrap();
        let original_word = word.text.clone();

        document.apply_word_edit(page, &word, "CHANGED").unwrap();
        document.save_as(&out).unwrap();

        // Reload from the saved bytes and re-extract text: the edit must be a
        // real, persisted change.
        let reloaded = OpenDocument::load(pdfium, &out).unwrap();
        let all_text = (0..reloaded.page_count)
            .filter_map(|p| reloaded.text_runs(p).ok())
            .flatten()
            .map(|r| r.text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            all_text.contains("CHANGED"),
            "edited word should persist: {all_text:?}"
        );
        assert!(
            !all_text.contains(&original_word),
            "original word should be gone ({original_word:?}): {all_text:?}"
        );

        let _ = std::fs::remove_file(&out);
    }

    #[cfg(feature = "ocr")]
    #[test]
    fn invisible_text_layer_produces_word_spacing() {
        let pdfium: &'static Pdfium =
            Box::leak(Box::new(Pdfium::new(crate::load_pdfium_bindings())));
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("test.pdf");
        let out = unique_pdf_path("ocr-spacing");

        let mut document = OpenDocument::load(pdfium, &source).unwrap();

        let words = vec![
            crate::ocr::OcrWord {
                text: "Hello".into(),
                left: 40.0,
                bottom: 400.0,
                right: 90.0,
                top: 420.0,
            },
            crate::ocr::OcrWord {
                text: "World".into(),
                left: 100.0,
                bottom: 400.0,
                right: 150.0,
                top: 420.0,
            },
        ];
        document.add_invisible_text_layer(0, &words).unwrap();
        document.save_as(&out).unwrap();

        let reloaded = OpenDocument::load(pdfium, &out).unwrap();
        let text = reloaded
            .text_runs(0)
            .unwrap()
            .iter()
            .map(|r| r.text.trim().to_string())
            .collect::<Vec<_>>()
            .join(" ");

        assert!(
            text.contains("Hello World"),
            "invisible text layer should copy with word spacing: {text:?}"
        );

        let _ = std::fs::remove_file(&out);
    }
}
