//! Verify OpenDocument::save regenerates text-field appearance streams.

use pdfium_render::prelude::Pdfium;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let in_path = args.get(1).cloned().unwrap_or_else(|| "/tmp/test-form.pdf".into());
    let out_path = args.get(2).cloned().unwrap_or_else(|| "/tmp/test-form-saved.pdf".into());
    let value = args.get(3).cloned().unwrap_or_else(|| "Daniel Edwards".into());
    let field_index: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(0);

    let pdfium: &'static Pdfium =
        Box::leak(Box::new(Pdfium::new(pdf_viewer::load_pdfium_bindings())));
    let mut doc =
        pdf_viewer::OpenDocument::load(pdfium, std::path::Path::new(&in_path)).expect("load");
    println!("fields: {}", doc.form_fields().expect("fields").len());

    doc.set_form_field_value(0, field_index, &value).expect("set");
    doc.save_as(std::path::Path::new(&out_path)).expect("save");
    println!("saved {out_path}");
}
