//! Spike: prove a filled form value renders through the crate's public render
//! path (which uses `FPDF_FFLDraw` for form fields when `do_render_form_data`
//! is true — the default).
//!
//! Usage:
//!   cargo run --example form_render -- <pdf> <field_index> <value> [out.png]

use pdfium_render::prelude::*;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: form_render <pdf> <field_index> <value> [out.png]");
        std::process::exit(2);
    }
    let path = &args[1];
    let index: usize = args[2].parse().expect("field index");
    let value = &args[3];
    let out = args.get(4).cloned().unwrap_or_else(|| "/tmp/form-render-crate.png".into());

    let pdfium = Pdfium::new(pdf_viewer::load_pdfium_bindings());
    let doc = pdfium.load_pdf_from_file(path, None).expect("open");

    // Set the value on the chosen field (writes /V only).
    let mut seen = 0usize;
    let mut done = false;
    for page_idx in 0..doc.pages().len() {
        let page = doc.pages().get(page_idx).expect("page");
        let annots: Vec<PdfPageAnnotation> = page.annotations().iter().collect();
        for mut a in annots {
            let Some(field) = a.as_form_field_mut() else { continue };
            if seen == index {
                match field {
                    PdfFormField::Text(t) => t.set_value(value).expect("set value"),
                    PdfFormField::Checkbox(c) => c
                        .set_checked(value.eq_ignore_ascii_case("true"))
                        .expect("set checked"),
                    _ => {}
                }
                done = true;
                break;
            }
            seen += 1;
        }
        if done { break; }
    }
    assert!(done, "field {index} not found");

    // Render page 0 at 2x with the default config (renders form data).
    let page = doc.pages().get(0).expect("page");
    let cfg = PdfRenderConfig::new()
        .set_target_width(1224)
        .set_target_height(1584);
    let bitmap = page.render_with_config(&cfg).expect("render");
    let img = bitmap.as_image().expect("as image");
    img.save(&out).expect("save png");
    println!("wrote {out}");
}
