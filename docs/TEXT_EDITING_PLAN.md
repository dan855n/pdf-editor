# PDF Editor — Original Text Editing: Project Plan

**Goal:** teach `pdf-editor` to edit the original text already in a PDF
(currently the editor is annotation-only).

**Stage 1 target (this plan):** click on an existing line of text and replace
that line in place, with the result saved as a real PDF text edit that other
viewers can see.

**Non-goals for Stage 1:** multi-line paragraph reflow, scanned-page OCR,
right-to-left text, complex script shaping, full font subsetting, PDF/A or
encrypted documents.

---

## 1. Current State

What we already have that we can reuse:

- Rust + eframe/egui 0.37 editor UI (`src/editor/app.rs`)
- PDFium rendering via `pdfium-render` 0.9 (`src/lib.rs`, `src/pdfium_bindings.rs`)
- Annotation model + save/load round-trip (`src/annotations.rs`, `src/document.rs`)
- Search over the PDF text layer using `FPDFText_FindStart`
- Undo/redo backed by annotation snapshots (`src/editor/undo.rs`)
- Page cache, thumbnails, navigation, zoom, Save/Save As with full-rewrite option
- Universal macOS packaging (`packaging/build-universal-app.sh`)

---

## 2. Proposed Module Layout

New/changed source files:

| File | Purpose |
|---|---|
| `src/text_model.rs` | `TextRun`, `TextLine`, and page text-edit model (plain data, no PDFium deps if possible) |
| `src/text_edit.rs` | Apply text replacements to a PDF page (PDFium object API or content-stream surgery behind one trait) |
| `src/text_font.rs` | Font coverage checking, fallback/subset embedding (Stage 1 keeps this minimal) |
| `src/document.rs` | Add text-edit persistence: re-read text after save, expose page text runs |
| `src/editor/text_tool.rs` | Text-edit tool: hit-testing, caret/selection, double-click-to-edit UI |
| `src/editor/undo.rs` | Extend snapshots to include text-edit state |
| `docs/TEXT_EDITING_TESTING.md` | Manual + automated test checklist (created after Phase 2) |

---

## 3. Stage 1 Architecture

```
PDF page (PDFium)  ──text page API──►  text runs / char boxes
                                          │
                                          ▼
UI click ──hit-test──► selected TextRun ──► in-place editor overlay
                                          │
                                     user types new text
                                          │
                                          ▼
                              TextEditCommand {page, run_id, new_text}
                                          │
                                          ▼
                         text_edit::apply_replacement()
                                          │
              ┌───────────────────────────┴──────────────────────────┐
              ▼                                                      ▼
   Path A: PDFium object API                              Path B: content stream rewrite
   (fast, simple PDFs)                                     (more compatible, harder)
                                          │
                                          ▼
                          invalidate page cache + mark dirty
                                          │
                                          ▼
                          Save (full rewrite) → re-open → verify
```

Stage 1 will evaluate Path A first, fall back to Path B where needed.
The UI and model are identical regardless of the apply path.

---

## 4. Phase Breakdown

### Phase 0 — Research Spike (1–2 days) — ✅ COMPLETE

**Decision: Path A (PDFium object API) works for Stage 1.**

Findings:

- [x] `pdfium-render` exposes `PdfPageTextObject` with `set_text()`.
- [x] `PdfPageTextChar::text_object()` maps a character back to its text object.
- [x] `PdfPage::regenerate_content()` is required after `set_text()` before save.
- [x] Saved and re-read `test.pdf`; the edit persisted as a real text change.
- [x] Bulletin PDF checked: each visible line is typically its own text object.
- [x] `lopdf`/content-stream surgery is **not required** for Stage 1; deferred.

Spike harness: `examples/text_edit_spike.rs`

Decision record: `docs/TEXT_EDITING_RESEARCH.md`

---

### Phase 1 — Text Selection & Hit-Testing (2–3 days) — ✅ COMPLETE

Make clicking text show exactly which line is under the cursor.

Tasks:

- [x] Added `TextRun` extraction in `src/text_model.rs` from PDFium's text page.
  - Fields: page, text, bbox, baseline, font size, first/last char index.
  - Group chars into lines by baseline Y (2.0 pt tolerance).
- [x] Cache extracted text per page; invalidated after page insert/delete/rotate.
- [x] Added `Tool::EditText` to the toolbar (icon `🔤`, label `Edit text`).
- [x] On click in `Tool::EditText`, hit-test lines and select the run.
- [x] Draw a blue selection rectangle around the selected line.
- [x] Unit tests for line grouping and hit-testing added in `src/text_model.rs`.
- [x] UI smoke test: selected a line on `test-search.pdf`; status showed
      `Text line selected…`.

**Deliverable met:** clicking a line with the Edit Text tool selects it and
shows a selection rectangle. (Caret/typing is Phase 2.)

---

### Phase 2 — In-Place Editing UI & Model (2–3 days) — ✅ CODE COMPLETE

Turn a selected line into an editable text field.

Tasks:

- [x] Added `TextEditState { page, run_index, original, buffer, font_size }`.
- [x] Double-click a line (or press Enter when selected) enters edit mode.
- [x] Rendered an egui `TextEdit` overlay sized to the line box.
- [x] Commit on Enter or click-away; Escape cancels.
- [x] Generalized `UndoStack` to snapshot annotations and text edits.
- [x] Modified lines show a white patch, new text, and a blue underline.
- [x] Dry-run edits are stored in `text_edits` and can be undone/redone.
- [x] UI smoke test on the unlocked Mac: selected **Edit text**, double-clicked a line, typed `Hello Mars`, pressed Enter, saw `Text edit staged`, and verified undo/redo visually.
- [ ] Toolbar font/size controls for text edits (deferred to Phase 4).

**Deliverable met:** typing over a selected line updates the UI model and
undo stack, but does not write the PDF yet (dry-run mode).

---

### Phase 3 — Applying Edits to the PDF (4–6 days, the core) — ✅ COMPLETE

Implement the apply strategy chosen in Phase 0.

Tasks:

- [x] Added `OpenDocument::apply_text_edit` (Path A).
  - Finds the run's PDF text object via `PdfPageTextChar::text_object()`.
  - Replaces the object text with `PdfPageTextObject::set_text()`.
  - Preserves leading/trailing whitespace in the object.
  - Regenerates the page content stream before save.
- [x] Added `OpenDocument::set_text_for_run` for undo/redo restoration.
- [x] Added `OpenDocument::refresh_render_docs` so background renders use the
      updated in-memory PDF after an edit.
- [x] Wired `commit_text_edit` to apply real PDF edits, clear caches, and
      refresh render docs.
- [x] Undo/redo now write object text back to the PDF and re-render.
- [x] Stage 1 limitation enforced: the line must map to a whole text object.
- [ ] Font fallback for missing glyphs (deferred to Phase 4).

**Deliverable met:** replacing a single line produces a real, saved PDF text
edit. Verified end-to-end: edited `test-search.pdf`, saved, reopened with
`print_text_runs`, and the text read back as the edited string.

---

### Phase 4 — Font Coverage & Fallback — REVERTED BY USER REQUEST

The fallback-font chain, glyph-mapping layer, and font-picker UI were removed.
The editor is back to the simpler behavior:

- Original-text edits are **ASCII-only**.
- The **original font and size are always preserved**.
- Non-ASCII edits are rejected with a clear message instead of being mangled.

Removed: bundled fonts, `ttf-parser`, vendored `pdfium-render`, font-picker
dropdown, forced-font path, and fallback undo/redo machinery.

## 5. Dependencies to Evaluate

| Crate | Role | Notes |
|---|---|---|
| `lopdf` | Read/rewrite content streams, object surgery | Pure Rust, good for Path B |
| `pdf-writer` | Write PDF objects and embed fonts | Clean writer API |
| `ttf-parser` | Font tables and glyph coverage | Lightweight |
| `fontdue` | Rasterization/shaping fallback if needed | Only if we render fallback text ourselves |

Decision to be finalized in Phase 0.

---

## 6. Risks & Mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| PDFium doesn't expose text-object editing | Blocks Path A | Use Path B (`lopdf`) |
| Font encoding is complex (CID/Type0) | Edited text garbled | Limit Stage 1 to simple fonts; fallback font |
| Reflow expectations | Users expect paragraphs to reflow | Stage 1 edits a single line only; document it |
| Content stream parsing bugs | Broken pages after edit | Backup object on undo; test on many PDFs |
| Performance on 1,800-page PDFs | UI lag | Extract text lazily per page and cache |
| Round-trip layout drift | Text moves slightly | Keep baseline/box from original run; verify |

---

## 7. Acceptance Criteria for Stage 1

1. A user can select a line of text with the Text Edit tool.
2. The line highlights and enters edit mode (double-click).
3. Typing replaces that line and undo/redo works.
4. Save (or Save As) writes a real text edit into the PDF.
5. Reopening the file in our editor and another viewer shows the new text.
6. Non-edited pages are unchanged.
7. `cargo fmt`, `cargo test`, and `cargo clippy --all-targets -- -D warnings` pass.
8. The app still packages as a Universal macOS app.

---

## 8. Future Stages (after Stage 1)

- **Stage 2:** multi-line blocks with basic reflow
- **Stage 3:** font substitution and full Latin shaping
- **Stage 4:** scanned-page OCR text edit
- **Stage 5:** advanced layout: columns, images, tables, rotated text
- **Stage 6:** PDF/A, encrypted PDFs, redaction

---

## 9. Definition of Done (per phase)

- [ ] Code compiles with no warnings (`clippy -D warnings`)
- [ ] Unit/round-trip tests pass
- [ ] Manual smoke checklist executed on macOS
- [ ] Findings and limitations written down in `docs/`
- [ ] README updated when user-facing behavior changes

---

## 10. Rung 2 — Word-Level Text Editing (proposed plan)

**Goal:** move from line-level text editing (Stage 1, complete) to *word-level*
editing: select and rewrite a single word, restyle it, and insert/delete words
— all with the original font intact. This targets "PDF Expert-plus" capability.

**In scope:** word selection, word replacement, font size/colour/style,
word insert/delete, splitting multi-line text objects.

**Out of scope (Rung 3 / Rung 4, separate plans):** non-ASCII editing and font
embedding/subsetting (Rung 3), paragraph reflow (Rung 4).

### Grounding — pdfium-render API findings

| Capability | API | Verdict |
|---|---|---|
| Rewrite a whole text object | `PdfPageTextObject::set_text()` | ✅ available |
| Font size change | `set_unscaled_font_size()` | ✅ available |
| Render mode (fill/stroke) | `render_mode()` / `set_render_mode()` | ✅ available |
| Font access | `font()` | ✅ available |
| Char-level bounds | `chars()` → `PdfPageTextChar::loose_bounds()` | ✅ available |
| Split one text object into two | — none — | ✅ done in §11 by hand: rewrite the parent's text, append prefix/suffix objects with the parent's font token, matrix and colours |

**Key risk:** editing a word inside a *multi-line* text object needs an object
split, which pdfium-render does not expose directly. Single-line objects are
the common case and can be handled by rewriting the object's full text.

### Phase R2-1 — Word model + selection (foundation, low risk) — ✅ COMPLETE

- Extend `text_model.rs` with a word model: group each `TextRun`'s characters
  into words with per-word bounds and char ranges. ✅ (`Word` struct + `words`
  field, whitespace-delimited grouping in `build_words`.)
- Extend `Tool::EditText` click handling: select a *word* (not the whole line). ✅
  (`selected_word: (page, run_index, word_index)`; click hit-tests words.)
- Draw a word-level selection highlight. ✅ (filled highlight + outline in
  `central_panel`.)
- No PDF mutation; purely model + UI. Add unit tests for word grouping/hit-test.
  ✅ (`groups_words_within_a_run`.)

**Deliverable:** clicking with Edit Text selects a single word; the line-level
double-click-to-edit keeps working.

### Phase R2-2 — Edit a word (medium) — ✅ COMPLETE

- Map the selected word to its text object + char range (via `chars()`). ✅
  (`apply_word_edit` locates the word's text object by first char and replaces
  the word's text within the object string.)
- Rewrite the object text with the word replaced (`set_text`), preserving the
  rest of the object's characters. ✅
- Reuse the existing `TextEdit` undo; refresh render docs + caches. ✅
  (`TextEditState.word_index`; word edits snapshot the full line before/after.)
- Multi-line objects: fall back to whole-object rewrite, or show a clear
  "not yet supported" message. ✅ Superseded by §11 — the word is now located by
  its own character offset inside the object, never by searching for its text.

**Deliverable:** double-clicking a word and typing replaces just that word in
the saved PDF. (`word_edit_replaces_a_word` test; UI: double-click a selected
word edits it in a word-sized overlay.)

### Phase R2-3 — Font size / colour / style (medium) — ⚠️ PARTIAL (size + bold)

- Apply `set_unscaled_font_size` to the selected word/line. ✅ (unblocked by the
  pdfium upgrade 7763 → 8125; `text_object_font_size` / `set_text_object_font_size`
  added.)
- Apply `set_render_mode` for outline/fill style. ✅ a "Bold" toggle (Fill vs
  Fill+Stroke) is wired into the toolbar and undoable.
- Colour via content-stream or object colour state. ❌ Deferred — text objects
  have no colour setter in pdfium-render; needs content-stream surgery (Rung 3).

**Deliverable:** a selected original-text word/line can be resized and toggled
bold; both persist and are undoable. Colour remains deferred.

### Phase R2-4 — Insert / delete words (medium) — ⚠️ PARTIAL (delete only)

- Caret-based insert: place a caret in a line and type (inserts at caret). ❌
  **Deferred** — a caret model is a larger change; whole-line editing already
  covers most insertion needs.
- Delete selected word (backspace/delete). ✅ Selecting a word (🔤 Edit text)
  and pressing Delete/Backspace removes it, undoable (reuses `apply_word_edit`
  with an empty replacement and the `TextEdit` snapshot).
- No reflow — the edited line grows/shrinks in place. ✅ (unchanged behaviour).

**Deliverable:** a selected word can be deleted with Delete/Backspace and the
change persists. Caret-based insertion remains deferred.

### Phase R2-5 — Split multi-line text objects (hard, optional) — ⏸ DEFERRED

Investigating this phase found it is both the hardest and the rarest case:

- pdfium-render exposes **no split API**; splitting means creating a new text
  object with `PdfPageTextObject::new`, copying `font()` / `set_unscaled_font_size`
  / `set_render_mode` / the matrix, and repositioning by per-char `origin()`.
- The bundled test PDFs contain **no** multi-line text objects (every object is
  a single visual line), so there is nothing to verify against.
- The common cases already work: `apply_word_edit` targets the correct object
  via the word's `first_char_index` → `text_object()`, even when one visual
  line spans several text objects.

**Decision:** defer full splitting until a real PDF demands it. The known
limitation is: a *single text object containing several lines* is edited by
replacing the first textual occurrence of a word (ambiguous only when the same
word appears twice in that object), and whole-line edits on such objects still
reject with "line shares a text object with other lines".

### Rung 2 risks

| Risk | Impact | Mitigation |
|---|---|---|
| No object-split API | Blocks word edits in multi-line objects | Phase R2-5 (create new object + copy state), or fall back to whole-object rewrite |
| Word boundaries are ambiguous (kerning, justification) | Mis-selected words | Use char bounds from `chars()`; snap to whitespace |
| Font re-encoding for new characters | Garbled output | Keep ASCII-only for now (same as Stage 1) |
| Edit drift on round-trip | Text shifts | Preserve original char boxes/baseline; verify after save |

### Rung 2 acceptance criteria

1. Click selects a word; double-click edits that word.
2. Editing a word produces a real, saved PDF change.
3. Font size/colour changes apply to the selected word/line and persist.
4. Insert/delete words work within a line.
5. Undo/redo covers all of the above.
6. Non-edited content is unchanged; `cargo test` and clippy stay green.

## 11. Free text dragging — word/span moves (implemented)

**Goal:** stop dragging whole lines only. In a PDFGear-like editor you drag a
word, or any run of characters, wherever you like and the rest of the line
stays put.

**Why it was line-only:** the editor drives PDFium's *page object* API. One
`PdfPageObject::Text` is one text-showing chunk of the page content stream, and
Word / LibreOffice / LaTeX / InDesign emit roughly one chunk per visual line, so
`translate_text_object()` can only ever move a line. PDFium has no sub-object
model — there is no "move word" call to expose. A word becomes free to move only
once it *is* an object.

### 11.1 Precise object lookup (done)

`text_object_index_for_char()` resolves a character to the object that really
owns it, using PDFium's own character→object table (`chars_for_object`), not by
comparing object text. It replaces the old `position(|obj| t.text() == …)` match,
which silently picked the first lookalike line — `Name:`, `--`, page numbers,
form labels — and moved or edited the wrong one.

Call sites now routed through it:

| Call site | Before | Now |
|---|---|---|
| `text_object_index_for_run` | first object whose text matched | owning object |
| `apply_text_edit` | first object whose text matched | owning object |
| `apply_word_edit` | first match by text, then `find(word)` in the string | owning object + `object_text_byte_offset()` |

`text_object_indices_for_char_range()` returns *every* object that owns part of a
character range, so a line that was split across several objects (ligatures,
kerning runs, colour changes, superscripts) moves as one piece instead of tearing.

### 11.2 Split-then-translate (done)

`split_text_object_for_span(page, first_char, last_char)` turns the span into its
own object without changing a single pixel:

1. The span stays in the original object (`span_index`), whose text is set to
   just the span.
2. The text before and after it is copied into two new objects that copy the
   parent's **font token, unscaled size, matrix, fill colour, stroke colour and
   render mode** — so nothing is re-encoded, re-styled or re-subset.
3. Both new objects are offset by the span's / suffix's character origin
   (`PdfPageTextChar::origin()`), so they land exactly where those glyphs were —
   correct for rotated and scaled text too, not just unrotated lines.

`translate_text_objects()` then moves a set of objects with a single content
regeneration, so a multi-object drag stays one cheap operation.

### 11.3 Editor behaviour (done)

| Gesture | Result |
|---|---|
| Click a word | selects it (starts a new selection) |
| **Shift-click another word** | selects the whole reading-order range between the anchor and that word — a phrase, part of a line, or across lines |
| Drag a selected word | moves the **whole selection as one group**; each line's remainder stays put |
| Drag a word that is not selected | starts a new single-word selection and drags it |
| Drag elsewhere on a line | move every object the line is made of |
| Drag with no text under the pointer | moves the current selection |
| Double-click a word / line | unchanged word / line rewrite |

The ghost preview draws every selected word for a group drag (glyph previews are
shown for a single word only, where they are readable) and the line for a line
drag. `DocumentEdit::TextMove` carries `moves: Vec<TextObjectMove>`, so a
multi-object drag is still a single undo step.

### 11.3.1 Selection grouping

`split_selection_for_move(page, ranges)` takes one `(first, last)` character
range per line and splits each object's share separately, because a selection can
straddle text objects: select "the quick **brown** fox" and the phrase may start
in one object and end in another. The editor groups the selected words by run,
takes the min/max character index per run, and moves every resulting span with a
single `translate_text_objects()` call.

### 11.3.2 Gotcha: `text()` is not sliceable

`PdfPageTextObject::text()` can contain characters the object's character list
does not have — after `set_text()`, PDFium reports an inferred trailing space
(`"PDF "` for three characters). Slicing that string by character position would
therefore mis-slice the text, so the split builds its pieces from the character
list's Unicode values instead (`object_chars_layout()`), and validates the result
by comparing the rejoined text against `text()` with whitespace ignored.
Objects whose characters have no Unicode value are refused rather than mangled.

If the split cannot be done (font whose characters do not map one-to-one), the
drag degrades to the old line move with a clear status message instead of
failing.

### 11.4 Tests

| Test | Guards |
|---|---|
| `run_lookup_finds_the_owning_object` | every run resolves to an object that owns its characters |
| `splitting_a_word_keeps_the_line_in_place` | the three pieces rejoin exactly; prefix does not move; span moves by the requested delta |
| `splitting_a_word_changes_nothing_on_screen` | identical character origins and a pixel-level render match after the split |
| `line_move_covers_every_object_of_the_line` | all objects of a line move together |
| `moved_word_survives_save_and_reload` | the moved word is at its new position after save → reload |
| `multi_word_span_moves_as_one_piece` | a phrase splits into one span, moves as a unit, and its line's other words stay |
| `selection_across_objects_is_split_per_object` | a range crossing object boundaries is split per object and still moves as one group |
| `two_column_layout_has_one_run_per_line` | two columns sharing baselines stay separate lines, one object each, no `\r` in text |
| `line_drag_stays_within_its_column` | dragging a left-column line does not move the right column |
| `multi_line_selection_moves_every_selected_line` | a selection spanning both columns moves every selected line, and only those |
| `reading_order_is_column_major` / `…_top_down_for_a_single_column` | selection order is column-major |
| `line_fragments_move_with_their_line` | a line drawn as two objects moves as one line |
| `multi_line_drag_carries_all_fragments` | every fragment of every selected line moves |
| `tight_invented_space_does_not_split_a_word` | invented whitespace across a tight fragment boundary stays inside the word |
| `space_sized_gaps_still_split_words` | real word gaps still separate words |
| `reflow_closes_the_gap_of_a_removed_line` | lines below a removed line shift up by one leading |
| `reflow_keeps_paragraph_spacing` / `reflow_leaves_a_footer_alone` | spacing between blocks survives; a footer is not dragged up |
| `reflow_keeps_the_other_column_alone` | the neighbouring column does not move |
| `partly_selected_lines_do_not_reflow` | a partly selected line keeps its place |

### 11.4.1 Two-column layouts and paragraph selections (fixed)

Reported symptoms: "it selects a whole paragraph but only the top line moves",
and "does not work in two columns". Three separate defects were behind them, all
in how characters were grouped and mapped to objects:

1. **PDFium's synthetic line breaks were treated as text.** Its text page contains
   `\r` / `\n` characters that belong to no text object. They were being pushed
   into runs (so run text contained `\r\n`) and into word splitting.
   → `group_chars_into_runs()` now cuts the character stream into blocks at any
   control character, then groups each block by baseline. A run therefore never
   spans a line break, and the block boundary is PDFium's own idea of "new line".

2. **Runs were grouped by baseline alone, so the two columns fused.** Left and
   right column text share baselines, so one "run" covered both — with a text
   string `"Left column…\r\nRight column…"` and a character index range
   `0..157` that swept up every line in between. Selecting that run selected the
   paragraph, and dragging it moved the wrong objects.
   → Blocks fix the common case; a fallback geometry break (gap > 2em) covers
   producers that emit no line break between columns.

3. **Object resolution and splitting used `first..=last` index ranges.** A range is
   only meaningful when a selection's characters are contiguous page indices,
   which two-column text and merged runs are not.
   → `TextRun` and `Word` now carry the actual character set (`chars`), and the
   document API takes sets: `text_object_indices_for_chars(page, chars)` and
   `split_selection_for_move(page, groups)`. The editor passes `run.chars`
   (line drag) and each selected line's word characters (group drag). Range-based
   `text_object_indices_for_char_range()` is gone.

Selection *order* was also line-by-line across the whole page, so shift-clicking
down the left column picked up the right column's lines too. `reading_order()`
now clusters runs into columns by horizontal overlap and orders column-major
(top to bottom inside a column), which leaves single-column documents unchanged.

Verified on a generated two-column PDF (`examples/make_layout_pdf.rs`): each of
the 7 lines is its own run backed by exactly one object; dragging one left-column
line leaves the right column untouched; a 3-line selection spanning both columns
moves all three and nothing else.

### 11.4.2 Drags leaving the first letters behind (fixed)

Reported on a real two-column bulletin: selecting two lines and dragging left the
first letter or two of a line behind ("A_", "tr" in the screenshot). Two causes:

1. **`set_text()` shortens the page's character stream.** When a split rewrote an
   object's text, PDFium's *inferred* trailing space disappeared with it. The page
   then had one character fewer, so every later character index shifted by one —
   and the next line's split, computed from indices taken before the rewrite,
   landed one character too far in. Its first character therefore stayed in the
   unselected prefix. With several lines the error accumulated (1, 2, 4, … chars
   left behind), which is exactly the pattern that was reported.
   → Splitting is now **plan-then-apply**: every split is planned from the
   untouched page (`plan_object_split()`), and only then applied
   (`apply_object_split()`), so no index is ever read after a mutation. A selection
   that covers a whole object no longer rewrites its text at all.

2. **PDFium invents a space at fragment boundaries.** Any gap wider than ~0.15em
   between two text objects becomes a space character belonging to no object, so a
   line drawn as fragments ("tr" + "ust.") was modelled as two words — dragging one
   left the other behind.
   → Invented, unowned whitespace separates words only when the gap around it is
   really space-sized (≥ 0.2em). Real space characters still always separate.

A split also no longer leaves debris behind: the source piece of a moved span keeps
its glyphs only if it still has some, and a piece that would hold nothing but
whitespace is erased (`set_text("")`) or not created at all, so the page is not left
with blank text objects after a drag.

With those two fixes the reported gesture works on the document that showed it:
selecting from "Leader: When we stand…" through "…humble" produced six complete
line spans and moved them as one group, with nothing left at the source.

### 11.4.3 Closing the gap: paragraph reflow (implemented)

Dragging a whole line away used to leave a hole exactly as tall as the line.
`reflow_after_removal()` now closes it: everything below the removed line **in the
same column** moves up by the document's own leading, so the paragraph — and
everything under it — slides up as one.

Details that make it behave on real documents:

- **Leading is measured, not assumed.** The column's line pitch is the lower
  quartile of its baseline gaps, because templates routinely set text looser than
  1.2 em (the reported bulletin uses 11 pt type on a 17–20 pt pitch).
- **The whole hole closes.** Everything below moves up by the removed line's entire
  vertical slot — its pitch *plus* whatever paragraph break followed it. Shifting
  by one leading only is not enough: a removed line that sat above a paragraph
  break leaves the break behind as a hole under the heading that preceded it.
- **Wide gaps are respected.** A gap over 3× the leading (a footer, a page number)
  is treated as a separate block and stays put; the whitespace above it absorbs the
  shift. Removing a line never drags a footer up.
- **Only whole lines count.** A line counts as removed only when *all* its runs'
  words are selected. Move one word out of a line and the line keeps its place —
  otherwise the rest of the sentence would be pulled over the hole.
- **One undo step.** The shifts are recorded as extra matrix moves in the same
  `TextMove` as the drag, so a single undo restores both the text and the layout.

What this is *not*: it does not re-wrap or re-break lines, and it does not insert
the dropped text into a paragraph at the destination. Moving a line out of the
middle of a paragraph shortens that paragraph without pulling words up from the
line below; proper re-wrapping needs line breaking with the embedded font's metrics
(Stage 4 in the original plan).

### 11.4.4 Moving justified text (root cause found, fix proven)

Dragging a line inside a **justified** paragraph (`The Lord's Prayer` on the test
bulletin) made it render spread across the page, while text extraction still
reported normal positions and widths — which is why it first looked like a
character-to-object mapping bug. It is not: every line maps to exactly one object.

The cause is PDFium's page-object API. A moved object is re-serialised on
`FPDFPage_RegenerateContent()`, and for justified text that re-serialisation
re-emits the line's justification as extreme inter-word adjustments. Ragged-right
text is unaffected, which is why the left column always moved perfectly.

Attempts that do **not** work, all reverted:

| Attempt | Result |
|---|---|
| One regeneration for a whole batch (`PdfPageContentRegenerationStrategy::Manual`) | no change |
| Stretch the moved line back to its original width (matrix scale about its left edge) | width restored exactly, spacing still wrong |
| Re-issue the object's own text before moving (`set_text`) | worse: PDFium re-parsed the page and split lines into fragments |

What **does** work, proven on the bulletin: *author a fresh object* instead of
moving the parsed one. Read the line's text, font token, size, matrix, colours and
its exact width; create a new text object at the target; stretch it to the original
width; blank the original (`set_text("")`, which keeps all object indices stable).
Objects we author render cleanly, and the stretched copy keeps the line's justified
extent, so a moved justified line is indistinguishable from its neighbours.

Making that the move path is a contained but real refactor, because the current
undo records *matrix* snapshots (`TextMove`) — a redrawn line has no such matrix.
It needs a `TextRedraw` record (original index + text + matrix, plus the copy's
index), the split path to author its first piece as a fresh object too, and the
matrix-based move tests rewritten to assert geometry.

### 11.4.5 Re-authoring moved lines: justified text fixed, slots still off

`Document::repair_moved_text()` re-authors every line a gesture moved as a fresh
object, stretched back to the width it had *before* the gesture, and blanks the
original. `RedrawnLine` + `restore_redrawn_line()` make it reversible and `TextMove`
carries the records, so one undo restores text and layout.

Three defects were found by isolation and fixed:

1. **The copy drifted 8.05 pt right** — exactly the justified-vs-natural width
   difference. The stretch composition shifts the origin, so horizontal placement is
   now set absolutely (`matrix.e() += item.left - copy_left`) after stretching.
   Verified: the copy's box is `435.66..741.95`, identical to the original's.
2. **Copies were located by a predicted index**, which regeneration invalidates.
   They are now found by text + nearest position.
3. **Repairing stage by stage doubled every line.** Fixed by repairing
   **once per gesture**: every index the gesture moved (dragged lines, gap-closing
   lines, slot lines) is collected with its pre-gesture width, and a single
   `repair_moved_text()` call runs at drop time. That matches the case verified in
   isolation (1–3 lines in one call render cleanly).

Result on the test bulletin: dragging a line in the justified right column now moves
it **without** spreading it across the page — the paragraph's other lines stay
typeset correctly.

**Drop slots stay off** (`DROP_SLOTS_ENABLED = false`) although the logic is
complete and unit-tested. Slot pushing moves *more* lines, and the end-to-end check
still produced overlaps where the dropped line lands on an existing baseline (the
slot excludes lines at the drop baseline itself). That needs its own pass.

### 11.4.6 Mid-word splits: fixed

Reported from a screenshot: dragging picked up `Ani` and left `ta Thompson`, or moved
`The Lord's Pray` and left `er` behind. Cause: PDFium invents spaces at text-object
boundaries, so a word can be modelled as two tokens (`Ani` + `ta`); a selection made
at that boundary split mid-word.

`plan_object_split()` now **grows the span to whole tokens** before splitting: the
start extends left while the preceding character is not whitespace, and the end
extends right until whitespace. A selection inside a token takes the whole token with
it; a selection on whole words is unchanged. Verified on the bulletin: a partial
selection of "Anita" now moves `"Anita"` in full, and on the fixture a two-character
selection of "PDF" moves `"PDF"` (test `selection_mid_word_moves_the_whole_word`).

### 11.4.7 Slot pushing: enabled

Dropping text now opens a slot for it: the destination lines move down by the dropped
block's height, so nothing lands on top of anything.

Two refinements made it correct on a real document:

- **A line sitting at the drop baseline is pushed too** — otherwise text dropped
  exactly onto a line lands on it.
- **A merged line moves only its stationary runs.** When a dropped line shares a
  baseline with existing text, `visual_lines()` fuses them into one line; that line's
  non-dropped runs are what move, never the dropped ones (which would double-shift).

Verified on the bulletin: dropping a line onto `Our Father, who art in heaven…` pushed
14 runs down 17.04 pt, the dropped line took the vacated baseline, the paragraph
stayed justified and typeset, and nothing overlapped. Test:
`dropping_onto_a_line_pushes_it_too`.

`DROP_SLOTS_ENABLED` is now `true`.

### 11.4.8 Fragments join their line by text object

`UMH #514` and `"Stand Up, Stand Up for Jesus"` are two halves of **one text object**
that PDFium's line detection reports as two blocks, so they travelled separately
(selecting the song line left the code behind, and vice versa).

`text_runs()` now merges runs that (a) share the same owning text object — resolved
per run with `text_object_index_for_char` — and (b) sit on the same baseline. Verified
on the bulletin: the line is now a single run of 8 words spanning `36.0..378.3`, so
selecting, dragging or shifting it takes the whole line.

Not covered by this: a right-aligned **name** such as `Jan Rinehart` lives in its own
text object, so it stays a separate line. The next signal for it is alignment, not
geometry: such a fragment's right edge matches the column's right edge to within a
point (381.0 here), which is what distinguishes it from the neighbouring column.

### 11.5 Known limitations (still open)

- **Delete removes one word.** With a group selected, Delete/Backspace acts on the
  anchor word only; multi-word delete needs a per-run undo record.
- **Undo leaves the object split.** Undo/redo restore matrices, so the page looks
  exactly as before, but the split objects remain. Visually lossless; the page
  keeps one extra object per side.
- **Text inside Form XObjects** (headers, footers, watermarks, some InDesign and
  print exports) is not listed by `page.objects()`, so it cannot be selected or
  dragged at all. Needs a separate content-stream walk.
- **No re-wrapping at the destination.** Dropped text is redrawn where it lands as
  its own object; it is not inserted into the paragraph under the pointer and
  re-broken. The source side now closes up (§11.4.3), the destination side does not.
- **Cross-page drops** are not supported: this is still a page-object mover.
- **Justification / per-character kerning** inside a split line is inherited from
  PDFium's parsed object text. The split places pieces at their true origins, so it
  is lossless at split time, but any spacing PDFium already drops on
  `regenerate_content()` stays dropped — pre-existing for every move or edit.

### 11.6 Next: the text-block model (PDFGear parity)

To move text *anywhere* — other column, other page, with the original spot wiped:

1. Add `TextBlock { rect, baseline, font_ref, text, source_objects }` to
   `text_model.rs`, grouped from runs.
2. Drag changes block geometry only; commit on release.
3. On commit, erase the source (real removal of the show-text operators, not just
   a white rectangle, so copy-paste cannot leak the old text) and emit a new text
   object at the drop point with the block's font.
4. Handle the fonts that cannot be re-embedded, and cross-page moves.
