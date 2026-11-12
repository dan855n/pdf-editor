//! Debug helper: list image objects on a PDF page with their bounds.
use pdf_viewer::OpenDocument;
use pdfium_render::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let path = std::path::PathBuf::from(&args[1]);
    let page_index = if args.len() > 2 { args[2].parse()? } else { 0 };

    let pdfium: &'static Pdfium =
        Box::leak(Box::new(Pdfium::new(pdf_viewer::load_pdfium_bindings())));
    let doc = OpenDocument::load(pdfium, &path).map_err(|e| e.to_string())?;
    let pdf_page = doc
        .doc
        .pages()
        .get(page_index as i32)
        .map_err(|e| e.to_string())?;
    println!(
        "page {} size: {} x {} pts",
        page_index, doc.pages[page_index].width_pts, doc.pages[page_index].height_pts
    );

    for (i, obj) in pdf_page.objects().iter().enumerate() {
        let Some(image) = obj.as_image_object() else {
            continue;
        };
        let bounds = image.bounds().map_err(|e| e.to_string())?;
        println!(
            "image {i}: bounds=({:.1},{:.1})-({:.1},{:.1}) size={}x{} dpi={:.0} bpc={}",
            bounds.left().value,
            bounds.bottom().value,
            bounds.right().value,
            bounds.top().value,
            image.width().unwrap_or(0),
            image.height().unwrap_or(0),
            image.horizontal_dpi().unwrap_or(0.0),
            image.bits_per_pixel().unwrap_or(0)
        );
    }
    Ok(())
}
