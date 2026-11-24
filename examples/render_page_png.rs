//! Render a page with PDFium (the engine the app uses) to a PNG.
//!
//! cargo run --example render_page_png -- in.pdf out.png [page] [scale]
use std::path::Path;

use pdfium_render::prelude::*;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let input = args.get(1).cloned().expect("input pdf");
    let output = args.get(2).cloned().expect("output png");
    let page: usize = args.get(3).and_then(|n| n.parse().ok()).unwrap_or(0);
    let scale: f32 = args.get(4).and_then(|n| n.parse().ok()).unwrap_or(1.4);

    let pdfium: &'static Pdfium =
        Box::leak(Box::new(Pdfium::new(pdf_viewer::load_pdfium_bindings())));
    let document = pdfium.load_pdf_from_file(Path::new(&input), None).unwrap();
    let image = pdf_viewer::render_page(&document, page, scale, false);

    let width = image.size[0] as u32;
    let height = image.size[1] as u32;
    let mut bytes = Vec::with_capacity((width * height * 4) as usize);
    for pixel in &image.pixels {
        bytes.extend_from_slice(&[pixel.r(), pixel.g(), pixel.b(), 255]);
    }
    image::save_buffer(
        Path::new(&output),
        &bytes,
        width,
        height,
        image::ColorType::Rgba8,
    )
    .unwrap();
    println!("wrote {output} ({width}x{height})");
}
