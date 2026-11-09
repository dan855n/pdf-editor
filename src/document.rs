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

/// Options controlling how a document is written back to disk.
#[derive(Debug, Clone, Copy, Default)]
pub struct SaveOptions {
    /// Rewrite the whole file rather than incrementally appending changes.
    pub full_rewrite: bool,
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
        };

        // Metadata reads need a fully-built struct, so fill it in now.
        open.title = meta(&open.doc, PdfDocumentMetadataTagType::Title);
        open.author = meta(&open.doc, PdfDocumentMetadataTagType::Author);
        open.producer = meta(&open.doc, PdfDocumentMetadataTagType::Producer);

        open.read_annotations();
        Ok(open)
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
    fn remove_annotation_from_pdf(
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

    /// Save the document to disk.
    pub fn save(&mut self, options: SaveOptions) -> Result<(), DocumentError> {
        // Flush pending page content before writing.
        for i in 0..self.doc.pages().len() {
            if let Ok(mut page) = self.doc.pages_mut().get(i) {
                let _ = page.regenerate_content();
            }
        }
        self.doc.save_to_file(&self.path)?;
        let _ = options;
        Ok(())
    }

    /// Save to a new path (Save As).
    pub fn save_as(&mut self, path: &Path) -> Result<(), DocumentError> {
        for i in 0..self.doc.pages().len() {
            if let Ok(mut page) = self.doc.pages_mut().get(i) {
                let _ = page.regenerate_content();
            }
        }
        self.doc.save_to_file(path)?;
        Ok(())
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
}
