//! Demo of the selection-drag path without the UI: split a phrase out of its
//! line, move it as one group, and save the result so it can be opened and
//! eyeballed.
//!
//! ```text
//! cargo run --example word_drag_demo -- test-search.pdf /tmp/group.pdf 2
//! ```
use std::path::Path;

use pdf_viewer::OpenDocument;
use pdfium_render::prelude::*;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let input = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "test-search.pdf".into());
    let output = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| "/tmp/word-drag-demo.pdf".into());
    let phrase_len: usize = args.get(3).and_then(|n| n.parse().ok()).unwrap_or(2);

    let pdfium: &'static Pdfium =
        Box::leak(Box::new(Pdfium::new(pdf_viewer::load_pdfium_bindings())));
    let mut document = OpenDocument::load(pdfium, Path::new(&input)).unwrap();

    let (page, run) = (0..document.page_count)
        .find_map(|page| {
            document.text_runs(page).ok().and_then(|runs| {
                runs.into_iter()
                    .find(|run| run.words.len() > phrase_len)
                    .map(|run| (page, run))
            })
        })
        .expect("the document should contain a line with enough words");

    // Select words 1..=phrase_len, exactly like a click plus shift-clicks.
    let first_word = &run.words[1];
    let last_word = &run.words[phrase_len];
    let phrase = run.words[1..=phrase_len]
        .iter()
        .map(|word| word.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    println!("line:   {:?}", run.text);
    println!("phrase: {phrase:?} on page {page}");

    // The characters of the phrase, exactly as a click + shift-click would select.
    let phrase_chars: Vec<usize> = run.words[1..=phrase_len]
        .iter()
        .flat_map(|word| word.chars.iter().copied())
        .collect();
    debug_assert!(phrase_chars.contains(&first_word.first_char_index));
    debug_assert!(phrase_chars.contains(&last_word.last_char_index));

    let splits = document
        .split_selection_for_move(page, &[phrase_chars])
        .unwrap();
    println!("spans:  {}", splits.len());

    let indices: Vec<usize> = splits.iter().map(|split| split.span_index).collect();
    document
        .translate_text_objects(page, &indices, 90.0, -50.0)
        .unwrap();
    document.save_as(Path::new(&output)).unwrap();
    println!("wrote {output}");
}
