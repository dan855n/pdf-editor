//! Verify text-markup annotations (strikeout etc.) with per-line quads.

use pdf_viewer::annotations::{Annotation, AnnotationKind, StrokeStyle};
use pdfium_render::prelude::{PdfRect, Pdfium};

fn main() {
    let pdfium: &'static Pdfium =
        Box::leak(Box::new(Pdfium::new(pdf_viewer::load_pdfium_bindings())));
    let mut doc =
        pdf_viewer::OpenDocument::load(pdfium, std::path::Path::new("/tmp/test-form.pdf")).unwrap();

    // Strikeout over "Please fill this form" (roughly 72,716 .. 260,726).
    let mut a = Annotation::new(0, AnnotationKind::Strikeout);
    a.rect = PdfRect::new_from_values(716.0, 72.0, 726.0, 260.0);
    a.quads = vec![(72.0, 716.0, 260.0, 716.0, 260.0, 726.0, 72.0, 726.0)];
    a.text = "Please fill this form".into();
    a.style = StrokeStyle {
        stroke: Some([0, 0, 0, 255]),
        fill: None,
        width: 1.0,
    };
    doc.add_annotation(0, a).unwrap();

    doc.save_as(std::path::Path::new("/tmp/test-markup.pdf")).unwrap();

    // Reload and re-read.
    let reopened =
        pdf_viewer::OpenDocument::load(pdfium, std::path::Path::new("/tmp/test-markup.pdf")).unwrap();
    for (page, list) in reopened.annotations.iter().enumerate() {
        for a in list {
            println!("page {page}: {:?} quads={} text={:?}", a.kind, a.quads.len(), a.text);
        }
    }
}
