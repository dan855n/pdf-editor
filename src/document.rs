//! Open-document model plus the bridge to PDFium's annotation and page APIs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pdfium_render::prelude::*;

use crate::annotations::{Annotation, AnnotationKind, StrokeStyle};

const ELLIPSE_CONTENTS_MARKER: &str = "__pdf_editor_ellipse__";
const IMAGE_CONTENTS_MARKER: &str = "__pdf_editor_image__";
const REDACTION_CONTENTS_MARKER: &str = "__pdf_editor_redaction__";

/// Create a text-markup annotation (highlight/strikeout/underline/squiggly)
/// with per-line quads from `annotation.quads` (or a single rect quad when no
/// per-line quads are stored).
macro_rules! text_markup_annotation {
    ($page:expr, $annotation:expr, $creator:ident, $l:expr, $b:expr, $r:expr, $t:expr) => {{
        let mut a = $page.annotations_mut().$creator()?;
        a.set_bounds(PdfRect::new_from_values($b, $l, $t, $r))?;
        if let Some(fill) = $annotation.style.fill {
            a.set_fill_color(crate::annotations::rgba_to_pdf_color(fill))?;
        }
        if let Some(stroke) = $annotation.style.stroke {
            a.set_stroke_color(crate::annotations::rgba_to_pdf_color(stroke))?;
        }
        if !$annotation.text.is_empty() {
            a.set_contents(&$annotation.text)?;
        }
        add_text_markup_quads(a.attachment_points_mut(), $annotation, $l, $b, $r, $t)?;
    }};
}

/// Append the quad points for a text-markup annotation: one quad per selected
/// line, or a single quad covering the whole rect when no lines are stored.
fn add_text_markup_quads(
    attachment_points: &mut pdfium_render::prelude::PdfPageAnnotationAttachmentPoints<'_>,
    annotation: &Annotation,
    l: f32,
    b: f32,
    r: f32,
    t: f32,
) -> Result<(), DocumentError> {
    if annotation.quads.is_empty() {
        attachment_points.create_attachment_point_at_end(PdfQuadPoints::new_from_values(
            l, b, r, b, r, t, l, t,
        ))?;
    } else {
        for &(x1, y1, x2, y2, x3, y3, x4, y4) in &annotation.quads {
            attachment_points
                .create_attachment_point_at_end(PdfQuadPoints::new_from_values(
                    x1, y1, x2, y2, x3, y3, x4, y4,
                ))?;
        }
    }
    Ok(())
}

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

/// What [`OpenDocument::split_text_object_for_span`] did to a text object.
///
/// Splitting is visually lossless: the prefix and suffix stay exactly where
/// they were, the span keeps the original font, size, colour and render mode,
/// and only afterwards does translating `span_index` move anything.
#[derive(Debug, Clone, PartialEq)]
pub struct TextObjectSplit {
    /// The object that now holds only the span (the original object, same index).
    pub span_index: usize,
    /// The span's text.
    pub span_text: String,
    /// New object created for the text before the span, if there was any.
    pub prefix_index: Option<usize>,
    /// New object created for the text after the span, if there was any.
    pub suffix_index: Option<usize>,
}

/// The style and pieces needed to split one text object.
type ObjectSplitPlan = (TextStyleSnapshot, Vec<PlannedPiece>);

/// A text object's characters: their page indices, their origins in page space,
/// and their Unicode values — all in object order.
type ObjectCharsLayout = (Vec<usize>, Vec<(f32, f32)>, Vec<char>);

/// One piece of a text object being split.
#[derive(Debug, Clone)]
struct PlannedPiece {
    /// Text for the piece; `None` keeps the original object's text untouched.
    text: Option<String>,
    /// Page-space offset from the object's first character origin.
    offset: (f32, f32),
    /// Whether this piece is part of the selection that will be moved.
    selected: bool,
}

/// A line that had to be re-authored because PDFium mangled it on a move.
///
/// Re-serialising a *justified* line after a move re-emits its justification as
/// extreme inter-word adjustments, so the line renders spread across the page. Such
/// a line is replaced by a copy we author ourselves (stretched back to its exact
/// width) and the mangled original is blanked. Both objects are kept, and every
/// index stays put, so the change is exactly reversible.
#[derive(Debug, Clone, PartialEq)]
pub struct RedrawnLine {
    pub page: usize,
    /// The object that was mangled; blanked once the copy takes over.
    pub original_index: usize,
    /// The freshly authored copy.
    pub copy_index: usize,
    /// The original object's text, for an exact restore.
    pub original_text: String,
    pub original_matrix: PdfMatrix,
    pub copy_matrix: PdfMatrix,
}

/// The look of a text object, copied onto the pieces when one is split.
#[derive(Debug, Copy, Clone)]
struct TextStyleSnapshot {
    font_token: PdfFontToken,
    font_size: PdfPoints,
    matrix: PdfMatrix,
    render_mode: PdfPageTextRenderMode,
    fill: Option<PdfColor>,
    stroke: Option<PdfColor>,
    stroke_width: Option<PdfPoints>,
}

/// Copy a split parent's look onto a newly created text object, so the pieces
/// render exactly like the text they replace.
fn apply_text_object_style<'a>(
    object: &mut PdfPageObject<'a>,
    style: &TextStyleSnapshot,
) -> Result<(), PdfiumError> {
    let Some(text_object) = object.as_text_object_mut() else {
        return Ok(());
    };
    text_object.reset_matrix(style.matrix)?;
    text_object.set_unscaled_font_size(style.font_size)?;
    let _ = text_object.set_render_mode(style.render_mode);
    if let Some(fill) = style.fill {
        let _ = text_object.set_fill_color(fill);
    }
    if let Some(stroke) = style.stroke {
        let _ = text_object.set_stroke_color(stroke);
    }
    if let Some(width) = style.stroke_width {
        let _ = text_object.set_stroke_width(width);
    }
    Ok(())
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
    pub subject: String,
    pub keywords: String,
    pub producer: String,
    pub metadata_dirty: bool,
    pub is_encrypted: bool,
    pub pending_encryption: Option<crate::security::EncryptionOptions>,
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
        Self::load_with_password(pdfium, path, None)
    }

    /// Load a document from disk with an optional password.
    pub fn load_with_password(
        pdfium: &'static Pdfium,
        path: &Path,
        password: Option<&str>,
    ) -> Result<Self, DocumentError> {
        let doc = pdfium.load_pdf_from_file(path, password)?;
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
            if let Ok(d) = pdfium.load_pdf_from_file(path, password) {
                // SAFETY: same reasoning as the main handle above.
                let d: PdfDocument<'static> = unsafe { std::mem::transmute(d) };
                render_docs.push(Arc::new(d));
            }
        }

        let pending_encryption = password.filter(|pw| !pw.is_empty()).map(|pw| {
            crate::security::EncryptionOptions {
                user_password: pw.to_string(),
                ..Default::default()
            }
        });
        let is_encrypted = pending_encryption.is_some();

        let mut open = Self {
            path: path.to_path_buf(),
            page_count,
            doc,
            render_docs,
            pages,
            title: String::new(),
            author: String::new(),
            subject: String::new(),
            keywords: String::new(),
            producer: String::new(),
            metadata_dirty: false,
            is_encrypted,
            pending_encryption,
            file_size,
            annotations: vec![Vec::new(); page_count],
            next_id: 1,
            temp_stamp_files: Vec::new(),
        };

        // Metadata reads need a fully-built struct, so fill it in now.
        open.title = meta(&open.doc, PdfDocumentMetadataTagType::Title);
        open.author = meta(&open.doc, PdfDocumentMetadataTagType::Author);
        open.subject = meta(&open.doc, PdfDocumentMetadataTagType::Subject);
        open.keywords = meta(&open.doc, PdfDocumentMetadataTagType::Keywords);
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
        let runs = crate::text_model::group_chars_into_runs(page, &text_page.chars())?;

        // Merge runs drawn by the same text object: a line can be split into blocks
        // by PDFium's line detection (a centred title and its right-aligned code, for
        // instance), and separate blocks travel separately unless they are rejoined.
        let mut owners: Vec<Option<usize>> = Vec::with_capacity(runs.len());
        for run in &runs {
            let owner = self
                .text_object_index_for_char(page, run.first_char_index)
                .ok();
            owners.push(owner);
        }

        let mut merged: Vec<crate::text_model::TextRun> = Vec::with_capacity(runs.len());
        for (index, run) in runs.iter().enumerate() {
            let same_line = merged.last().is_some_and(|candidate| {
                let previous = runs
                    .iter()
                    .position(|other| other.first_char_index == candidate.first_char_index);
                match (previous.and_then(|p| owners[p]), owners[index]) {
                    (Some(a), Some(b)) if a == b => {
                        (candidate.baseline_y - run.baseline_y).abs()
                            <= 0.5 * candidate.font_size.max(run.font_size)
                    }
                    _ => false,
                }
            });
            if same_line {
                let last = merged.pop().expect("checked above");
                merged.push(crate::text_model::merge_runs(&last, run));
            } else {
                merged.push(run.clone());
            }
        }

        Ok(merged)
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

            let object_index = self.text_object_index_for_char(page, run.first_char_index)?;

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
            let object_index = self.text_object_index_for_char(page, word.first_char_index)?;
            (object_original, object_index)
        };

        // Replace the word at its own character offset. Searching for its text
        // instead would hit the wrong "the" on a line that repeats a word.
        let word_pos = self.object_text_byte_offset(page, object_index, word.first_char_index)?;
        let word_len = word.text.chars().count();
        if object_original[word_pos..].chars().take(word_len).collect::<String>() != word.text {
            return Err(DocumentError(
                "could not locate the word at its character offset in the text object".into(),
            ));
        }
        let word_end = object_original[word_pos..]
            .char_indices()
            .nth(word_len)
            .map(|(byte, _)| word_pos + byte)
            .unwrap_or(object_original.len());
        let object_new = format!(
            "{}{}{}",
            &object_original[..word_pos],
            new_text,
            &object_original[word_end..]
        );

        self.set_text_at_object_index(page, object_index, &object_new)?;

        Ok(TextEditApplication {
            object_original,
            object_new,
            object_index,
        })
    }

    /// Return the index of the PDF text object that owns the character at
    /// `char_index`.
    ///
    /// The mapping comes from PDFium's own character-to-object table (the same
    /// one behind `PdfPageText::chars_for_object`), never from comparing object
    /// text. That matters on pages that repeat a string — `Name:`, `--`, page
    /// numbers, form labels — where matching on text alone picks the first
    /// lookalike and edits the wrong line.
    pub fn text_object_index_for_char(
        &self,
        page: usize,
        char_index: usize,
    ) -> Result<usize, DocumentError> {
        let pdf_page = self.doc.pages().get(page as i32)?;
        let text_page = pdf_page.text()?;

        let owning_text = text_page
            .chars()
            .iter()
            .find(|c| c.index() == char_index)
            .ok_or_else(|| DocumentError("could not find the character on the page".into()))?
            .text_object()?
            .text();

        // Only objects whose text matches can own the character, so this narrows
        // the candidates to a handful; the character test then picks the right one.
        let mut fallback = None;
        for (index, object) in pdf_page.objects().iter().enumerate() {
            let Some(text_object) = object.as_text_object() else {
                continue;
            };
            if text_object.text() != owning_text {
                continue;
            }
            fallback.get_or_insert(index);
            if text_object
                .chars(&text_page)?
                .iter()
                .any(|c| c.index() == char_index)
            {
                return Ok(index);
            }
        }

        fallback.ok_or_else(|| DocumentError("could not locate the text object on the page".into()))
    }

    /// Return the index of the PDF text object backing `run`.
    pub fn text_object_index_for_run(
        &self,
        page: usize,
        run: &crate::text_model::TextRun,
    ) -> Result<usize, DocumentError> {
        self.text_object_index_for_char(page, run.first_char_index)
    }

    /// Return every text object that owns at least one of the given characters.
    ///
    /// One visual line is not guaranteed to be one text object (producers split
    /// lines at ligatures, kerning tweaks, superscripts and colour changes), and a
    /// line's characters are not guaranteed to be a contiguous run of page indices
    /// (a two-column layout interleaves them), so callers pass the character set
    /// itself rather than a range.
    pub fn text_object_indices_for_chars(
        &self,
        page: usize,
        chars: &[usize],
    ) -> Result<Vec<usize>, DocumentError> {
        let wanted: HashSet<usize> = chars.iter().copied().collect();
        if wanted.is_empty() {
            return Ok(Vec::new());
        }

        let pdf_page = self.doc.pages().get(page as i32)?;
        let text_page = pdf_page.text()?;

        let mut indices = Vec::new();
        for (index, object) in pdf_page.objects().iter().enumerate() {
            let Some(text_object) = object.as_text_object() else {
                continue;
            };
            let owns = text_object
                .chars(&text_page)?
                .iter()
                .any(|c| wanted.contains(&c.index()));
            if owns {
                indices.push(index);
            }
        }
        Ok(indices)
    }

    /// Return the page character indices a text object owns (in object order),
    /// each of their origins in page space, and each of their Unicode characters.
    ///
    /// This is the only trustworthy view of an object's characters: PDFium's
    /// `text()` can *add* inferred whitespace (a trailing space appears after
    /// `set_text()`, for instance), so text that came from `text()` cannot be
    /// sliced by character position. Characters without a Unicode value cannot be
    /// reconstructed at all, and are reported rather than silently mangled.
    fn object_chars_layout(
        &self,
        page: usize,
        object_index: usize,
    ) -> Result<ObjectCharsLayout, DocumentError> {
        let pdf_page = self.doc.pages().get(page as i32)?;
        let text_page = pdf_page.text()?;
        let object = pdf_page.objects().get(object_index)?;
        let Some(text_object) = object.as_text_object() else {
            return Err(DocumentError("object is not a text object".into()));
        };

        let object_text = text_object.text();
        let object_chars: Vec<usize> = text_object
            .chars(&text_page)?
            .iter()
            .map(|c| c.index())
            .collect();

        let mut origins = Vec::with_capacity(object_chars.len());
        let mut chars = Vec::with_capacity(object_chars.len());
        for ch in text_page.chars().iter() {
            if !object_chars.contains(&ch.index()) {
                continue;
            }
            let (x, y) = ch.origin()?;
            origins.push((x.value, y.value));
            let Some(unicode) = ch.unicode_char() else {
                return Err(DocumentError(
                    "this text object has characters with no Unicode value, so it cannot be edited in place"
                        .into(),
                ));
            };
            chars.push(unicode);
        }
        if chars.len() != object_chars.len() {
            return Err(DocumentError(
                "this text object's character origins are incomplete, so it cannot be edited in place"
                    .into(),
            ));
        }

        // Whitespace is allowed to differ (PDFium infers it), nothing else is.
        let rebuilt: String = chars.iter().collect();
        if rebuilt.trim() != object_text.trim() {
            return Err(DocumentError(
                "this text object's characters cannot be mapped one-to-one, so it cannot be edited in place"
                    .into(),
            ));
        }

        Ok((object_chars, origins, chars))
    }

    /// Translate the text object at `object_index` by the given PDF-point deltas.
    pub fn translate_text_object(
        &mut self,
        page: usize,
        object_index: usize,
        dx_pt: f32,
        dy_pt: f32,
    ) -> Result<(), DocumentError> {
        self.translate_text_objects(page, &[object_index], dx_pt, dy_pt)
    }

    /// Translate several text objects by the same page-space delta, regenerating
    /// the content stream once at the end.
    ///
    /// Rewriting a text object's content makes PDFium re-serialise it, and a
    /// *justified* line comes back with its word spacing collapsed — it is a few
    /// percent narrower and ragged against the lines around it. A line that is only
    /// moving must not change width, so each one is stretched back about its left
    /// edge afterwards.
    pub fn translate_text_objects(
        &mut self,
        page: usize,
        object_indices: &[usize],
        dx_pt: f32,
        dy_pt: f32,
    ) -> Result<(), DocumentError> {
        let mut pdf_page = self.doc.pages_mut().get(page as i32)?;

        {
            let objects = pdf_page.objects_mut();
            for &object_index in object_indices {
                let mut object = objects.get(object_index)?;
                if !matches!(object, PdfPageObject::Text(_)) {
                    continue;
                }
                object.translate(PdfPoints::new(dx_pt), PdfPoints::new(dy_pt))?;
            }
        }
        pdf_page.regenerate_content()?;
        Ok(())
    }

    /// Byte offset, inside the object's text, of the character at `char_index`.
    pub fn object_text_byte_offset(
        &self,
        page: usize,
        object_index: usize,
        char_index: usize,
    ) -> Result<usize, DocumentError> {
        let (chars, _, _) = self.object_chars_layout(page, object_index)?;
        let position = chars.iter().position(|&index| index == char_index).ok_or_else(|| {
            DocumentError("the character is not inside this text object".into())
        })?;

        let pdf_page = self.doc.pages().get(page as i32)?;
        let object = pdf_page.objects().get(object_index)?;
        let PdfPageObject::Text(text_object) = object else {
            return Err(DocumentError("object is not a text object".into()));
        };
        let object_text = text_object.text();
        Ok(object_text
            .char_indices()
            .nth(position)
            .map(|(byte, _)| byte)
            .unwrap_or(object_text.len()))
    }

    /// Everything needed to split one object, captured before anything mutates.
    ///
    /// Splitting rewrites object text, and `set_text()` drops the whitespace PDFium
    /// only *infers* (a trailing space, for instance). That removes characters from
    /// the page's text stream and renumbers every later character, so character
    /// indices cannot be used after the first split. Plans are therefore all taken
    /// from the untouched page, then applied.
    fn plan_object_split(
        &self,
        page: usize,
        object_index: usize,
        selected: &HashSet<usize>,
    ) -> Result<Option<ObjectSplitPlan>, DocumentError> {
        let (object_chars, origins, chars_text) =
            self.object_chars_layout(page, object_index)?;

        let positions: Vec<usize> = (0..object_chars.len())
            .filter(|position| selected.contains(&object_chars[*position]))
            .collect();
        let (Some(&first_position), Some(&last_position)) = (positions.first(), positions.last())
        else {
            return Ok(None);
        };

        // Grow the span to whole tokens inside this object. A selection can begin or
        // end mid-word — PDFium invents spaces at object boundaries, so a word like
        // "Anita" can be modelled as "Ani" + "ta" — and splitting there would leave
        // half a word behind ("ta Thompson").
        let mut start = first_position;
        while start > 0 && !chars_text[start - 1].is_whitespace() {
            start -= 1;
        }
        let mut end = last_position + 1;
        while end < chars_text.len() && !chars_text[end].is_whitespace() {
            end += 1;
        }
        if start == 0 && end == object_chars.len() {
            // The selection is the whole object: leave its text untouched, so no
            // characters appear or disappear.
            return Ok(Some((
                self.object_style(page, object_index)?,
                vec![PlannedPiece {
                    text: None,
                    offset: (0.0, 0.0),
                    selected: true,
                }],
            )));
        }

        let base = origins[0];
        let offset_of = |position: usize| {
            (
                origins[position].0 - base.0,
                origins[position].1 - base.1,
            )
        };

        let mut pieces = Vec::new();
        if start > 0 {
            pieces.push(PlannedPiece {
                text: Some(chars_text[..start].iter().collect()),
                offset: (0.0, 0.0),
                selected: false,
            });
        }
        pieces.push(PlannedPiece {
            text: Some(chars_text[start..end].iter().collect()),
            offset: offset_of(start),
            selected: true,
        });
        if end < object_chars.len() {
            pieces.push(PlannedPiece {
                text: Some(chars_text[end..].iter().collect()),
                offset: offset_of(end),
                selected: false,
            });
        }

        Ok(Some((self.object_style(page, object_index)?, pieces)))
    }

    /// The look of a text object.
    fn object_style(
        &self,
        page: usize,
        object_index: usize,
    ) -> Result<TextStyleSnapshot, DocumentError> {
        let pdf_page = self.doc.pages().get(page as i32)?;
        let object = pdf_page.objects().get(object_index)?;
        let Some(text_object) = object.as_text_object() else {
            return Err(DocumentError("object is not a text object".into()));
        };
        Ok(TextStyleSnapshot {
            font_token: text_object.font().token(),
            font_size: text_object.unscaled_font_size(),
            matrix: text_object.matrix()?,
            render_mode: text_object.render_mode(),
            fill: text_object.fill_color().ok(),
            stroke: text_object.stroke_color().ok(),
            stroke_width: text_object.stroke_width().ok(),
        })
    }

    /// Apply a plan: the first piece stays in the original object, the rest become
    /// new objects. Returns `(object index, selected)` per piece.
    fn apply_object_split(
        &mut self,
        page: usize,
        object_index: usize,
        style: &TextStyleSnapshot,
        pieces: &[PlannedPiece],
    ) -> Result<Vec<(usize, bool)>, DocumentError> {
        let mut pdf_page = self.doc.pages_mut().get(page as i32)?;
        let base_index = pdf_page.objects().len();
        let mut placed = Vec::with_capacity(pieces.len());

        {
            let objects = pdf_page.objects_mut();

            let mut created = 0;
            for (position, piece) in pieces.iter().enumerate() {
                if position == 0 {
                    if let Some(text) = &piece.text {
                        let mut object = objects.get(object_index)?;
                        let Some(text_object) = object.as_text_object_mut() else {
                            return Err(DocumentError("object is not a text object".into()));
                        };
                        // The source is erased, not left behind: a piece that keeps
                        // only whitespace loses its glyphs entirely.
                        let text = if text.trim().is_empty() { "" } else { text.as_str() };
                        text_object.set_text(text)?;
                        text_object.translate(
                            PdfPoints::new(piece.offset.0),
                            PdfPoints::new(piece.offset.1),
                        )?;
                    }
                    placed.push((object_index, piece.selected));
                    continue;
                }

                let text = piece.text.clone().unwrap_or_default();
                if text.trim().is_empty() {
                    // Nothing to draw: an object holding only whitespace would be
                    // left in the content stream as debris.
                    continue;
                }
                let mut object = objects.create_text_object(
                    PdfPoints::ZERO,
                    PdfPoints::ZERO,
                    text.as_str(),
                    style.font_token,
                    style.font_size,
                )?;
                apply_text_object_style(&mut object, style)?;
                object.translate(
                    PdfPoints::new(piece.offset.0),
                    PdfPoints::new(piece.offset.1),
                )?;
                placed.push((base_index + created, piece.selected));
                created += 1;
            }
        }

        pdf_page.regenerate_content()?;
        Ok(placed)
    }

    /// Split the text object that owns `first_char_index..=last_char_index` so the
    /// span becomes its own object, leaving the prefix and suffix in place.
    ///
    /// This is what makes a word, or any run of characters, draggable on its own.
    /// PDF text has no sub-object model — a whole text object moves as one — so a
    /// span is only free to move once it *is* an object. The split copies the
    /// original font, size, matrix, colour and render mode, so nothing is
    /// re-encoded and nothing moves on screen; the offsets come from PDFium's
    /// character origins, so rotated and scaled text split correctly too.
    pub fn split_text_object_for_span(
        &mut self,
        page: usize,
        first_char_index: usize,
        last_char_index: usize,
    ) -> Result<TextObjectSplit, DocumentError> {
        let object_index = self.text_object_index_for_char(page, first_char_index)?;
        let selected: HashSet<usize> = (first_char_index..=last_char_index).collect();

        let Some((style, pieces)) = self.plan_object_split(page, object_index, &selected)? else {
            return Err(DocumentError(
                "the selection is not inside this text object".into(),
            ));
        };
        let span_text = self
            .object_chars_layout(page, object_index)?
            .2
            .iter()
            .collect::<String>();
        let span_text: String = span_text
            .chars()
            .skip_while(|_| false)
            .collect();
        let span_text = pieces
            .iter()
            .find(|piece| piece.selected && piece.text.is_some())
            .and_then(|piece| piece.text.clone())
            .unwrap_or(span_text);

        let placed = self.apply_object_split(page, object_index, &style, &pieces)?;

        let mut prefix_index = None;
        let mut suffix_index = None;
        let mut span_index = object_index;
        for (position, (index, selected)) in placed.iter().enumerate() {
            if *selected {
                span_index = *index;
            } else if position == 0 {
                prefix_index = Some(*index);
            } else {
                suffix_index = Some(*index);
            }
        }

        Ok(TextObjectSplit {
            span_index,
            span_text,
            prefix_index,
            suffix_index,
        })
    }

    /// Split a multi-word selection into movable spans.
    ///
    /// Each group is the set of characters of one selection (a word, a phrase, or
    /// the selected part of one line). A group may straddle text objects, so each
    /// object's share of it is split separately. Every plan is taken from the
    /// untouched page before any object is rewritten — see [`Self::plan_object_split`]
    /// for why re-reading characters between splits is unsafe.
    pub fn split_selection_for_move(
        &mut self,
        page: usize,
        groups: &[Vec<usize>],
    ) -> Result<Vec<TextObjectSplit>, DocumentError> {
        // Which object owns which selected characters, in the untouched state.
        let mut per_object: Vec<(usize, HashSet<usize>)> = Vec::new();
        for group in groups {
            if group.is_empty() {
                continue;
            }
            for object_index in self.text_object_indices_for_chars(page, group)? {
                match per_object.iter_mut().find(|(index, _)| *index == object_index) {
                    Some((_, selected)) => selected.extend(group.iter().copied()),
                    None => {
                        let selected: HashSet<usize> = group.iter().copied().collect();
                        per_object.push((object_index, selected));
                    }
                }
            }
        }

        // Plan everything first, then apply.
        let mut plans = Vec::with_capacity(per_object.len());
        for (object_index, selected) in &per_object {
            if let Some((style, pieces)) =
                self.plan_object_split(page, *object_index, selected)?
            {
                let span_text = pieces
                    .iter()
                    .filter(|piece| piece.selected)
                    .filter_map(|piece| piece.text.clone())
                    .collect::<Vec<_>>()
                    .join("");
                let span_text = if span_text.is_empty() {
                    // Whole-object selection: report the object's own text.
                    self.text_object_text(page, *object_index)?
                } else {
                    span_text
                };
                plans.push((*object_index, style, pieces, span_text));
            }
        }

        let mut splits = Vec::with_capacity(plans.len());
        for (object_index, style, pieces, span_text) in plans {
            let placed = self.apply_object_split(page, object_index, &style, &pieces)?;
            let span_index = placed
                .iter()
                .find(|(_, selected)| *selected)
                .map(|(index, _)| *index)
                .unwrap_or(object_index);
            splits.push(TextObjectSplit {
                span_index,
                span_text,
                prefix_index: None,
                suffix_index: None,
            });
        }

        Ok(splits)
    }

    /// Widths of the given objects, for [`Self::repair_moved_text`].
    pub fn object_widths(
        &self,
        page: usize,
        object_indices: &[usize],
    ) -> Vec<(usize, f32)> {
        object_indices
            .iter()
            .filter_map(|&index| {
                let (left, _, right, _) = self.object_bounds(page, index).ok()?;
                Some((index, right - left))
            })
            .collect()
    }

    /// Re-author moved lines so PDFium cannot mangle them.
    ///
    /// A moved object is re-serialised by PDFium, and for *justified* text that
    /// re-serialisation re-emits the justification as extreme inter-word
    /// adjustments: the line then renders spread across the page. It cannot be
    /// detected from the object's bounds (those already report the intended width),
    /// so every moved line is re-authored.
    ///
    /// The work is done in **batches**, not line by line: reading, creating,
    /// blanking and measuring interleaved per line made PDFium re-parse the page
    /// between steps and draw each line twice. So every line is measured first, then
    /// all copies are created and all originals blanked, one regeneration commits
    /// that, and only then are the copies stretched to their widths.
    pub fn repair_moved_text(
        &mut self,
        page: usize,
        expected: &[(usize, f32)],
    ) -> Result<Vec<RedrawnLine>, DocumentError> {
        struct Pending {
            index: usize,
            original_text: String,
            text: String,
            matrix: PdfMatrix,
            style: TextStyleSnapshot,
            width: f32,
            left: f32,
            bottom: f32,
        }

        // Pass 1: measure everything from the untouched page.
        let mut pending: Vec<Pending> = Vec::new();
        for &(index, width) in expected {
            if width <= 1.0 {
                continue;
            }
            let Ok(original_text) = self.text_object_text(page, index) else {
                continue;
            };
            let text = original_text.trim_end_matches(' ').to_string();
            if text.is_empty() {
                continue;
            }
            let Ok((left, bottom, right, _)) = self.object_bounds(page, index) else {
                continue;
            };
            if right - left <= 1.0 {
                continue;
            }
            let (Ok(matrix), Ok(style)) = (
                self.text_object_matrix(page, index),
                self.object_style(page, index),
            ) else {
                continue;
            };
            pending.push(Pending {
                index,
                original_text,
                text,
                matrix,
                style,
                width,
                left,
                bottom,
            });
        }
        if pending.is_empty() {
            return Ok(Vec::new());
        }

        // Pass 2: create every copy, then blank every original, then commit once.
        let mut copies: Vec<usize> = Vec::with_capacity(pending.len());
        {
            let mut pdf_page = self.doc.pages_mut().get(page as i32)?;
            let mut next_index = pdf_page.objects().len();
            {
                let objects = pdf_page.objects_mut();
                for item in &pending {
                    let mut copy = objects.create_text_object(
                        PdfPoints::ZERO,
                        PdfPoints::ZERO,
                        item.text.as_str(),
                        item.style.font_token,
                        item.style.font_size,
                    )?;
                    apply_text_object_style(&mut copy, &item.style)?;
                    copy.reset_matrix(item.matrix)?;
                    copies.push(next_index);
                    next_index += 1;
                }
                for item in &pending {
                    let mut original = objects.get(item.index)?;
                    if let Some(text_object) = original.as_text_object_mut() {
                        text_object.set_text("")?;
                    }
                }
            }
            pdf_page.regenerate_content()?;
        }

        // Pass 3: stretch each copy back to the width its line must keep.
        //
        // Object indices are not stable across regeneration — PDFium re-parses the
        // page and may renumber or split objects — so each copy is located by its
        // text and its position, never by a predicted index.
        let mut redrawn = Vec::with_capacity(pending.len());
        let mut used_copies: HashSet<usize> = HashSet::new();
        for item in &pending {
            let count = self.page_object_count(page)?;
            let mut found: Option<(usize, f32)> = None;
            for index in 0..count {
                if index == item.index || used_copies.contains(&index) {
                    continue;
                }
                let Ok(text) = self.text_object_text(page, index) else {
                    continue;
                };
                if text.trim_end_matches(' ') != item.text {
                    continue;
                }
                let Ok((left, bottom, _, _)) = self.object_bounds(page, index) else {
                    continue;
                };
                // Nearest match to where the line should be: a fresh object's ink
                // origin sits a few points away from the parsed original's.
                let distance = (left - item.left).abs() + (bottom - item.bottom).abs();
                if distance > 40.0 {
                    continue;
                }
                if found.map_or(true, |(_, best)| distance < best) {
                    found = Some((index, distance));
                }
            }
            let Some((copy_index, _)) = found else {
                continue;
            };
            used_copies.insert(copy_index);

            // Put the copy exactly where the line was, measured by ink bounds — this
            // makes repair idempotent, so a line repaired twice does not creep.
            if let Ok((copy_left, copy_bottom, _, _)) = self.object_bounds(page, copy_index) {
                let dx = item.left - copy_left;
                let dy = item.bottom - copy_bottom;
                if dx.abs() > 0.01 || dy.abs() > 0.01 {
                    let _ = self.translate_text_objects(page, &[copy_index], dx, dy);
                }
            }

            let mut copy_matrix = self.text_object_matrix(page, copy_index)?;
            if let Ok((copy_left, _, copy_right, _)) = self.object_bounds(page, copy_index) {
                let copy_width = copy_right - copy_left;
                if copy_width > 1.0 {
                    let scale = item.width / copy_width;
                    if (0.5..=2.0).contains(&scale) && (scale - 1.0).abs() > 0.002 {
                        let to_origin = PdfMatrix::new(1.0, 0.0, 0.0, 1.0, -copy_left, 0.0);
                        let stretch = PdfMatrix::new(scale, 0.0, 0.0, 1.0, 0.0, 0.0);
                        let back = PdfMatrix::new(1.0, 0.0, 0.0, 1.0, copy_left, 0.0);
                        let restored =
                            back.multiply(stretch).multiply(to_origin).multiply(copy_matrix);
                        if self.set_text_object_matrix(page, copy_index, restored).is_ok() {
                            copy_matrix = restored;
                        }
                    }
                }
            }
            // The stretch can shift the origin by exactly the width difference;
            // set the horizontal placement absolutely so the copy sits where the
            // line was, whatever the composition did.
            if let (Ok((copy_left, _, _, _)), Ok(matrix)) = (
                self.object_bounds(page, copy_index),
                self.text_object_matrix(page, copy_index),
            ) {
                let correction = item.left - copy_left;
                if correction.abs() > 0.05 {
                    let corrected = PdfMatrix::new(
                        matrix.a(),
                        matrix.b(),
                        matrix.c(),
                        matrix.d(),
                        matrix.e() + correction,
                        matrix.f(),
                    );
                    if self
                        .set_text_object_matrix(page, copy_index, corrected)
                        .is_ok()
                    {
                        copy_matrix = corrected;
                    }
                }
            }

            redrawn.push(RedrawnLine {
                page,
                original_index: item.index,
                copy_index,
                original_text: item.original_text.clone(),
                original_matrix: item.matrix,
                copy_matrix,
            });
        }

        Ok(redrawn)
    }

    /// Undo (`to_original`) or redo one [`RedrawnLine`].
    pub fn restore_redrawn_line(
        &mut self,
        line: &RedrawnLine,
        to_original: bool,
    ) -> Result<(), DocumentError> {
        if to_original {
            self.set_text_at_object_index(line.page, line.original_index, &line.original_text)?;
            self.set_text_object_matrix(line.page, line.original_index, line.original_matrix)?;
            self.set_text_at_object_index(line.page, line.copy_index, "")?;
        } else {
            self.set_text_at_object_index(line.page, line.original_index, "")?;
            self.set_text_at_object_index(
                line.page,
                line.copy_index,
                line.original_text.trim_end_matches(' '),
            )?;
            self.set_text_object_matrix(line.page, line.copy_index, line.copy_matrix)?;
        }
        Ok(())
    }

    /// Bounding box of a page object in page space: `(left, bottom, right, top)`.
    pub fn object_bounds(
        &self,
        page: usize,
        object_index: usize,
    ) -> Result<(f32, f32, f32, f32), DocumentError> {
        let pdf_page = self.doc.pages().get(page as i32)?;
        let object = pdf_page.objects().get(object_index)?;
        let bounds = object.bounds()?;
        Ok((
            bounds.left().value,
            bounds.bottom().value,
            bounds.right().value,
            bounds.top().value,
        ))
    }

    /// Number of page objects (text, image, path, …) on a page.
    pub fn page_object_count(&self, page: usize) -> Result<usize, DocumentError> {
        let pdf_page = self.doc.pages().get(page as i32)?;
        Ok(pdf_page.objects().len())
    }

    /// Return the text of the page object at `object_index`.
    pub fn text_object_text(
        &self,
        page: usize,
        object_index: usize,
    ) -> Result<String, DocumentError> {
        let pdf_page = self.doc.pages().get(page as i32)?;
        let object = pdf_page.objects().get(object_index)?;
        match object {
            PdfPageObject::Text(text_object) => Ok(text_object.text()),
            _ => Err(DocumentError("object is not a text object".into())),
        }
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
    #[cfg(any(feature = "ocr", target_os = "windows"))]
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
                text_markup_annotation!(pdf_page, annotation, create_highlight_annotation, l, b, r, t);
            }
            AnnotationKind::Strikeout => {
                text_markup_annotation!(pdf_page, annotation, create_strikeout_annotation, l, b, r, t);
            }
            AnnotationKind::Underline => {
                text_markup_annotation!(pdf_page, annotation, create_underline_annotation, l, b, r, t);
            }
            AnnotationKind::Squiggly => {
                text_markup_annotation!(pdf_page, annotation, create_squiggly_annotation, l, b, r, t);
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

            AnnotationKind::Redaction => {
                let mut a = pdf_page.annotations_mut().create_square_annotation()?;
                a.set_bounds(PdfRect::new_from_values(b, l, t, r))?;
                a.set_contents(REDACTION_CONTENTS_MARKER)?;
                a.set_fill_color(crate::annotations::rgba_to_pdf_color(
                    annotation.style.fill.unwrap_or([220, 30, 30, 75]),
                ))?;
                a.set_stroke_color(crate::annotations::rgba_to_pdf_color(
                    annotation.style.stroke.unwrap_or([220, 30, 30, 255]),
                ))?;
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

    /// Move a page from `from` (0-based) to `to` (0-based).
    pub fn reorder_page(
        &mut self,
        pdfium: &'static Pdfium,
        from: usize,
        to: usize,
    ) -> Result<(), DocumentError> {
        if from >= self.page_count || to >= self.page_count {
            return Err(DocumentError("page index out of bounds".into()));
        }
        if from == to {
            return Ok(());
        }
        let bytes = self.doc.save_to_bytes()?;
        let temp_doc = pdfium.load_pdf_from_byte_vec(bytes, None)?;
        {
            let pdf_page = self.doc.pages().get(from as i32)?;
            pdf_page.delete()?;
        }
        self.doc
            .pages_mut()
            .copy_page_from_document(&temp_doc, from as i32, to as i32)?;
        let annots = self.annotations.remove(from);
        self.annotations.insert(to, annots);
        self.refresh_pages()?;
        self.refresh_render_docs(pdfium);
        Ok(())
    }

    /// Duplicate `page` (0-based) immediately after itself, returning the new
    /// page index.
    pub fn duplicate_page(
        &mut self,
        pdfium: &'static Pdfium,
        page: usize,
    ) -> Result<usize, DocumentError> {
        if page >= self.page_count {
            return Err(DocumentError("page index out of bounds".into()));
        }
        let bytes = self.doc.save_to_bytes()?;
        let temp_doc = pdfium.load_pdf_from_byte_vec(bytes, None)?;
        let insert_at = page + 1;
        self.doc
            .pages_mut()
            .copy_page_from_document(&temp_doc, page as i32, insert_at as i32)?;
        let mut cloned = self.annotations.get(page).cloned().unwrap_or_default();
        for a in &mut cloned {
            a.id = self.next_id;
            self.next_id += 1;
        }
        self.annotations.insert(insert_at, cloned);
        self.refresh_pages()?;
        self.refresh_render_docs(pdfium);
        Ok(insert_at)
    }

    /// Merge all pages from another PDF into this document starting at
    /// `insert_at` (0-based), returning the number of pages added.
    pub fn merge_pdf(
        &mut self,
        pdfium: &'static Pdfium,
        other_path: &Path,
        insert_at: usize,
    ) -> Result<usize, DocumentError> {
        let other = pdfium.load_pdf_from_file(other_path, None)?;
        let added = other.pages().len().max(0) as usize;
        if added == 0 {
            return Ok(0);
        }
        let idx = insert_at.min(self.page_count);
        self.doc.pages_mut().copy_page_range_from_document(
            &other,
            0..=(added as i32 - 1),
            idx as i32,
        )?;
        self.refresh_pages()?;
        self.read_annotations();
        self.refresh_render_docs(pdfium);
        Ok(added)
    }

    /// Extract the given 0-based `pages` (in the order provided) into a new
    /// PDF saved atomically to `dest_path`.
    pub fn extract_pages_to(
        &self,
        pdfium: &'static Pdfium,
        pages: &[usize],
        dest_path: &Path,
    ) -> Result<(), DocumentError> {
        if pages.is_empty() {
            return Err(DocumentError("no pages selected to extract".into()));
        }
        for &p in pages {
            if p >= self.page_count {
                return Err(DocumentError(format!("page {} out of bounds", p + 1)));
            }
        }
        let mut out = pdfium.create_new_pdf()?;
        for &p in pages {
            let dst = out.pages().len();
            out.pages_mut()
                .copy_page_from_document(&self.doc, p as i32, dst)?;
        }
        let bytes = out.save_to_bytes()?;
        let dir = dest_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let file_name = dest_path
            .file_name()
            .ok_or_else(|| DocumentError("destination path has no file name".into()))?;
        let mut tmp_name = std::ffi::OsString::from(".");
        tmp_name.push(file_name);
        tmp_name.push(format!(".tmp-{}", std::process::id()));
        let tmp_path = dir.join(tmp_name);
        if let Err(e) = std::fs::write(&tmp_path, &bytes) {
            return Err(DocumentError(format!(
                "could not write {}: {e}",
                tmp_path.display()
            )));
        }
        if let Err(e) = std::fs::rename(&tmp_path, dest_path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(DocumentError(format!(
                "could not replace {}: {e}",
                dest_path.display()
            )));
        }
        Ok(())
    }

    /// Crop a page's visible margins by `(left, bottom, right, top)` PDF points
    /// inward from its current CropBox (or MediaBox if no CropBox is set).
    pub fn crop_page_margins(
        &mut self,
        page: usize,
        left: f32,
        bottom: f32,
        right: f32,
        top: f32,
    ) -> Result<(), DocumentError> {
        let current = {
            let pdf_page = self.doc.pages().get(page as i32)?;
            pdf_page
                .boundaries()
                .crop()
                .or_else(|_| pdf_page.boundaries().media())
                .map(|b| b.bounds)
                .unwrap_or_else(|_| {
                    let w = self.pages.get(page).map(|p| p.width_pts).unwrap_or(612.0);
                    let h = self.pages.get(page).map(|p| p.height_pts).unwrap_or(792.0);
                    PdfRect::new_from_values(0.0, 0.0, h, w)
                })
        };
        let nl = current.left().value + left;
        let nb = current.bottom().value + bottom;
        let nr = current.right().value - right;
        let nt = current.top().value - top;
        if nr - nl < 36.0 || nt - nb < 36.0 {
            return Err(DocumentError(
                "crop margins are too large (remaining page must be at least 36 pt wide and tall)"
                    .into(),
            ));
        }
        let mut pdf_page = self.doc.pages_mut().get(page as i32)?;
        pdf_page
            .boundaries_mut()
            .set_crop(PdfRect::new_from_values(nb, nl, nt, nr))?;
        self.refresh_pages()?;
        Ok(())
    }

    /// Reset a page's CropBox back to its MediaBox.
    pub fn reset_page_crop(&mut self, page: usize) -> Result<(), DocumentError> {
        let media = {
            let pdf_page = self.doc.pages().get(page as i32)?;
            pdf_page.boundaries().media()?.bounds
        };
        let mut pdf_page = self.doc.pages_mut().get(page as i32)?;
        pdf_page.boundaries_mut().set_crop(media)?;
        self.refresh_pages()?;
        Ok(())
    }

    /// Permanently apply all pending [`AnnotationKind::Redaction`] marks on
    /// `page`, destroying underlying text, image pixels, and vector paths that
    /// overlap the redacted regions, removing overlapping annotations, and
    /// burning opaque black redaction boxes into the page content stream.
    /// Returns the number of redaction regions applied.
    pub fn apply_redactions(
        &mut self,
        pdfium: &'static Pdfium,
        page: usize,
    ) -> Result<usize, DocumentError> {
        if page >= self.page_count {
            return Err(DocumentError("page index out of bounds".into()));
        }
        let redaction_rects: Vec<(f32, f32, f32, f32)> = self.annotations[page]
            .iter()
            .filter(|a| a.kind == AnnotationKind::Redaction)
            .map(|a| {
                let (l, b, r, t) = a.bounds_f32();
                (l.min(r), b.min(t), l.max(r), b.max(t))
            })
            .filter(|(l, b, r, t)| (r - l) >= 0.5 && (t - b) >= 0.5)
            .collect();
        if redaction_rects.is_empty() {
            return Ok(0);
        }

        let intersects_any = |l: f32, b: f32, r: f32, t: f32| -> bool {
            redaction_rects
                .iter()
                .any(|&(rl, rb, rr, rt)| l < rr && r > rl && b < rt && t > rb)
        };

        // 1. Remove all redaction marks and any other annotations overlapping them.
        let doomed_annots: Vec<Annotation> = self.annotations[page]
            .iter()
            .filter(|a| {
                if a.kind == AnnotationKind::Redaction {
                    return true;
                }
                let (l, b, r, t) = a.bounds_f32();
                intersects_any(l.min(r), b.min(t), l.max(r), b.max(t))
            })
            .cloned()
            .collect();
        for a in &doomed_annots {
            let _ = self.remove_annotation_from_pdf(page, a);
        }
        let doomed_ids: HashSet<u64> = doomed_annots.iter().map(|a| a.id).collect();
        self.annotations[page].retain(|a| !doomed_ids.contains(&a.id));

        // 2. Inspect text objects intersecting redaction rects before mutating.
        struct SurvivingSpan {
            text: String,
            origin: (f32, f32),
        }
        struct TextRedactionPlan {
            style: Option<TextStyleSnapshot>,
            base_origin: (f32, f32),
            surviving: Vec<SurvivingSpan>,
        }

        let mut text_plans: Vec<TextRedactionPlan> = Vec::new();
        let mut remove_indices: Vec<usize> = Vec::new();
        let mut image_masks: Vec<(usize, (f32, f32, f32, f32))> = Vec::new();

        {
            let pdf_page = self.doc.pages().get(page as i32)?;
            let text_page = pdf_page.text()?;
            let char_bounds_map: std::collections::HashMap<usize, (f32, f32, f32, f32)> = text_page
                .chars()
                .iter()
                .filter_map(|ch| {
                    let bounds = ch
                        .loose_bounds()
                        .or_else(|_| ch.tight_bounds())
                        .ok()
                        .map(|r| (
                            r.left().value,
                            r.bottom().value,
                            r.right().value,
                            r.top().value,
                        ))?;
                    Some((ch.index(), bounds))
                })
                .collect();

            for (obj_idx, obj) in pdf_page.objects().iter().enumerate() {
                let Ok(bounds) = obj.bounds() else {
                    continue;
                };
                let rect = bounds.to_rect();
                let (ol, ob, or, ot) = (
                    rect.left().value,
                    rect.bottom().value,
                    rect.right().value,
                    rect.top().value,
                );
                if !intersects_any(ol, ob, or, ot) {
                    continue;
                }

                match obj {
                    PdfPageObject::Text(_) => {
                        if let (Ok((obj_chars, origins, chars_text)), Ok(style)) = (
                            self.object_chars_layout(page, obj_idx),
                            self.object_style(page, obj_idx),
                        ) && !obj_chars.is_empty()
                        {
                            let base_origin = origins[0];
                            let mut spans: Vec<SurvivingSpan> = Vec::new();
                            let mut current = String::new();
                            let mut current_origin = base_origin;

                            for i in 0..obj_chars.len() {
                                let cidx = obj_chars[i];
                                let is_redacted = if let Some(&(cl, cb, cr, ct)) =
                                    char_bounds_map.get(&cidx)
                                {
                                    let cx = (cl + cr) * 0.5;
                                    let cy = (cb + ct) * 0.5;
                                    redaction_rects.iter().any(|&(rl, rb, rr, rt)| {
                                        cx >= rl && cx <= rr && cy >= rb && cy <= rt
                                    })
                                } else {
                                    let (ox, oy) = origins[i];
                                    redaction_rects.iter().any(|&(rl, rb, rr, rt)| {
                                        ox >= rl && ox <= rr && oy >= rb && oy <= rt
                                    })
                                };

                                if is_redacted {
                                    if !current.trim().is_empty() {
                                        spans.push(SurvivingSpan {
                                            text: std::mem::take(&mut current),
                                            origin: current_origin,
                                        });
                                    } else {
                                        current.clear();
                                    }
                                } else {
                                    if current.is_empty() {
                                        current_origin = origins[i];
                                    }
                                    current.push(chars_text[i]);
                                }
                            }
                            if !current.trim().is_empty() {
                                spans.push(SurvivingSpan {
                                    text: current,
                                    origin: current_origin,
                                });
                            }

                            text_plans.push(TextRedactionPlan {
                                style: Some(style),
                                base_origin,
                                surviving: spans,
                            });
                        } else {
                            text_plans.push(TextRedactionPlan {
                                style: None,
                                base_origin: (0.0, 0.0),
                                surviving: Vec::new(),
                            });
                        }
                        remove_indices.push(obj_idx);
                    }
                    PdfPageObject::Image(_) => {
                        let obj_area = ((or - ol).max(0.0) * (ot - ob).max(0.0)).max(1.0);
                        let covered_area: f32 = redaction_rects
                            .iter()
                            .map(|&(rl, rb, rr, rt)| {
                                let iw = (or.min(rr) - ol.max(rl)).max(0.0);
                                let ih = (ot.min(rt) - ob.max(rb)).max(0.0);
                                iw * ih
                            })
                            .sum();
                        if covered_area / obj_area >= 0.90 {
                            remove_indices.push(obj_idx);
                        } else {
                            image_masks.push((obj_idx, (ol, ob, or, ot)));
                        }
                    }
                    PdfPageObject::Path(_) => {
                        let obj_area = ((or - ol).max(0.0) * (ot - ob).max(0.0)).max(1.0);
                        let covered_area: f32 = redaction_rects
                            .iter()
                            .map(|&(rl, rb, rr, rt)| {
                                let iw = (or.min(rr) - ol.max(rl)).max(0.0);
                                let ih = (ot.min(rt) - ob.max(rb)).max(0.0);
                                iw * ih
                            })
                            .sum();
                        let mid_x = (ol + or) * 0.5;
                        let mid_y = (ob + ot) * 0.5;
                        let mid_inside = redaction_rects.iter().any(|&(rl, rb, rr, rt)| {
                            mid_x >= rl && mid_x <= rr && mid_y >= rb && mid_y <= rt
                        });
                        if covered_area / obj_area >= 0.75 || mid_inside {
                            remove_indices.push(obj_idx);
                        }
                    }
                    _ => {}
                }
            }
        }

        // 3. Mask partial image overlaps in-place before removing objects.
        if !image_masks.is_empty() {
            let mut pdf_page = self.doc.pages_mut().get(page as i32)?;
            let objects = pdf_page.objects_mut();
            for (obj_idx, (il, ib, ir, it)) in image_masks {
                let Ok(mut obj) = objects.get(obj_idx) else {
                    continue;
                };
                let Some(image_obj) = obj.as_image_object_mut() else {
                    continue;
                };
                let Ok(raw) = image_obj.get_raw_image() else {
                    continue;
                };
                let mut rgba = raw.to_rgba8();
                let (w, h) = rgba.dimensions();
                let iw = (ir - il).max(1e-3);
                let ih = (it - ib).max(1e-3);
                for &(rl, rb, rr, rt) in &redaction_rects {
                    let u0 = ((rl - il) / iw).clamp(0.0, 1.0);
                    let u1 = ((rr - il) / iw).clamp(0.0, 1.0);
                    let v0 = ((it - rt) / ih).clamp(0.0, 1.0);
                    let v1 = ((it - rb) / ih).clamp(0.0, 1.0);
                    let px0 = (u0 * w as f32).floor() as u32;
                    let px1 = (u1 * w as f32).ceil().min(w as f32) as u32;
                    let py0 = (v0 * h as f32).floor() as u32;
                    let py1 = (v1 * h as f32).ceil().min(h as f32) as u32;
                    for py in py0..py1 {
                        for px in px0..px1 {
                            rgba.put_pixel(px, py, image::Rgba([0, 0, 0, 255]));
                        }
                    }
                }
                let _ = image_obj.set_image(&image::DynamicImage::ImageRgba8(rgba));
            }
        }

        // 4. Remove redacted objects in descending index order, then recreate surviving spans.
        remove_indices.sort_unstable();
        remove_indices.dedup();
        {
            let mut pdf_page = self.doc.pages_mut().get(page as i32)?;
            {
                let objects = pdf_page.objects_mut();
                for idx in remove_indices.into_iter().rev() {
                    let _ = objects.remove_object_at_index(idx);
                }
                for plan in &text_plans {
                    let Some(style) = plan.style.as_ref() else {
                        continue;
                    };
                    for span in &plan.surviving {
                        let mut new_obj = objects.create_text_object(
                            PdfPoints::ZERO,
                            PdfPoints::ZERO,
                            span.text.as_str(),
                            style.font_token,
                            style.font_size,
                        )?;
                        apply_text_object_style(&mut new_obj, style)?;
                        new_obj.translate(
                            PdfPoints::new(span.origin.0 - plan.base_origin.0),
                            PdfPoints::new(span.origin.1 - plan.base_origin.1),
                        )?;
                    }
                }
            }
            // 5. Burn opaque black redaction rectangles into the page content stream.
            for &(l, b, r, t) in &redaction_rects {
                let rect = PdfPagePathObject::new_rect(
                    &self.doc,
                    PdfRect::new_from_values(b, l, t, r),
                    Some(PdfColor::BLACK),
                    Some(PdfPoints::new(0.5)),
                    Some(PdfColor::BLACK),
                )?;
                pdf_page.objects_mut().add_path_object(rect)?;
            }
            pdf_page.regenerate_content()?;
        }

        self.refresh_pages()?;
        self.refresh_render_docs(pdfium);
        Ok(redaction_rects.len())
    }

    /// Permanently apply all pending redactions across every page.
    pub fn apply_all_redactions(
        &mut self,
        pdfium: &'static Pdfium,
    ) -> Result<usize, DocumentError> {
        let mut total = 0;
        for p in 0..self.page_count {
            total += self.apply_redactions(pdfium, p)?;
        }
        Ok(total)
    }

    /// Ensure AcroForm text-field appearance streams are up to date in `self.doc`
    /// before flattening, so filled form values render into static page content.
    fn sync_form_appearances_for_flatten(
        &mut self,
        pdfium: &'static Pdfium,
    ) -> Result<(), DocumentError> {
        let has_forms = self.form_fields().map(|f| !f.is_empty()).unwrap_or(false);
        if !has_forms {
            return Ok(());
        }
        let bytes = self.doc.save_to_bytes()?;
        let regenerated = crate::appearance::regenerate_form_appearances(self.doc.bindings(), &bytes);
        let reloaded = pdfium.load_pdf_from_byte_vec(regenerated, None)?;
        // SAFETY: the Pdfium this document borrows is leaked and lives for the whole process.
        self.doc = unsafe { std::mem::transmute(reloaded) };
        Ok(())
    }

    /// Flatten all annotations and form fields on `page` permanently into static
    /// page content.
    pub fn flatten_page(
        &mut self,
        pdfium: &'static Pdfium,
        page: usize,
    ) -> Result<(), DocumentError> {
        if page >= self.page_count {
            return Err(DocumentError("page index out of bounds".into()));
        }
        self.sync_form_appearances_for_flatten(pdfium)?;
        {
            let mut pdf_page = self.doc.pages_mut().get(page as i32)?;
            pdf_page.flatten()?;
        }
        self.annotations[page].clear();
        self.refresh_pages()?;
        self.refresh_render_docs(pdfium);
        Ok(())
    }

    /// Flatten all annotations and form fields on every page permanently into
    /// static page content, returning the number of pages flattened.
    pub fn flatten_all_pages(
        &mut self,
        pdfium: &'static Pdfium,
    ) -> Result<usize, DocumentError> {
        self.sync_form_appearances_for_flatten(pdfium)?;
        let count = self.page_count;
        for p in 0..count {
            let mut pdf_page = self.doc.pages_mut().get(p as i32)?;
            pdf_page.flatten()?;
        }
        for list in &mut self.annotations {
            list.clear();
        }
        self.refresh_pages()?;
        self.refresh_render_docs(pdfium);
        Ok(count)
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
        let mut bytes = self.doc.save_to_bytes()?;

        // If the document has AcroForm fields, regenerate their appearance
        // streams so filled values show in viewers that do not honor
        // /NeedAppearances. A failed regeneration keeps the original bytes.
        if let Ok(fields) = self.form_fields()
            && !fields.is_empty()
        {
            bytes = crate::appearance::regenerate_form_appearances(self.doc.bindings(), &bytes);
        }

        if self.metadata_dirty {
            bytes = apply_metadata_to_pdf_bytes(&bytes, &self.metadata())?;
        }

        if let Some(ref enc_opts) = self.pending_encryption {
            bytes = crate::security::encrypt_pdf_bytes(&bytes, enc_opts)?;
        }

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
        self.save_atomic(self.path.as_path())?;
        self.metadata_dirty = false;
        self.is_encrypted = self.pending_encryption.is_some();
        Ok(())
    }

    /// Save to a new path (Save As), written atomically.
    pub fn save_as(&mut self, path: &Path) -> Result<(), DocumentError> {
        self.flush_pending_content()?;
        self.save_atomic(path)?;
        self.metadata_dirty = false;
        self.is_encrypted = self.pending_encryption.is_some();
        Ok(())
    }

    /// Configure password encryption (applied on next save).
    pub fn set_encryption(&mut self, opts: Option<crate::security::EncryptionOptions>) {
        self.pending_encryption = opts;
    }

    /// Remove password encryption from the in-memory document by copying
    /// pages into a fresh unencrypted PDF so the next save writes plaintext.
    pub fn strip_encryption(&mut self, pdfium: &'static Pdfium) -> Result<(), DocumentError> {
        if self.page_count > 0 {
            let mut clean = pdfium.create_new_pdf()?;
            clean.pages_mut().copy_page_range_from_document(
                &self.doc,
                0..=(self.page_count as i32 - 1),
                0,
            )?;
            // SAFETY: the Pdfium this document borrows is leaked and lives for
            // the whole process.
            self.doc = unsafe { std::mem::transmute(clean) };
            if !self.title.is_empty()
                || !self.author.is_empty()
                || !self.subject.is_empty()
                || !self.keywords.is_empty()
            {
                self.metadata_dirty = true;
            }
        }
        self.is_encrypted = false;
        self.pending_encryption = None;
        self.refresh_render_docs(pdfium);
        Ok(())
    }

    /// Whether the document will be password-protected on save.
    pub fn is_password_protected(&self) -> bool {
        self.pending_encryption.is_some()
    }

    /// Current document metadata.
    pub fn metadata(&self) -> PdfMetadata {
        PdfMetadata {
            title: self.title.clone(),
            author: self.author.clone(),
            subject: self.subject.clone(),
            keywords: self.keywords.clone(),
        }
    }

    /// Update document metadata (persisted on next save).
    pub fn set_metadata(&mut self, meta: PdfMetadata) {
        self.title = meta.title;
        self.author = meta.author;
        self.subject = meta.subject;
        self.keywords = meta.keywords;
        self.metadata_dirty = true;
    }

    /// Enumerate every AcroForm field in the document as plain data for the UI.
    ///
    /// `index` is the field's annotation index within its page, which is stable
    /// for the lifetime of the open document (adding/removing annotations shifts
    /// indices, so the UI refreshes its list after any such edit).
    pub fn form_fields(&self) -> Result<Vec<FormFieldInfo>, DocumentError> {
        let mut out = Vec::new();
        for page_idx in 0..self.page_count {
            let pdf_page = self.doc.pages().get(page_idx as i32)?;
            let annots: Vec<PdfPageAnnotation> = pdf_page.annotations().iter().collect();
            for (index, a) in annots.iter().enumerate() {
                let Some(field) = a.as_form_field() else {
                    continue;
                };
                let ft = field.field_type();
                let value = match ft {
                    PdfFormFieldType::Text => field.as_text_field().and_then(|f| f.value()),
                    PdfFormFieldType::Checkbox => field
                        .as_checkbox_field()
                        .and_then(|f| f.is_checked().ok())
                        .map(|b| b.to_string()),
                    PdfFormFieldType::RadioButton => field
                        .as_radio_button_field()
                        .and_then(|f| f.is_checked().ok())
                        .map(|b| b.to_string()),
                    PdfFormFieldType::ComboBox => field.as_combo_box_field().and_then(|f| f.value()),
                    PdfFormFieldType::ListBox => field.as_list_box_field().and_then(|f| f.value()),
                    _ => None,
                }
                .unwrap_or_default();

                let bounds = a.bounds().unwrap_or_else(|_| PdfRect::new_from_values(0.0, 0.0, 0.0, 0.0));
                let multiline = field
                    .as_text_field()
                    .map(|f| f.is_multiline())
                    .unwrap_or(false);
                out.push(FormFieldInfo {
                    page: page_idx,
                    index,
                    name: field.name().unwrap_or_default(),
                    field_type: ft.into(),
                    value,
                    multiline,
                    l: bounds.left().value,
                    b: bounds.bottom().value,
                    r: bounds.right().value,
                    t: bounds.top().value,
                });
            }
        }
        Ok(out)
    }

    /// Set the value of a single AcroForm field (matched by page + annotation
    /// index). Text fields take the string directly; checkboxes/radios take
    /// `"true"`/`"false"`. Combo/list boxes are read-only through pdfium-render.
    pub fn set_form_field_value(
        &mut self,
        page: usize,
        index: usize,
        value: &str,
    ) -> Result<(), DocumentError> {
        let pdf_page = self.doc.pages().get(page as i32)?;
        let annotations = pdf_page.annotations();
        let mut annot = annotations
            .get(index)
            .map_err(|_| DocumentError(format!("no such form field: {index}")))?;
        let Some(field) = annot.as_form_field_mut() else {
            return Err(DocumentError(format!("annotation {index} is not a form field")));
        };
        match field {
            PdfFormField::Text(t) => t.set_value(value)?,
            PdfFormField::Checkbox(c) => c.set_checked(value.eq_ignore_ascii_case("true"))?,
            PdfFormField::RadioButton(r) => {
                if value.eq_ignore_ascii_case("true") {
                    r.set_checked()?;
                }
            }
            PdfFormField::ComboBox(_) | PdfFormField::ListBox(_) => {
                return Err(DocumentError(
                    "combo/list fields are read-only through pdfium-render 0.9".into(),
                ))
            }
            PdfFormField::PushButton(_)
            | PdfFormField::Signature(_)
            | PdfFormField::Unknown(_) => {}
        }
        Ok(())
    }

    /// Extract the document's bookmark/outline tree.
    pub fn bookmarks(&self) -> Vec<BookmarkNode> {
        fn collect_siblings(first: Option<PdfBookmark<'_>>, depth: usize) -> Vec<BookmarkNode> {
            if depth > 32 {
                return Vec::new();
            }
            let mut out = Vec::new();
            let mut current = first;
            while let Some(bm) = current {
                let title = bm.title().unwrap_or_default();
                let page = bookmark_page(&bm);
                let children = collect_siblings(bm.first_child(), depth + 1);
                if !title.is_empty() || page.is_some() || !children.is_empty() {
                    out.push(BookmarkNode {
                        title: if title.is_empty() { "Untitled".into() } else { title },
                        page,
                        children,
                    });
                }
                current = bm.next_sibling();
            }
            out
        }
        collect_siblings(self.doc.bookmarks().root(), 0)
    }

    /// List all hyperlink targets on `page`.
    pub fn page_links(&self, page: usize) -> Result<Vec<PageLinkInfo>, DocumentError> {
        let pdf_page = self.doc.pages().get(page as i32)?;
        let mut out = Vec::new();
        for (index, link) in pdf_page.links().iter().enumerate() {
            let Ok(rect) = link.rect() else {
                continue;
            };
            let target = if let Some(dest) = link.destination() {
                if let Ok(idx) = dest.page_index() {
                    Some(LinkTarget::Page(idx as usize))
                } else {
                    None
                }
            } else if let Some(action) = link.action() {
                if let Some(local) = action.as_local_destination_action() {
                    local
                        .destination()
                        .ok()
                        .and_then(|d| d.page_index().ok())
                        .map(|idx| LinkTarget::Page(idx as usize))
                } else if let Some(uri_action) = action.as_uri_action() {
                    uri_action.uri().ok().map(|u| {
                        if let Some(rest) = u.strip_prefix("#page=")
                            && let Ok(p1) = rest.trim().parse::<usize>()
                            && p1 >= 1
                        {
                            LinkTarget::Page(p1 - 1)
                        } else {
                            LinkTarget::Uri(u)
                        }
                    })
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(target) = target {
                out.push(PageLinkInfo {
                    page,
                    index,
                    l: rect.left().value,
                    b: rect.bottom().value,
                    r: rect.right().value,
                    t: rect.top().value,
                    target,
                });
            }
        }
        Ok(out)
    }

    /// Add a clickable hyperlink annotation on `page`.
    pub fn add_link_annotation(
        &mut self,
        pdfium: &'static Pdfium,
        page: usize,
        l: f32,
        b: f32,
        r: f32,
        t: f32,
        target: &LinkTarget,
    ) -> Result<(), DocumentError> {
        let uri = match target {
            LinkTarget::Uri(u) => u.trim().to_string(),
            LinkTarget::Page(p) => format!("#page={}", p + 1),
        };
        if uri.is_empty() {
            return Err(DocumentError("link target cannot be empty".into()));
        }
        {
            let mut pdf_page = self.doc.pages_mut().get(page as i32)?;
            let mut annot = pdf_page.annotations_mut().create_link_annotation(&uri)?;
            annot.set_bounds(PdfRect::new_from_values(b, l, t, r))?;
        }
        self.refresh_render_docs(pdfium);
        Ok(())
    }

    /// Stamp headers, footers, or page numbers across a range of pages.
    ///
    /// Supports `{page}` / `{n}` for the page number and `{total}` for total pages.
    pub fn apply_header_footer(
        &mut self,
        pdfium: &'static Pdfium,
        opts: &HeaderFooterOptions,
    ) -> Result<usize, DocumentError> {
        if opts.text.trim().is_empty() {
            return Err(DocumentError("header/footer text cannot be empty".into()));
        }
        if self.page_count == 0 {
            return Ok(0);
        }
        let start = opts.start_page.min(self.page_count - 1);
        let end = opts
            .end_page
            .unwrap_or(self.page_count - 1)
            .min(self.page_count - 1);
        if start > end {
            return Ok(0);
        }

        let font = self.doc.fonts_mut().helvetica();
        let total_str = self.page_count.to_string();
        let mut count = 0;
        for p in start..=end {
            let num = (opts.start_number + (p - start)).to_string();
            let resolved = opts
                .text
                .replace("{page}", &num)
                .replace("{n}", &num)
                .replace("{total}", &total_str);
            if resolved.trim().is_empty() {
                continue;
            }
            let mut pdf_page = self.doc.pages_mut().get(p as i32)?;
            let pw = pdf_page.width().value;
            let ph = pdf_page.height().value;
            let mut obj = PdfPageTextObject::new(
                &self.doc,
                &resolved,
                font,
                PdfPoints::new(opts.font_size.max(4.0)),
            )?;
            obj.set_fill_color(PdfColor::new(
                opts.color[0],
                opts.color[1],
                opts.color[2],
                opts.color[3],
            ))?;
            let bounds = obj.bounds()?;
            let tw = bounds.width().value;
            let th = bounds.height().value;
            let target_x = match opts.position {
                StampPosition::TopLeft | StampPosition::BottomLeft => opts.margin_x,
                StampPosition::TopCenter | StampPosition::BottomCenter => (pw - tw) * 0.5,
                StampPosition::TopRight | StampPosition::BottomRight => {
                    (pw - opts.margin_x - tw).max(0.0)
                }
            };
            let target_y = match opts.position {
                StampPosition::TopLeft | StampPosition::TopCenter | StampPosition::TopRight => {
                    (ph - opts.margin_y - th).max(0.0)
                }
                StampPosition::BottomLeft
                | StampPosition::BottomCenter
                | StampPosition::BottomRight => opts.margin_y,
            };
            obj.translate(
                PdfPoints::new(target_x - bounds.left().value),
                PdfPoints::new(target_y - bounds.bottom().value),
            )?;
            pdf_page.objects_mut().add_text_object(obj)?;
            pdf_page.regenerate_content()?;
            count += 1;
        }
        self.refresh_render_docs(pdfium);
        Ok(count)
    }

    /// Stamp a centered, optionally rotated text watermark across a range of pages.
    pub fn apply_watermark(
        &mut self,
        pdfium: &'static Pdfium,
        opts: &WatermarkOptions,
    ) -> Result<usize, DocumentError> {
        let trimmed = opts.text.trim();
        if trimmed.is_empty() {
            return Err(DocumentError("watermark text cannot be empty".into()));
        }
        if self.page_count == 0 {
            return Ok(0);
        }
        let start = opts.start_page.min(self.page_count - 1);
        let end = opts
            .end_page
            .unwrap_or(self.page_count - 1)
            .min(self.page_count - 1);
        if start > end {
            return Ok(0);
        }

        let font = self.doc.fonts_mut().helvetica_bold();
        let mut count = 0;
        for p in start..=end {
            let mut pdf_page = self.doc.pages_mut().get(p as i32)?;
            let pw = pdf_page.width().value;
            let ph = pdf_page.height().value;
            let mut obj = PdfPageTextObject::new(
                &self.doc,
                trimmed,
                font,
                PdfPoints::new(opts.font_size.max(8.0)),
            )?;
            obj.set_fill_color(PdfColor::new(
                opts.color[0],
                opts.color[1],
                opts.color[2],
                opts.color[3],
            ))?;
            if opts.angle_deg.abs() > 0.01 {
                obj.rotate_counter_clockwise_degrees(opts.angle_deg)?;
            }
            let bounds = obj.bounds()?;
            let cx = (bounds.left().value + bounds.right().value) * 0.5;
            let cy = (bounds.bottom().value + bounds.top().value) * 0.5;
            obj.translate(PdfPoints::new(pw * 0.5 - cx), PdfPoints::new(ph * 0.5 - cy))?;
            pdf_page.objects_mut().add_text_object(obj)?;
            pdf_page.regenerate_content()?;
            count += 1;
        }
        self.refresh_render_docs(pdfium);
        Ok(count)
    }

    /// Return the fill colour `[r, g, b, a]` of the text object at `object_index`.
    pub fn text_object_fill_color(
        &self,
        page: usize,
        object_index: usize,
    ) -> Result<[u8; 4], DocumentError> {
        let pdf_page = self.doc.pages().get(page as i32)?;
        let object = pdf_page.objects().get(object_index)?;
        let PdfPageObject::Text(text_obj) = object else {
            return Err(DocumentError("object is not a text object".into()));
        };
        let c = text_obj.fill_color().unwrap_or(PdfColor::BLACK);
        Ok([c.red(), c.green(), c.blue(), c.alpha()])
    }

    /// Set the fill colour `[r, g, b, a]` of the text object at `object_index`.
    pub fn set_text_object_fill_color(
        &mut self,
        pdfium: &'static Pdfium,
        page: usize,
        object_index: usize,
        color: [u8; 4],
    ) -> Result<(), DocumentError> {
        let mut pdf_page = self.doc.pages_mut().get(page as i32)?;
        {
            let objects = pdf_page.objects_mut();
            let mut object = objects.get(object_index)?;
            if object.as_text_object().is_none() {
                return Err(DocumentError("object is not a text object".into()));
            }
            object.set_fill_color(PdfColor::new(color[0], color[1], color[2], color[3]))?;
        }
        pdf_page.regenerate_content()?;
        self.refresh_render_docs(pdfium);
        Ok(())
    }

    /// Return the best-matching built-in font family for the text object at `object_index`.
    pub fn text_object_font_family(
        &self,
        page: usize,
        object_index: usize,
    ) -> Result<TextFontFamily, DocumentError> {
        let pdf_page = self.doc.pages().get(page as i32)?;
        let object = pdf_page.objects().get(object_index)?;
        let PdfPageObject::Text(text_obj) = object else {
            return Err(DocumentError("object is not a text object".into()));
        };
        Ok(TextFontFamily::from_font_name(&text_obj.font().name()))
    }

    /// Replace the text object at `object_index` with a copy using `family`,
    /// preserving text, font size, fill colour, render mode, and position.
    /// Returns the new object index.
    pub fn set_text_object_font_family(
        &mut self,
        pdfium: &'static Pdfium,
        page: usize,
        object_index: usize,
        family: TextFontFamily,
    ) -> Result<usize, DocumentError> {
        let (text, size, fill, mode, left, bottom) = {
            let pdf_page = self.doc.pages().get(page as i32)?;
            let object = pdf_page.objects().get(object_index)?;
            let PdfPageObject::Text(text_obj) = object else {
                return Err(DocumentError("object is not a text object".into()));
            };
            let bounds = text_obj.bounds()?;
            (
                text_obj.text(),
                text_obj.unscaled_font_size(),
                text_obj.fill_color().unwrap_or(PdfColor::BLACK),
                text_obj.render_mode(),
                bounds.left().value,
                bounds.bottom().value,
            )
        };
        let font = self.doc.fonts_mut().new_built_in(family.to_pdfium());
        let mut new_obj = PdfPageTextObject::new(&self.doc, &text, font, size)?;
        let _ = new_obj.set_fill_color(fill);
        let _ = new_obj.set_render_mode(mode);
        let new_bounds = new_obj.bounds()?;
        new_obj.translate(
            PdfPoints::new(left - new_bounds.left().value),
            PdfPoints::new(bottom - new_bounds.bottom().value),
        )?;

        let new_index = {
            let mut pdf_page = self.doc.pages_mut().get(page as i32)?;
            let objects = pdf_page.objects_mut();
            objects.remove_object_at_index(object_index)?;
            objects.add_text_object(new_obj)?;
            let idx = objects.len().saturating_sub(1);
            let _ = objects;
            pdf_page.regenerate_content()?;
            idx
        };
        self.refresh_render_docs(pdfium);
        Ok(new_index)
    }

    /// Permanently delete the page objects at `object_indices` on `page`.
    pub fn delete_text_objects(
        &mut self,
        pdfium: &'static Pdfium,
        page: usize,
        object_indices: &[usize],
    ) -> Result<usize, DocumentError> {
        let mut sorted: Vec<usize> = object_indices.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        sorted.reverse();
        let mut deleted = 0;
        {
            let mut pdf_page = self.doc.pages_mut().get(page as i32)?;
            let objects = pdf_page.objects_mut();
            for idx in sorted {
                if idx < objects.len() && objects.remove_object_at_index(idx).is_ok() {
                    deleted += 1;
                }
            }
            let _ = objects;
            pdf_page.regenerate_content()?;
        }
        self.refresh_render_docs(pdfium);
        Ok(deleted)
    }

    /// Add a new text object on `page` at `(x, y)` PDF points.
    pub fn add_text_object(
        &mut self,
        pdfium: &'static Pdfium,
        page: usize,
        text: &str,
        family: TextFontFamily,
        font_size: f32,
        color: [u8; 4],
        x: f32,
        y: f32,
    ) -> Result<usize, DocumentError> {
        let font = self.doc.fonts_mut().new_built_in(family.to_pdfium());
        let mut obj = PdfPageTextObject::new(
            &self.doc,
            text,
            font,
            PdfPoints::new(font_size.max(4.0)),
        )?;
        obj.set_fill_color(PdfColor::new(color[0], color[1], color[2], color[3]))?;
        let bounds = obj.bounds()?;
        obj.translate(
            PdfPoints::new(x - bounds.left().value),
            PdfPoints::new(y - bounds.bottom().value),
        )?;
        let idx = {
            let mut pdf_page = self.doc.pages_mut().get(page as i32)?;
            let objects = pdf_page.objects_mut();
            objects.add_text_object(obj)?;
            let idx = objects.len().saturating_sub(1);
            let _ = objects;
            pdf_page.regenerate_content()?;
            idx
        };
        self.refresh_render_docs(pdfium);
        Ok(idx)
    }

    /// Decode an image from bytes and place it on `page` at `(l, b)` with size `(w, h)`.
    pub fn add_image_object_from_bytes(
        &mut self,
        pdfium: &'static Pdfium,
        page: usize,
        image_bytes: &[u8],
        l: f32,
        b: f32,
        w: f32,
        h: f32,
    ) -> Result<usize, DocumentError> {
        let img = image::load_from_memory(image_bytes)
            .map_err(|e| DocumentError(format!("could not decode image: {e}")))?;
        let mut obj = PdfPageImageObject::new_with_size(
            &self.doc,
            &img,
            PdfPoints::new(w.max(4.0)),
            PdfPoints::new(h.max(4.0)),
        )?;
        obj.translate(PdfPoints::new(l), PdfPoints::new(b))?;
        let idx = {
            let mut pdf_page = self.doc.pages_mut().get(page as i32)?;
            let objects = pdf_page.objects_mut();
            objects.add_image_object(obj)?;
            let idx = objects.len().saturating_sub(1);
            let _ = objects;
            pdf_page.regenerate_content()?;
            idx
        };
        self.refresh_render_docs(pdfium);
        Ok(idx)
    }

    /// Downsample and JPEG-recompress embedded images to reduce PDF file size.
    pub fn compress(
        &mut self,
        pdfium: &'static Pdfium,
        opts: &CompressionOptions,
    ) -> Result<CompressionStats, DocumentError> {
        let bytes_before = self.doc.save_to_bytes()?.len();
        let mut images_processed = 0usize;
        let max_dpi = opts.max_dpi.max(36.0);
        let quality = opts.jpeg_quality.clamp(10, 95);

        for page_idx in 0..self.page_count {
            let mut replacements: Vec<(usize, PdfMatrix, image::DynamicImage, bool)> = Vec::new();
            {
                let pdf_page = self.doc.pages().get(page_idx as i32)?;
                for (obj_idx, obj) in pdf_page.objects().iter().enumerate() {
                    let Some(img_obj) = obj.as_image_object() else {
                        continue;
                    };
                    let Ok(bounds) = img_obj.bounds() else {
                        continue;
                    };
                    let Ok(matrix) = img_obj.matrix() else {
                        continue;
                    };
                    let Ok(raw_img) = img_obj.get_raw_image() else {
                        continue;
                    };
                    let w_pts = bounds.width().value.abs().max(1.0);
                    let h_pts = bounds.height().value.abs().max(1.0);
                    let target_w = ((w_pts / 72.0) * max_dpi).round().max(16.0) as u32;
                    let target_h = ((h_pts / 72.0) * max_dpi).round().max(16.0) as u32;
                    let (orig_w, orig_h) = (raw_img.width(), raw_img.height());
                    let needs_resize = orig_w > target_w || orig_h > target_h;
                    let resized = if needs_resize {
                        raw_img.resize(target_w, target_h, image::imageops::FilterType::Lanczos3)
                    } else {
                        raw_img
                    };
                    let has_alpha = resized.color().has_alpha()
                        && resized.to_rgba8().pixels().any(|p| p[3] < 255);
                    replacements.push((obj_idx, matrix, resized, has_alpha));
                }
            }

            if replacements.is_empty() {
                continue;
            }

            // Apply in reverse index order so removing/adding doesn't shift earlier indices.
            let mut page_changed = false;
            let mut pdf_page = self.doc.pages_mut().get(page_idx as i32)?;
            for (obj_idx, matrix, img, has_alpha) in replacements.into_iter().rev() {
                if has_alpha {
                    let objects = pdf_page.objects_mut();
                    if let Ok(mut obj) = objects.get(obj_idx)
                        && let Some(img_obj) = obj.as_image_object_mut()
                        && img_obj.set_image(&img).is_ok()
                    {
                        images_processed += 1;
                        page_changed = true;
                    }
                } else {
                    let rgb = img.to_rgb8();
                    let mut jpeg_buf = Vec::new();
                    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(
                        &mut jpeg_buf,
                        quality,
                    );
                    if encoder.encode_image(&rgb).is_ok() {
                        if let Ok(mut new_obj) = PdfPageImageObject::new_from_jpeg_reader(
                            &self.doc,
                            std::io::Cursor::new(&jpeg_buf),
                        ) {
                            if new_obj.apply_matrix(matrix).is_ok() {
                                let objects = pdf_page.objects_mut();
                                if objects.remove_object_at_index(obj_idx).is_ok()
                                    && objects.add_image_object(new_obj).is_ok()
                                {
                                    images_processed += 1;
                                    page_changed = true;
                                }
                            }
                        }
                    }
                }
            }
            if page_changed {
                pdf_page.regenerate_content()?;
            }
        }

        // Compact into a fresh document so unreferenced old image XObjects are
        // garbage-collected rather than kept in the indirect object table.
        if images_processed > 0 && self.page_count > 0 {
            let mut compacted = pdfium.create_new_pdf()?;
            compacted.pages_mut().copy_page_range_from_document(
                &self.doc,
                0..=(self.page_count as i32 - 1),
                0,
            )?;
            let compacted_bytes = compacted.save_to_bytes()?;
            if compacted_bytes.len() < bytes_before {
                // SAFETY: the Pdfium this document borrows is leaked and lives
                // for the whole process.
                self.doc = unsafe { std::mem::transmute(compacted) };
                if !self.title.is_empty()
                    || !self.author.is_empty()
                    || !self.subject.is_empty()
                    || !self.keywords.is_empty()
                {
                    self.metadata_dirty = true;
                }
            }
        }

        self.refresh_render_docs(pdfium);
        let bytes_after = self.doc.save_to_bytes()?.len();
        Ok(CompressionStats {
            images_processed,
            bytes_before,
            bytes_after,
        })
    }
}

/// Presets for PDF image compression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionPreset {
    Screen,
    Balanced,
    High,
}

impl CompressionPreset {
    pub fn label(self) -> &'static str {
        match self {
            CompressionPreset::Screen => "Screen (72 DPI)",
            CompressionPreset::Balanced => "Balanced (150 DPI)",
            CompressionPreset::High => "High (200 DPI)",
        }
    }

    pub fn defaults(self) -> (f32, u8) {
        match self {
            CompressionPreset::Screen => (72.0, 65),
            CompressionPreset::Balanced => (150.0, 75),
            CompressionPreset::High => (200.0, 85),
        }
    }
}

/// Options for PDF compression.
#[derive(Debug, Clone)]
pub struct CompressionOptions {
    pub max_dpi: f32,
    pub jpeg_quality: u8,
}

/// Result statistics from compressing a PDF.
#[derive(Debug, Clone, Default)]
pub struct CompressionStats {
    pub images_processed: usize,
    pub bytes_before: usize,
    pub bytes_after: usize,
}

/// Editable PDF document metadata (`/Info` dictionary fields).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PdfMetadata {
    pub title: String,
    pub author: String,
    pub subject: String,
    pub keywords: String,
}

fn encode_pdf_string(s: &str) -> String {
    if s.is_ascii() {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('(');
        for ch in s.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                '(' => out.push_str("\\("),
                ')' => out.push_str("\\)"),
                '\r' => out.push_str("\\r"),
                '\n' => out.push_str("\\n"),
                _ => out.push(ch),
            }
        }
        out.push(')');
        out
    } else {
        let mut out = String::from("<FEFF");
        for unit in s.encode_utf16() {
            use std::fmt::Write;
            let _ = write!(out, "{unit:04X}");
        }
        out.push('>');
        out
    }
}

fn find_last_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).rposition(|w| w == needle)
}

fn apply_metadata_to_pdf_bytes(
    bytes: &[u8],
    meta: &PdfMetadata,
) -> Result<Vec<u8>, DocumentError> {
    let sx_pos = find_last_subslice(bytes, b"startxref")
        .ok_or_else(|| DocumentError("missing startxref in PDF".into()))?;
    let after_sx = std::str::from_utf8(&bytes[sx_pos + 9..])
        .map_err(|_| DocumentError("invalid UTF-8 after startxref".into()))?;
    let prev_xref: usize = after_sx
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| DocumentError("invalid startxref offset".into()))?;

    let tr_pos = find_last_subslice(&bytes[..sx_pos], b"trailer")
        .ok_or_else(|| DocumentError("missing trailer in PDF".into()))?;
    let trailer_str = String::from_utf8_lossy(&bytes[tr_pos..sx_pos]);

    let after_size = trailer_str
        .split("/Size")
        .nth(1)
        .ok_or_else(|| DocumentError(format!("missing /Size in PDF trailer: {trailer_str}")))?;
    let size_digits: String = after_size
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let size: usize = size_digits
        .parse()
        .map_err(|_| DocumentError(format!("invalid /Size in PDF trailer: {trailer_str}")))?;

    let after_root = trailer_str
        .split("/Root")
        .nth(1)
        .ok_or_else(|| DocumentError("missing /Root in PDF trailer".into()))?;
    let root_tokens: Vec<&str> = after_root.split_whitespace().take(3).collect();
    if root_tokens.len() < 3 || !root_tokens[2].starts_with('R') {
        return Err(DocumentError("missing /Root in PDF trailer".into()));
    }
    let root_ref = format!("{} {} R", root_tokens[0], root_tokens[1]);

    let info_id = size;
    let mut out = bytes.to_vec();
    if !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    let obj_offset = out.len();
    let info_obj = format!(
        "{info_id} 0 obj\n<< /Title {} /Author {} /Subject {} /Keywords {} >>\nendobj\n",
        encode_pdf_string(&meta.title),
        encode_pdf_string(&meta.author),
        encode_pdf_string(&meta.subject),
        encode_pdf_string(&meta.keywords),
    );
    out.extend_from_slice(info_obj.as_bytes());

    let xref_offset = out.len();
    let xref_and_trailer = format!(
        "xref\n0 1\n0000000000 65535 f \n{info_id} 1\n{obj_offset:010} 00000 n \ntrailer\n<< /Size {} /Root {root_ref} /Info {info_id} 0 R /Prev {prev_xref} >>\nstartxref\n{xref_offset}\n%%EOF\n",
        info_id + 1
    );
    out.extend_from_slice(xref_and_trailer.as_bytes());
    Ok(out)
}

/// Standard built-in PDF font families supported for original-text restyling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextFontFamily {
    Helvetica,
    HelveticaBold,
    HelveticaOblique,
    TimesRoman,
    TimesBold,
    TimesItalic,
    Courier,
    CourierBold,
    CourierOblique,
}

impl TextFontFamily {
    pub const ALL: [TextFontFamily; 9] = [
        TextFontFamily::Helvetica,
        TextFontFamily::HelveticaBold,
        TextFontFamily::HelveticaOblique,
        TextFontFamily::TimesRoman,
        TextFontFamily::TimesBold,
        TextFontFamily::TimesItalic,
        TextFontFamily::Courier,
        TextFontFamily::CourierBold,
        TextFontFamily::CourierOblique,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TextFontFamily::Helvetica => "Helvetica",
            TextFontFamily::HelveticaBold => "Helvetica Bold",
            TextFontFamily::HelveticaOblique => "Helvetica Italic",
            TextFontFamily::TimesRoman => "Times",
            TextFontFamily::TimesBold => "Times Bold",
            TextFontFamily::TimesItalic => "Times Italic",
            TextFontFamily::Courier => "Courier",
            TextFontFamily::CourierBold => "Courier Bold",
            TextFontFamily::CourierOblique => "Courier Italic",
        }
    }

    pub fn to_pdfium(self) -> PdfFontBuiltin {
        match self {
            TextFontFamily::Helvetica => PdfFontBuiltin::Helvetica,
            TextFontFamily::HelveticaBold => PdfFontBuiltin::HelveticaBold,
            TextFontFamily::HelveticaOblique => PdfFontBuiltin::HelveticaOblique,
            TextFontFamily::TimesRoman => PdfFontBuiltin::TimesRoman,
            TextFontFamily::TimesBold => PdfFontBuiltin::TimesBold,
            TextFontFamily::TimesItalic => PdfFontBuiltin::TimesItalic,
            TextFontFamily::Courier => PdfFontBuiltin::Courier,
            TextFontFamily::CourierBold => PdfFontBuiltin::CourierBold,
            TextFontFamily::CourierOblique => PdfFontBuiltin::CourierOblique,
        }
    }

    pub fn from_font_name(name: &str) -> Self {
        let lower = name.to_ascii_lowercase();
        let bold = lower.contains("bold");
        let italic = lower.contains("italic") || lower.contains("oblique");
        if lower.contains("courier") || lower.contains("mono") {
            if bold {
                TextFontFamily::CourierBold
            } else if italic {
                TextFontFamily::CourierOblique
            } else {
                TextFontFamily::Courier
            }
        } else if lower.contains("times") || lower.contains("serif") {
            if bold {
                TextFontFamily::TimesBold
            } else if italic {
                TextFontFamily::TimesItalic
            } else {
                TextFontFamily::TimesRoman
            }
        } else if bold {
            TextFontFamily::HelveticaBold
        } else if italic {
            TextFontFamily::HelveticaOblique
        } else {
            TextFontFamily::Helvetica
        }
    }
}

fn bookmark_page(bookmark: &PdfBookmark<'_>) -> Option<usize> {
    if let Some(dest) = bookmark.destination()
        && let Ok(idx) = dest.page_index()
    {
        return Some(idx as usize);
    }
    if let Some(action) = bookmark.action()
        && let Some(local) = action.as_local_destination_action()
        && let Ok(dest) = local.destination()
        && let Ok(idx) = dest.page_index()
    {
        return Some(idx as usize);
    }
    None
}

/// A node in the PDF document's bookmark/outline tree.
#[derive(Debug, Clone, PartialEq)]
pub struct BookmarkNode {
    pub title: String,
    pub page: Option<usize>,
    pub children: Vec<BookmarkNode>,
}

/// The destination of a PDF hyperlink.
#[derive(Debug, Clone, PartialEq)]
pub enum LinkTarget {
    Uri(String),
    Page(usize),
}

/// A hyperlink annotation on a PDF page.
#[derive(Debug, Clone, PartialEq)]
pub struct PageLinkInfo {
    pub page: usize,
    pub index: usize,
    pub l: f32,
    pub b: f32,
    pub r: f32,
    pub t: f32,
    pub target: LinkTarget,
}

/// Anchor position on the page for headers, footers, and page numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StampPosition {
    TopLeft,
    TopCenter,
    TopRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

impl StampPosition {
    pub const ALL: [StampPosition; 6] = [
        StampPosition::TopLeft,
        StampPosition::TopCenter,
        StampPosition::TopRight,
        StampPosition::BottomLeft,
        StampPosition::BottomCenter,
        StampPosition::BottomRight,
    ];

    pub fn label(self) -> &'static str {
        match self {
            StampPosition::TopLeft => "Top Left",
            StampPosition::TopCenter => "Top Center",
            StampPosition::TopRight => "Top Right",
            StampPosition::BottomLeft => "Bottom Left",
            StampPosition::BottomCenter => "Bottom Center",
            StampPosition::BottomRight => "Bottom Right",
        }
    }
}

/// Options for stamping headers, footers, or page numbers.
#[derive(Debug, Clone)]
pub struct HeaderFooterOptions {
    pub position: StampPosition,
    pub text: String,
    pub font_size: f32,
    pub color: [u8; 4],
    pub margin_x: f32,
    pub margin_y: f32,
    pub start_page: usize,
    pub end_page: Option<usize>,
    pub start_number: usize,
}

/// Options for stamping a text watermark.
#[derive(Debug, Clone)]
pub struct WatermarkOptions {
    pub text: String,
    pub font_size: f32,
    pub color: [u8; 4],
    pub angle_deg: f32,
    pub start_page: usize,
    pub end_page: Option<usize>,
}

/// A fillable AcroForm field, flattened into plain data for the UI.
#[derive(Debug, Clone)]
pub struct FormFieldInfo {
    pub page: usize,
    /// The field's annotation index within its page.
    pub index: usize,
    pub name: String,
    pub field_type: FormFieldType,
    pub value: String,
    /// Whether a text field accepts multiple lines (from the `/Ff` Multiline flag).
    pub multiline: bool,
    /// Bounding box in PDF points (page space, origin bottom-left).
    pub l: f32,
    pub b: f32,
    pub r: f32,
    pub t: f32,
}

/// The kind of an AcroForm field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormFieldType {
    Text,
    Checkbox,
    RadioButton,
    ComboBox,
    ListBox,
    Other,
}

impl FormFieldType {
    pub fn label(&self) -> &'static str {
        match self {
            FormFieldType::Text => "Text",
            FormFieldType::Checkbox => "Checkbox",
            FormFieldType::RadioButton => "Radio",
            FormFieldType::ComboBox => "Combo",
            FormFieldType::ListBox => "List",
            FormFieldType::Other => "Other",
        }
    }

    /// Whether the editor can write a value for this kind.
    pub fn editable(&self) -> bool {
        matches!(
            self,
            FormFieldType::Text | FormFieldType::Checkbox | FormFieldType::RadioButton
        )
    }
}

impl From<PdfFormFieldType> for FormFieldType {
    fn from(ft: PdfFormFieldType) -> Self {
        match ft {
            PdfFormFieldType::Text => FormFieldType::Text,
            PdfFormFieldType::Checkbox => FormFieldType::Checkbox,
            PdfFormFieldType::RadioButton => FormFieldType::RadioButton,
            PdfFormFieldType::ComboBox => FormFieldType::ComboBox,
            PdfFormFieldType::ListBox => FormFieldType::ListBox,
            _ => FormFieldType::Other,
        }
    }
}

/// Map a PDFium annotation to the editor's [`AnnotationKind`], if supported.
fn kind_of(annotation: &PdfPageAnnotation<'_>) -> Option<AnnotationKind> {
    match annotation.annotation_type() {
        PdfPageAnnotationType::Highlight => Some(AnnotationKind::Highlight),
        PdfPageAnnotationType::Strikeout => Some(AnnotationKind::Strikeout),
        PdfPageAnnotationType::Underline => Some(AnnotationKind::Underline),
        PdfPageAnnotationType::Squiggly => Some(AnnotationKind::Squiggly),
        PdfPageAnnotationType::Ink => Some(AnnotationKind::Ink),
        PdfPageAnnotationType::Square
            if annotation.contents().as_deref() == Some(REDACTION_CONTENTS_MARKER) =>
        {
            Some(AnnotationKind::Redaction)
        }
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
        PdfPageAnnotationType::Strikeout => AnnotationKind::Strikeout,
        PdfPageAnnotationType::Underline => AnnotationKind::Underline,
        PdfPageAnnotationType::Squiggly => AnnotationKind::Squiggly,
        PdfPageAnnotationType::Ink => AnnotationKind::Ink,
        PdfPageAnnotationType::Square if contents == REDACTION_CONTENTS_MARKER => {
            AnnotationKind::Redaction
        }
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
    if kind != AnnotationKind::Ellipse && kind != AnnotationKind::Redaction {
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

    // Recover per-line quads for text-markup annotations.
    if kind.is_text_markup() {
        model.quads = annotation
            .attachment_points()
            .iter()
            .map(|q| {
                (
                    q.x1().value,
                    q.y1().value,
                    q.x2().value,
                    q.y2().value,
                    q.x3().value,
                    q.y3().value,
                    q.x4().value,
                    q.y4().value,
                )
            })
            .collect();
    }

    Some(model)
}

/// Render a typed signature string into a tightly cropped transparent RGBA
/// image using PDFium's built-in fonts (`TimesBoldItalic`, `HelveticaOblique`, or
/// `CourierOblique`).
pub fn render_typed_signature_rgba(
    pdfium: &'static Pdfium,
    text: &str,
    style_index: usize,
    color: [u8; 4],
) -> Result<image::RgbaImage, DocumentError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(DocumentError("signature text cannot be empty".into()));
    }
    let mut doc = pdfium.create_new_pdf()?;
    let font = match style_index {
        0 => doc.fonts_mut().new_built_in(PdfFontBuiltin::TimesBoldItalic),
        1 => doc.fonts_mut().new_built_in(PdfFontBuiltin::HelveticaOblique),
        _ => doc.fonts_mut().new_built_in(PdfFontBuiltin::CourierOblique),
    };
    let mut page = doc
        .pages_mut()
        .create_page_at_end(PdfPagePaperSize::new_custom(
            PdfPoints::new(800.0),
            PdfPoints::new(200.0),
        ))?;
    {
        let mut obj = PdfPageTextObject::new(
            &doc,
            trimmed,
            font,
            PdfPoints::new(48.0),
        )?;
        obj.set_fill_color(PdfColor::new(color[0], color[1], color[2], 255))?;
        obj.translate(PdfPoints::new(24.0), PdfPoints::new(72.0))?;
        page.objects_mut().add_text_object(obj)?;
    }
    page.regenerate_content()?;

    let config = PdfRenderConfig::new()
        .set_target_width(1600)
        .set_target_height(400);
    let bitmap = page.render_with_config(&config)?;
    let img = bitmap.as_image()?.to_rgba8();
    let (w, h) = img.dimensions();

    let mut min_x = w;
    let mut min_y = h;
    let mut max_x = 0u32;
    let mut max_y = 0u32;
    let mut out = image::RgbaImage::from_pixel(w, h, image::Rgba([0, 0, 0, 0]));
    for y in 0..h {
        for x in 0..w {
            let p = img.get_pixel(x, y);
            let max_c = p[0].max(p[1]).max(p[2]);
            let min_c = p[0].min(p[1]).min(p[2]);
            let whiteness = min_c as f32;
            let ink_strength = (255.0 - whiteness).max((max_c - min_c) as f32);
            if ink_strength > 10.0 {
                let alpha = ink_strength.clamp(0.0, 255.0) as u8;
                out.put_pixel(x, y, image::Rgba([color[0], color[1], color[2], alpha]));
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
    }
    if max_x < min_x || max_y < min_y {
        return Err(DocumentError("could not render typed signature".into()));
    }
    let pad = 12u32;
    let cx = min_x.saturating_sub(pad);
    let cy = min_y.saturating_sub(pad);
    let cw = (max_x + pad + 1).min(w).saturating_sub(cx).max(1);
    let ch = (max_y + pad + 1).min(h).saturating_sub(cy).max(1);
    Ok(image::imageops::crop_imm(&out, cx, cy, cw, ch).to_image())
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
        let pdfium = test_pdfium();
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
        let pdfium = test_pdfium();
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

    /// Write a two-column PDF: four lines on the left, three on the right, with
    /// the columns sharing baselines (the layout that used to fuse into one run).
    fn write_two_column_pdf(label: &str) -> PathBuf {
        let pdfium = test_pdfium();
        let path = unique_pdf_path(label);
        let mut document = pdfium.create_new_pdf().unwrap();
        let font = document.fonts_mut().helvetica();
        let mut page = document
            .pages_mut()
            .create_page_at_end(PdfPagePaperSize::a4())
            .unwrap();

        let left = [
            "Left column, first line of text",
            "Left column, second line here",
            "Left column, third and last",
            "Left column, fourth line also",
        ];
        let right = [
            "Right column, first line of text",
            "Right column, second line here",
            "Right column, third and last",
        ];

        {
            let objects = page.objects_mut();
            for (row, text) in left.iter().enumerate() {
                objects
                    .create_text_object(
                        PdfPoints::new(72.0),
                        PdfPoints::new(720.0 - row as f32 * 14.0),
                        *text,
                        font,
                        PdfPoints::new(11.0),
                    )
                    .unwrap();
            }
            for (row, text) in right.iter().enumerate() {
                objects
                    .create_text_object(
                        PdfPoints::new(320.0),
                        PdfPoints::new(720.0 - row as f32 * 14.0),
                        *text,
                        font,
                        PdfPoints::new(11.0),
                    )
                    .unwrap();
            }
        }

        page.regenerate_content().unwrap();
        document.save_to_file(&path).unwrap();
        path
    }

    /// Two columns sharing baselines must stay separate lines, each backed by one
    /// object — the bug was one fused run spanning the gutter.
    #[test]
    fn two_column_layout_has_one_run_per_line() {
        let path = write_two_column_pdf("two-column-runs");
        let document = OpenDocument::load(test_pdfium(), &path).unwrap();

        let runs = document.text_runs(0).unwrap();
        assert_eq!(runs.len(), 7, "4 left lines + 3 right lines");

        for run in &runs {
            assert!(
                !run.text.contains('\r') && !run.text.contains('\n'),
                "runs must not contain PDFium's synthetic line breaks: {:?}",
                run.text
            );
            assert!(
                run.text.starts_with("Left column") || run.text.starts_with("Right column"),
                "a run must stay inside one column, got {:?}",
                run.text
            );
            let owners = document.text_object_indices_for_chars(0, &run.chars).unwrap();
            assert_eq!(owners.len(), 1, "one object per line, got {owners:?}");
        }

        let _ = std::fs::remove_file(&path);
    }

    /// Dragging a line in the left column must not disturb the right column.
    #[test]
    fn line_drag_stays_within_its_column() {
        let path = write_two_column_pdf("two-column-drag");
        let mut document = OpenDocument::load(test_pdfium(), &path).unwrap();

        let runs = document.text_runs(0).unwrap();
        let run_index = runs
            .iter()
            .position(|run| run.text.starts_with("Left column, second"))
            .expect("left column second line");
        let run = runs[run_index].clone();
        let baseline = run.baseline_y;

        let owners = document.text_object_indices_for_chars(0, &run.chars).unwrap();
        assert_eq!(owners.len(), 1);
        let others: Vec<PdfMatrix> = runs
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != run_index)
            .map(|(_, other)| {
                let index = document
                    .text_object_index_for_char(0, other.first_char_index)
                    .unwrap();
                document.text_object_matrix(0, index).unwrap()
            })
            .collect();

        document.translate_text_objects(0, &owners, 40.0, 0.0).unwrap();

        let after = document.text_runs(0).unwrap();
        let moved = after
            .iter()
            .find(|candidate| candidate.text.starts_with("Left column, second"))
            .expect("the dragged line is still there");
        assert!((moved.baseline_y - baseline).abs() < 1.5);
        assert!((moved.left - (run.left + 40.0)).abs() < 1.5, "the line itself moved");

        for (other, before) in after
            .iter()
            .filter(|candidate| !candidate.text.starts_with("Left column, second"))
            .zip(others.iter())
        {
            let index = document
                .text_object_index_for_char(0, other.first_char_index)
                .unwrap();
            let now = document.text_object_matrix(0, index).unwrap();
            assert_eq!(now, *before, "{:?} should not have moved", other.text);
        }

        let _ = std::fs::remove_file(&path);
    }

    /// A multi-line selection must move every selected line, in both columns.
    #[test]
    fn multi_line_selection_moves_every_selected_line() {
        let path = write_two_column_pdf("two-column-group");
        let mut document = OpenDocument::load(test_pdfium(), &path).unwrap();

        let runs = document.text_runs(0).unwrap();
        let selected: Vec<&crate::text_model::TextRun> = runs
            .iter()
            .filter(|run| {
                run.text.starts_with("Left column, first")
                    || run.text.starts_with("Left column, second")
                    || run.text.starts_with("Right column, first")
            })
            .collect();
        assert_eq!(selected.len(), 3, "two left lines and one right line");

        let groups: Vec<Vec<usize>> = selected.iter().map(|run| run.chars.clone()).collect();
        let splits = document.split_selection_for_move(0, &groups).unwrap();
        assert_eq!(splits.len(), 3, "one span per selected line");

        let indices: Vec<usize> = splits.iter().map(|split| split.span_index).collect();
        document.translate_text_objects(0, &indices, 0.0, -60.0).unwrap();

        let after = document.text_runs(0).unwrap();
        for pressed in ["Left column, first", "Left column, second", "Right column, first"] {
            let moved = after
                .iter()
                .find(|run| run.text.starts_with(pressed))
                .unwrap_or_else(|| panic!("{pressed} should still be there"));
            let before = selected
                .iter()
                .find(|run| run.text.starts_with(pressed))
                .unwrap();
            assert!(
                (moved.baseline_y - (before.baseline_y - 60.0)).abs() < 1.5,
                "{pressed} should have moved with the group"
            );
        }

        // Lines outside the selection keep their baselines.
        for untouched in [
            "Left column, third",
            "Left column, fourth",
            "Right column, second",
            "Right column, third",
        ] {
            let run = after
                .iter()
                .find(|run| run.text.starts_with(untouched))
                .unwrap_or_else(|| panic!("{untouched} should still be there"));
            let before = runs.iter().find(|run| run.text.starts_with(untouched)).unwrap();
            assert!(
                (run.baseline_y - before.baseline_y).abs() < 1.5,
                "{untouched} should not have moved"
            );
        }

        let _ = std::fs::remove_file(&path);
    }

    /// Write a PDF whose lines are drawn as fragments: the first letter of each
    /// line is its own text object, slightly larger and 1.2pt higher, the way a
    /// bulletins-and-forms template lays out a lead-in capital.
    fn write_fragmented_lines_pdf(label: &str) -> PathBuf {
        let pdfium = test_pdfium();
        let path = unique_pdf_path(label);
        let mut document = pdfium.create_new_pdf().unwrap();
        let font = document.fonts_mut().helvetica();
        let mut page = document
            .pages_mut()
            .create_page_at_end(PdfPagePaperSize::a4())
            .unwrap();

        {
            let objects = page.objects_mut();
            for (row, (lead, rest)) in [("A", "nswer the call"), ("T", "ruth and light")]
                .iter()
                .enumerate()
            {
                let baseline = 720.0 - row as f32 * 20.0;
                objects
                    .create_text_object(
                        PdfPoints::new(72.0),
                        PdfPoints::new(baseline + 3.5),
                        *lead,
                        font,
                        PdfPoints::new(14.0),
                    )
                    .unwrap();
                objects
                    .create_text_object(
                        PdfPoints::new(81.0),
                        PdfPoints::new(baseline),
                        *rest,
                        font,
                        PdfPoints::new(11.0),
                    )
                    .unwrap();
            }
        }

        page.regenerate_content().unwrap();
        document.save_to_file(&path).unwrap();
        path
    }

    /// A line drawn as fragments must behave as one line: selected together, and
    /// moved together. Leaving the lead-in behind was the reported bug.
    #[test]
    fn line_fragments_move_with_their_line() {
        let path = write_fragmented_lines_pdf("fragmented-lines");
        let mut document = OpenDocument::load(test_pdfium(), &path).unwrap();

        let runs = document.text_runs(0).unwrap();
        let lines = crate::text_model::visual_lines(&runs);
        assert_eq!(lines.len(), 2, "two lines, each drawn as two fragments");
        for line in &lines {
            assert_eq!(line.runs.len(), 2, "the lead-in joins its line");
        }

        // Dragging the first line must carry both of its fragments.
        let first_line_chars = lines[0].chars(&runs);
        let owners = document
            .text_object_indices_for_chars(0, &first_line_chars)
            .unwrap();
        assert_eq!(owners.len(), 2, "one object per fragment");
        let before: Vec<PdfMatrix> = owners
            .iter()
            .map(|&index| document.text_object_matrix(0, index).unwrap())
            .collect();
        document.translate_text_objects(0, &owners, 25.0, 0.0).unwrap();

        let delta = translation(25.0, 0.0);
        for (&index, &matrix) in owners.iter().zip(before.iter()) {
            assert_eq!(
                document.text_object_matrix(0, index).unwrap(),
                delta.multiply(matrix),
                "fragment object {index} should have moved with its line"
            );
        }

        let _ = std::fs::remove_file(&path);
    }

    /// A multi-line drag must carry every fragment of every selected line.
    #[test]
    fn multi_line_drag_carries_all_fragments() {
        let path = write_fragmented_lines_pdf("fragmented-group");
        let mut document = OpenDocument::load(test_pdfium(), &path).unwrap();

        let runs = document.text_runs(0).unwrap();
        let lines = crate::text_model::visual_lines(&runs);
        assert_eq!(lines.len(), 2);

        let groups: Vec<Vec<usize>> = lines.iter().map(|line| line.chars(&runs)).collect();
        let splits = document.split_selection_for_move(0, &groups).unwrap();
        assert_eq!(splits.len(), 4, "two fragments per line, split separately");

        let indices: Vec<usize> = splits.iter().map(|split| split.span_index).collect();
        let before: Vec<PdfMatrix> = indices
            .iter()
            .map(|&index| document.text_object_matrix(0, index).unwrap())
            .collect();
        document.translate_text_objects(0, &indices, 0.0, -45.0).unwrap();

        let delta = translation(0.0, -45.0);
        for (&index, &matrix) in indices.iter().zip(before.iter()) {
            assert_eq!(
                document.text_object_matrix(0, index).unwrap(),
                delta.multiply(matrix),
                "fragment object {index} should have moved with the group"
            );
        }

        // Both lines still read as two visual lines after the move.
        let after = document.text_runs(0).unwrap();
        assert_eq!(crate::text_model::visual_lines(&after).len(), 2);

        let _ = std::fs::remove_file(&path);
    }

    /// A mangled line is re-authored at its original width, and the change is
    /// exactly reversible.
    #[test]
    fn repair_reauthors_a_mangled_line_and_restores_it() {
        let path = write_two_column_pdf("repair-line");
        let mut document = OpenDocument::load(test_pdfium(), &path).unwrap();

        let runs = document.text_runs(0).unwrap();
        let run = runs
            .iter()
            .find(|run| run.text.starts_with("Left column, second"))
            .expect("line");
        let object = document.text_object_index_for_char(0, run.first_char_index).unwrap();
        let text = document.text_object_text(0, object).unwrap();
        let (left, bottom, right, top) = document.object_bounds(0, object).unwrap();
        let width = right - left;

        // Pretend the move mangled the line: claim it should be wider than it now is.
        let expected = [(object, width * 1.06)];
        let redrawn = document.repair_moved_text(0, &expected).unwrap();
        assert_eq!(redrawn.len(), 1, "a width change must trigger a re-author");
        let line = redrawn[0].clone();

        assert_eq!(
            document.text_object_text(0, line.original_index).unwrap().trim(),
            "",
            "the mangled original is blanked"
        );
        assert_eq!(
            document.text_object_text(0, line.copy_index).unwrap().trim(),
            text.trim(),
            "the copy carries the line's text"
        );
        let (copy_left, copy_bottom, copy_right, copy_top) =
            document.object_bounds(0, line.copy_index).unwrap();
        assert!(
            ((copy_right - copy_left) - expected[0].1).abs() < 1.0,
            "the copy is stretched back to the expected width"
        );
        // Bounds are ink extents, so a re-authored line can differ by a point or
        // two; what must hold is that it stays on the same spot of the page.
        assert!(
            (copy_left - left).abs() < 6.0,
            "it stays in place horizontally: original left {left:.2}, copy left {copy_left:.2}"
        );
        assert!((copy_bottom - bottom).abs() < 0.5, "and vertically");
        assert!((copy_top - top).abs() < 1.5);

        // Undo: the original gets its text back, the copy goes blank.
        document.restore_redrawn_line(&line, true).unwrap();
        assert_eq!(
            document.text_object_text(0, line.original_index).unwrap(),
            text,
            "undo restores the original text exactly"
        );
        assert_eq!(
            document.text_object_text(0, line.copy_index).unwrap().trim(),
            "",
            "undo blanks the copy"
        );

        // Redo puts the copy back.
        document.restore_redrawn_line(&line, false).unwrap();
        assert_eq!(
            document.text_object_text(0, line.copy_index).unwrap().trim(),
            text.trim()
        );
        assert_eq!(
            document.text_object_text(0, line.original_index).unwrap().trim(),
            ""
        );

        let _ = std::fs::remove_file(&path);
    }

    /// Repairing a moved line keeps its text, its place and its width.
    #[test]
    fn repair_keeps_a_moved_line_intact() {
        let path = write_two_column_pdf("repair-clean");
        let mut document = OpenDocument::load(test_pdfium(), &path).unwrap();

        let runs = document.text_runs(0).unwrap();
        let run = runs.first().unwrap().clone();
        let object = document.text_object_index_for_char(0, run.first_char_index).unwrap();
        let text = document.text_object_text(0, object).unwrap();
        let widths = document.object_widths(0, &[object]);

        document.translate_text_objects(0, &[object], 0.0, -20.0).unwrap();
        let redrawn = document.repair_moved_text(0, &widths).unwrap();
        assert_eq!(redrawn.len(), 1, "every moved line is re-authored");

        let line = &redrawn[0];
        assert_eq!(
            document.text_object_text(0, line.copy_index).unwrap().trim(),
            text.trim()
        );
        let (left, bottom, right, _) = document.object_bounds(0, line.copy_index).unwrap();
        assert!(
            ((right - left) - widths[0].1).abs() < 1.0,
            "the copy keeps the line's width"
        );
        assert!((bottom - (run.bottom - 20.0)).abs() < 3.0, "and its new position");

        let _ = std::fs::remove_file(&path);
    }

    /// A selection that stops mid-word must not leave half a word behind.
    #[test]
    fn selection_mid_word_moves_the_whole_word() {
        let pdfium = test_pdfium();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-search.pdf");
        let mut document = OpenDocument::load(pdfium, &source).unwrap();

        let runs = document.text_runs(0).unwrap();
        let run = runs.first().unwrap();
        // The word "PDF": take only its first two characters.
        let word = run
            .words
            .iter()
            .find(|word| word.text == "PDF")
            .expect("PDF word");
        let partial: Vec<usize> = word.chars.iter().copied().take(2).collect();

        let splits = document.split_selection_for_move(0, &[partial]).unwrap();
        assert_eq!(splits.len(), 1);
        assert_eq!(
            splits[0].span_text.trim(),
            "PDF",
            "a partial selection grows to the whole word"
        );

        let _ = pdfium;
    }

    /// Re-authoring a whole block must draw every line exactly once.
    #[test]
    fn repair_a_whole_block_without_duplicating() {
        let path = write_two_column_pdf("repair-block");
        let mut document = OpenDocument::load(test_pdfium(), &path).unwrap();

        let runs = document.text_runs(0).unwrap();
        let lines: Vec<(usize, String)> = runs
            .iter()
            .filter(|run| run.text.starts_with("Left column"))
            .map(|run| {
                let index = document
                    .text_object_index_for_char(0, run.first_char_index)
                    .unwrap();
                (index, run.text.clone())
            })
            .collect();
        assert_eq!(lines.len(), 4, "four left-column lines");

        let indices: Vec<usize> = lines.iter().map(|(index, _)| *index).collect();
        let widths = document.object_widths(0, &indices);
        document.translate_text_objects(0, &indices, 0.0, -30.0).unwrap();
        let redrawn = document.repair_moved_text(0, &widths).unwrap();
        assert_eq!(redrawn.len(), 4, "every moved line is re-authored");

        // Exactly one object carries each line's text — no doubled drawing.
        let count = document.page_object_count(0).unwrap();
        let texts: Vec<String> = (0..count)
            .filter_map(|index| document.text_object_text(0, index).ok())
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
            .collect();
        for (_, text) in &lines {
            let want = text.trim();
            let seen = texts.iter().filter(|candidate| candidate.as_str() == want).count();
            assert_eq!(seen, 1, "{want:?} is drawn {seen} times");
        }

        // Widths survive the round trip.
        for ((index, _), (_, width)) in lines.iter().zip(widths.iter()) {
            let _ = index;
            let copy = redrawn
                .iter()
                .find(|line| line.original_index == *index)
                .map(|line| line.copy_index);
            let copy = copy.expect("copy for line");
            let (left, _, right, _) = document.object_bounds(0, copy).unwrap();
            assert!(
                ((right - left) - width).abs() < 1.0,
                "copy keeps the line's width: {} vs {}",
                right - left,
                width
            );
        }

        let _ = std::fs::remove_file(&path);
    }

    /// The first run on the document with at least `words` words.
    fn first_run_with_words(
        document: &OpenDocument,
        words: usize,
    ) -> (usize, crate::text_model::TextRun) {
        (0..document.page_count)
            .find_map(|page| {
                document.text_runs(page).ok().and_then(|runs| {
                    runs.into_iter()
                        .find(|run| run.words.len() >= words)
                        .map(|run| (page, run))
                })
            })
            .expect("the fixture should contain a line with enough words")
    }

    /// One shared PDFium instance for the whole test module.
    ///
    /// `Pdfium::bind_to_library` succeeds once per process, so creating a bindings
    /// object per test races when tests run in parallel.
    fn test_pdfium() -> &'static Pdfium {
        static PDFIUM: std::sync::OnceLock<Pdfium> = std::sync::OnceLock::new();
        PDFIUM.get_or_init(|| Pdfium::new(crate::load_pdfium_bindings()))
    }

    /// Page-space translation matrix, matching `PdfPageObjectCommon::translate`.
    fn translation(dx: f32, dy: f32) -> PdfMatrix {
        PdfMatrix::new(1.0, 0.0, 0.0, 1.0, dx, dy)
    }

    /// Every run must resolve to the object that really owns its first character,
    /// even when two lines carry identical text.
    #[test]
    fn run_lookup_finds_the_owning_object() {
        let pdfium = test_pdfium();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-search.pdf");
        let document = OpenDocument::load(pdfium, &source).unwrap();

        let mut seen = 0;
        for page in 0..document.page_count {
            for run in document.text_runs(page).unwrap() {
                let object_index = document.text_object_index_for_run(page, &run).unwrap();
                let owners = document
                    .text_object_indices_for_chars(page, &run.chars)
                    .unwrap();
                assert!(
                    owners.contains(&object_index),
                    "run {:?} resolved to object {object_index}, which owns none of its characters (owners: {owners:?})",
                    run.text
                );
                seen += 1;
            }
        }
        assert!(seen > 0, "test-search.pdf should contain text runs");
    }

    /// Splitting a word out of a line must leave the line looking identical: the
    /// prefix and suffix keep their matrices and the three texts rejoin exactly.
    #[test]
    fn splitting_a_word_keeps_the_line_in_place() {
        let pdfium = test_pdfium();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-search.pdf");
        let mut document = OpenDocument::load(pdfium, &source).unwrap();

        let (page, run) = (0..document.page_count)
            .find_map(|page| {
                document.text_runs(page).ok().and_then(|runs| {
                    runs.into_iter()
                        .find(|run| run.words.len() >= 2)
                        .map(|run| (page, run))
                })
            })
            .expect("test-search.pdf should contain a line with two words");
        let word_index = 1;

        let line_object = document.text_object_index_for_run(page, &run).unwrap();
        let line_text = document.text_object_text(page, line_object).unwrap();
        let line_matrix = document.text_object_matrix(page, line_object).unwrap();
        let before_count = document.page_object_count(page).unwrap();

        let word = run.words[word_index].clone();
        let split = document
            .split_text_object_for_span(page, word.first_char_index, word.last_char_index)
            .unwrap();

        assert_eq!(split.span_text, word.text, "the span keeps the word's text");
        assert_eq!(
            document
                .text_object_text(page, split.span_index)
                .unwrap()
                .trim(),
            word.text,
            "the span is its own object"
        );
        assert!(
            split.prefix_index.is_some(),
            "text before the word stays in its own object"
        );
        assert_eq!(
            document.page_object_count(page).unwrap(),
            before_count + 1 + usize::from(split.suffix_index.is_some()),
            "splitting adds one object per side that needs one"
        );

        let prefix = document
            .text_object_text(page, split.prefix_index.unwrap())
            .unwrap();
        let suffix = split
            .suffix_index
            .map(|index| document.text_object_text(page, index).unwrap())
            .unwrap_or_default();
        assert_eq!(
            format!("{prefix}{}{suffix}", split.span_text),
            line_text,
            "the pieces must rejoin into the original line"
        );

        // The span now sits where its first character always was, not at the
        // line origin, so moving it later does not jump it back to the margin.
        let span_text = split.span_text.clone();
        let span_matrix = document.text_object_matrix(page, split.span_index).unwrap();
        assert!(
            (span_matrix.e() - line_matrix.e()).abs() > 0.5
                || (span_matrix.f() - line_matrix.f()).abs() > 0.5,
            "a second word is not at the origin of its line, so the split must offset it"
        );

        // No debris at the source: every object still on the page has glyphs.
        for index in 0..document.page_object_count(page).unwrap() {
            if let Ok(text) = document.text_object_text(page, index) {
                assert!(
                    !text.trim().is_empty(),
                    "object {index} was left holding only whitespace: {text:?}"
                );
            }
        }

        // Moving the span leaves its neighbours exactly where they were.
        let prefix_before = document
            .text_object_matrix(page, split.prefix_index.unwrap())
            .unwrap();
        let span_before = document.text_object_matrix(page, split.span_index).unwrap();
        document
            .translate_text_objects(page, &[split.span_index], 30.0, -20.0)
            .unwrap();
        assert_eq!(
            document.text_object_matrix(page, split.span_index).unwrap(),
            translation(30.0, -20.0).multiply(span_before),
            "the span moves by the requested offset"
        );
        assert_eq!(
            document
                .text_object_matrix(page, split.prefix_index.unwrap())
                .unwrap(),
            prefix_before,
            "the text before the word must not move"
        );
        assert_eq!(
            document.text_object_text(page, split.span_index).unwrap(),
            span_text,
            "moving does not re-encode the span"
        );
    }

    /// Splitting must not move or restyle anything: same character origins, same
    /// pixels. This is the guarantee that lets a word drag start with a split
    /// without the line appearing to jump.
    #[test]
    fn splitting_a_word_changes_nothing_on_screen() {
        let pdfium = test_pdfium();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-search.pdf");
        let mut document = OpenDocument::load(pdfium, &source).unwrap();

        let (page, run) = (0..document.page_count)
            .find_map(|page| {
                document.text_runs(page).ok().and_then(|runs| {
                    runs.into_iter()
                        .find(|run| run.words.len() >= 2)
                        .map(|run| (page, run))
                })
            })
            .expect("test-search.pdf should contain a line with two words");

        let origins_before: Vec<(usize, (f32, f32))> = {
            let pdf_page = document.doc.pages().get(page as i32).unwrap();
            let text_page = pdf_page.text().unwrap();
            text_page
                .chars()
                .iter()
                .map(|ch| {
                    let (x, y) = ch.origin().unwrap();
                    (ch.index(), (x.value, y.value))
                })
                .collect()
        };
        let pixels_before = crate::engine::render_page(&document.doc, page, 1.0, false);

        let word = run.words[1].clone();
        document
            .split_text_object_for_span(page, word.first_char_index, word.last_char_index)
            .unwrap();

        let origins_after: Vec<(usize, (f32, f32))> = {
            let pdf_page = document.doc.pages().get(page as i32).unwrap();
            let text_page = pdf_page.text().unwrap();
            text_page
                .chars()
                .iter()
                .map(|ch| {
                    let (x, y) = ch.origin().unwrap();
                    (ch.index(), (x.value, y.value))
                })
                .collect()
        };

        assert_eq!(
            origins_before.len(),
            origins_after.len(),
            "splitting must not add or drop characters"
        );
        for ((before_index, before), (after_index, after)) in
            origins_before.iter().zip(origins_after.iter())
        {
            assert_eq!(before_index, after_index);
            assert!(
                (before.0 - after.0).abs() < 0.5 && (before.1 - after.1).abs() < 0.5,
                "character {before_index} moved from {before:?} to {after:?}"
            );
        }

        let pixels_after = crate::engine::render_page(&document.doc, page, 1.0, false);
        assert_eq!(pixels_before.size, pixels_after.size);
        let differing = pixels_before
            .pixels
            .iter()
            .zip(pixels_after.pixels.iter())
            .filter(|(before, after)| {
                let delta = (before.r() as i32 - after.r() as i32).abs()
                    + (before.g() as i32 - after.g() as i32).abs()
                    + (before.b() as i32 - after.b() as i32).abs();
                delta > 30
            })
            .count();
        let total = pixels_before.pixels.len().max(1);
        assert!(
            differing * 500 < total,
            "splitting should not be visible: {differing} of {total} pixels changed"
        );
    }

    /// A multi-word phrase must split into one piece that moves as a whole.
    #[test]
    fn multi_word_span_moves_as_one_piece() {
        let pdfium = test_pdfium();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-search.pdf");
        let mut document = OpenDocument::load(pdfium, &source).unwrap();

        let (page, run) = first_run_with_words(&document, 3);
        let phrase = format!("{} {}", run.words[1].text, run.words[2].text);
        let phrase_left = run.words[1].left;
        let baseline = run.baseline_y;

        let phrase_chars: Vec<usize> = run.words[1..=2]
            .iter()
            .flat_map(|word| word.chars.iter().copied())
            .collect();
        let splits = document
            .split_selection_for_move(page, &[phrase_chars])
            .unwrap();
        assert_eq!(splits.len(), 1, "a contiguous phrase is one span");
        assert_eq!(splits[0].span_text, phrase);

        document
            .translate_text_objects(page, &[splits[0].span_index], 50.0, 25.0)
            .unwrap();

        let runs = document.text_runs(page).unwrap();
        let moved = runs
            .iter()
            .find(|candidate| candidate.text.contains(&phrase))
            .expect("the phrase should still be on the page");
        assert!(
            (moved.left - (phrase_left + 50.0)).abs() < 1.5,
            "phrase should move as one piece: expected x≈{}, found {}",
            phrase_left + 50.0,
            moved.left
        );
        assert!((moved.baseline_y - (baseline + 25.0)).abs() < 1.5);

        // The words left behind must not have moved.
        let hello = runs
            .iter()
            .find(|candidate| candidate.text.trim_start().starts_with("Hello"))
            .expect("the rest of the line should still be there");
        assert!((hello.baseline_y - baseline).abs() < 1.5);
    }

    /// A selection that crosses text-object boundaries is split per object and
    /// still moves as one group.
    #[test]
    fn selection_across_objects_is_split_per_object() {
        let pdfium = test_pdfium();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-search.pdf");
        let mut document = OpenDocument::load(pdfium, &source).unwrap();

        let (page, run) = first_run_with_words(&document, 3);
        let baseline = run.baseline_y;

        // Split one word out first, so the line now lives in three objects.
        document
            .split_text_object_for_span(
                page,
                run.words[1].first_char_index,
                run.words[1].last_char_index,
            )
            .unwrap();

        // Select from the first word to the third: that range straddles objects.
        let selection: Vec<usize> = run.words[0..=2]
            .iter()
            .flat_map(|word| word.chars.iter().copied())
            .collect();
        let splits = document
            .split_selection_for_move(page, &[selection])
            .unwrap();
        assert!(
            splits.len() >= 2,
            "a range crossing objects should need one split per object, got {}",
            splits.len()
        );

        let indices: Vec<usize> = splits.iter().map(|split| split.span_index).collect();
        document.translate_text_objects(page, &indices, 0.0, -40.0).unwrap();

        let runs = document.text_runs(page).unwrap();
        // The three words now share a baseline, so they group into one run.
        let moved = runs
            .iter()
            .find(|candidate| {
                ["Hello", "PDF", "Viewer"]
                    .iter()
                    .all(|word| candidate.text.contains(word))
            })
            .expect("the selected words should still be on the page");
        assert!(
            (moved.baseline_y - (baseline - 40.0)).abs() < 1.5,
            "the group should have moved, baseline {} vs {}",
            moved.baseline_y,
            baseline - 40.0
        );

        // Text outside the selection stays exactly where it was.
        let rest = runs
            .iter()
            .find(|candidate| candidate.text.contains("Viewer - Page"))
            .or_else(|| runs.iter().find(|candidate| candidate.text.contains("Page")))
            .expect("the unselected part of the line should still be there");
        assert!((rest.baseline_y - baseline).abs() < 1.5);
    }

    /// A moved word must keep its new position after saving and reloading.
    #[test]
    fn moved_word_survives_save_and_reload() {
        let pdfium = test_pdfium();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-search.pdf");
        let out = unique_pdf_path("word-move");
        let mut document = OpenDocument::load(pdfium, &source).unwrap();
        document.path = out.clone();

        let (page, run) = (0..document.page_count)
            .find_map(|page| {
                document.text_runs(page).ok().and_then(|runs| {
                    runs.into_iter()
                        .find(|run| run.words.len() >= 2)
                        .map(|run| (page, run))
                })
            })
            .expect("test-search.pdf should contain a line with two words");

        let word = run.words[1].clone();
        let (word_left, word_baseline) = (word.left, run.baseline_y);

        document
            .split_text_object_for_span(page, word.first_char_index, word.last_char_index)
            .unwrap();
        let span_index = document
            .text_object_index_for_char(page, word.first_char_index)
            .unwrap();
        // Move it somewhere obviously different: up and to the right.
        document
            .translate_text_objects(page, &[span_index], 40.0, 30.0)
            .unwrap();
        document.save().unwrap();

        let reloaded = OpenDocument::load(pdfium, &out).unwrap();
        let moved = reloaded
            .text_runs(page)
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.text.trim() == word.text)
            .expect("the moved word should still be on the page");

        assert!(
            (moved.left - (word_left + 40.0)).abs() < 1.5,
            "expected the word at x≈{}, found {}",
            word_left + 40.0,
            moved.left
        );
        assert!(
            (moved.baseline_y - (word_baseline + 30.0)).abs() < 1.5,
            "expected the word at y≈{}, found {}",
            word_baseline + 30.0,
            moved.baseline_y
        );

        let _ = std::fs::remove_file(&out);
    }

    /// A line that lives in several text objects must move as one piece.
    #[test]
    fn line_move_covers_every_object_of_the_line() {
        let pdfium = test_pdfium();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-search.pdf");
        let mut document = OpenDocument::load(pdfium, &source).unwrap();

        let (page, run) = (0..document.page_count)
            .find_map(|page| {
                document
                    .text_runs(page)
                    .ok()
                    .and_then(|runs| runs.into_iter().next().map(|run| (page, run)))
            })
            .expect("test-search.pdf should contain text");

        let indices = document
            .text_object_indices_for_chars(page, &run.chars)
            .unwrap();
        assert!(!indices.is_empty(), "a line must map to at least one object");

        let before: Vec<PdfMatrix> = indices
            .iter()
            .map(|&index| document.text_object_matrix(page, index).unwrap())
            .collect();
        document.translate_text_objects(page, &indices, 12.0, 7.0).unwrap();

        let delta = translation(12.0, 7.0);
        for (&index, &matrix) in indices.iter().zip(before.iter()) {
            assert_eq!(
                document.text_object_matrix(page, index).unwrap(),
                delta.multiply(matrix),
                "object {index} of the line should have moved with the rest"
            );
        }
    }

    #[test]
    fn text_move_translates_and_restores() {
        let pdfium = test_pdfium();
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
        let pdfium = test_pdfium();
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
        let pdfium = test_pdfium();
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
        let pdfium = test_pdfium();
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
        let pdfium = test_pdfium();
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
        let pdfium = test_pdfium();
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

    #[cfg(any(feature = "ocr", target_os = "windows"))]
    #[test]
    fn invisible_text_layer_produces_word_spacing() {
        let pdfium = test_pdfium();
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

    #[test]
    fn page_organizer_reorder_duplicate_merge_extract_and_crop() {
        let pdfium = test_pdfium();
        let path_a = unique_pdf_path("org-a");
        let path_b = unique_pdf_path("org-b");
        let path_extracted = unique_pdf_path("org-extracted");

        // Create PDF A with 2 distinct pages ("Page A1", "Page A2").
        {
            let mut doc_a = pdfium.create_new_pdf().unwrap();
            let font = doc_a.fonts_mut().helvetica();
            for label in ["Page A1", "Page A2"] {
                let mut page = doc_a
                    .pages_mut()
                    .create_page_at_end(PdfPagePaperSize::a4())
                    .unwrap();
                page.objects_mut()
                    .create_text_object(
                        PdfPoints::new(72.0),
                        PdfPoints::new(720.0),
                        label,
                        font,
                        PdfPoints::new(14.0),
                    )
                    .unwrap();
                page.regenerate_content().unwrap();
            }
            doc_a.save_to_file(&path_a).unwrap();
        }

        // Create PDF B with 1 page ("Page B1").
        {
            let mut doc_b = pdfium.create_new_pdf().unwrap();
            let font = doc_b.fonts_mut().helvetica();
            let mut page = doc_b
                .pages_mut()
                .create_page_at_end(PdfPagePaperSize::a4())
                .unwrap();
            page.objects_mut()
                .create_text_object(
                    PdfPoints::new(72.0),
                    PdfPoints::new(720.0),
                    "Page B1",
                    font,
                    PdfPoints::new(14.0),
                )
                .unwrap();
            page.regenerate_content().unwrap();
            doc_b.save_to_file(&path_b).unwrap();
        }

        let mut doc = OpenDocument::load(pdfium, &path_a).unwrap();
        assert_eq!(doc.page_count, 2);

        // 1. Reorder: move page 0 (A1) to index 1 -> [A2, A1]
        doc.reorder_page(pdfium, 0, 1).unwrap();
        assert_eq!(doc.page_count, 2);
        assert!(doc.text_runs(0).unwrap()[0].text.contains("Page A2"));
        assert!(doc.text_runs(1).unwrap()[0].text.contains("Page A1"));

        // 2. Duplicate page 0 (A2) -> [A2, A2, A1]
        let dup_idx = doc.duplicate_page(pdfium, 0).unwrap();
        assert_eq!(dup_idx, 1);
        assert_eq!(doc.page_count, 3);
        assert!(doc.text_runs(1).unwrap()[0].text.contains("Page A2"));

        // 3. Merge PDF B at end -> [A2, A2, A1, B1]
        let added = doc.merge_pdf(pdfium, &path_b, doc.page_count).unwrap();
        assert_eq!(added, 1);
        assert_eq!(doc.page_count, 4);
        assert!(doc.text_runs(3).unwrap()[0].text.contains("Page B1"));

        // 4. Extract pages [2, 3] (A1, B1) into a new PDF
        doc.extract_pages_to(pdfium, &[2, 3], &path_extracted).unwrap();
        let extracted = OpenDocument::load(pdfium, &path_extracted).unwrap();
        assert_eq!(extracted.page_count, 2);
        assert!(extracted.text_runs(0).unwrap()[0].text.contains("Page A1"));
        assert!(extracted.text_runs(1).unwrap()[0].text.contains("Page B1"));

        // 5. Crop page 0 margins by 36 pt on each side, then reset
        let orig_w = doc.pages[0].width_pts;
        let orig_h = doc.pages[0].height_pts;
        doc.crop_page_margins(0, 36.0, 36.0, 36.0, 36.0).unwrap();
        assert!((doc.pages[0].width_pts - (orig_w - 72.0)).abs() < 1.0);
        assert!((doc.pages[0].height_pts - (orig_h - 72.0)).abs() < 1.0);
        doc.reset_page_crop(0).unwrap();
        assert!((doc.pages[0].width_pts - orig_w).abs() < 1.0);
        assert!((doc.pages[0].height_pts - orig_h).abs() < 1.0);

        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
        let _ = std::fs::remove_file(&path_extracted);
    }

    #[test]
    fn permanent_redaction_destroys_text_and_burns_box() {
        let pdfium = test_pdfium();
        let path = unique_pdf_path("redact");

        {
            let mut doc = pdfium.create_new_pdf().unwrap();
            let font = doc.fonts_mut().helvetica();
            let mut page = doc
                .pages_mut()
                .create_page_at_end(PdfPagePaperSize::a4())
                .unwrap();
            page.objects_mut()
                .create_text_object(
                    PdfPoints::new(72.0),
                    PdfPoints::new(720.0),
                    "Public Secret Word",
                    font,
                    PdfPoints::new(14.0),
                )
                .unwrap();
            page.regenerate_content().unwrap();
            doc.save_to_file(&path).unwrap();
        }

        let mut doc = OpenDocument::load(pdfium, &path).unwrap();
        let runs = doc.text_runs(0).unwrap();
        let secret_word = runs[0]
            .words
            .iter()
            .find(|w| w.text == "Secret")
            .unwrap()
            .clone();

        let mut redaction = Annotation::new(0, AnnotationKind::Redaction);
        redaction.rect = PdfRect::new_from_values(
            secret_word.bottom - 2.0,
            secret_word.left - 1.0,
            secret_word.top + 2.0,
            secret_word.right + 1.0,
        );
        doc.add_annotation(0, redaction).unwrap();
        assert_eq!(doc.annotations[0].len(), 1);

        let count = doc.apply_redactions(pdfium, 0).unwrap();
        assert_eq!(count, 1);
        assert!(doc.annotations[0].is_empty(), "redaction mark should be removed");
        doc.save().unwrap();

        let reloaded = OpenDocument::load(pdfium, &path).unwrap();
        let text = reloaded
            .text_runs(0)
            .unwrap()
            .into_iter()
            .map(|r| r.text)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!text.contains("Secret"), "redacted word must be gone: {text:?}");
        assert!(text.contains("Public"), "non-redacted prefix should remain: {text:?}");
        assert!(text.contains("Word"), "non-redacted suffix should remain: {text:?}");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn flatten_page_turns_annotations_into_static_content() {
        let pdfium = test_pdfium();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("test.pdf");
        let out = unique_pdf_path("flatten");

        let mut doc = OpenDocument::load(pdfium, &source).unwrap();
        let mut rect = Annotation::new(0, AnnotationKind::Rectangle);
        rect.rect = PdfRect::new_from_values(100.0, 100.0, 200.0, 220.0);
        rect.style = StrokeStyle {
            fill: None,
            stroke: Some([220, 40, 40, 255]),
            width: 2.0,
        };
        doc.add_annotation(0, rect).unwrap();
        assert!(!doc.annotations[0].is_empty());

        doc.flatten_page(pdfium, 0).unwrap();
        assert!(doc.annotations[0].is_empty());
        doc.save_as(&out).unwrap();

        let reloaded = OpenDocument::load(pdfium, &out).unwrap();
        assert!(reloaded.annotations[0].is_empty());

        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn typed_signature_renders_non_empty_rgba() {
        let pdfium = test_pdfium();
        let rgba = render_typed_signature_rgba(pdfium, "Jane Doe", 0, [15, 30, 120, 255]).unwrap();
        assert!(rgba.width() > 20 && rgba.height() > 10);
        assert!(rgba.pixels().any(|p| p[3] > 128));
    }

    #[test]
    fn header_footer_watermark_and_links_round_trip() {
        let pdfium = test_pdfium();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("test.pdf");
        let out = unique_pdf_path("hfwl");
        let _ = std::fs::remove_file(&out);

        let mut doc = OpenDocument::load(pdfium, &src).unwrap();
        let total = doc.page_count;

        let hf_count = doc
            .apply_header_footer(
                pdfium,
                &HeaderFooterOptions {
                    position: StampPosition::BottomCenter,
                    text: "Page {page} of {total}".into(),
                    font_size: 10.0,
                    color: [30, 30, 30, 255],
                    margin_x: 36.0,
                    margin_y: 24.0,
                    start_page: 0,
                    end_page: None,
                    start_number: 1,
                },
            )
            .unwrap();
        assert_eq!(hf_count, total);

        let wm_count = doc
            .apply_watermark(
                pdfium,
                &WatermarkOptions {
                    text: "CONFIDENTIAL".into(),
                    font_size: 48.0,
                    color: [200, 40, 40, 90],
                    angle_deg: 45.0,
                    start_page: 0,
                    end_page: None,
                },
            )
            .unwrap();
        assert_eq!(wm_count, total);

        doc.add_link_annotation(
            pdfium,
            0,
            50.0,
            50.0,
            200.0,
            80.0,
            &LinkTarget::Uri("https://example.com".into()),
        )
        .unwrap();

        doc.save_as(&out).unwrap();
        let reloaded = OpenDocument::load(pdfium, &out).unwrap();
        let text0 = reloaded.doc.pages().get(0).unwrap().text().unwrap().all();
        assert!(text0.contains(&format!("Page 1 of {total}")));
        assert!(text0.contains("CONFIDENTIAL"));

        let links = reloaded.page_links(0).unwrap();
        assert!(links.iter().any(|l| l.target == LinkTarget::Uri("https://example.com".into())));

        // Ensure bookmarks() runs cleanly even on a fixture without outlines.
        let _bms = reloaded.bookmarks();

        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn richer_text_and_object_tools_round_trip() {
        let pdfium = test_pdfium();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("test.pdf");
        let out = unique_pdf_path("richer_text");

        let mut doc = OpenDocument::load(pdfium, &src).unwrap();
        let idx = doc
            .add_text_object(
                pdfium,
                0,
                "Custom Styled Line",
                TextFontFamily::Helvetica,
                16.0,
                [10, 20, 30, 255],
                72.0,
                600.0,
            )
            .unwrap();

        doc.set_text_object_fill_color(pdfium, 0, idx, [200, 30, 30, 255])
            .unwrap();
        assert_eq!(
            doc.text_object_fill_color(0, idx).unwrap(),
            [200, 30, 30, 255]
        );

        let new_idx = doc
            .set_text_object_font_family(pdfium, 0, idx, TextFontFamily::TimesBold)
            .unwrap();
        assert_eq!(
            doc.text_object_font_family(0, new_idx).unwrap(),
            TextFontFamily::TimesBold
        );

        // Copy text to another position / delete original.
        let copy_idx = doc
            .add_text_object(
                pdfium,
                0,
                "Copied Line",
                TextFontFamily::CourierBold,
                14.0,
                [0, 100, 200, 255],
                72.0,
                550.0,
            )
            .unwrap();
        let deleted = doc.delete_text_objects(pdfium, 0, &[new_idx]).unwrap();
        assert_eq!(deleted, 1);

        doc.save_as(&out).unwrap();
        let reloaded = OpenDocument::load(pdfium, &out).unwrap();
        let text = reloaded.doc.pages().get(0).unwrap().text().unwrap().all();
        assert!(text.contains("Copied Line"));
        assert!(!text.contains("Custom Styled Line"));
        let _ = copy_idx;
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn metadata_edit_round_trips_through_save() {
        let pdfium = test_pdfium();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("test.pdf");
        let out = unique_pdf_path("metadata");

        let mut doc = OpenDocument::load(pdfium, &src).unwrap();
        doc.set_metadata(PdfMetadata {
            title: "Quarterly Report — Q4".into(),
            author: "Dan Edwards".into(),
            subject: "Financial Summary".into(),
            keywords: "rust, pdf, q4".into(),
        });
        doc.save_as(&out).unwrap();

        let reloaded = OpenDocument::load(pdfium, &out).unwrap();
        let meta = reloaded.metadata();
        assert_eq!(meta.title, "Quarterly Report — Q4");
        assert_eq!(meta.author, "Dan Edwards");
        assert_eq!(meta.subject, "Financial Summary");
        assert_eq!(meta.keywords, "rust, pdf, q4");

        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn compress_reduces_large_image_pdf_size() {
        let pdfium = test_pdfium();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("test.pdf");
        let out = unique_pdf_path("compress");

        // Create a noisy 1200x900 image so uncompressed bitmap size is ~3 MB.
        let mut img = image::RgbImage::new(1200, 900);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgb([
                ((x * 7 + y * 3) % 256) as u8,
                ((x * 13 + y * 5) % 256) as u8,
                ((x * 3 + y * 11) % 256) as u8,
            ]);
        }
        let mut png_bytes = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut png_bytes), image::ImageFormat::Png)
            .unwrap();

        let mut doc = OpenDocument::load(pdfium, &src).unwrap();
        doc.add_image_object_from_bytes(pdfium, 0, &png_bytes, 50.0, 50.0, 200.0, 150.0)
            .unwrap();

        let stats = doc
            .compress(
                pdfium,
                &CompressionOptions {
                    max_dpi: 150.0,
                    jpeg_quality: 75,
                },
            )
            .unwrap();
        assert!(stats.images_processed >= 1);
        assert!(
            stats.bytes_after < stats.bytes_before,
            "before={} after={} images={}",
            stats.bytes_before,
            stats.bytes_after,
            stats.images_processed
        );

        doc.save_as(&out).unwrap();
        let _reloaded = OpenDocument::load(pdfium, &out).unwrap();
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn password_encryption_round_trips_and_removes() {
        let pdfium = test_pdfium();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("test.pdf");
        let enc_path = unique_pdf_path("encrypted");
        let dec_path = unique_pdf_path("decrypted");

        let mut doc = OpenDocument::load(pdfium, &src).unwrap();
        doc.set_encryption(Some(crate::security::EncryptionOptions {
            user_password: "secret123".into(),
            owner_password: "owner456".into(),
            ..Default::default()
        }));
        assert!(doc.is_password_protected());
        doc.save_as(&enc_path).unwrap();

        // Opening without password or with wrong password must fail.
        assert!(OpenDocument::load(pdfium, &enc_path).is_err());
        assert!(OpenDocument::load_with_password(pdfium, &enc_path, Some("wrong")).is_err());

        // Opening with user password or owner password must succeed.
        let mut reloaded = OpenDocument::load_with_password(pdfium, &enc_path, Some("secret123")).unwrap();
        assert!(reloaded.is_encrypted);
        assert!(reloaded.is_password_protected());
        let _owner_open = OpenDocument::load_with_password(pdfium, &enc_path, Some("owner456")).unwrap();

        // Removing encryption and saving must produce an unprotected PDF.
        reloaded.strip_encryption(pdfium).unwrap();
        assert!(!reloaded.is_password_protected());
        reloaded.save_as(&dec_path).unwrap();

        let plain = OpenDocument::load(pdfium, &dec_path).unwrap();
        assert!(!plain.is_encrypted);
        assert!(!plain.is_password_protected());

        let _ = std::fs::remove_file(&enc_path);
        let _ = std::fs::remove_file(&dec_path);
    }

}
