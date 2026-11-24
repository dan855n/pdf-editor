//! Build a two-column, multi-line PDF for exercising text selection/dragging.
//!
//! cargo run --example make_layout_pdf -- /tmp/layout.pdf
use std::path::Path;

use pdfium_render::prelude::*;

const LEFT_X: f32 = 72.0;
const RIGHT_X: f32 = 320.0;
const TOP_Y: f32 = 720.0;
const LINE_STEP: f32 = 14.0;

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/layout.pdf".into());

    let pdfium: &'static Pdfium =
        Box::leak(Box::new(Pdfium::new(pdf_viewer::load_pdfium_bindings())));
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
                PdfPoints::new(LEFT_X),
                PdfPoints::new(TOP_Y - row as f32 * LINE_STEP),
                *text,
                font,
                PdfPoints::new(11.0),
            )
            .unwrap();
    }
        for (row, text) in right.iter().enumerate() {
            objects
                .create_text_object(
                    PdfPoints::new(RIGHT_X),
                    PdfPoints::new(TOP_Y - row as f32 * LINE_STEP),
                    *text,
                    font,
                    PdfPoints::new(11.0),
                )
                .unwrap();
        }
    }

    page.regenerate_content().unwrap();
    document.save_to_file(Path::new(&out)).unwrap();
    println!("wrote {out}");
}
