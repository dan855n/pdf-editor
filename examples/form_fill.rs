//! Spike: enumerate AcroForm fields and set their values via pdfium-render 0.9.
//!
//! Usage:
//!   cargo run --example form_fill -- <pdf>                 # list fields
//!   cargo run --example form_fill -- <pdf> <index> <value> # list, set one field, save, re-read

use pdfium_render::prelude::*;
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: form_fill <pdf> [field_index] [value]");
        std::process::exit(2);
    }
    let path = PathBuf::from(&args[1]);
    let set_index: Option<usize> = args.get(2).and_then(|s| s.parse().ok());
    let new_value = args.get(3).cloned().unwrap_or_default();

    let pdfium = Pdfium::new(pdf_viewer::load_pdfium_bindings());
    let mut doc = pdfium.load_pdf_from_file(&path, None).expect("open pdf");

    println!(
        "form type: {:?}",
        doc.form().map(|f| f.form_type()).unwrap_or(PdfFormType::None)
    );

    list_fields(&doc);

    if let Some(index) = set_index {
        println!("--- setting field {index} to {new_value:?} ---");
        set_field(&mut doc, index, &new_value);

        let save_path = path.with_file_name("test-form-filled.pdf");
        doc.save_to_file(&save_path).expect("save");
        println!("saved {}", save_path.display());

        // Re-load from the saved bytes and verify the value round-trips.
        let check = pdfium
            .load_pdf_from_file(&save_path, None)
            .expect("reopen");
        list_fields(&check);
    }
}

fn list_fields(doc: &PdfDocument) {
    let mut global = 0usize;
    for page_idx in 0..doc.pages().len() {
        let page = doc.pages().get(page_idx).expect("page");
        let annots: Vec<PdfPageAnnotation> = page.annotations().iter().collect();
        for a in &annots {
            let Some(field) = a.as_form_field() else {
                continue;
            };
            let ft = field.field_type();
            let name = field.name().unwrap_or_default();
            let value = field_value(field);
            let b = a.bounds().unwrap_or_else(|_| PdfRect::new_from_values(0.0, 0.0, 0.0, 0.0));
            println!(
                "[{global}] page {page_idx} {ft:?} name={name:?} value={value:?} rect=({:.0},{:.0},{:.0},{:.0})",
                b.left().value, b.bottom().value, b.right().value, b.top().value
            );
            global += 1;
        }
    }
    println!("total fields: {global}");
}

fn field_value(field: &PdfFormField) -> String {
    match field {
        PdfFormField::Text(t) => t.value().unwrap_or_default(),
        PdfFormField::Checkbox(c) => c
            .is_checked()
            .map(|b| b.to_string())
            .unwrap_or_else(|_| "err".into()),
        PdfFormField::RadioButton(r) => r
            .is_checked()
            .map(|b| b.to_string())
            .unwrap_or_else(|_| "err".into()),
        PdfFormField::ComboBox(c) => c.value().unwrap_or_default(),
        PdfFormField::ListBox(l) => l.value().unwrap_or_default(),
        PdfFormField::PushButton(_)
        | PdfFormField::Signature(_)
        | PdfFormField::Unknown(_) => String::new(),
    }
}

fn set_field(doc: &mut PdfDocument, index: usize, value: &str) {
    let mut seen = 0usize;
    for page_idx in 0..doc.pages().len() {
        let page = doc.pages().get(page_idx).expect("page");
        let annots: Vec<PdfPageAnnotation> = page.annotations().iter().collect();
        for mut a in annots {
            let Some(field) = a.as_form_field_mut() else {
                continue;
            };
            if seen == index {
                match field {
                    PdfFormField::Text(t) => t.set_value(value).expect("set text value"),
                    PdfFormField::Checkbox(c) => c
                        .set_checked(value.eq_ignore_ascii_case("true"))
                        .expect("set checkbox"),
                    PdfFormField::RadioButton(r) => {
                        if value.eq_ignore_ascii_case("true") {
                            r.set_checked().expect("set radio");
                        }
                    }
                    PdfFormField::ComboBox(_) | PdfFormField::ListBox(_) => {
                        eprintln!("note: pdfium-render 0.9 has no public setter for combo/list");
                    }
                    PdfFormField::PushButton(_)
                    | PdfFormField::Signature(_)
                    | PdfFormField::Unknown(_) => {}
                }
                println!("set [{seen}] type={:?}", field.field_type());
                return;
            }
            seen += 1;
        }
    }
}
