//! Plain-data model for the original text in a PDF page.
//!
//! Stage 1 keeps this deliberately small: we group PDFium's character-level
//! text into visual lines (`TextRun`) so the UI can hit-test and highlight a
//! single line of original text.

use pdfium_render::prelude::*;

/// A single visual line of original text on a PDF page.
#[derive(Debug, Clone, PartialEq)]
pub struct TextRun {
    /// Page this run belongs to.
    pub page: usize,
    /// The text of the line, assembled from its characters.
    pub text: String,
    /// Bounding box in PDF page space (origin bottom-left, units = points).
    pub left: f32,
    pub bottom: f32,
    pub right: f32,
    pub top: f32,
    /// The line's baseline in PDF page space.
    pub baseline_y: f32,
    /// Typical font size for the run.
    pub font_size: f32,
    /// First character index in the page's text character collection.
    pub first_char_index: usize,
    /// Last character index in the page's text character collection.
    pub last_char_index: usize,
    /// Whitespace-delimited words in this line, in reading order.
    pub words: Vec<Word>,
}

/// A single word within a visual line of original text.
#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    /// The word's text, assembled from its characters.
    pub text: String,
    /// Bounding box in PDF page space (origin bottom-left, units = points).
    pub left: f32,
    pub bottom: f32,
    pub right: f32,
    pub top: f32,
    /// First character index in the page's text character collection.
    pub first_char_index: usize,
    /// Last character index in the page's text character collection.
    pub last_char_index: usize,
}

impl Word {
    /// Hit test in PDF page space, with a small tolerance for click precision.
    pub fn contains_point(&self, x: f32, y: f32, tolerance: f32) -> bool {
        x >= self.left - tolerance
            && x <= self.right + tolerance
            && y >= self.bottom - tolerance
            && y <= self.top + tolerance
    }
}

impl TextRun {
    /// Hit test in PDF page space, with a small tolerance for click precision.
    pub fn contains_point(&self, x: f32, y: f32, tolerance: f32) -> bool {
        x >= self.left - tolerance
            && x <= self.right + tolerance
            && y >= self.bottom - tolerance
            && y <= self.top + tolerance
    }
}

/// One raw character extracted from PDFium's text page.
struct RawChar {
    index: usize,
    baseline_y: f32,
    left: f32,
    bottom: f32,
    right: f32,
    top: f32,
    font_size: f32,
    ch: char,
}

/// Group a page's characters into visual lines.
///
/// Characters are grouped by their PDFium origin Y (the baseline). A new run
/// is started whenever the baseline moves more than 2.0 points.
pub fn group_chars_into_runs(
    page: usize,
    chars: &PdfPageTextChars<'_>,
) -> Result<Vec<TextRun>, PdfiumError> {
    let mut raw: Vec<RawChar> = Vec::new();

    for ch in chars.iter() {
        let Some(c) = ch.unicode_char() else {
            continue;
        };
        let bounds = ch.loose_bounds()?;
        raw.push(RawChar {
            index: ch.index(),
            baseline_y: ch.origin_y()?.value,
            left: bounds.left().value,
            bottom: bounds.bottom().value,
            right: bounds.right().value,
            top: bounds.top().value,
            font_size: ch.unscaled_font_size().value,
            ch: c,
        });
    }

    Ok(group_raw_chars(page, raw))
}

/// Group already-extracted characters into visual lines.
fn group_raw_chars(page: usize, mut raw: Vec<RawChar>) -> Vec<TextRun> {
    // Sort top-to-bottom, then left-to-right. PDF origin Y is the baseline and
    // grows upward, so a larger Y is a higher line on the page.
    raw.sort_by(|a, b| {
        b.baseline_y
            .partial_cmp(&a.baseline_y)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                a.left
                    .partial_cmp(&b.left)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });

    let mut runs: Vec<TextRun> = Vec::new();
    let mut current: Vec<RawChar> = Vec::new();

    for item in raw {
        if let Some(last) = current.last()
            && (item.baseline_y - last.baseline_y).abs() > 2.0
        {
            runs.push(build_run(page, &current));
            current.clear();
        }
        current.push(item);
    }

    if !current.is_empty() {
        runs.push(build_run(page, &current));
    }

    runs
}

fn build_run(page: usize, chars: &[RawChar]) -> TextRun {
    let mut text = String::new();
    let mut left = f32::MAX;
    let mut bottom = f32::MAX;
    let mut right = f32::MIN;
    let mut top = f32::MIN;
    let mut baseline_sum = 0.0_f32;
    let mut font_size_sum = 0.0_f32;
    let mut first_char_index = usize::MAX;
    let mut last_char_index = usize::MIN;

    for c in chars {
        text.push(c.ch);
        left = left.min(c.left);
        bottom = bottom.min(c.bottom);
        right = right.max(c.right);
        top = top.max(c.top);
        baseline_sum += c.baseline_y;
        font_size_sum += c.font_size;
        first_char_index = first_char_index.min(c.index);
        last_char_index = last_char_index.max(c.index);
    }

    let len = chars.len().max(1) as f32;

    TextRun {
        page,
        text,
        left,
        bottom,
        right,
        top,
        baseline_y: baseline_sum / len,
        font_size: font_size_sum / len,
        first_char_index,
        last_char_index,
        words: build_words(chars),
    }
}

/// Group a line's characters into whitespace-delimited words, in reading order.
fn build_words(chars: &[RawChar]) -> Vec<Word> {
    let mut words = Vec::new();
    let mut current: Vec<&RawChar> = Vec::new();
    for c in chars {
        if c.ch.is_whitespace() {
            if !current.is_empty() {
                words.push(build_word(&current));
                current.clear();
            }
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        words.push(build_word(&current));
    }
    words
}

fn build_word(chars: &[&RawChar]) -> Word {
    let text: String = chars.iter().map(|c| c.ch).collect();
    let mut left = f32::MAX;
    let mut bottom = f32::MAX;
    let mut right = f32::MIN;
    let mut top = f32::MIN;
    let mut first = usize::MAX;
    let mut last = usize::MIN;
    for c in chars {
        left = left.min(c.left);
        bottom = bottom.min(c.bottom);
        right = right.max(c.right);
        top = top.max(c.top);
        first = first.min(c.index);
        last = last.max(c.index);
    }
    Word {
        text,
        left,
        bottom,
        right,
        top,
        first_char_index: first,
        last_char_index: last,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn raw_char(
        index: usize,
        baseline_y: f32,
        left: f32,
        bottom: f32,
        right: f32,
        top: f32,
        font_size: f32,
        ch: char,
    ) -> RawChar {
        RawChar {
            index,
            baseline_y,
            left,
            bottom,
            right,
            top,
            font_size,
            ch,
        }
    }

    #[test]
    fn contains_point_hits_inside_and_misses_outside() {
        let run = TextRun {
            page: 0,
            text: "Hi".into(),
            left: 10.0,
            bottom: 20.0,
            right: 30.0,
            top: 30.0,
            baseline_y: 22.0,
            font_size: 10.0,
            first_char_index: 0,
            last_char_index: 1,
            words: vec![],
        };

        assert!(run.contains_point(15.0, 25.0, 1.0));
        assert!(run.contains_point(9.0, 25.0, 1.5));
        assert!(!run.contains_point(40.0, 25.0, 1.0));
    }

    #[test]
    fn groups_chars_into_runs_by_baseline() {
        let chars = vec![
            raw_char(0, 100.0, 10.0, 98.0, 20.0, 110.0, 10.0, 'A'),
            raw_char(1, 100.0, 22.0, 98.0, 30.0, 110.0, 10.0, 'B'),
            raw_char(2, 80.0, 10.0, 78.0, 20.0, 90.0, 10.0, 'C'),
        ];

        let runs = group_raw_chars(0, chars);
        assert_eq!(runs.len(), 2);
        // Higher baseline first.
        assert_eq!(runs[0].text, "AB");
        assert_eq!(runs[1].text, "C");
        assert_eq!(runs[0].left, 10.0);
        assert_eq!(runs[0].right, 30.0);
        assert_eq!(runs[0].first_char_index, 0);
        assert_eq!(runs[0].last_char_index, 1);
        assert_eq!(runs[1].first_char_index, 2);
    }

    #[test]
    fn groups_words_within_a_run() {
        let chars = vec![
            raw_char(0, 100.0, 10.0, 98.0, 20.0, 110.0, 10.0, 'A'),
            raw_char(1, 100.0, 22.0, 98.0, 30.0, 110.0, 10.0, 'B'),
            raw_char(2, 100.0, 34.0, 98.0, 38.0, 110.0, 10.0, ' '),
            raw_char(3, 100.0, 42.0, 98.0, 50.0, 110.0, 10.0, 'C'),
        ];

        let runs = group_raw_chars(0, chars);
        assert_eq!(runs.len(), 1);
        let run = &runs[0];
        assert_eq!(run.words.len(), 2);

        assert_eq!(run.words[0].text, "AB");
        assert_eq!(run.words[0].first_char_index, 0);
        assert_eq!(run.words[0].last_char_index, 1);

        assert_eq!(run.words[1].text, "C");
        assert_eq!(run.words[1].first_char_index, 3);
        assert_eq!(run.words[1].last_char_index, 3);

        // Hit-testing a word.
        assert!(run.words[0].contains_point(15.0, 100.0, 1.0));
        assert!(!run.words[0].contains_point(45.0, 100.0, 1.0));
    }
}
