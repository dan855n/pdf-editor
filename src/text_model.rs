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
    /// The characters this run owns, in reading order.
    ///
    /// Character indices are *not* necessarily contiguous across a page — a
    /// two-column layout puts one column's text far from the other's — so the
    /// editor resolves objects and splits selections from this set, never from
    /// `first_char_index..=last_char_index`.
    pub chars: Vec<usize>,
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
    /// The characters this word owns, in reading order.
    pub chars: Vec<usize>,
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
    /// False for whitespace PDFium invents at a text-object boundary.
    owned: bool,
}

/// Group a page's characters into visual lines.
///
/// PDFium's text page contains synthetic `\r` / `\n` characters with no text
/// object behind them; they mark where one line (or one column of a line) ends
/// and the next begins. Grouping therefore works in two stages: cut the stream
/// at those characters, then group each block by baseline. Grouping by baseline
/// alone would fuse a two-column line into one "line" spanning the gutter, whose
/// character range also sweeps up everything in between.
pub fn group_chars_into_runs(
    page: usize,
    chars: &PdfPageTextChars<'_>,
) -> Result<Vec<TextRun>, PdfiumError> {
    let mut blocks: Vec<Vec<RawChar>> = Vec::new();
    let mut current: Vec<RawChar> = Vec::new();

    for ch in chars.iter() {
        let Some(c) = ch.unicode_char() else {
            continue;
        };
        // Line breaks are page furniture, not text: they end the current block.
        if c.is_control() {
            if !current.is_empty() {
                blocks.push(std::mem::take(&mut current));
            }
            continue;
        }
        let bounds = ch.loose_bounds()?;
        current.push(RawChar {
            index: ch.index(),
            baseline_y: ch.origin_y()?.value,
            left: bounds.left().value,
            bottom: bounds.bottom().value,
            right: bounds.right().value,
            top: bounds.top().value,
            font_size: ch.unscaled_font_size().value,
            ch: c,
            owned: ch.text_object().is_ok(),
        });
    }
    if !current.is_empty() {
        blocks.push(current);
    }

    let mut runs: Vec<TextRun> = Vec::new();
    for block in blocks {
        runs.extend(group_raw_chars(page, block));
    }
    runs.sort_by(|a, b| {
        b.baseline_y
            .partial_cmp(&a.baseline_y)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                a.left
                    .partial_cmp(&b.left)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });
    Ok(runs)
}

/// One visual line of a page: the runs that sit on the same baseline, in order.
///
/// A line is usually one run, but producers split lines into fragments — a bold
/// lead-in, a drop cap, a hyphenation piece, a capital drawn slightly higher than
/// the rest — and PDFium reports those as separate runs. Treating a fragment as
/// its own line is what made a dragged selection leave the first letter behind.
#[derive(Debug, Clone, PartialEq)]
pub struct VisualLine {
    /// Run indices, left to right.
    pub runs: Vec<usize>,
    pub left: f32,
    pub right: f32,
    pub baseline_y: f32,
    pub font_size: f32,
}

impl VisualLine {
    /// Every character of the line.
    pub fn chars(&self, runs: &[TextRun]) -> Vec<usize> {
        self.runs
            .iter()
            .filter_map(|&index| runs.get(index))
            .flat_map(|run| run.chars.iter().copied())
            .collect()
    }
}

/// Group runs into visual lines and return them in reading order: column by
/// column (left to right), top to bottom within a column.
///
/// Runs join a line when they share a baseline (within half a line height) and
/// no gutter separates them, so a line's fragments stay together while columns
/// stay apart.
pub fn visual_lines(runs: &[TextRun]) -> Vec<VisualLine> {
    let mut lines: Vec<VisualLine> = Vec::new();

    let mut order: Vec<usize> = (0..runs.len()).collect();
    order.sort_by(|&a, &b| {
        runs[b]
            .baseline_y
            .partial_cmp(&runs[a].baseline_y)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                runs[a]
                    .left
                    .partial_cmp(&runs[b].left)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });

    for run_index in order {
        let run = &runs[run_index];
        let mut placed = false;

        for line in lines.iter_mut() {
            let baseline_close =
                (run.baseline_y - line.baseline_y).abs() <= 0.5 * run.font_size.max(line.font_size);
            let gap = (run.left - line.right).max(line.left - run.right);
            let joined = gap <= 2.0 * run.font_size.max(line.font_size);
            if baseline_close && joined {
                line.runs.push(run_index);
                line.left = line.left.min(run.left);
                line.right = line.right.max(run.right);
                placed = true;
                break;
            }
        }

        if !placed {
            lines.push(VisualLine {
                runs: vec![run_index],
                left: run.left,
                right: run.right,
                baseline_y: run.baseline_y,
                font_size: run.font_size,
            });
        }
    }

    for line in lines.iter_mut() {
        line.runs.sort_by(|&a, &b| {
            runs[a]
                .left
                .partial_cmp(&runs[b].left)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    // Cluster lines into columns by horizontal overlap, then order columns
    // left-to-right and lines top-to-bottom.
    let mut columns: Vec<(f32, f32, Vec<usize>)> = Vec::new();
    let mut by_left: Vec<usize> = (0..lines.len()).collect();
    by_left.sort_by(|&a, &b| {
        lines[a]
            .left
            .partial_cmp(&lines[b].left)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    for line_index in by_left {
        let line = &lines[line_index];
        let line_width = (line.right - line.left).max(1.0);
        let mut placed = false;
        for column in columns.iter_mut() {
            let overlap = line.right.min(column.1) - line.left.max(column.0);
            // The line must be mostly *inside* the column. A centred heading that
            // straddles a gutter overlaps two columns only partially; letting it in
            // would merge the columns and interleave their lines in reading order.
            if overlap > 0.5 * line_width {
                column.0 = column.0.min(line.left);
                column.1 = column.1.max(line.right);
                column.2.push(line_index);
                placed = true;
                break;
            }
        }
        if !placed {
            columns.push((line.left, line.right, vec![line_index]));
        }
    }

    columns.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    for (_, _, indices) in columns.iter_mut() {
        indices.sort_by(|&a, &b| {
            lines[b]
                .baseline_y
                .partial_cmp(&lines[a].baseline_y)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(
                    lines[a]
                        .left
                        .partial_cmp(&lines[b].left)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
        });
    }

    let mut ordered = Vec::with_capacity(lines.len());
    for (_, _, indices) in columns {
        for index in indices {
            ordered.push(lines[index].clone());
        }
    }

    ordered
}

/// Join two runs that belong to the same line, keeping reading order.
///
/// Producers often keep a whole line in one text object — a centred title plus a
/// right-aligned code, for example — but PDFium's line detection can report the two
/// halves as separate blocks. Merging them keeps the fragments together when the
/// line is selected, dragged or shifted.
pub fn merge_runs(first: &TextRun, second: &TextRun) -> TextRun {
    let (left_run, right_run) = if first.left <= second.left {
        (first, second)
    } else {
        (second, first)
    };

    let mut chars = left_run.chars.clone();
    chars.extend(right_run.chars.iter().copied());
    let mut words = left_run.words.clone();
    words.extend(right_run.words.iter().cloned());
    words.sort_by(|a, b| a.left.partial_cmp(&b.left).unwrap_or(std::cmp::Ordering::Equal));

    TextRun {
        page: left_run.page,
        text: format!("{}{}", left_run.text, right_run.text),
        left: left_run.left.min(right_run.left),
        bottom: left_run.bottom.min(right_run.bottom),
        right: left_run.right.max(right_run.right),
        top: left_run.top.max(right_run.top),
        baseline_y: left_run.baseline_y,
        font_size: left_run.font_size.max(right_run.font_size),
        first_char_index: left_run.first_char_index.min(right_run.first_char_index),
        last_char_index: left_run.last_char_index.max(right_run.last_char_index),
        chars,
        words,
    }
}

/// Characters of right-aligned fragments that belong to `line`.
///
/// A name set at the margin ("Jan Rinehart") is its own text object, sitting far from
/// the heading it belongs to, so the line model cannot join them — every geometric
/// rule tried either missed it or welded real columns together. The reliable signal
/// is *alignment*: the fragment's right edge sits on the column's right margin, and it
/// shares the line's baseline. Callers add these characters when a line is moved, so
/// the fragment travels with its line without changing how lines are modelled.
pub fn right_margin_fragments(runs: &[TextRun], line: &TextRun) -> Vec<usize> {
    let tolerance = 2.0 * line.font_size.max(1.0);

    // The column's left margin is the left edge shared by the lines around it.
    let margin_left = runs
        .iter()
        .filter(|run| (run.left - line.left).abs() <= tolerance)
        .map(|run| run.left)
        .fold(f32::MAX, f32::min);
    if !margin_left.is_finite() {
        return Vec::new();
    }
    // …and its right margin is the furthest right edge among those lines.
    let margin_right = runs
        .iter()
        .filter(|run| (run.left - margin_left).abs() <= tolerance)
        .map(|run| run.right)
        .fold(f32::MIN, f32::max);
    if !margin_right.is_finite() {
        return Vec::new();
    }

    let mut chars = Vec::new();
    for run in runs {
        if run.first_char_index == line.first_char_index {
            continue;
        }
        let same_baseline =
            (run.baseline_y - line.baseline_y).abs() <= 0.5 * line.font_size.max(run.font_size);
        let on_margin = (run.right - margin_right).abs() <= 1.5;
        // It must sit to the right of the line, not overlap it.
        let clear_of_line = run.left >= line.right - 1.0;
        if same_baseline && on_margin && clear_of_line {
            chars.extend(run.chars.iter().copied());
        }
    }
    chars
}

/// Order runs the way a person reads them: column by column, left to right, and
/// top to bottom inside each column.
///
/// Sorting by baseline alone interleaves a two-column page line by line, so
/// shift-clicking down one column would also select the other column's lines.
pub fn reading_order(runs: &[TextRun]) -> Vec<usize> {
    visual_lines(runs)
        .into_iter()
        .flat_map(|line| line.runs)
        .collect()
}

/// A line that must move to close a gap, and how far (in points, positive = up).
#[derive(Debug, Clone, PartialEq)]
pub struct ReflowShift {
    /// Runs of the line that moves.
    pub runs: Vec<usize>,
    /// Upward distance in PDF points.
    pub dy: f32,
}

/// How far the lines below a removed block must move up so nothing overlaps.
///
/// This is paragraph reflow *within the existing layout*: it closes vertical gaps,
/// it does not re-break or re-wrap lines. Everything below the removed line in the
/// same column moves up by the document's own leading, so paragraph spacing is
/// preserved and the gap does not simply reappear further down. A very wide gap
/// (a footer, a page number) is treated as a separate block and left where it is —
/// the whitespace above it absorbs the shift instead.
///
/// `removed_runs` are the runs of lines being taken out; only lines whose runs are
/// *all* removed count, so a partly selected line (which still shows its remainder)
/// does not cause a shift.
pub fn reflow_after_removal(runs: &[TextRun], removed_runs: &[usize]) -> Vec<ReflowShift> {
    let lines = visual_lines(runs);
    let removed: Vec<bool> = lines
        .iter()
        .map(|line| line.runs.iter().all(|run| removed_runs.contains(run)))
        .collect();

    let mut shifts: Vec<ReflowShift> = Vec::new();

    for column in line_columns(&lines) {
        let leading = column_leading(&lines, &column);
        let isolated_gap = leading * 3.0;
        // Vertical space taken out of the column, still to be recovered.
        let mut dropped = 0.0_f32;

        for (position, &line_index) in column.iter().enumerate() {
            let baseline = lines[line_index].baseline_y;
            let gap_below = match column.get(position + 1) {
                Some(&next) => baseline - lines[next].baseline_y,
                None => leading,
            };
            let gap_above = match position.checked_sub(1).and_then(|previous| column.get(previous)) {
                Some(&previous) => lines[previous].baseline_y - baseline,
                None => leading,
            };

            if removed[line_index] {
                // The line's whole vertical slot goes, not just one leading: taking
                // out a line that sat above a paragraph break must close that break
                // too, or the hole simply reappears under the heading above it.
                if gap_below > isolated_gap {
                    // A separate block follows (footer, page number): leave it and
                    // let the whitespace absorb the shift instead.
                    dropped = 0.0;
                } else {
                    dropped += gap_below;
                }
                continue;
            }

            if dropped <= 0.0 {
                continue;
            }
            if gap_above > isolated_gap {
                // This line belongs to a block of its own; it keeps its position.
                dropped = 0.0;
                continue;
            }

            shifts.push(ReflowShift {
                runs: lines[line_index].runs.clone(),
                dy: dropped,
            });
        }
    }

    shifts
}

/// The room that must be opened when text is dropped into a column.
#[derive(Debug, Clone, PartialEq)]
pub struct DropSlot {
    /// Runs of the lines that move down.
    pub runs: Vec<usize>,
    /// Downward distance in PDF points.
    pub dy: f32,
}

/// Which lines must move down — and how far — so text dropped at
/// `dropped_baseline` does not land on top of what is already there.
///
/// This is the mirror of [`reflow_after_removal`]: the source side closes up, the
/// destination side opens a slot of exactly the dropped block's height. Lines are
/// shifted *down* rigidly; they are not re-wrapped, and a footer separated by a
/// wide gap keeps its position.
///
/// `dropped_runs` are the runs of the text being dropped, and `dropped_extent` is
/// its horizontal span, which decides which column it landed in.
pub fn room_for_drop(
    runs: &[TextRun],
    dropped_runs: &[usize],
    dropped_baseline: f32,
    dropped_extent: (f32, f32),
) -> Option<DropSlot> {
    if dropped_runs.is_empty() {
        return None;
    }
    // Lines are counted per run and by distinct baseline: fragments of one line
    // share a baseline and must only open one slot. Runs are used directly rather
    // than grouped visual lines, because dropped text sitting close to the line
    // below it would otherwise be merged with that line.
    let mut dropped_baselines: Vec<f32> = dropped_runs
        .iter()
        .filter_map(|&index| runs.get(index))
        .map(|run| (run.baseline_y * 10.0).round() / 10.0)
        .collect();
    dropped_baselines.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    dropped_baselines.dedup();
    let dropped_lines = dropped_baselines.len().max(1) as f32;

    let lines = visual_lines(runs);
    let (dropped_left, dropped_right) = dropped_extent;
    let dropped_width = (dropped_right - dropped_left).max(1.0);

    // Destination runs: same column, at or below the drop point. A line that the
    // dropped text landed *on* shares its baseline and is merged with it into one
    // visual line, so only that line's stationary runs move — never the dropped ones.
    let mut destination: Vec<(Vec<usize>, f32)> = Vec::new();
    for line in &lines {
        let stationary: Vec<usize> = line
            .runs
            .iter()
            .copied()
            .filter(|run| !dropped_runs.contains(run))
            .collect();
        if stationary.is_empty() {
            continue;
        }
        // Include a line sitting *at* (or overlapping vertically with) the
        // drop baseline: aiming at the middle or lower half of a line puts
        // `dropped_baseline` a few points below `line.baseline_y`, and leaving
        // that line in place would overlap it.
        if line.baseline_y >= dropped_baseline + 0.5 * line.font_size.max(10.0) {
            continue;
        }
        let overlap = line.right.min(dropped_right) - line.left.max(dropped_left);
        let narrower = (line.right - line.left).min(dropped_width);
        if overlap <= 0.3 * narrower.max(1.0) {
            continue;
        }
        destination.push((stationary, line.baseline_y));
    }
    if destination.is_empty() {
        return None;
    }
    destination.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // The document's own leading, taken from the lines the slot is cut into.
    let mut gaps: Vec<f32> = destination
        .windows(2)
        .map(|pair| pair[0].1 - pair[1].1)
        .filter(|gap| *gap > 0.5)
        .collect();
    let leading = if gaps.is_empty() {
        let first = destination[0].0.first().copied().unwrap_or(0);
        runs.get(first).map(|run| run.font_size * 1.2).unwrap_or(14.0)
    } else {
        gaps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        gaps[gaps.len() / 4]
    };
    if leading <= 0.0 {
        return None;
    }

    // Walk down from the drop point while the lines remain one block.
    let mut shifted: Vec<usize> = Vec::new();
    let mut previous = dropped_baseline;
    for (line_runs, baseline) in destination {
        if previous - baseline > leading * 3.0 {
            // A separate block (footer, page number): it keeps its place.
            break;
        }
        previous = baseline;
        shifted.extend(line_runs);
    }
    if shifted.is_empty() {
        return None;
    }

    Some(DropSlot {
        runs: shifted,
        dy: leading * dropped_lines,
    })
}

/// Groups of line indices that belong to one column, in reading order.
fn line_columns(lines: &[VisualLine]) -> Vec<Vec<usize>> {
    let mut columns: Vec<(f32, f32, Vec<usize>)> = Vec::new();
    let mut by_left: Vec<usize> = (0..lines.len()).collect();
    by_left.sort_by(|&a, &b| {
        lines[a]
            .left
            .partial_cmp(&lines[b].left)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    for line_index in by_left {
        let line = &lines[line_index];
        let mut placed = false;
        for column in columns.iter_mut() {
            let overlap = line.right.min(column.1) - line.left.max(column.0);
            let line_width = (line.right - line.left).max(1.0);
            if overlap > 0.5 * line_width {
                column.0 = column.0.min(line.left);
                column.1 = column.1.max(line.right);
                column.2.push(line_index);
                placed = true;
                break;
            }
        }
        if !placed {
            columns.push((line.left, line.right, vec![line_index]));
        }
    }

    columns.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    for (_, _, indices) in columns.iter_mut() {
        indices.sort_by(|&a, &b| {
            lines[b]
                .baseline_y
                .partial_cmp(&lines[a].baseline_y)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    columns.into_iter().map(|(_, _, indices)| indices).collect()
}

/// The typical baseline-to-baseline distance of a column.
fn column_leading(lines: &[VisualLine], column: &[usize]) -> f32 {
    let mut gaps: Vec<f32> = column
        .windows(2)
        .map(|pair| lines[pair[0]].baseline_y - lines[pair[1]].baseline_y)
        .filter(|gap| *gap > 0.0)
        .collect();
    if gaps.is_empty() {
        let size = column
            .first()
            .map(|&index| lines[index].font_size)
            .unwrap_or(11.0);
        return size * 1.2;
    }
    // The lower quartile, not the median: a column usually holds several lines at
    // the true leading plus a few paragraph breaks that would drag a median up.
    gaps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    gaps[gaps.len() / 4]
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
            && breaks_run(last, &item)
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

/// Whether `item` starts a new visual line after `last`.
fn breaks_run(last: &RawChar, item: &RawChar) -> bool {
    // A different baseline is a new line.
    if (item.baseline_y - last.baseline_y).abs() > 2.0 {
        return true;
    }
    // A gap wider than a couple of ems is a gutter or a tab stop, not a space:
    // some producers emit no line break between columns, so fall back on geometry.
    let gap = item.left - last.right;
    gap > 2.0 * item.font_size.max(last.font_size)
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
    let mut char_indices = Vec::with_capacity(chars.len());

    for c in chars {
        text.push(c.ch);
        char_indices.push(c.index);
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
        chars: char_indices,
        words: build_words(chars),
    }
}

/// Group a line's characters into whitespace-delimited words, in reading order.
fn build_words(chars: &[RawChar]) -> Vec<Word> {
    let mut words = Vec::new();
    let mut current: Vec<&RawChar> = Vec::new();
    let mut previous_right: Option<f32> = None;

    for (index, c) in chars.iter().enumerate() {
        if c.ch.is_whitespace() {
            // PDFium invents a space at every text-object boundary whose gap is
            // wider than ~0.15em, even in the middle of a word (a line drawn as
            // fragments: "tr" + "ust."). Invented whitespace separates words only
            // when the gap around it is space-sized; otherwise dragging half a word
            // would leave the other half behind.
            let next_left = chars
                .iter()
                .skip(index + 1)
                .find(|next| !next.ch.is_whitespace())
                .map(|next| next.left);
            let space_sized = match (previous_right, next_left) {
                (Some(right), Some(left)) => left - right >= 0.2 * c.font_size,
                _ => true,
            };
            if !current.is_empty() && (c.owned || space_sized) {
                words.push(build_word(&current));
                current.clear();
            }
            continue;
        }
        current.push(c);
        previous_right = Some(previous_right.map_or(c.right, |right| right.max(c.right)));
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
    let mut char_indices = Vec::with_capacity(chars.len());
    for c in chars {
        left = left.min(c.left);
        bottom = bottom.min(c.bottom);
        right = right.max(c.right);
        top = top.max(c.top);
        first = first.min(c.index);
        last = last.max(c.index);
        char_indices.push(c.index);
    }
    Word {
        text,
        left,
        bottom,
        right,
        top,
        first_char_index: first,
        last_char_index: last,
        chars: char_indices,
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
            owned: true,
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
            chars: vec![0, 1],
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

    fn synthetic_run(text: &str, left: f32, right: f32, baseline_y: f32) -> TextRun {
        TextRun {
            page: 0,
            text: text.into(),
            left,
            bottom: baseline_y - 2.0,
            right,
            top: baseline_y + 8.0,
            baseline_y,
            font_size: 10.0,
            first_char_index: 0,
            last_char_index: 0,
            chars: vec![],
            words: vec![],
        }
    }

    /// A line drawn as touching fragments gets an invented space from PDFium; a
    /// tight gap must not split the word, or dragging part of it leaves the rest.
    #[test]
    fn tight_invented_space_does_not_split_a_word() {
        let mut tr = raw_char(0, 700.0, 72.0, 698.0, 75.06, 708.0, 11.0, 't');
        let r = raw_char(1, 700.0, 75.06, 698.0, 78.72, 708.0, 11.0, 'r');
        let space = raw_char(2, 700.0, 78.72, 698.0, 81.5, 708.0, 11.0, ' ');
        let u = raw_char(3, 700.0, 79.5, 698.0, 84.84, 708.0, 11.0, 'u');
        let s = raw_char(4, 700.0, 84.84, 698.0, 90.34, 708.0, 11.0, 's');
        let t = raw_char(5, 700.0, 90.34, 698.0, 93.39, 708.0, 11.0, 't');
        // The invented space is not owned by any text object.
        tr.owned = true;
        let mut space = space;
        space.owned = false;
        let chars = vec![tr, r, space, u, s, t];

        let words = build_words(&chars);
        assert_eq!(words.len(), 1, "a tight fragment boundary is inside the word");
        assert_eq!(words[0].text, "trust");
        assert_eq!(words[0].chars, vec![0, 1, 3, 4, 5]);
    }

    /// A real space, and an invented one with a space-sized gap, still separate.
    #[test]
    fn space_sized_gaps_still_split_words() {
        let a = raw_char(0, 700.0, 72.0, 698.0, 78.0, 708.0, 11.0, 'a');
        let mut space = raw_char(1, 700.0, 78.0, 698.0, 81.0, 708.0, 11.0, ' ');
        space.owned = false;
        let b = raw_char(2, 700.0, 84.0, 698.0, 90.0, 708.0, 11.0, 'b');

        let words = build_words(&[a, space, b]);
        assert_eq!(words.len(), 2, "a wide gap is a word break");
        assert_eq!(words[0].text, "a");
        assert_eq!(words[1].text, "b");
    }

    #[test]
    fn dropping_into_a_paragraph_opens_one_line() {
        let runs: Vec<TextRun> = (0..4)
            .map(|row| {
                synthetic_run(
                    &format!("line {row}"),
                    72.0,
                    300.0,
                    700.0 - row as f32 * 14.0,
                )
            })
            .collect();

        // Dropped text sits between line 1 (686) and line 2 (672).
        let slot = room_for_drop(&runs, &[9], 679.0, (72.0, 300.0)).expect("slot");
        assert_eq!(slot.runs, vec![2, 3], "only the lines below move");
        assert_eq!(slot.dy, 14.0);
    }

    /// Text dropped *onto* a line must push that line down, not sit on it. The
    /// dropped run and the line's own run share a baseline and merge into one
    /// visual line, so only the stationary run may move.
    #[test]
    fn dropping_onto_a_line_pushes_it_too() {
        let runs = vec![
            synthetic_run("above", 72.0, 300.0, 707.0),
            synthetic_run("dropped", 72.0, 150.0, 693.0),
            synthetic_run("rest of that line", 160.0, 300.0, 693.0),
            synthetic_run("line below", 72.0, 300.0, 679.0),
        ];

        // Run 1 is the text being dropped; it lands on the line run 2 continues.
        let slot = room_for_drop(&runs, &[1], 693.0, (72.0, 150.0)).expect("slot");
        assert_eq!(
            slot.runs,
            vec![2, 3],
            "the landed-on line and the lines below move; the dropped run does not"
        );
        assert_eq!(slot.dy, 14.0);
    }

    #[test]
    fn dropping_outside_a_column_opens_nothing() {
        let runs = vec![
            synthetic_run("left", 72.0, 220.0, 700.0),
            synthetic_run("left 2", 72.0, 220.0, 686.0),
        ];
        // Dropped far to the right of any text.
        assert!(room_for_drop(&runs, &[9], 693.0, (400.0, 500.0)).is_none());
    }

    #[test]
    fn dropping_in_the_other_column_leaves_this_one_alone() {
        let runs = vec![
            synthetic_run("left 1", 72.0, 220.0, 700.0),
            synthetic_run("right 1", 320.0, 470.0, 700.0),
            synthetic_run("left 2", 72.0, 220.0, 686.0),
            synthetic_run("right 2", 320.0, 470.0, 686.0),
        ];

        let slot = room_for_drop(&runs, &[9], 693.0, (320.0, 470.0)).expect("slot");
        assert_eq!(slot.runs, vec![3], "only the right column opens");
    }

    #[test]
    fn dropping_does_not_push_a_footer() {
        let runs = vec![
            synthetic_run("body", 72.0, 300.0, 700.0),
            synthetic_run("page 1", 500.0, 540.0, 60.0),
        ];
        // The only line below the drop point is a footer, far away.
        assert!(room_for_drop(&runs, &[9], 693.0, (72.0, 300.0)).is_none());
    }

    #[test]
    fn dropping_two_lines_opens_two_lines() {
        let runs: Vec<TextRun> = (0..4)
            .map(|row| {
                synthetic_run(
                    &format!("line {row}"),
                    72.0,
                    300.0,
                    700.0 - row as f32 * 14.0,
                )
            })
            .collect();

        // Both dropped "runs" are lines 0 and 1 in the model above.
        let slot = room_for_drop(&runs, &[0, 1], 679.0, (72.0, 300.0)).expect("slot");
        assert_eq!(slot.runs, vec![2, 3]);
        assert_eq!(slot.dy, 28.0, "two lines of room");
    }

    #[test]
    fn reflow_closes_the_gap_of_a_removed_line() {
        // Five 11pt lines on a 14pt pitch; the second is taken out.
        let runs: Vec<TextRun> = (0..5)
            .map(|row| {
                synthetic_run(
                    &format!("line {row}"),
                    72.0,
                    300.0,
                    700.0 - row as f32 * 14.0,
                )
            })
            .collect();

        let shifts = reflow_after_removal(&runs, &[1]);
        let moved: Vec<(usize, f32)> = shifts.iter().map(|shift| (shift.runs[0], shift.dy)).collect();
        assert_eq!(moved, vec![(2, 14.0), (3, 14.0), (4, 14.0)]);
    }

    #[test]
    fn reflow_closes_the_whole_gap() {
        // The removed line sat above a paragraph break (28pt instead of 14pt): the
        // break is part of the hole and closes with it, so nothing is left under
        // the heading that preceded the line.
        let runs = vec![
            synthetic_run("first", 72.0, 300.0, 700.0),
            synthetic_run("later", 72.0, 300.0, 672.0),
            synthetic_run("later still", 72.0, 300.0, 658.0),
        ];

        let shifts = reflow_after_removal(&runs, &[0]);
        let moved: Vec<(usize, f32)> = shifts.iter().map(|shift| (shift.runs[0], shift.dy)).collect();
        assert_eq!(moved, vec![(1, 28.0), (2, 28.0)]);
    }

    #[test]
    fn reflow_leaves_a_footer_alone() {
        let runs = vec![
            synthetic_run("body", 72.0, 300.0, 700.0),
            synthetic_run("body 2", 72.0, 300.0, 686.0),
            // A page number far below the text block.
            synthetic_run("page 1", 500.0, 540.0, 60.0),
        ];

        let shifts = reflow_after_removal(&runs, &[0]);
        let moved: Vec<usize> = shifts.iter().map(|shift| shift.runs[0]).collect();
        assert_eq!(moved, vec![1], "the footer keeps its place");
    }

    #[test]
    fn reflow_keeps_the_other_column_alone() {
        let runs = vec![
            synthetic_run("left 1", 72.0, 220.0, 700.0),
            synthetic_run("right 1", 320.0, 470.0, 700.0),
            synthetic_run("left 2", 72.0, 220.0, 686.0),
            synthetic_run("right 2", 320.0, 470.0, 686.0),
        ];

        let shifts = reflow_after_removal(&runs, &[0]);
        assert_eq!(shifts.len(), 1, "only the left column closes up: {shifts:?}");
        assert_eq!(shifts[0].runs, vec![2]);
        assert_eq!(shifts[0].dy, 14.0);
    }

    #[test]
    fn partly_selected_lines_do_not_reflow() {
        // Run 0 is a single run: selecting it is a whole-line removal, but the
        // caller passes only the runs it is removing. A line whose runs are not all
        // removed keeps its place.
        let runs = vec![
            synthetic_run("one", 72.0, 300.0, 700.0),
            synthetic_run("two", 72.0, 300.0, 686.0),
        ];
        let shifts = reflow_after_removal(&runs, &[]);
        assert!(shifts.is_empty());
    }

    /// A right-aligned name is picked up when its line is moved, because its right
    /// edge sits on the column's margin and it shares the baseline.
    #[test]
    fn right_margin_fragment_is_found_for_its_line() {
        let mut heading = synthetic_run("* Opening Prayer", 36.0, 146.0, 163.3);
        heading.chars = vec![10, 11, 12];
        heading.first_char_index = 10;
        let mut name = synthetic_run("Jan Rinehart", 304.0, 381.0, 163.3);
        name.chars = vec![20, 21];
        name.first_char_index = 20;
        // A body line sets the column's right margin at 381.
        let mut body = synthetic_run("Leader: When we stand at the edge", 36.0, 381.0, 190.0);
        body.chars = vec![30];
        body.first_char_index = 30;
        // Something in the other column must not be picked up.
        let mut other = synthetic_run("Our Father, who art in heaven", 435.0, 742.0, 163.3);
        other.chars = vec![40];
        other.first_char_index = 40;
        other.font_size = 11.0;

        let runs = vec![heading.clone(), name, body, other];
        let mut fragments = right_margin_fragments(&runs, &heading);
        fragments.sort_unstable();
        assert_eq!(fragments, vec![20, 21], "the name joins its heading");
    }

    #[test]
    fn visual_lines_join_fragments_of_one_line() {
        // A bold lead-in drawn 1.2pt higher than the rest of its line: PDFium
        // reports two runs, but a reader sees one line.
        let runs = vec![
            synthetic_run("A", 72.0, 80.0, 701.2),
            synthetic_run("nswer the call", 81.0, 180.0, 700.0),
            synthetic_run("Second line here", 72.0, 190.0, 686.0),
        ];

        let lines = visual_lines(&runs);
        assert_eq!(lines.len(), 2, "the lead-in belongs to its line");
        assert_eq!(lines[0].runs, vec![0, 1]);
        assert_eq!(lines[1].runs, vec![2]);
    }

    /// A centred heading over two columns must not fuse them: reading order has to
    /// stay column by column, or a shift-click down one column grabs the other.
    #[test]
    fn a_centred_heading_does_not_fuse_two_columns() {
        let runs = vec![
            synthetic_run("Centred Title Over Both Columns", 180.0, 430.0, 720.0),
            synthetic_run("left 1", 72.0, 220.0, 700.0),
            synthetic_run("right 1", 320.0, 470.0, 700.0),
            synthetic_run("left 2", 72.0, 220.0, 686.0),
            synthetic_run("right 2", 320.0, 470.0, 686.0),
        ];

        let order = reading_order(&runs);
        let texts: Vec<&str> = order.iter().map(|&i| runs[i].text.as_str()).collect();
        let position = |needle: &str| texts.iter().position(|text| *text == needle).unwrap();
        // Each column must stay contiguous; the heading forms a block of its own
        // rather than being spliced between the columns' lines.
        assert_eq!(
            position("left 2") - position("left 1"),
            1,
            "left column contiguous: {texts:?}"
        );
        assert_eq!(
            position("right 2") - position("right 1"),
            1,
            "right column contiguous: {texts:?}"
        );
        assert!(
            !texts[..position("left 2")].contains(&"right 1")
                && !texts[position("left 2")..position("right 1")].contains(&"Centred Title Over Both Columns")
                    || position("right 1") > position("left 2"),
            "no interleaving: {texts:?}"
        );
    }

    #[test]
    fn visual_lines_keep_columns_apart_on_the_same_baseline() {
        let runs = vec![
            synthetic_run("left column line", 72.0, 220.0, 700.0),
            synthetic_run("right column line", 320.0, 470.0, 700.0),
        ];

        let lines = visual_lines(&runs);
        assert_eq!(lines.len(), 2, "a gutter must not be bridged");
        assert_eq!(lines[0].runs, vec![0]);
        assert_eq!(lines[1].runs, vec![1]);
    }

    #[test]
    fn visual_line_orders_fragments_left_to_right() {
        let runs = vec![
            synthetic_run("tail", 120.0, 170.0, 700.0),
            synthetic_run("head", 72.0, 118.0, 700.4),
        ];

        let lines = visual_lines(&runs);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].runs, vec![1, 0]);
        assert_eq!(lines[0].chars(&runs).len(), 0, "no characters recorded in this fixture");
    }

    #[test]
    fn reading_order_is_column_major() {
        // Two columns whose lines share baselines.
        let runs = vec![
            synthetic_run("L1", 72.0, 200.0, 700.0),
            synthetic_run("R1", 320.0, 460.0, 700.0),
            synthetic_run("L2", 72.0, 210.0, 686.0),
            synthetic_run("R2", 320.0, 470.0, 686.0),
            synthetic_run("L3", 72.0, 180.0, 672.0),
        ];

        let order = reading_order(&runs);
        let texts: Vec<&str> = order.iter().map(|&i| runs[i].text.as_str()).collect();
        assert_eq!(texts, vec!["L1", "L2", "L3", "R1", "R2"]);
    }

    #[test]
    fn reading_order_is_top_down_for_a_single_column() {
        let runs = vec![
            synthetic_run("second", 72.0, 300.0, 686.0),
            synthetic_run("first", 72.0, 260.0, 700.0),
            synthetic_run("third", 72.0, 280.0, 672.0),
        ];

        let order = reading_order(&runs);
        let texts: Vec<&str> = order.iter().map(|&i| runs[i].text.as_str()).collect();
        assert_eq!(texts, vec!["first", "second", "third"]);
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
