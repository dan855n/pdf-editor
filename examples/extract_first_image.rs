//! Extract the first image on a PDF page and save it to /tmp/img.png.
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

    for obj in pdf_page.objects().iter() {
        let Some(image) = obj.as_image_object() else {
            continue;
        };
        let raw = image.get_raw_image().map_err(|e| e.to_string())?;
        raw.save("/tmp/img.png").map_err(|e| e.to_string())?;
        println!("saved /tmp/img.png {}x{}", raw.width(), raw.height());
        return Ok(());
    }
    Err("no images found".into())
}
