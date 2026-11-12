//! Debug helper: print the original-text runs extracted for a page.
use pdf_viewer::OpenDocument;
use pdfium_render::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let path = std::path::PathBuf::from(&args[1]);
    let page = if args.len() > 2 { args[2].parse()? } else { 0 };

    let pdfium: &'static Pdfium =
        Box::leak(Box::new(Pdfium::new(pdf_viewer::load_pdfium_bindings())));
    let doc = OpenDocument::load(pdfium, &path).map_err(|e| e.to_string())?;
    for run in doc.text_runs(page).map_err(|e| e.to_string())? {
        println!(
            "run {:?} bbox=({:.1},{:.1})-({:.1},{:.1}) baseline={:.1} font={:.1} chars=[{}..{}]",
            run.text,
            run.left,
            run.bottom,
            run.right,
            run.top,
            run.baseline_y,
            run.font_size,
            run.first_char_index,
            run.last_char_index
        );
    }
    Ok(())
}
