//! Print how the editor model sees a page's text: runs, words, and which text
//! object each run maps to.
//!
//! cargo run --example inspect_text -- file.pdf [page]
use std::path::Path;

use std::collections::HashSet;

use pdf_viewer::OpenDocument;
use pdfium_render::prelude::*;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let input = args.get(1).cloned().expect("usage: inspect_text file.pdf [page]");
    let page: usize = args.get(2).and_then(|n| n.parse().ok()).unwrap_or(0);

    let pdfium: &'static Pdfium =
        Box::leak(Box::new(Pdfium::new(pdf_viewer::load_pdfium_bindings())));
    let document = OpenDocument::load(pdfium, Path::new(&input)).unwrap();
    println!(
        "{input}: {} pages, {} objects on page {page}",
        document.page_count,
        document.page_object_count(page).unwrap()
    );

    // Characters the text page reports but no *page* object owns: text drawn
    // inside a Form XObject, or otherwise not addressable as a page object. Such
    // glyphs can be selected but never moved or edited.
    let (owned, unowned) = {
        let pdf_page = document.doc.pages().get(page as i32).unwrap();
        let text_page = pdf_page.text().unwrap();
        let mut owned: HashSet<usize> = HashSet::new();
        for object in pdf_page.objects().iter() {
            if let Some(text_object) = object.as_text_object() {
                for character in text_object.chars(&text_page).unwrap().iter() {
                    owned.insert(character.index());
                }
            }
        }
        let unowned: Vec<(usize, String, f32, f32)> = text_page
            .chars()
            .iter()
            .filter(|c| !owned.contains(&c.index()))
            .filter_map(|c| {
                let ch = c.unicode_char()?;
                if ch.is_control() {
                    return None;
                }
                let (x, y) = c.origin().unwrap();
                Some((c.index(), ch.to_string(), x.value, y.value))
            })
            .collect();
        (owned, unowned)
    };
    println!(
        "characters: {} owned by page objects, {} unowned (Form XObject / non-object text)",
        owned.len(),
        unowned.len()
    );
    for (index, ch, x, y) in unowned.iter().take(20) {
        println!("  unowned char {index:>4} {ch:?} at ({x:7.2},{y:7.2})");
    }

    for (index, run) in document.text_runs(page).unwrap().iter().enumerate() {
        let object = document
            .text_object_index_for_run(page, run)
            .map(|o| o.to_string())
            .unwrap_or_else(|e| format!("ERR {e}"));
        let owners = document
            .text_object_indices_for_chars(page, &run.chars)
            .map(|v| format!("{v:?}"))
            .unwrap_or_else(|e| format!("ERR {e}"));
        let stuck = run.chars.iter().filter(|c| !owned.contains(c)).count();
        println!(
            "run {index:2} obj {object:>3} owners {owners:>8} chars {:>4}..{:<4} words {:>2} y={:7.2} x={:7.2}..{:7.2} stuck={stuck} {:?}",
            run.first_char_index,
            run.last_char_index,
            run.words.len(),
            run.baseline_y,
            run.left,
            run.right,
            truncate(&run.text, 60)
        );
    }
}

fn truncate(text: &str, max: usize) -> String {
    let flat = text.replace('\n', "\\n");
    if flat.chars().count() <= max {
        flat
    } else {
        format!("{}…", flat.chars().take(max).collect::<String>())
    }
}
