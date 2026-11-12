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
| Split one text object into two | — none — | ❌ not exposed; hardest part |

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
  "not yet supported" message. ⚠️ Partially handled — word is matched by first
  occurrence in the object; multi-line objects work only when the word text is
  unambiguous.

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
