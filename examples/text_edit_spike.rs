//! Phase 0 spike: can we replace the text inside a PDF page text object
//! using PDFium's object API through pdfium-render?
//!
//! Usage:
//!   cargo run --example text_edit_spike -- <input.pdf> "new text" [page]
//! It writes <input-stem>_edited.pdf and prints the text objects it found.

use pdfium_render::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: text_edit_spike <input.pdf> <new-text> [page-index]");
        std::process::exit(2);
    }
    let path = std::path::PathBuf::from(&args[1]);
    let new_text = &args[2];
    let page_index = if args.len() > 3 { args[3].parse()? } else { 0 };

    let pdfium = Pdfium::new(pdf_viewer::load_pdfium_bindings());
    let doc = pdfium.load_pdf_from_file(&path, None)?;
    let mut page = doc.pages().get(page_index as i32)?;

    let target_index = {
        let text_page = page.text()?;
        println!("page {} objects:", page_index);
        let objects: Vec<_> = page.objects().iter().collect();
        let mut first_text = None;
        for (i, obj) in objects.iter().enumerate() {
            let PdfPageObject::Text(text_obj) = obj else {
                continue;
            };
            let t = text_obj.text();
            let chars = text_page.chars_for_object(text_obj)?;
            let mut min_y = f32::MAX;
            let mut max_y = f32::MIN;
            for c in chars.iter() {
                let b = c.loose_bounds()?;
                min_y = min_y.min(b.bottom().value);
                max_y = max_y.max(b.top().value);
            }
            let single_line = chars.is_empty() || (max_y - min_y).abs() < 8.0;
            println!(
                "  object {i}: text={t:?} chars={} single_line={single_line} font_size={:?}",
                chars.len(),
                text_obj.unscaled_font_size().value
            );
            if first_text.is_none() && !t.trim().is_empty() {
                first_text = Some(i);
            }
        }
        first_text
    };

    let mut replaced = false;
    if let Some(index) = target_index
        && let PdfPageObject::Text(mut text_obj) = page.objects().get(index)?
    {
        let original = text_obj.text();
        println!("\nreplacing object text {original:?} -> {new_text:?}");
        text_obj.set_text(new_text)?;
        println!("after set_text, object.text() = {:?}", text_obj.text());
        replaced = true;
    }
    if replaced {
        page.regenerate_content()?;
        println!("regenerated page content");
    } else {
        eprintln!("no non-empty text object found on page {page_index}");
    }

    let out_path = path.with_file_name(format!(
        "{}_edited.pdf",
        path.file_stem().unwrap().to_string_lossy()
    ));
    doc.save_to_file(&out_path)?;
    println!("saved {}", out_path.display());

    Ok(())
}
