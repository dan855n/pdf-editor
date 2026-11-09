//! A Rust PDF editor built with egui/eframe + PDFium.
//!
//! Opens a PDF, lets you annotate it (highlight, ink, shapes, text, images),
//! rearrange pages, and save the result. Run `pdf-editor` (or `cargo run --bin
//! pdf-editor -- file.pdf`).

mod app;
mod tools;
mod undo;

pub use app::EditorApp;

use eframe::egui;

/// Entry point for the `pdf-editor` binary.
pub fn main() -> eframe::Result {
    let pdfium = pdfium_render::Pdfium::new(pdf_viewer::load_pdfium_bindings());

    // `PdfDocument<'a>` borrows the `Pdfium` instance. Leaking the single
    // Pdfium keeps the lifetime plumbing simple; documents are still closed
    // properly when replaced (their Drop calls FPDF_CloseDocument).
    let pdfium: &'static pdfium_render::Pdfium = Box::leak(Box::new(pdfium));

    let mut app = EditorApp::new(pdfium);

    if let Some(arg) = std::env::args().nth(1) {
        let path = std::path::PathBuf::from(arg);
        if path.exists() {
            app.load_document(&path);
        } else {
            eprintln!("no such file: {}", path.display());
        }
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 780.0])
            .with_min_inner_size([720.0, 480.0]),
        ..Default::default()
    };

    eframe::run_native(
        "pdf-editor",
        options,
        Box::new(move |cc| {
            egui::Context::set_theme(cc.egui_ctx.clone(), egui::Theme::Dark);
            Ok(Box::new(app))
        }),
    )
}
