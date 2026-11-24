//! Drag several lines of a two-column page as one group and save the result.
//!
//! cargo run --example lines_drag_demo -- /tmp/layout.pdf /tmp/out.pdf
use std::path::Path;

use pdf_viewer::OpenDocument;
use pdfium_render::prelude::*;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let input = args.get(1).cloned().unwrap_or_else(|| "/tmp/layout.pdf".into());
    let output = args.get(2).cloned().unwrap_or_else(|| "/tmp/layout-dragged.pdf".into());

    let pdfium: &'static Pdfium =
        Box::leak(Box::new(Pdfium::new(pdf_viewer::load_pdfium_bindings())));
    let mut document = OpenDocument::load(pdfium, Path::new(&input)).unwrap();

    let runs = document.text_runs(0).unwrap();
    for run in &runs {
        println!("run y={:7.2} x={:7.2} {:?}", run.baseline_y, run.left, run.text);
    }

    // Select two left-column lines and one right-column line, as a shift-click
    // selection across lines would.
    let selected: Vec<_> = runs
        .iter()
        .filter(|run| {
            run.text.starts_with("Left column, first")
                || run.text.starts_with("Left column, second")
                || run.text.starts_with("Right column, first")
        })
        .cloned()
        .collect();
    println!("selected {} lines", selected.len());

    let groups: Vec<Vec<usize>> = selected.iter().map(|run| run.chars.clone()).collect();
    let splits = document.split_selection_for_move(0, &groups).unwrap();
    println!("spans: {}", splits.len());

    let indices: Vec<usize> = splits.iter().map(|split| split.span_index).collect();
    document.translate_text_objects(0, &indices, 30.0, -80.0).unwrap();
    document.save_as(Path::new(&output)).unwrap();
    println!("wrote {output}");
}
