//! The editor application: UI, canvas interaction, and document operations.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};

use eframe::egui;
use pdf_viewer::annotations::{Annotation, AnnotationKind, StrokeStyle};
use pdf_viewer::text_model::{
    TextRun, Word, reflow_after_removal, right_margin_fragments, room_for_drop, visual_lines,
};
use pdf_viewer::{
    BookmarkNode, CompressionOptions, CompressionPreset, CompressionStats, EncryptionOptions,
    FormFieldInfo, FormFieldType, HeaderFooterOptions, LinkTarget, OpenDocument, PageLinkInfo,
    PdfMetadata, RedrawnLine, SearchHit, StampPosition, TextFontFamily, WatermarkOptions,
    render_page, render_thumbnail, search_all_pages,
};
use pdfium_render::prelude::*;

use crate::tools::{DragState, Interaction, ResizeCorner, Tool, annotation_from_drag};
use crate::undo::{
    DocumentEdit, FormFieldEdit, ImageCrop, ImageMove, ImageReplace, ImageRotate, TextColor,
    TextEdit, TextMove, TextObjectMove, TextStyle, UndoStack,
};

mod image_tools;
mod search;
mod ui;

const GAP: f32 = 16.0;

/// Whether a drop opens a slot for the moved text (push-down insertion).
///
/// The logic is complete and unit-tested (`text_model::room_for_drop`), but it is
/// off: opening a slot moves *other* lines through PDFium's page-object API, and on
/// documents with **justified** text that re-serialisation re-emits the line with
/// inflated inter-word spacing, so the line renders spread across the page. Until
/// moving justified text is fixed (content-stream level edits, or cut-and-re-emit),
/// leaving the text where it was dropped is the lesser evil.
const DROP_SLOTS_ENABLED: bool = true;

/// Which build is running.
///
/// The packaged app bakes this in at compile time; macOS happily re-activates an
/// already running process when a bundle is replaced on disk, so "I rebuilt and
/// reopened it" is not proof that the new code is on screen.
fn build_stamp() -> &'static str {
    option_env!("BUILD_STAMP").unwrap_or("dev")
}

/// A rendered page waiting to be uploaded to the GPU on the UI thread.
type PendingTexture = Arc<Mutex<Option<(usize, f32, egui::ColorImage)>>>;

#[derive(Clone)]
enum ClipboardItem {
    Annotation(Annotation),
    Text {
        text: String,
        family: TextFontFamily,
        font_size: f32,
        color: [u8; 4],
    },
    Image {
        png_bytes: Vec<u8>,
        width: f32,
        height: f32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignatureTab {
    Saved,
    Draw,
    Type,
}

/// A pending confirmation dialog.
#[derive(Debug, Clone)]
enum PendingConfirm {
    /// The user asked to quit; confirm before discarding changes.
    Close,
    /// The user asked to open `PathBuf`; confirm before discarding changes.
    Open(PathBuf),
    /// Permanently apply all marked redactions.
    ApplyRedactions,
    /// Permanently flatten annotations and form fields on the current page.
    FlattenPage,
    /// Permanently flatten annotations and form fields on all pages.
    FlattenAll,
}

/// An in-progress drag of original text.
///
/// Each target is one text object plus the matrix it had when the drag started,
/// so the whole gesture becomes a single undoable step. A word drag has one
/// target (the span that was split out of its line); a line drag has one target
/// per text object the line is made of.
struct TextDrag {
    page: usize,
    targets: Vec<TextObjectMove>,
    /// Lines shifted once, at drag start, to close the gap the selection leaves.
    reflow: Vec<TextObjectMove>,
    /// Pre-move widths of the lines shifted to close that gap.
    reflow_widths: Vec<(usize, f32)>,
    /// Widths of the dragged objects before the gesture, for the (disabled)
    /// re-authoring repair that keeps justified lines intact.
    #[allow(dead_code)]
    widths: Vec<(usize, f32)>,
    /// True when the drag moves a word or span rather than a whole line.
    word_drag: bool,
}

/// In-progress original-text edit for a single visual line or word.
struct TextEditState {
    page: usize,
    run_index: usize,
    /// `Some` when editing a single word (index within the run); `None` for a
    /// whole-line edit.
    word_index: Option<usize>,
    original: String,
    buffer: String,
    font_size: f32,
}

/// In-progress AcroForm text-field edit.
struct FormEditState {
    page: usize,
    field_index: usize,
    original: String,
    buffer: String,
    /// Whether the field is a multi-line text field.
    multiline: bool,
}

/// A completed drag-text selection.
struct TextSelection {
    page: usize,
    /// Selected words as (run_index, word_index).
    words: Vec<(usize, usize)>,
    /// Selected text, lines joined with `\n`.
    text: String,
}

#[derive(Clone)]
struct CachedPage {
    scale: f32,
    texture: egui::TextureHandle,
}

pub struct EditorApp {
    pdfium: &'static Pdfium,
    document: Option<OpenDocument>,
    error: Option<String>,

    current_page: usize,
    zoom: f32,
    fit_width: bool,
    last_available_width: f32,

    pending: PendingTexture,
    rendering: Arc<Mutex<Vec<usize>>>,
    cache: Vec<(usize, CachedPage)>,

    show_thumbs: bool,
    show_help: bool,
    /// Show the OCR-result preview panel.
    show_page_text: bool,
    /// Show the AcroForm fields panel.
    show_forms: bool,
    /// Recognized text per page, keyed by page index (filled by OCR).
    ocr_results: HashMap<usize, String>,
    thumb_pending: Arc<Mutex<Vec<(usize, egui::ColorImage)>>>,
    thumb_rendering: Arc<Mutex<Vec<usize>>>,
    thumbs: Vec<(usize, egui::TextureHandle)>,
    stamp_textures: HashMap<String, egui::TextureHandle>,

    page_input: String,

    // ---- search ----
    search_query: String,
    search_hits: Vec<SearchHit>,
    /// Index into search_hits of the currently selected hit.
    search_selected: usize,
    /// Set when we want the scroll strip to centre on the selected hit.
    pending_hit_scroll: Option<usize>,
    /// Set by ⌘F; the toolbar picks it up next frame to focus the field.
    search_should_focus: bool,
    search_field_id: Option<egui::Id>,
    /// Last query actually run through pdfium, to avoid re-searching per frame.
    last_searched: String,
    /// Receiver for an in-flight background search, tagged with its generation.
    search_rx: Option<mpsc::Receiver<(u64, Vec<SearchHit>)>>,
    /// Incremented each new search so stale results are discarded.
    search_gen: u64,
    /// Debounced query waiting to run.
    search_pending_query: Option<String>,
    /// Earliest time the debounced query may run.
    search_debounce_deadline: Option<std::time::Instant>,

    // ---- editing ----
    tool: Tool,
    color: [u8; 4],
    fill: Option<[u8; 4]>,
    stroke_width: f32,
    font_size: f32,
    interaction: Interaction,
    selected: Option<(usize, u64)>,
    /// Cached original-text runs per page (Stage 1 text editing).
    text_runs: Vec<Vec<TextRun>>,
    text_runs_valid: Vec<bool>,
    /// Currently selected original-text run: (page, run index).
    selected_text_run: Option<(usize, usize)>,
    /// Selected original-text words, in reading order: (page, run, word).
    ///
    /// One word is a normal click; shift-click extends the selection, and the
    /// whole set is dragged as one group.
    word_selection: Vec<(usize, usize, usize)>,
    /// The word a shift-click range is measured from.
    word_anchor: Option<(usize, usize, usize)>,
    /// Original text being dragged, with the objects and start matrices for undo.
    text_drag: Option<TextDrag>,
    /// Accumulated drag offset in PDF points, for the live ghost preview.
    text_drag_offset: (f32, f32),
    /// Whether moving text automatically shifts surrounding lines (off by default).
    auto_reflow: bool,
    /// Currently selected existing image: (page, object index).
    selected_image: Option<(usize, usize)>,
    /// Existing image being dragged: (page, object index).
    image_drag: Option<(usize, usize)>,
    /// Matrix of the dragged image when the drag started, for move undo.
    image_drag_start_matrix: Option<PdfMatrix>,
    /// Receiver for an in-flight image extraction save dialog.
    image_extract_pending: Option<mpsc::Receiver<Option<PathBuf>>>,
    /// Receiver for an in-flight image replacement open dialog: page, object
    /// index, and the decoded bytes of the chosen file.
    image_replace_pending: Option<mpsc::Receiver<Option<(usize, usize, Vec<u8>, Vec<u8>)>>>,
    /// True while the user is dragging a crop rectangle over the selected image.
    crop_mode: bool,
    /// Start point of an in-progress crop drag: (page, object index, screen pos).
    crop_start: Option<(usize, usize, egui::Pos2)>,
    /// Current pointer position of an in-progress crop drag.
    crop_current: Option<egui::Pos2>,
    /// Dry-run original-text edits applied in the UI model (Phase 2).
    text_edits: Vec<TextEdit>,
    /// Active in-place edit state.
    text_edit_state: Option<TextEditState>,
    /// Screen rect where the in-place editor should be drawn.
    text_edit_rect: Option<egui::Rect>,
    /// Set when the in-place editor should request focus next frame.
    text_edit_should_focus: bool,
    /// Cached AcroForm fields (refreshed on load and after edits).
    form_fields: Vec<FormFieldInfo>,
    /// Active in-place form-field edit state.
    form_edit_state: Option<FormEditState>,
    /// Screen rect where the in-place form-field editor should be drawn.
    form_edit_rect: Option<egui::Rect>,
    /// Set when the in-place form-field editor should request focus next frame.
    form_edit_should_focus: bool,
    /// Selected form field (page, field index) highlighted on the page.
    selected_form_field: Option<(usize, usize)>,
    /// In-progress text-selection drag: (page, start screen pos, current screen pos).
    text_select_drag: Option<(usize, egui::Pos2, egui::Pos2)>,
    /// The current completed text selection.
    text_selection: Option<TextSelection>,
    undo: UndoStack,
    dirty: bool,
    status: String,

    /// Text being typed for a new free-text annotation.
    text_buffer: String,
    /// Receiver for an in-flight native file dialog.
    open_pending: Option<mpsc::Receiver<Option<PathBuf>>>,
    save_pending: Option<mpsc::Receiver<Option<PathBuf>>>,
    image_pending: Option<mpsc::Receiver<Option<PathBuf>>>,
    /// Receiver for an in-flight "Merge PDF" file picker.
    merge_pending: Option<mpsc::Receiver<Option<PathBuf>>>,
    /// Receiver for an in-flight "Extract pages" save dialog.
    extract_pages_pending: Option<mpsc::Receiver<Option<PathBuf>>>,

    /// Selected pages (0-based) for multi-page organizer actions.
    selected_pages: HashSet<usize>,
    /// Page index currently being dragged in the thumbnail sidebar.
    page_drag_source: Option<usize>,
    /// Whether the "Crop page margins" window is open.
    show_crop_page: bool,
    /// Pending page crop margins in PDF points: `[left, bottom, right, top]`.
    crop_margins: [f32; 4],
    /// Whether page cropping applies to all pages instead of just the current page.
    crop_all_pages: bool,

    /// Set when we want the ScrollArea to jump to a page next frame.
    scroll_target: Option<usize>,
    ignore_next_scroll: bool,

    /// Screen rects of the pages as laid out last frame, used for hit-testing.
    page_rects: Vec<egui::Rect>,

    /// Pending confirmation, if any.
    confirm: Option<PendingConfirm>,

    // ---- signatures & flatten ----
    show_signatures: bool,
    sig_tab: SignatureTab,
    saved_signatures: Vec<crate::persist::SavedSignature>,
    sig_textures: HashMap<String, egui::TextureHandle>,
    sig_draw_strokes: Vec<Vec<(f32, f32)>>,
    sig_draw_active: Option<Vec<(f32, f32)>>,
    sig_name: String,
    sig_type_text: String,
    sig_type_style: usize,
    sig_color: [u8; 4],
    sig_stroke_width: f32,
    sig_import_pending: Option<mpsc::Receiver<Option<PathBuf>>>,
    show_flatten: bool,

    // ---- bookmarks, headers/footers, watermarks, hyperlinks ----
    show_bookmarks: bool,
    bookmarks: Vec<BookmarkNode>,
    show_headers_footers: bool,
    hf_position: StampPosition,
    hf_text: String,
    hf_font_size: f32,
    hf_color: [u8; 4],
    hf_margin_x: f32,
    hf_margin_y: f32,
    hf_start_number: usize,
    hf_all_pages: bool,
    hf_from_page: usize,
    hf_to_page: usize,
    show_watermark: bool,
    wm_text: String,
    wm_font_size: f32,
    wm_color: [u8; 4],
    wm_angle: f32,
    wm_all_pages: bool,
    page_links: Vec<Vec<PageLinkInfo>>,
    link_drag: Option<(usize, egui::Pos2, egui::Pos2)>,
    pending_link_rect: Option<(usize, f32, f32, f32, f32)>,
    link_target_is_page: bool,
    link_url_input: String,
    link_page_input: usize,
    clipboard: Option<ClipboardItem>,
    show_metadata: bool,
    meta_draft: PdfMetadata,
    show_compress: bool,
    compress_preset: CompressionPreset,
    compress_max_dpi: f32,
    compress_quality: u8,
    compress_stats: Option<CompressionStats>,
    show_security: bool,
    security_user_pw: String,
    security_owner_pw: String,
    security_allow_print: bool,
    security_allow_copy: bool,
    security_allow_modify: bool,
    security_allow_annotations: bool,
    password_prompt_path: Option<PathBuf>,
    password_prompt_input: String,
    password_prompt_error: Option<String>,
}

impl EditorApp {
    pub fn new(pdfium: &'static Pdfium) -> Self {
        Self {
            pdfium,
            document: None,
            error: None,
            current_page: 0,
            zoom: 2.0,
            fit_width: true,
            last_available_width: 0.0,
            pending: Arc::new(Mutex::new(None)),
            rendering: Arc::new(Mutex::new(Vec::new())),
            cache: Vec::new(),
            show_thumbs: true,
            show_help: false,
            show_page_text: false,
            show_forms: false,
            ocr_results: HashMap::new(),
            thumb_pending: Arc::new(Mutex::new(Vec::new())),
            thumb_rendering: Arc::new(Mutex::new(Vec::new())),
            thumbs: Vec::new(),
            stamp_textures: HashMap::new(),
            page_input: String::new(),
            search_query: String::new(),
            search_hits: Vec::new(),
            search_selected: 0,
            pending_hit_scroll: None,
            search_should_focus: false,
            search_field_id: None,
            last_searched: String::new(),
            search_rx: None,
            search_gen: 0,
            search_pending_query: None,
            search_debounce_deadline: None,
            tool: Tool::Select,
            color: [230, 30, 30, 255],
            fill: None,
            stroke_width: 2.0,
            font_size: 14.0,
            interaction: Interaction::None,
            selected: None,
            text_runs: Vec::new(),
            text_runs_valid: Vec::new(),
            selected_text_run: None,
            word_selection: Vec::new(),
            word_anchor: None,
            text_drag: None,
            text_drag_offset: (0.0, 0.0),
            auto_reflow: false,
            selected_image: None,
            image_drag: None,
            image_drag_start_matrix: None,
            image_extract_pending: None,
            image_replace_pending: None,
            crop_mode: false,
            crop_start: None,
            crop_current: None,
            text_edits: Vec::new(),
            text_edit_state: None,
            text_edit_rect: None,
            text_edit_should_focus: false,
            form_fields: Vec::new(),
            form_edit_state: None,
            form_edit_rect: None,
            form_edit_should_focus: false,
            selected_form_field: None,
            text_select_drag: None,
            text_selection: None,
            undo: UndoStack::default(),
            dirty: false,
            status: format!("pdf-editor · build {}", build_stamp()),
            text_buffer: String::new(),
            open_pending: None,
            save_pending: None,
            image_pending: None,
            merge_pending: None,
            extract_pages_pending: None,
            selected_pages: HashSet::new(),
            page_drag_source: None,
            show_crop_page: false,
            crop_margins: [36.0; 4],
            crop_all_pages: false,
            scroll_target: None,
            ignore_next_scroll: false,
            page_rects: Vec::new(),
            confirm: None,
            show_signatures: false,
            sig_tab: SignatureTab::Saved,
            saved_signatures: crate::persist::list_signatures(),
            sig_textures: HashMap::new(),
            sig_draw_strokes: Vec::new(),
            sig_draw_active: None,
            sig_name: String::new(),
            sig_type_text: String::new(),
            sig_type_style: 0,
            sig_color: [15, 30, 120, 255],
            sig_stroke_width: 3.0,
            sig_import_pending: None,
            show_flatten: false,
            show_bookmarks: false,
            bookmarks: Vec::new(),
            show_headers_footers: false,
            hf_position: StampPosition::BottomCenter,
            hf_text: "Page {page} of {total}".into(),
            hf_font_size: 10.0,
            hf_color: [40, 40, 40, 255],
            hf_margin_x: 36.0,
            hf_margin_y: 24.0,
            hf_start_number: 1,
            hf_all_pages: true,
            hf_from_page: 1,
            hf_to_page: 1,
            show_watermark: false,
            wm_text: "CONFIDENTIAL".into(),
            wm_font_size: 54.0,
            wm_color: [200, 40, 40, 80],
            wm_angle: 45.0,
            wm_all_pages: true,
            page_links: Vec::new(),
            link_drag: None,
            pending_link_rect: None,
            link_target_is_page: false,
            link_url_input: "https://".into(),
            link_page_input: 1,
            clipboard: None,
            show_metadata: false,
            meta_draft: PdfMetadata::default(),
            show_compress: false,
            compress_preset: CompressionPreset::Balanced,
            compress_max_dpi: 150.0,
            compress_quality: 75,
            compress_stats: None,
            show_security: false,
            security_user_pw: String::new(),
            security_owner_pw: String::new(),
            security_allow_print: true,
            security_allow_copy: true,
            security_allow_modify: true,
            security_allow_annotations: true,
            password_prompt_path: None,
            password_prompt_input: String::new(),
            password_prompt_error: None,
        }
    }

    // ------------------------------------------------------------- opening

    fn open_dialog(&mut self) {
        if self.open_pending.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.open_pending = Some(rx);
        std::thread::spawn(move || {
            let picked = rfd::FileDialog::new()
                .add_filter("PDF documents", &["pdf"])
                .set_title("Open a PDF")
                .pick_file();
            let _ = tx.send(picked);
        });
    }

    fn save_as_dialog(&mut self) {
        if self.save_pending.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.save_pending = Some(rx);
        std::thread::spawn(move || {
            let picked = rfd::FileDialog::new()
                .add_filter("PDF documents", &["pdf"])
                .set_title("Save a copy")
                .save_file();
            let _ = tx.send(picked);
        });
    }

    fn image_dialog(&mut self) {
        if self.image_pending.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.image_pending = Some(rx);
        std::thread::spawn(move || {
            let picked = rfd::FileDialog::new()
                .add_filter("Images", &["png", "jpg", "jpeg", "webp", "bmp"])
                .set_title("Choose an image")
                .pick_file();
            let _ = tx.send(picked);
        });
    }

    fn poll_dialogs(&mut self) {
        // Open
        let mut open_ready = None;
        if let Some(rx) = &self.open_pending {
            match rx.try_recv() {
                Ok(v) => open_ready = Some(v),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => open_ready = Some(None),
            }
        }
        if let Some(result) = open_ready {
            self.open_pending = None;
            if let Some(path) = result {
                if self.dirty {
                    // Don't silently discard edits: ask first.
                    self.confirm = Some(PendingConfirm::Open(path));
                } else {
                    self.load_document(&path);
                }
            }
        }

        // Save As
        let mut save_ready = None;
        if let Some(rx) = &self.save_pending {
            match rx.try_recv() {
                Ok(v) => save_ready = Some(v),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => save_ready = Some(None),
            }
        }
        if let Some(result) = save_ready {
            self.save_pending = None;
            if let Some(path) = result {
                self.save_to(&path);
            }
        }

        // Image
        let mut image_ready = None;
        if let Some(rx) = &self.image_pending {
            match rx.try_recv() {
                Ok(v) => image_ready = Some(v),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => image_ready = Some(None),
            }
        }
        if let Some(result) = image_ready {
            self.image_pending = None;
            if let Some(path) = result {
                self.place_image(&path);
            }
        }

        // Extract selected image
        let mut doc_edited_after_replace = false;
        let mut extract_ready = None;
        if let Some(rx) = &self.image_extract_pending {
            match rx.try_recv() {
                Ok(v) => extract_ready = Some(v),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => extract_ready = Some(None),
            }
        }
        if let Some(result) = extract_ready {
            self.image_extract_pending = None;
            if result.is_some() {
                self.status = "Image extracted".into();
            }
        }

        // Replace selected image
        let mut replace_ready = None;
        if let Some(rx) = &self.image_replace_pending {
            match rx.try_recv() {
                Ok(v) => replace_ready = Some(v),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => replace_ready = Some(None),
            }
        }
        if let Some(result) = replace_ready {
            self.image_replace_pending = None;
            if let Some((page, object_index, before_bytes, after_bytes)) = result {
                if let Some(doc) = self.document.as_mut()
                    && let Err(e) =
                        doc.replace_image_object_from_bytes(page, object_index, &after_bytes)
                {
                    self.error = Some(format!("Could not replace image: {e}"));
                } else {
                    self.undo.push(
                        "Replace image",
                        DocumentEdit::ImageReplace(ImageReplace {
                            page,
                            object_index,
                            before_bytes,
                            after_bytes,
                        }),
                    );
                    doc_edited_after_replace = true;
                    self.status = "Image replaced".into();
                }
            }
        }
        if doc_edited_after_replace {
            if let Some(doc) = self.document.as_mut() {
                doc.refresh_render_docs(self.pdfium);
            }
            self.cache.clear();
            self.thumbs.clear();
            self.dirty = true;
        }

        // Merge PDF
        let mut merge_ready = None;
        if let Some(rx) = &self.merge_pending {
            match rx.try_recv() {
                Ok(v) => merge_ready = Some(v),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => merge_ready = Some(None),
            }
        }
        if let Some(result) = merge_ready {
            self.merge_pending = None;
            if let Some(path) = result {
                self.merge_pdf(&path);
            }
        }

        // Extract pages
        let mut extract_pages_ready = None;
        if let Some(rx) = &self.extract_pages_pending {
            match rx.try_recv() {
                Ok(v) => extract_pages_ready = Some(v),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => extract_pages_ready = Some(None),
            }
        }
        if let Some(result) = extract_pages_ready {
            self.extract_pages_pending = None;
            if let Some(path) = result {
                self.extract_selected_pages_to(&path);
            }
        }

        // Import signature image
        let mut sig_import_ready = None;
        if let Some(rx) = &self.sig_import_pending {
            match rx.try_recv() {
                Ok(v) => sig_import_ready = Some(v),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => sig_import_ready = Some(None),
            }
        }
        if let Some(result) = sig_import_ready {
            self.sig_import_pending = None;
            if let Some(path) = result {
                match crate::persist::save_signature_from_image_file("", &path) {
                    Ok(saved) => {
                        self.refresh_signatures();
                        self.status = format!("Saved signature '{}'", saved.name);
                    }
                    Err(e) => {
                        self.error = Some(format!("Could not import signature: {e}"));
                    }
                }
            }
        }
    }

    fn refresh_signatures(&mut self) {
        self.saved_signatures = crate::persist::list_signatures();
        let ids: HashSet<String> = self.saved_signatures.iter().map(|s| s.id.clone()).collect();
        self.sig_textures.retain(|id, _| ids.contains(id));
    }

    fn ensure_signature_textures(&mut self, ctx: &egui::Context) {
        for sig in &self.saved_signatures {
            if self.sig_textures.contains_key(&sig.id) {
                continue;
            }
            let Ok(image) = image::open(&sig.path) else {
                continue;
            };
            let rgba = image.to_rgba8();
            let color_image = egui::ColorImage::from_rgba_unmultiplied(
                [rgba.width() as usize, rgba.height() as usize],
                rgba.as_raw(),
            );
            let texture = ctx.load_texture(
                format!("sig:{}", sig.id),
                color_image,
                egui::TextureOptions::LINEAR,
            );
            self.sig_textures.insert(sig.id.clone(), texture);
        }
    }

    fn place_signature_at_center(&mut self, path: &std::path::Path, px_w: u32, px_h: u32) {
        if self.document.is_none() {
            return;
        }
        let w = 160.0f32;
        let h = (w * (px_h.max(1) as f32 / px_w.max(1) as f32)).clamp(20.0, 180.0);
        let mut a = Annotation::new(0, AnnotationKind::Stamp);
        a.image_path = Some(path.to_string_lossy().to_string());
        a.image_size = Some((w, h));
        let (pw, ph) = self.page_size_pts(self.current_page);
        let l = (pw - w) / 2.0;
        let b = (ph - h) / 2.0;
        a.rect = PdfRect::new_from_values(b, l, b + h, l + w);
        self.commit_annotation(self.current_page, a);
        self.tool = Tool::Select;
    }

    fn import_signature_dialog(&mut self) {
        if self.sig_import_pending.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.sig_import_pending = Some(rx);
        std::thread::spawn(move || {
            let picked = rfd::FileDialog::new()
                .add_filter("Images", &["png", "jpg", "jpeg", "webp", "bmp"])
                .set_title("Import Signature Image")
                .pick_file();
            let _ = tx.send(picked);
        });
    }

    fn has_pending_redactions(&self) -> bool {
        self.document.as_ref().is_some_and(|d| {
            d.annotations
                .iter()
                .flatten()
                .any(|a| a.kind == AnnotationKind::Redaction)
        })
    }

    fn apply_redactions_action(&mut self) {
        let Some(doc) = self.document.as_mut() else {
            return;
        };
        match doc.apply_all_redactions(self.pdfium) {
            Ok(count) => {
                self.undo.clear();
                self.dirty = true;
                self.clear_page_caches();
                self.status = if count == 0 {
                    "No pending redactions to apply".into()
                } else {
                    format!("Permanently applied {count} redaction(s)")
                };
            }
            Err(e) => {
                self.error = Some(format!("Could not apply redactions: {e}"));
            }
        }
    }

    fn flatten_action(&mut self, all_pages: bool) {
        let page = self.current_page;
        let Some(doc) = self.document.as_mut() else {
            return;
        };
        let res = if all_pages {
            doc.flatten_all_pages(self.pdfium).map(|n| format!("Flattened {n} page(s)"))
        } else {
            doc.flatten_page(self.pdfium, page).map(|()| format!("Flattened page {}", page + 1))
        };
        match res {
            Ok(msg) => {
                self.undo.clear();
                self.dirty = true;
                self.clear_page_caches();
                self.status = msg;
            }
            Err(e) => {
                self.error = Some(format!("Could not flatten: {e}"));
            }
        }
    }

    pub(crate) fn load_document(&mut self, path: &std::path::Path) {
        self.load_document_with_password(path, None);
    }

    pub(crate) fn load_document_with_password(
        &mut self,
        path: &std::path::Path,
        password: Option<&str>,
    ) {
        match OpenDocument::load_with_password(self.pdfium, path, password) {
            Ok(doc) => {
                self.password_prompt_path = None;
                self.password_prompt_input.clear();
                self.password_prompt_error = None;
                self.document = Some(doc);
                self.error = None;
                self.current_page = 0;
                self.cache.clear();
                self.thumbs.clear();
                *self.thumb_pending.lock().unwrap() = Vec::new();
                *self.thumb_rendering.lock().unwrap() = Vec::new();
                self.fit_width = true;
                *self.pending.lock().unwrap() = None;
                self.scroll_target = Some(0);
                self.selected = None;
                let page_count = self.document.as_ref().unwrap().page_count;
                self.text_runs = vec![Vec::new(); page_count];
                self.text_runs_valid = vec![false; page_count];
                self.selected_text_run = None;
                self.word_selection.clear();
                self.word_anchor = None;
                self.text_drag = None;
                self.text_drag_offset = (0.0, 0.0);
                self.selected_image = None;
                self.image_drag = None;
                self.image_drag_start_matrix = None;
                self.image_extract_pending = None;
                self.image_replace_pending = None;
                self.crop_mode = false;
                self.crop_start = None;
                self.crop_current = None;
                self.text_edits.clear();
                self.text_edit_state = None;
                self.text_edit_rect = None;
                self.text_edit_should_focus = false;
                self.form_fields = self
                    .document
                    .as_ref()
                    .and_then(|d| d.form_fields().ok())
                    .unwrap_or_default();
                self.form_edit_state = None;
                self.form_edit_rect = None;
                self.form_edit_should_focus = false;
                self.selected_form_field = None;
                self.text_select_drag = None;
                self.text_selection = None;
                self.selected_pages.clear();
                self.selected_pages.insert(0);
                self.page_drag_source = None;
                self.merge_pending = None;
                self.extract_pages_pending = None;
                self.show_crop_page = false;
                self.bookmarks = self
                    .document
                    .as_ref()
                    .map(|d| d.bookmarks())
                    .unwrap_or_default();
                self.meta_draft = self
                    .document
                    .as_ref()
                    .map(|d| d.metadata())
                    .unwrap_or_default();
                self.compress_stats = None;
                if let Some(opts) = self
                    .document
                    .as_ref()
                    .and_then(|d| d.pending_encryption.as_ref())
                {
                    self.security_user_pw = opts.user_password.clone();
                    self.security_owner_pw = opts.owner_password.clone();
                    self.security_allow_print = opts.allow_print;
                    self.security_allow_copy = opts.allow_copy;
                    self.security_allow_modify = opts.allow_modify;
                    self.security_allow_annotations = opts.allow_annotations;
                } else {
                    self.security_user_pw.clear();
                    self.security_owner_pw.clear();
                    self.security_allow_print = true;
                    self.security_allow_copy = true;
                    self.security_allow_modify = true;
                    self.security_allow_annotations = true;
                }
                self.hf_from_page = 1;
                self.hf_to_page = page_count.max(1);
                self.page_links = (0..page_count)
                    .map(|p| {
                        self.document
                            .as_ref()
                            .and_then(|d| d.page_links(p).ok())
                            .unwrap_or_default()
                    })
                    .collect();
                self.link_drag = None;
                self.pending_link_rect = None;
                self.interaction = Interaction::None;
                self.undo.clear();
                self.clear_search();
                self.dirty = false;
                self.status = "Document opened".into();
                crate::persist::save_last_file(
                    &path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
                );
            }
            Err(e) => {
                if e.0.contains("PasswordError") {
                    self.password_prompt_path = Some(path.to_path_buf());
                    self.password_prompt_error = if password.is_some() {
                        Some("Incorrect password. Please try again.".into())
                    } else {
                        None
                    };
                } else {
                    self.error = Some(format!("Could not open {}: {e}", path.display()));
                }
            }
        }
    }

    // ------------------------------------------------------------ geometry

    /// Rotation of a page, defaulting to unrotated.
    fn page_rotation(&self, page: usize) -> PdfPageRenderRotation {
        self.document
            .as_ref()
            .and_then(|d| d.pages.get(page))
            .map(|p| p.rotation)
            .unwrap_or(PdfPageRenderRotation::None)
    }

    /// Unrotated page dimensions in PDF points (width, height).
    fn page_size_pts(&self, page: usize) -> (f32, f32) {
        self.document
            .as_ref()
            .and_then(|d| d.pages.get(page))
            .map(|p| (p.width_pts, p.height_pts))
            .unwrap_or((612.0, 792.0))
    }

    /// On-screen (rotation-adjusted) page dimensions in PDF points.
    fn page_display_pts(&self, page: usize) -> (f32, f32) {
        let (w, h) = self.page_size_pts(page);
        match self.page_rotation(page) {
            PdfPageRenderRotation::Degrees90 | PdfPageRenderRotation::Degrees270 => (h, w),
            _ => (w, h),
        }
    }

    fn effective_scale(&self, page: usize) -> f32 {
        let (page_w_pts, _) = self.page_display_pts(page);

        if self.fit_width && self.last_available_width > 50.0 {
            ((self.last_available_width - 48.0) / page_w_pts).clamp(0.2, 6.0)
        } else {
            self.zoom.clamp(0.2, 6.0)
        }
    }

    fn page_size_px(&self, page: usize) -> egui::Vec2 {
        let (w, h) = self.page_display_pts(page);
        let s = self.effective_scale(page);
        egui::vec2(w * s, h * s)
    }

    /// Convert a PDF page point (unrotated page space) to an absolute screen
    /// position for the given laid-out page rectangle.
    fn pdf_to_screen(&self, page: usize, page_rect: egui::Rect, x: f32, y: f32) -> egui::Pos2 {
        let (pw, ph) = self.page_size_pts(page);
        let s = self.effective_scale(page);
        let (sx, sy) = match self.page_rotation(page) {
            PdfPageRenderRotation::Degrees90 => (y, x),
            PdfPageRenderRotation::Degrees180 => (pw - x, ph - y),
            PdfPageRenderRotation::Degrees270 => (ph - y, pw - x),
            _ => (x, ph - y),
        };
        egui::pos2(page_rect.min.x + sx * s, page_rect.min.y + sy * s)
    }

    /// The four screen-space corners of an axis-aligned page-space rectangle,
    /// in clockwise order starting from the bottom-left page-space corner.
    fn page_rect_corners(
        &self,
        page: usize,
        page_rect: egui::Rect,
        l: f32,
        b: f32,
        r: f32,
        t: f32,
    ) -> [egui::Pos2; 4] {
        [
            self.pdf_to_screen(page, page_rect, l, b),
            self.pdf_to_screen(page, page_rect, r, b),
            self.pdf_to_screen(page, page_rect, r, t),
            self.pdf_to_screen(page, page_rect, l, t),
        ]
    }

    /// Axis-aligned screen bounding box of a page-space rectangle. Exact for
    /// 0°/180° rotation; a conservative box for 90°/270°.
    fn screen_bbox(
        &self,
        page: usize,
        page_rect: egui::Rect,
        l: f32,
        b: f32,
        r: f32,
        t: f32,
    ) -> egui::Rect {
        egui::Rect::from_points(&self.page_rect_corners(page, page_rect, l, b, r, t))
    }

    /// Convert a screen point to PDF page coordinates (unrotated page space)
    /// for the given page rect. This is the inverse of [`Self::pdf_to_screen`].
    fn screen_to_pdf(&self, page: usize, page_rect: egui::Rect, pos: egui::Pos2) -> (f32, f32) {
        let (pw, ph) = self.page_size_pts(page);
        let s = self.effective_scale(page);
        let sx = (pos.x - page_rect.min.x) / s;
        let sy = (pos.y - page_rect.min.y) / s;
        match self.page_rotation(page) {
            PdfPageRenderRotation::Degrees90 => (sy, sx),
            PdfPageRenderRotation::Degrees180 => (pw - sx, ph - sy),
            PdfPageRenderRotation::Degrees270 => (pw - sy, ph - sx),
            _ => (sx, ph - sy),
        }
    }

    // ------------------------------------------------------------ rendering

    const MAX_CONCURRENT_RENDERS: usize = 3;

    fn request_missing_renders(&mut self, priority: &[usize], all_pages: &[usize]) {
        let mut queue: Vec<usize> = Vec::new();
        let rendering = self.rendering.lock().unwrap();
        for page in priority.iter().chain(all_pages.iter()).copied() {
            if rendering.contains(&page) || queue.contains(&page) {
                continue;
            }
            if self.cached_scale(page).is_some() {
                continue;
            }
            queue.push(page);
        }
        drop(rendering);

        let free =
            Self::MAX_CONCURRENT_RENDERS.saturating_sub(self.rendering.lock().unwrap().len());
        for page in queue.into_iter().take(free) {
            // A render may have landed in the cache since the queue was built.
            if self.cached_scale(page).is_some() {
                continue;
            }
            let scale = self.effective_scale(page);
            let Some(doc) = self.document.as_ref() else {
                return;
            };
            // Each render job gets its own read-only document handle.
            let doc_arc = match doc.render_docs.get(page % doc.render_docs.len().max(1)) {
                Some(d) => Arc::clone(d),
                None => return,
            };
            let pending = Arc::clone(&self.pending);
            let rendering = Arc::clone(&self.rendering);

            {
                let mut r = rendering.lock().unwrap();
                if r.len() >= Self::MAX_CONCURRENT_RENDERS || r.contains(&page) {
                    continue;
                }
                r.push(page);
            }

            std::thread::spawn(move || {
                let image = render_page(&doc_arc, page, scale, false);
                loop {
                    let mut slot = pending.lock().unwrap();
                    if slot.is_none() {
                        *slot = Some((page, scale, image));
                        break;
                    }
                    drop(slot);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                rendering.lock().unwrap().retain(|&p| p != page);
            });
        }
    }

    fn request_thumbnails(&mut self) {
        if !self.show_thumbs {
            return;
        }
        let Some(doc) = self.document.as_ref() else {
            return;
        };
        let total = doc.page_count;

        const THUMB_W_PX: i32 = 140;
        const MAX_THUMB_JOBS: usize = 4;

        let mut jobs: Vec<usize> = Vec::new();
        {
            let rendering = self.thumb_rendering.lock().unwrap();
            let pending = self.thumb_pending.lock().unwrap();
            let mut order: Vec<usize> = (self.current_page..total).collect();
            order.extend((0..self.current_page.min(total)).rev());
            for p in order {
                if rendering.len() + jobs.len() >= MAX_THUMB_JOBS {
                    break;
                }
                if rendering.contains(&p)
                    || pending.iter().any(|(t, _)| *t == p)
                    || self.thumbs.iter().any(|(t, _)| *t == p)
                {
                    continue;
                }
                jobs.push(p);
            }
        }

        for page in jobs {
            let doc_arc = match doc.render_docs.get(page % doc.render_docs.len().max(1)) {
                Some(d) => Arc::clone(d),
                None => break,
            };
            let pending = Arc::clone(&self.thumb_pending);
            let rendering = Arc::clone(&self.thumb_rendering);
            self.thumb_rendering.lock().unwrap().push(page);

            std::thread::spawn(move || {
                let image = render_thumbnail(&doc_arc, page, THUMB_W_PX, false);
                let mut slot = pending.lock().unwrap();
                if !slot.iter().any(|(p, _)| *p == page) {
                    slot.push((page, image));
                }
                rendering.lock().unwrap().retain(|&p| p != page);
            });
        }
    }

    fn upload_thumbnails(&mut self, ui: &egui::Ui) {
        let ready = std::mem::take(&mut *self.thumb_pending.lock().unwrap());
        for (page, image) in ready {
            let texture =
                ui.ctx()
                    .load_texture(format!("thumb-{page}"), image, egui::TextureOptions::LINEAR);
            self.thumbs.retain(|(p, _)| *p != page);
            self.thumbs.push((page, texture));
        }
    }

    fn ensure_stamp_textures(&mut self, ctx: &egui::Context) {
        let mut needed = HashSet::new();
        if let Some(document) = self.document.as_ref() {
            for annotation in document.annotations.iter().flatten() {
                if annotation.kind == AnnotationKind::Stamp
                    && let Some(path) = annotation.image_path.as_ref()
                {
                    needed.insert(path.clone());
                }
            }
        }

        for path in &needed {
            if self.stamp_textures.contains_key(path) {
                continue;
            }
            let Ok(image) = image::open(path) else {
                continue;
            };
            let rgba = image.to_rgba8();
            let color_image = egui::ColorImage::from_rgba_unmultiplied(
                [rgba.width() as usize, rgba.height() as usize],
                rgba.as_raw(),
            );
            let texture = ctx.load_texture(
                format!("stamp:{path}"),
                color_image,
                egui::TextureOptions::LINEAR,
            );
            self.stamp_textures.insert(path.clone(), texture);
        }

        self.stamp_textures.retain(|path, _| needed.contains(path));
    }

    fn cached_scale(&self, page: usize) -> Option<f32> {
        self.cache
            .iter()
            .find(|(p, _)| *p == page)
            .map(|(_, c)| c.scale)
    }

    fn evict_cache(&mut self, keep: &[usize]) {
        self.cache.retain(|(p, _)| keep.contains(p));
    }

    fn request_fit_rebuild(&mut self) {
        let mut stale: Vec<usize> = Vec::new();
        for (p, c) in &self.cache {
            if (c.scale - self.effective_scale(*p)).abs() > 0.01 {
                stale.push(*p);
            }
        }
        self.cache.retain(|(p, _)| !stale.contains(p));
    }

    // ------------------------------------------------------------ editing

    fn current_style(&self) -> StrokeStyle {
        match self.tool {
            Tool::Highlight => StrokeStyle::highlight(),
            Tool::Redact => StrokeStyle {
                fill: Some([220, 30, 30, 70]),
                stroke: Some([220, 30, 30, 255]),
                width: 1.5,
            },
            Tool::Ink => StrokeStyle {
                fill: None,
                stroke: Some(self.color),
                width: self.stroke_width,
            },
            Tool::Rectangle | Tool::Ellipse => StrokeStyle {
                fill: self.fill,
                stroke: Some(self.color),
                width: self.stroke_width,
            },
            Tool::Text => StrokeStyle {
                fill: None,
                stroke: Some(self.color),
                width: 0.0,
            },
            Tool::Image | Tool::Link => StrokeStyle::default(),
            Tool::Select | Tool::EditText | Tool::ImageSelect | Tool::TextSelect => {
                StrokeStyle::default()
            }
        }
    }

    fn commit_annotation(&mut self, page: usize, mut annotation: Annotation) {
        if annotation.kind == AnnotationKind::Highlight {
            annotation.style = StrokeStyle::highlight();
        }
        annotation.font_size = self.font_size;

        let Some(doc) = self.document.as_mut() else {
            return;
        };

        let before = doc.annotations.clone();
        match doc.add_annotation(page, annotation.clone()) {
            Ok(()) => {
                let id = annotation.id;
                self.dirty = true;
                self.selected = Some((page, id));
                self.status = format!("{} added", annotation.kind.label());
                self.undo.push(
                    "add annotation",
                    DocumentEdit::Annotations {
                        before,
                        after: doc.annotations.clone(),
                    },
                );
            }
            Err(e) => {
                self.error = Some(format!("Could not add annotation: {e}"));
            }
        }
    }

    /// Delete the selected annotation.
    fn delete_selected(&mut self) {
        let Some((page, id)) = self.selected else {
            return;
        };
        let Some(doc) = self.document.as_mut() else {
            return;
        };
        let Some(annotation) = doc.annotations[page].iter().find(|a| a.id == id).cloned() else {
            return;
        };

        let before = doc.annotations.clone();
        match doc.delete_annotation(page, id) {
            Ok(()) => {
                self.dirty = true;
                self.selected = None;
                self.status = "Annotation deleted".into();
                self.undo.push(
                    "delete annotation",
                    DocumentEdit::Annotations {
                        before,
                        after: doc.annotations.clone(),
                    },
                );
                let _ = annotation;
            }
            Err(e) => self.error = Some(format!("Could not delete annotation: {e}")),
        }
    }

    /// The font size and stroke colour of the selected free-text annotation,
    /// if one is selected.
    fn selected_free_text(&self) -> Option<(f32, [u8; 4])> {
        let (page, id) = self.selected?;
        let doc = self.document.as_ref()?;
        doc.annotations
            .get(page)?
            .iter()
            .find(|a| a.id == id && a.kind == AnnotationKind::FreeText)
            .map(|a| (a.font_size, a.style.stroke.unwrap_or([0, 0, 0, 255])))
    }

    /// Update the font size and colour of the selected free-text annotation.
    fn edit_selected_text(&mut self, font_size: f32, color: [u8; 4]) {
        let Some((page, id)) = self.selected else {
            return;
        };
        let Some(doc) = self.document.as_mut() else {
            return;
        };
        let Some(current) = doc.annotations[page]
            .iter()
            .find(|a| a.id == id && a.kind == AnnotationKind::FreeText)
            .cloned()
        else {
            return;
        };

        let before = doc.annotations.clone();
        let mut updated = current;
        updated.font_size = font_size;
        updated.style.stroke = Some(color);
        if let Err(e) = doc.update_annotation(page, updated) {
            self.error = Some(format!("Could not update text: {e}"));
            return;
        }
        let after = doc.annotations.clone();
        self.dirty = true;
        self.undo
            .push("edit text", DocumentEdit::Annotations { before, after });
    }

    /// The page, object index, font size, and render mode of the selected
    /// original text (word or line), if the Edit Text tool has one selected.
    fn selected_text_style(&self) -> Option<(usize, usize, f32, PdfPageTextRenderMode)> {
        let (page, run_index) = self.selected_text_run?;
        let run = self.text_runs.get(page)?.get(run_index)?;
        let doc = self.document.as_ref()?;
        let object_index = doc.text_object_index_for_run(page, run).ok()?;
        let size = doc.text_object_font_size(page, object_index).ok()?;
        let mode = doc.text_object_render_mode(page, object_index).ok()?;
        Some((page, object_index, size, mode))
    }

    /// Restyle the selected original text (font size + render mode), undoable.
    ///
    /// Applies to the whole text object (typically one line), not a single word.
    fn edit_selected_text_style(&mut self, size: f32, mode: PdfPageTextRenderMode) {
        let Some((page, object_index, before_size, before_mode)) = self.selected_text_style()
        else {
            return;
        };
        let Some(doc) = self.document.as_mut() else {
            return;
        };
        if let Err(e) = doc.set_text_object_font_size(page, object_index, size) {
            self.error = Some(format!("Could not set text size: {e}"));
            return;
        }
        if let Err(e) = doc.set_text_object_render_mode(page, object_index, mode) {
            self.error = Some(format!("Could not set text style: {e}"));
            return;
        }
        self.refresh_after_image_edit(page);
        // The run's rendering changed, but the selection (indices) stays valid.
        if let Some(valid) = self.text_runs_valid.get_mut(page) {
            *valid = false;
        }
        self.dirty = true;
        self.undo.push(
            "restyle text",
            DocumentEdit::TextStyle(TextStyle {
                page,
                object_index,
                before_size,
                after_size: size,
                before_mode,
                after_mode: mode,
            }),
        );
    }

    fn selected_text_color_and_font(&self) -> Option<([u8; 4], TextFontFamily)> {
        let (page, object_index, _, _) = self.selected_text_style()?;
        let doc = self.document.as_ref()?;
        let color = doc.text_object_fill_color(page, object_index).ok()?;
        let family = doc.text_object_font_family(page, object_index).ok()?;
        Some((color, family))
    }

    fn edit_selected_text_color(&mut self, color: [u8; 4]) {
        let Some((page, object_index, _, _)) = self.selected_text_style() else {
            return;
        };
        let Some((before, _)) = self.selected_text_color_and_font() else {
            return;
        };
        let Some(doc) = self.document.as_mut() else {
            return;
        };
        if let Err(e) = doc.set_text_object_fill_color(self.pdfium, page, object_index, color) {
            self.error = Some(format!("Could not set text colour: {e}"));
            return;
        }
        self.refresh_after_image_edit(page);
        if let Some(valid) = self.text_runs_valid.get_mut(page) {
            *valid = false;
        }
        self.dirty = true;
        self.undo.push(
            "text colour",
            DocumentEdit::TextColor(TextColor {
                page,
                object_index,
                before,
                after: color,
            }),
        );
    }

    fn edit_selected_text_font(&mut self, family: TextFontFamily) {
        let Some((page, object_index, _, _)) = self.selected_text_style() else {
            return;
        };
        let Some(doc) = self.document.as_mut() else {
            return;
        };
        match doc.set_text_object_font_family(self.pdfium, page, object_index, family) {
            Ok(_) => {
                self.dirty = true;
                self.clear_page_caches();
                self.status = format!("Changed font to {}", family.label());
            }
            Err(e) => self.error = Some(format!("Could not change font: {e}")),
        }
    }

    fn restore_text_color(&mut self, edit: &TextColor, to_original: bool) {
        let color = if to_original { edit.before } else { edit.after };
        if let Some(doc) = self.document.as_mut()
            && let Err(e) = doc.set_text_object_fill_color(self.pdfium, edit.page, edit.object_index, color)
        {
            self.error = Some(format!("Could not restore text colour: {e}"));
            return;
        }
        self.refresh_after_image_edit(edit.page);
        self.invalidate_text_runs();
    }

    fn delete_selected_text_line(&mut self) {
        let Some((page, run_index)) = self.selected_text_run else {
            return;
        };
        let Some(runs) = self.text_runs.get(page).cloned() else {
            return;
        };
        let mut line_chars = visual_lines(&runs)
            .into_iter()
            .find(|line| line.runs.contains(&run_index))
            .map(|line| line.chars(&runs))
            .unwrap_or_else(|| {
                runs.get(run_index)
                    .map(|run| run.chars.clone())
                    .unwrap_or_default()
            });
        if let Some(run) = runs.get(run_index) {
            line_chars.extend(right_margin_fragments(&runs, run));
            line_chars.sort_unstable();
            line_chars.dedup();
        }
        let Some(doc) = self.document.as_mut() else {
            return;
        };
        let Ok(indices) = doc.text_object_indices_for_chars(page, &line_chars) else {
            return;
        };
        if indices.is_empty() {
            return;
        }
        match doc.delete_text_objects(self.pdfium, page, &indices) {
            Ok(n) => {
                self.dirty = true;
                self.clear_page_caches();
                self.status = format!("Deleted line ({n} text object(s))");
            }
            Err(e) => self.error = Some(format!("Could not delete line: {e}")),
        }
    }

    fn can_copy(&self) -> bool {
        self.selected.is_some()
            || self.selected_image.is_some()
            || self.selected_text_run.is_some()
    }

    fn copy_selected_item(&mut self) {
        if let Some((page, id)) = self.selected
            && let Some(doc) = self.document.as_ref()
            && let Some(a) = doc.annotations.get(page).and_then(|l| l.iter().find(|a| a.id == id))
        {
            self.clipboard = Some(ClipboardItem::Annotation(a.clone()));
            self.status = format!("Copied {}", a.kind.label());
            return;
        }
        if let Some((page, object_index)) = self.selected_image
            && let Some(doc) = self.document.as_ref()
            && let Ok(png_bytes) = doc.image_object_png_bytes(page, object_index)
            && let Ok((l, b, r, t)) = doc.object_bounds(page, object_index)
        {
            self.clipboard = Some(ClipboardItem::Image {
                png_bytes,
                width: (r - l).abs().max(12.0),
                height: (t - b).abs().max(12.0),
            });
            self.status = "Copied image".into();
            return;
        }
        if let Some((page, run_index)) = self.selected_text_run
            && let Some(run) = self.text_runs.get(page).and_then(|r| r.get(run_index))
            && let Some((_, _, size, _)) = self.selected_text_style()
            && let Some((color, family)) = self.selected_text_color_and_font()
        {
            self.clipboard = Some(ClipboardItem::Text {
                text: run.text.clone(),
                family,
                font_size: size,
                color,
            });
            self.status = "Copied text object".into();
        }
    }

    fn paste_clipboard_item(&mut self) {
        let Some(item) = self.clipboard.clone() else {
            return;
        };
        let page = self.current_page;
        match item {
            ClipboardItem::Annotation(mut a) => {
                let (l, b, r, t) = a.bounds_f32();
                let dx = 18.0;
                let dy = -18.0;
                a.rect = PdfRect::new_from_values(b + dy, l + dx, t + dy, r + dx);
                if a.kind == AnnotationKind::Ink {
                    a.points = a.points.iter().map(|(x, y)| (x + dx, y + dy)).collect();
                }
                self.commit_annotation(page, a);
            }
            ClipboardItem::Text {
                text,
                family,
                font_size,
                color,
            } => {
                let (_, ph) = self.page_size_pts(page);
                let Some(doc) = self.document.as_mut() else {
                    return;
                };
                match doc.add_text_object(
                    self.pdfium,
                    page,
                    &text,
                    family,
                    font_size,
                    color,
                    72.0,
                    (ph - 108.0).max(36.0),
                ) {
                    Ok(_) => {
                        self.dirty = true;
                        self.clear_page_caches();
                        self.status = format!("Pasted text onto page {}", page + 1);
                    }
                    Err(e) => self.error = Some(format!("Could not paste text: {e}")),
                }
            }
            ClipboardItem::Image {
                png_bytes,
                width,
                height,
            } => {
                let (pw, ph) = self.page_size_pts(page);
                let l = (pw - width) * 0.5;
                let b = (ph - height) * 0.5;
                let Some(doc) = self.document.as_mut() else {
                    return;
                };
                match doc.add_image_object_from_bytes(
                    self.pdfium,
                    page,
                    &png_bytes,
                    l,
                    b,
                    width,
                    height,
                ) {
                    Ok(_) => {
                        self.dirty = true;
                        self.clear_page_caches();
                        self.status = format!("Pasted image onto page {}", page + 1);
                    }
                    Err(e) => self.error = Some(format!("Could not paste image: {e}")),
                }
            }
        }
    }


    /// Restore an annotation snapshot, re-applying it to the PDF objects.
    ///
    /// Failures are surfaced to the user instead of being silently ignored, so
    /// a failed re-apply can never leave the model and the PDF silently out of
    /// sync.
    fn restore_snapshot(&mut self, annotations: Vec<Vec<Annotation>>) {
        let Some(doc) = self.document.as_mut() else {
            return;
        };

        // Remove only the PDF objects backing the annotations the model
        // currently tracks. This preserves foreign annotations (links, popups,
        // text notes, ...) that the editor doesn't model.
        let current = doc.annotations.clone();
        for (page, list) in current.iter().enumerate() {
            for a in list {
                if let Err(e) = doc.remove_annotation_from_pdf(page, a) {
                    self.error = Some(format!("Could not undo/redo: {e}"));
                    return;
                }
            }
        }

        // Swap in the snapshot model, keeping ids stable, then rebuild the PDF
        // objects it describes. No re-read: that would reassign ids.
        doc.annotations = annotations;
        let snapshot = doc.annotations.clone();
        for (page, list) in snapshot.iter().enumerate() {
            for a in list {
                if let Err(e) = doc.apply_annotation_public(page, a) {
                    self.error = Some(format!("Could not re-apply annotation: {e}"));
                    return;
                }
            }
        }
    }

    fn undo(&mut self) {
        if let Some(step) = self.undo.take_undo() {
            match step.edit {
                DocumentEdit::Annotations { before, .. } => self.restore_snapshot(before),
                DocumentEdit::Text(edit) => self.restore_text_edit(&edit, true),
                DocumentEdit::ImageMove(edit) => self.restore_image_move(&edit, true),
                DocumentEdit::TextMove(edit) => self.restore_text_move(&edit, true),
                DocumentEdit::TextStyle(edit) => self.restore_text_style(&edit, true),
                DocumentEdit::TextColor(edit) => self.restore_text_color(&edit, true),
                DocumentEdit::ImageRotate(edit) => self.restore_image_rotate(&edit, true),
                DocumentEdit::ImageReplace(edit) => self.restore_image_replace(&edit, true),
                DocumentEdit::ImageCrop(edit) => self.restore_image_crop(&edit, true),
                DocumentEdit::FormField(edit) => self.restore_form_field(&edit, true),
            }
            self.selected = None;
            self.dirty = true;
            self.status = format!("Undid {}", step.label);
        }
    }

    fn redo(&mut self) {
        if let Some(step) = self.undo.take_redo() {
            match step.edit {
                DocumentEdit::Annotations { after, .. } => self.restore_snapshot(after),
                DocumentEdit::Text(edit) => self.restore_text_edit(&edit, false),
                DocumentEdit::ImageMove(edit) => self.restore_image_move(&edit, false),
                DocumentEdit::TextMove(edit) => self.restore_text_move(&edit, false),
                DocumentEdit::TextStyle(edit) => self.restore_text_style(&edit, false),
                DocumentEdit::TextColor(edit) => self.restore_text_color(&edit, false),
                DocumentEdit::ImageRotate(edit) => self.restore_image_rotate(&edit, false),
                DocumentEdit::ImageReplace(edit) => self.restore_image_replace(&edit, false),
                DocumentEdit::ImageCrop(edit) => self.restore_image_crop(&edit, false),
                DocumentEdit::FormField(edit) => self.restore_form_field(&edit, false),
            }
            self.selected = None;
            self.dirty = true;
            self.status = format!("Redid {}", step.label);
        }
    }

    /// Write a text edit back into the PDF for undo/redo.
    fn restore_text_edit(&mut self, edit: &TextEdit, to_original: bool) {
        let run_text = if to_original {
            edit.run_original.clone()
        } else {
            edit.run_new.clone()
        };

        let object_text = if to_original {
            edit.object_original.clone()
        } else {
            edit.object_new.clone()
        };

        if let Some(doc) = self.document.as_mut()
            && let Err(e) = doc.set_text_at_object_index(edit.page, edit.object_index, &object_text)
        {
            self.error = Some(format!("Could not restore text edit: {e}"));
            return;
        }

        if let Some(doc) = self.document.as_mut() {
            doc.refresh_render_docs(self.pdfium);
        }

        if let Some(run) = self
            .text_runs
            .get_mut(edit.page)
            .and_then(|runs| runs.get_mut(edit.run_index))
        {
            run.text = run_text;
        }
        self.text_runs_valid[edit.page] = false;
        self.text_edits.clear();
        self.cache.retain(|(p, _)| *p != edit.page);
        self.thumbs.retain(|(p, _)| *p != edit.page);
        self.selected_text_run = None;
        self.word_selection.clear();
        self.word_anchor = None;
    }

    /// Undo/redo a move-image edit by restoring the before or after matrix.
    fn restore_image_move(&mut self, edit: &ImageMove, to_original: bool) {
        let matrix = if to_original {
            edit.before_matrix
        } else {
            edit.after_matrix
        };

        if let Some(doc) = self.document.as_mut()
            && let Err(e) = doc.set_image_object_matrix(edit.page, edit.object_index, matrix)
        {
            self.error = Some(format!("Could not restore image move: {e}"));
            return;
        }

        self.refresh_after_image_edit(edit.page);
    }

    /// Undo/redo a move-text edit by restoring the before or after matrix.
    fn restore_text_move(&mut self, edit: &TextMove, to_original: bool) {
        let mut failure = None;
        if let Some(doc) = self.document.as_mut() {
            for moved in &edit.moves {
                let matrix = if to_original {
                    moved.before_matrix
                } else {
                    moved.after_matrix
                };
                if let Err(e) = doc.set_text_object_matrix(edit.page, moved.object_index, matrix) {
                    failure = Some(e);
                    break;
                }
            }
        }

        if let Some(e) = failure {
            self.error = Some(format!("Could not restore text move: {e}"));
            return;
        }

        let mut failure = None;
        if let Some(doc) = self.document.as_mut() {
            for line in &edit.redrawn {
                if let Err(e) = doc.restore_redrawn_line(line, to_original) {
                    failure = Some(e);
                    break;
                }
            }
        }
        if let Some(e) = failure {
            self.error = Some(format!("Could not restore a redrawn line: {e}"));
            return;
        }

        self.refresh_after_image_edit(edit.page);
    }

    /// Undo/redo a restyle-text edit by restoring font size and render mode.
    fn restore_text_style(&mut self, edit: &TextStyle, to_original: bool) {
        let size = if to_original {
            edit.before_size
        } else {
            edit.after_size
        };
        let mode = if to_original {
            edit.before_mode
        } else {
            edit.after_mode
        };

        if let Some(doc) = self.document.as_mut() {
            if let Err(e) = doc.set_text_object_font_size(edit.page, edit.object_index, size) {
                self.error = Some(format!("Could not restore text size: {e}"));
                return;
            }
            if let Err(e) = doc.set_text_object_render_mode(edit.page, edit.object_index, mode) {
                self.error = Some(format!("Could not restore text style: {e}"));
                return;
            }
        }

        self.refresh_after_image_edit(edit.page);
        self.invalidate_text_runs();
    }

    /// Undo/redo a rotate-image edit by applying the opposite rotation.
    fn restore_image_rotate(&mut self, edit: &ImageRotate, to_original: bool) {
        let degrees = if to_original {
            -edit.degrees_cw
        } else {
            edit.degrees_cw
        };

        if let Some(doc) = self.document.as_mut()
            && let Err(e) = doc.rotate_image_object(edit.page, edit.object_index, degrees)
        {
            self.error = Some(format!("Could not restore image rotation: {e}"));
            return;
        }

        self.refresh_after_image_edit(edit.page);
    }

    /// Undo/redo a replace-image edit by restoring the before or after bytes.
    fn restore_image_replace(&mut self, edit: &ImageReplace, to_original: bool) {
        let bytes = if to_original {
            &edit.before_bytes
        } else {
            &edit.after_bytes
        };

        if let Some(doc) = self.document.as_mut()
            && let Err(e) = doc.replace_image_object_from_bytes(edit.page, edit.object_index, bytes)
        {
            self.error = Some(format!("Could not restore image replacement: {e}"));
            return;
        }

        self.refresh_after_image_edit(edit.page);
    }

    /// Undo/redo a crop-image edit by restoring both bitmap and matrix.
    fn restore_image_crop(&mut self, edit: &ImageCrop, to_original: bool) {
        let (bytes, matrix) = if to_original {
            (&edit.before_bytes, edit.before_matrix)
        } else {
            (&edit.after_bytes, edit.after_matrix)
        };

        if let Some(doc) = self.document.as_mut()
            && let Err(e) = doc.restore_image_object(edit.page, edit.object_index, bytes, matrix)
        {
            self.error = Some(format!("Could not restore image crop: {e}"));
            return;
        }

        self.refresh_after_image_edit(edit.page);
    }

    /// Common cache refresh after an image-object undo/redo.
    fn refresh_after_image_edit(&mut self, page: usize) {
        if let Some(doc) = self.document.as_mut() {
            doc.refresh_render_docs(self.pdfium);
        }
        self.cache.retain(|(p, _)| *p != page);
        self.thumbs.retain(|(p, _)| *p != page);
    }

    fn save(&mut self) {
        let Some(doc) = self.document.as_mut() else {
            return;
        };
        match doc.save() {
            Ok(()) => {
                self.dirty = false;
                self.status = format!("Saved {}", doc.path.display());
            }
            Err(e) => self.error = Some(format!("Could not save: {e}")),
        }
    }

    fn save_to(&mut self, path: &std::path::Path) {
        let Some(doc) = self.document.as_mut() else {
            return;
        };
        match doc.save_as(path) {
            Ok(()) => {
                doc.path = path.to_path_buf();
                self.dirty = false;
                self.status = format!("Saved {}", path.display());
            }
            Err(e) => self.error = Some(format!("Could not save: {e}")),
        }
    }

    fn place_image(&mut self, path: &std::path::Path) {
        let img = match image::open(path) {
            Ok(img) => img,
            Err(e) => {
                self.error = Some(format!("Could not load image: {e}"));
                return;
            }
        };
        // Natural size: 150 pt wide, preserving aspect ratio.
        let w = 150.0f32;
        let h = w * (img.height() as f32 / img.width().max(1) as f32);

        let mut a = Annotation::new(0, AnnotationKind::Stamp);
        a.image_path = Some(path.to_string_lossy().to_string());
        a.image_size = Some((w, h));
        // Place at the centre of the current page.
        let (pw, ph) = self
            .document
            .as_ref()
            .and_then(|d| d.pages.get(self.current_page))
            .map(|p| (p.width_pts, p.height_pts))
            .unwrap_or((612.0, 792.0));
        let l = (pw - w) / 2.0;
        let b = (ph - h) / 2.0;
        a.rect = PdfRect::new_from_values(b, l, b + h, l + w);

        self.commit_annotation(self.current_page, a);
    }

    // ------------------------------------------------------------ actions

    pub(crate) fn go_to(&mut self, page: usize) {
        let Some(doc) = self.document.as_ref() else {
            return;
        };
        let page = page.min(doc.page_count.saturating_sub(1));
        self.current_page = page;
        self.scroll_target = Some(page);
        self.selected = None;
        crate::persist::save_last_position(page);
    }

    fn zoom_in(&mut self) {
        self.fit_width = false;
        self.zoom *= 1.25;
        self.request_fit_rebuild();
    }

    fn zoom_out(&mut self) {
        self.fit_width = false;
        self.zoom /= 1.25;
        self.request_fit_rebuild();
    }

    fn delete_page(&mut self) {
        let Some(doc) = self.document.as_mut() else {
            return;
        };
        match doc.delete_page(self.current_page) {
            Ok(()) => {
                self.dirty = true;
                self.current_page = self.current_page.min(doc.page_count.saturating_sub(1));
                self.cache.clear();
                self.thumbs.clear();
                self.selected = None;
                self.scroll_target = Some(self.current_page);
                self.invalidate_text_runs();
                self.status = "Page deleted".into();
            }
            Err(e) => self.error = Some(format!("Could not delete page: {e}")),
        }
    }

    fn insert_page(&mut self) {
        let Some(doc) = self.document.as_mut() else {
            return;
        };
        match doc.insert_blank_page(self.current_page) {
            Ok(()) => {
                self.dirty = true;
                self.current_page += 1;
                self.cache.clear();
                self.thumbs.clear();
                self.scroll_target = Some(self.current_page);
                self.invalidate_text_runs();
                self.status = "Blank page inserted".into();
            }
            Err(e) => self.error = Some(format!("Could not insert page: {e}")),
        }
    }

    fn rotate_page(&mut self) {
        let Some(doc) = self.document.as_mut() else {
            return;
        };
        match doc.rotate_page(self.current_page) {
            Ok(()) => {
                self.dirty = true;
                self.cache.clear();
                self.thumbs.clear();
                self.invalidate_text_runs();
                self.status = "Page rotated 90°".into();
            }
            Err(e) => self.error = Some(format!("Could not rotate page: {e}")),
        }
    }

    fn compress_action(&mut self) {
        let opts = CompressionOptions {
            max_dpi: self.compress_max_dpi,
            jpeg_quality: self.compress_quality,
        };
        let Some(doc) = self.document.as_mut() else {
            return;
        };
        match doc.compress(self.pdfium, &opts) {
            Ok(stats) => {
                self.undo.clear();
                self.dirty = true;
                let before_kb = stats.bytes_before as f64 / 1024.0;
                let after_kb = stats.bytes_after as f64 / 1024.0;
                let pct = if stats.bytes_before > 0 && stats.bytes_after < stats.bytes_before {
                    (1.0 - (stats.bytes_after as f64 / stats.bytes_before as f64)) * 100.0
                } else {
                    0.0
                };
                self.status = format!(
                    "Compressed {} image(s): {before_kb:.0} KB -> {after_kb:.0} KB ({pct:.0}% smaller)",
                    stats.images_processed
                );
                self.compress_stats = Some(stats);
                self.clear_page_caches();
            }
            Err(e) => self.error = Some(format!("Could not compress PDF: {e}")),
        }
    }

    fn clear_page_caches(&mut self) {
        self.cache.clear();
        self.thumbs.clear();
        *self.thumb_pending.lock().unwrap() = Vec::new();
        *self.thumb_rendering.lock().unwrap() = Vec::new();
        self.selected = None;
        self.invalidate_text_runs();
        self.refresh_form_fields();
        self.refresh_all_links();
    }

    fn refresh_all_links(&mut self) {
        let Some(doc) = self.document.as_ref() else {
            self.page_links.clear();
            return;
        };
        self.page_links = (0..doc.page_count)
            .map(|p| doc.page_links(p).unwrap_or_default())
            .collect();
    }

    fn follow_link(&mut self, target: &LinkTarget) {
        match target {
            LinkTarget::Page(p) => {
                self.go_to(*p);
                self.status = format!("Followed link to page {}", p + 1);
            }
            LinkTarget::Uri(url) => {
                let url_str = url.clone();
                #[cfg(target_os = "macos")]
                let res = std::process::Command::new("open").arg(&url_str).spawn();
                #[cfg(not(target_os = "macos"))]
                let res = std::process::Command::new("xdg-open").arg(&url_str).spawn();
                match res {
                    Ok(_) => self.status = format!("Opened {url_str}"),
                    Err(e) => self.error = Some(format!("Could not open link: {e}")),
                }
            }
        }
    }

    fn apply_headers_footers_action(&mut self) {
        let total = self.document.as_ref().map(|d| d.page_count).unwrap_or(0);
        if total == 0 {
            return;
        }
        let (start_page, end_page) = if self.hf_all_pages {
            (0, None)
        } else {
            let s = self.hf_from_page.saturating_sub(1).min(total - 1);
            let e = self.hf_to_page.saturating_sub(1).clamp(s, total - 1);
            (s, Some(e))
        };
        let opts = HeaderFooterOptions {
            position: self.hf_position,
            text: self.hf_text.clone(),
            font_size: self.hf_font_size,
            color: self.hf_color,
            margin_x: self.hf_margin_x,
            margin_y: self.hf_margin_y,
            start_page,
            end_page,
            start_number: self.hf_start_number,
        };
        let Some(doc) = self.document.as_mut() else {
            return;
        };
        match doc.apply_header_footer(self.pdfium, &opts) {
            Ok(n) => {
                self.dirty = true;
                self.clear_page_caches();
                self.status = format!("Applied header/footer to {n} page(s)");
            }
            Err(e) => self.error = Some(format!("Could not apply header/footer: {e}")),
        }
    }

    fn apply_watermark_action(&mut self) {
        let total = self.document.as_ref().map(|d| d.page_count).unwrap_or(0);
        if total == 0 {
            return;
        }
        let (start_page, end_page) = if self.wm_all_pages {
            (0, None)
        } else {
            (self.current_page, Some(self.current_page))
        };
        let opts = WatermarkOptions {
            text: self.wm_text.clone(),
            font_size: self.wm_font_size,
            color: self.wm_color,
            angle_deg: self.wm_angle,
            start_page,
            end_page,
        };
        let Some(doc) = self.document.as_mut() else {
            return;
        };
        match doc.apply_watermark(self.pdfium, &opts) {
            Ok(n) => {
                self.dirty = true;
                self.clear_page_caches();
                self.status = format!("Added watermark to {n} page(s)");
            }
            Err(e) => self.error = Some(format!("Could not add watermark: {e}")),
        }
    }

    fn commit_pending_link(&mut self) {
        let Some((page, l, b, r, t)) = self.pending_link_rect.take() else {
            return;
        };
        let target = if self.link_target_is_page {
            let max_p = self.document.as_ref().map(|d| d.page_count).unwrap_or(1).max(1);
            let p = self.link_page_input.clamp(1, max_p) - 1;
            LinkTarget::Page(p)
        } else {
            LinkTarget::Uri(self.link_url_input.trim().to_string())
        };
        let Some(doc) = self.document.as_mut() else {
            return;
        };
        match doc.add_link_annotation(self.pdfium, page, l, b, r, t, &target) {
            Ok(()) => {
                self.dirty = true;
                self.refresh_all_links();
                self.status = "Hyperlink added".into();
            }
            Err(e) => self.error = Some(format!("Could not add hyperlink: {e}")),
        }
    }

    fn duplicate_page(&mut self) {
        let page = self.current_page;
        let Some(doc) = self.document.as_mut() else {
            return;
        };
        match doc.duplicate_page(self.pdfium, page) {
            Ok(new_page) => {
                self.dirty = true;
                self.current_page = new_page;
                self.selected_pages.clear();
                self.selected_pages.insert(new_page);
                self.clear_page_caches();
                self.scroll_target = Some(new_page);
                self.status = format!("Duplicated page {}", page + 1);
            }
            Err(e) => self.error = Some(format!("Could not duplicate page: {e}")),
        }
    }

    fn reorder_page(&mut self, from: usize, to: usize) {
        if from == to {
            return;
        }
        let Some(doc) = self.document.as_mut() else {
            return;
        };
        match doc.reorder_page(self.pdfium, from, to) {
            Ok(()) => {
                self.dirty = true;
                self.current_page = to;
                self.selected_pages.clear();
                self.selected_pages.insert(to);
                self.clear_page_caches();
                self.scroll_target = Some(to);
                self.status = format!("Moved page {} to position {}", from + 1, to + 1);
            }
            Err(e) => self.error = Some(format!("Could not reorder page: {e}")),
        }
    }

    fn merge_pdf_dialog(&mut self) {
        if self.document.is_none() || self.merge_pending.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.merge_pending = Some(rx);
        std::thread::spawn(move || {
            let picked = rfd::FileDialog::new()
                .add_filter("PDF documents", &["pdf"])
                .set_title("Merge / Append PDF")
                .pick_file();
            let _ = tx.send(picked);
        });
    }

    fn merge_pdf(&mut self, path: &std::path::Path) {
        let Some(doc) = self.document.as_mut() else {
            return;
        };
        let insert_at = doc.page_count;
        match doc.merge_pdf(self.pdfium, path, insert_at) {
            Ok(added) => {
                self.dirty = true;
                self.clear_page_caches();
                if added > 0 {
                    self.current_page = insert_at;
                    self.selected_pages.clear();
                    self.selected_pages.insert(insert_at);
                    self.scroll_target = Some(insert_at);
                }
                self.status = format!("Merged {added} page(s) from {}", path.display());
            }
            Err(e) => self.error = Some(format!("Could not merge {}: {e}", path.display())),
        }
    }

    fn extract_pages_dialog(&mut self) {
        if self.document.is_none() || self.extract_pages_pending.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.extract_pages_pending = Some(rx);
        std::thread::spawn(move || {
            let picked = rfd::FileDialog::new()
                .add_filter("PDF documents", &["pdf"])
                .set_file_name("extracted-pages.pdf")
                .set_title("Extract Selected Pages")
                .save_file();
            let _ = tx.send(picked);
        });
    }

    fn extract_selected_pages_to(&mut self, dest_path: &std::path::Path) {
        let Some(doc) = self.document.as_ref() else {
            return;
        };
        let mut pages: Vec<usize> = self
            .selected_pages
            .iter()
            .copied()
            .filter(|&p| p < doc.page_count)
            .collect();
        pages.sort_unstable();
        if pages.is_empty() {
            pages.push(self.current_page);
        }
        let count = pages.len();
        match doc.extract_pages_to(self.pdfium, &pages, dest_path) {
            Ok(()) => {
                self.status = format!("Extracted {count} page(s) to {}", dest_path.display());
            }
            Err(e) => self.error = Some(format!("Could not extract pages: {e}")),
        }
    }

    fn apply_page_crop(&mut self) {
        let Some(doc) = self.document.as_mut() else {
            return;
        };
        let [left, bottom, right, top] = self.crop_margins;
        let targets: Vec<usize> = if self.crop_all_pages {
            (0..doc.page_count).collect()
        } else {
            vec![self.current_page]
        };
        for &p in &targets {
            if let Err(e) = doc.crop_page_margins(p, left, bottom, right, top) {
                self.error = Some(format!("Could not crop page {}: {e}", p + 1));
                return;
            }
        }
        doc.refresh_render_docs(self.pdfium);
        self.dirty = true;
        self.clear_page_caches();
        self.status = if self.crop_all_pages {
            format!("Cropped all {} pages", targets.len())
        } else {
            format!("Cropped page {}", self.current_page + 1)
        };
    }

    fn reset_page_crop(&mut self) {
        let Some(doc) = self.document.as_mut() else {
            return;
        };
        let targets: Vec<usize> = if self.crop_all_pages {
            (0..doc.page_count).collect()
        } else {
            vec![self.current_page]
        };
        for &p in &targets {
            if let Err(e) = doc.reset_page_crop(p) {
                self.error = Some(format!("Could not reset crop on page {}: {e}", p + 1));
                return;
            }
        }
        doc.refresh_render_docs(self.pdfium);
        self.dirty = true;
        self.clear_page_caches();
        self.status = if self.crop_all_pages {
            "Reset crop on all pages".into()
        } else {
            format!("Reset crop on page {}", self.current_page + 1)
        };
    }

    /// OCR the current page and add an invisible text layer, making a scanned
    /// page searchable/selectable without changing its appearance.
    #[cfg(any(feature = "ocr", target_os = "windows"))]
    fn ocr_page(&mut self) {
        #[cfg(feature = "ocr")]
        let Some(tessdata) = tessdata_dir() else {
            self.error = Some(
                "Tesseract language data (eng.traineddata) not found".into(),
            );
            return;
        };
        #[cfg(all(target_os = "windows", not(feature = "ocr")))]
        let tessdata = std::path::PathBuf::new();
        let page = self.current_page;

        let words = {
            let Some(doc) = self.document.as_ref() else {
                return;
            };
            match pdf_viewer::ocr_page_words(doc, page, &tessdata, "eng") {
                Ok(w) => w,
                Err(e) => {
                    self.error = Some(format!("OCR failed: {e}"));
                    return;
                }
            }
        };

        if words.is_empty() {
            self.status = "OCR found no text on this page".into();
            return;
        }
        let n = words.len();

        // Keep the recognized text for the preview panel.
        self.ocr_results.insert(page, pdf_viewer::words_to_text(&words));
        self.show_page_text = true;

        let Some(doc) = self.document.as_mut() else {
            return;
        };
        if let Err(e) = doc.add_invisible_text_layer(page, &words) {
            self.error = Some(format!("Could not add text layer: {e}"));
            return;
        }
        doc.refresh_render_docs(self.pdfium);

        self.invalidate_text_runs();
        self.cache.retain(|(p, _)| *p != page);
        self.thumbs.retain(|(p, _)| *p != page);
        self.dirty = true;
        self.status = format!("OCR added {n} words to page {}", page + 1);
    }


    fn central_panel(&mut self, ui: &mut egui::Ui) {
        let available = ui.available_rect_before_wrap();
        self.last_available_width = available.width();

        self.upload_thumbnails(ui);

        if let Some((page, scale, color_image)) = self.pending.lock().unwrap().take() {
            let texture = ui.ctx().load_texture(
                format!("page-{page}-{scale:.3}"),
                color_image,
                egui::TextureOptions::LINEAR,
            );
            self.cache.retain(|(p, _)| *p != page);
            self.cache.push((page, CachedPage { scale, texture }));
        }

        if self.document.is_none() {
            ui.vertical_centered(|ui| {
                ui.add_space(120.0);
                ui.heading("PDF Editor");
                ui.add_space(8.0);
                ui.label("Open a PDF to start editing.");
                ui.add_space(16.0);
                if ui.button("📂 Open a PDF…").clicked() {
                    self.open_dialog();
                }
            });
            return;
        }

        self.ensure_stamp_textures(ui.ctx());

        if let Some(err) = self.error.take() {
            ui.centered_and_justified(|ui| {
                ui.colored_label(egui::Color32::from_rgb(240, 120, 100), err);
            });
            return;
        }

        let total = self.document.as_ref().unwrap().page_count;

        let mut sa = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if let Some(target) = self.scroll_target.take() {
            let mut y = 0.0;
            for p in 0..target.min(total) {
                y += self.page_size_px(p).y + GAP;
            }
            sa = sa.scroll_offset(egui::vec2(0.0, y));
            self.ignore_next_scroll = true;
        }

        // Take the pending hit-scroll target before the closure borrows `self`.
        let hit_scroll_target = self.pending_hit_scroll.take();

        let out = sa.show(ui, |ui| {
            let mut laid_out: Vec<egui::Rect> = Vec::with_capacity(total);
            ui.vertical_centered(|ui| {
                for page in 0..total {
                    let size = self.page_size_px(page);
                    let (page_rect, response) =
                        ui.allocate_exact_size(size, egui::Sense::click_and_drag());
                    let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                    if let Some((_, c)) = self.cache.iter().find(|(p, _)| *p == page) {
                        ui.painter()
                            .rect_filled(page_rect.expand(1.0), 2.0, egui::Color32::WHITE);
                        ui.painter()
                            .image(c.texture.id(), page_rect, uv, egui::Color32::WHITE);
                    } else {
                        ui.painter()
                            .rect_filled(page_rect, 2.0, egui::Color32::from_gray(70));
                        if page_rect.intersects(ui.clip_rect()) {
                            let label = format!("{}", page + 1);
                            ui.painter().text(
                                page_rect.center(),
                                egui::Align2::CENTER_CENTER,
                                label,
                                egui::FontId::proportional(28.0),
                                egui::Color32::GRAY,
                            );
                        }
                    }

                    // ---- dry-run original-text edits ----
                    for (run_index, run) in
                        self.text_runs.get(page).into_iter().flatten().enumerate()
                    {
                        if let Some(edit) = self
                            .text_edits
                            .iter()
                            .find(|e| e.page == page && e.run_index == run_index)
                        {
                            let s = self.effective_scale(page);
                            let rect = self.screen_bbox(
                                page, page_rect, run.left, run.bottom, run.right, run.top,
                            );
                            let painter = ui.painter();
                            painter.rect_filled(rect.expand(2.0), 0.0, egui::Color32::WHITE);
                            let base = self.pdf_to_screen(page, page_rect, run.left, run.baseline_y);
                            painter.text(
                                base,
                                egui::Align2::LEFT_BOTTOM,
                                &edit.run_new,
                                egui::FontId::proportional(run.font_size * s),
                                egui::Color32::BLACK,
                            );
                            painter.line_segment(
                                [
                                    rect.left_bottom() + egui::vec2(0.0, 1.0),
                                    rect.right_bottom() + egui::vec2(0.0, 1.0),
                                ],
                                egui::Stroke::new(1.0, egui::Color32::from_rgb(90, 170, 255)),
                            );
                        }
                    }

                    // ---- existing annotations ----
                    if let Some(doc) = self.document.as_ref() {
                        for a in &doc.annotations[page] {
                            let is_selected = self.selected == Some((page, a.id));
                            self.paint_annotation(ui, a, page, page_rect, is_selected);
                        }
                    }

                    // ---- search highlights (in content space) ----
                    let selected = self.search_selected;
                    for (i, hit) in self.search_hits.iter().enumerate() {
                        if hit.0 != page {
                            continue;
                        }
                        let (_pg, l, b, r_, t) = *hit;
                        let rect = self.screen_bbox(page, page_rect, l, b, r_, t);
                        let fill = if i == selected {
                            egui::Color32::from_rgba_unmultiplied(255, 214, 0, 160)
                        } else {
                            egui::Color32::from_rgba_unmultiplied(255, 214, 0, 70)
                        };
                        ui.painter().rect_filled(rect.expand(1.0), 1.0, fill);
                    }

                    // ---- selected word highlight (one word, or a shift-click group) ----
                    for &(sel_page, run_index, word_index) in &self.word_selection {
                        if sel_page != page {
                            continue;
                        }
                        let Some(word) = self
                            .text_runs
                            .get(page)
                            .and_then(|runs| runs.get(run_index))
                            .and_then(|run| run.words.get(word_index))
                        else {
                            continue;
                        };
                        let rect = self.screen_bbox(
                            page, page_rect, word.left, word.bottom, word.right, word.top,
                        );
                        ui.painter().rect_filled(
                            rect.expand(1.0),
                            2.0,
                            egui::Color32::from_rgba_unmultiplied(90, 170, 255, 60),
                        );
                        ui.painter().rect_stroke(
                            rect.expand(2.0),
                            2.0,
                            egui::Stroke::new(1.5, egui::Color32::from_rgb(90, 170, 255)),
                            egui::StrokeKind::Middle,
                        );
                    }

                    // ---- selected original-text run ----
                    if let Some((sel_page, run_index)) = self.selected_text_run
                        && sel_page == page
                        && let Some(run) = self.text_runs.get(page).and_then(|r| r.get(run_index))
                    {
                        let rect = self.screen_bbox(
                            page, page_rect, run.left, run.bottom, run.right, run.top,
                        );
                        ui.painter().rect_stroke(
                            rect.expand(2.0),
                            2.0,
                            egui::Stroke::new(1.5, egui::Color32::from_rgb(90, 170, 255)),
                            egui::StrokeKind::Middle,
                        );

                        if let Some(state) = &self.text_edit_state
                            && state.page == page
                            && state.run_index == run_index
                        {
                            let edit_rect = if let Some(word_index) = state.word_index
                                && let Some(word) = run.words.get(word_index)
                            {
                                self.screen_bbox(
                                    page,
                                    page_rect,
                                    word.left,
                                    word.bottom,
                                    word.right,
                                    word.top,
                                )
                            } else {
                                rect
                            };
                            self.text_edit_rect = Some(edit_rect.expand(2.0));
                        }
                    }

                    // ---- text-drag ghost preview ----
                    if let Some(drag) = self.text_drag.as_ref()
                        && drag.page == page
                    {
                        let s = self.effective_scale(page);
                        let (ox, oy) = self.text_drag_offset;

                        // A word drag previews every selected word; a line drag the
                        // whole line.
                        let mut previews: Vec<(f32, f32, f32, f32, String, f32)> = Vec::new();
                        if drag.word_drag {
                            for &(sel_page, run_index, word_index) in &self.word_selection {
                                if sel_page != page {
                                    continue;
                                }
                                let Some(run) = self
                                    .text_runs
                                    .get(page)
                                    .and_then(|runs| runs.get(run_index))
                                else {
                                    continue;
                                };
                                let Some(word) = run.words.get(word_index) else {
                                    continue;
                                };
                                previews.push((
                                    word.left,
                                    word.bottom,
                                    word.right,
                                    word.top,
                                    word.text.clone(),
                                    run.font_size,
                                ));
                            }
                        } else if let Some((selected_page, run_index)) = self.selected_text_run
                            && selected_page == page
                            && let Some(run) =
                                self.text_runs.get(page).and_then(|r| r.get(run_index))
                        {
                            previews.push((
                                run.left,
                                run.bottom,
                                run.right,
                                run.top,
                                run.text.clone(),
                                run.font_size,
                            ));
                        }

                        for (left, bottom, right, top, preview_text, font_size) in &previews {
                            let rect = self.screen_bbox(
                                page,
                                page_rect,
                                left + ox,
                                bottom + oy,
                                right + ox,
                                top + oy,
                            );
                            ui.painter().rect_filled(
                                rect.expand(1.0),
                                2.0,
                                egui::Color32::from_rgba_unmultiplied(90, 170, 255, 40),
                            );
                            ui.painter().rect_stroke(
                                rect.expand(2.0),
                                2.0,
                                egui::Stroke::new(1.5, egui::Color32::from_rgb(90, 170, 255)),
                                egui::StrokeKind::Middle,
                            );
                            // A single word also previews its glyphs; a group would
                            // just be a smear of overlapping labels.
                            if previews.len() == 1 {
                                let top_left =
                                    self.pdf_to_screen(page, page_rect, left + ox, top + oy);
                                ui.painter().text(
                                    top_left,
                                    egui::Align2::LEFT_TOP,
                                    preview_text,
                                    egui::FontId::proportional(font_size * s),
                                    egui::Color32::from_rgba_unmultiplied(60, 120, 220, 160),
                                );
                            }
                        }
                    }

                    // ---- selected existing image ----
                    if let Some((sel_page, image_index)) = self.selected_image
                        && sel_page == page
                        && let Some(doc) = self.document.as_ref()
                        && let Ok(pdf_page) = doc.doc.pages().get(page as i32)
                        && let Ok(object) = pdf_page.objects().get(image_index)
                        && let Some(image) = object.as_image_object()
                        && let Ok(bounds) = image.bounds()
                    {
                        let rect = self.screen_bbox(
                            page,
                            page_rect,
                            bounds.left().value,
                            bounds.bottom().value,
                            bounds.right().value,
                            bounds.top().value,
                        );
                        ui.painter().rect_stroke(
                            rect.expand(2.0),
                            2.0,
                            egui::Stroke::new(1.5, egui::Color32::from_rgb(90, 170, 255)),
                            egui::StrokeKind::Middle,
                        );
                    }

                    // ---- selected form field highlight ----
                    if let Some((sel_page, sel_index)) = self.selected_form_field
                        && sel_page == page
                        && let Some(field) = self
                            .form_fields
                            .iter()
                            .find(|f| f.page == page && f.index == sel_index)
                    {
                        let rect =
                            self.screen_bbox(page, page_rect, field.l, field.b, field.r, field.t);
                        ui.painter().rect_filled(
                            rect.expand(2.0),
                            2.0,
                            egui::Color32::from_rgba_unmultiplied(60, 180, 120, 60),
                        );
                        ui.painter().rect_stroke(
                            rect.expand(2.0),
                            2.0,
                            egui::Stroke::new(1.5, egui::Color32::from_rgb(60, 180, 120)),
                            egui::StrokeKind::Middle,
                        );
                        self.form_edit_rect = Some(rect.expand(2.0));
                    }

                    // ---- text-selection highlights ----
                    if let Some(sel) = &self.text_selection
                        && sel.page == page
                        && let Some(runs) = self.text_runs.get(page)
                    {
                        let mut words: Vec<&Word> = Vec::new();
                        for (ri, wi) in &sel.words {
                            if let Some(run) = runs.get(*ri)
                                && let Some(w) = run.words.get(*wi)
                            {
                                words.push(w);
                            }
                        }
                        for w in words {
                            let rect = self.screen_bbox(page, page_rect, w.left, w.bottom, w.right, w.top);
                            ui.painter().rect_filled(
                                rect.expand(1.0),
                                0.0,
                                egui::Color32::from_rgba_unmultiplied(90, 170, 255, 70),
                            );
                        }
                    }

                    // ---- in-progress text-selection drag ----
                    if let Some((drag_page, start, current)) = self.text_select_drag
                        && drag_page == page
                    {
                        let rect = egui::Rect::from_two_pos(start, current);
                        ui.painter().rect_filled(
                            rect,
                            0.0,
                            egui::Color32::from_rgba_unmultiplied(90, 170, 255, 30),
                        );
                        ui.painter().rect_stroke(
                            rect,
                            0.0,
                            egui::Stroke::new(1.0, egui::Color32::from_rgb(90, 170, 255)),
                            egui::StrokeKind::Middle,
                        );
                    }

                    // ---- in-progress crop rectangle ----
                    if self.crop_mode
                        && let Some((crop_page, _, start)) = self.crop_start
                        && crop_page == page
                    {
                        let end = self.crop_current.unwrap_or(start);
                        ui.painter().rect_stroke(
                            egui::Rect::from_two_pos(start, end),
                            2.0,
                            egui::Stroke::new(1.5, egui::Color32::from_rgb(255, 200, 60)),
                            egui::StrokeKind::Middle,
                        );
                    }

                    // ---- in-progress drawing ----
                    self.paint_interaction(ui, page, page_rect);

                    // ---- click handling ----
                    self.handle_page_click(page, page_rect, response, ui);

                    laid_out.push(page_rect);
                    ui.add_space(GAP);
                }
            });

            // Scroll to the selected search hit.
            if let Some(idx) = hit_scroll_target
                && let Some(&(p, l, b, r_, t)) = self.search_hits.get(idx)
                && let Some(pr) = laid_out.get(p)
            {
                let hit_center = self.pdf_to_screen(p, *pr, (l + r_) / 2.0, (b + t) / 2.0);
                ui.scroll_to_rect(
                    egui::Rect::from_min_size(hit_center, egui::vec2(1.0, 1.0)),
                    Some(egui::Align::Center),
                );
            }

            // Return the laid-out rects through the scroll area's output.
            laid_out
        });

        self.page_rects = out.inner;

        self.show_text_edit_overlay(ui);
        self.show_form_edit_overlay(ui);

        let top_y = out.state.clamped_offset().y;
        let mut page = total.saturating_sub(1);
        let mut acc = 0.0f32;
        for p in 0..total {
            let h = self.page_size_px(p).y + GAP;
            if top_y < acc + h {
                page = p;
                break;
            }
            acc += h;
        }
        if self.ignore_next_scroll {
            self.ignore_next_scroll = false;
        } else if page != self.current_page {
            self.current_page = page;
        }

        self.request_thumbnails();

        let mut priority = vec![page];
        for d in 1..=2 {
            if page >= d {
                priority.push(page - d);
            }
            if page + d < total {
                priority.push(page + d);
            }
        }
        let all: Vec<usize> = (page.max(2).saturating_sub(2)..(page + 5).min(total)).collect();
        self.request_missing_renders(&priority, &all);

        let keep: Vec<usize> = (page.saturating_sub(6)..(page + 12).min(total)).collect();
        self.evict_cache(&keep);
    }

    /// Paint one annotation onto the page, honouring page rotation.
    fn paint_annotation(
        &self,
        ui: &egui::Ui,
        a: &Annotation,
        page: usize,
        page_rect: egui::Rect,
        selected: bool,
    ) {
        let painter = ui.painter();
        let fill = |c: [u8; 4]| egui::Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3]);
        let (l, b, r, t) = a.bounds_f32();
        let scale = self.effective_scale(page);
        let corners = self.page_rect_corners(page, page_rect, l, b, r, t);

        match a.kind {
            AnnotationKind::Highlight
            | AnnotationKind::Strikeout
            | AnnotationKind::Underline
            | AnnotationKind::Squiggly => {
                self.paint_text_markup(ui, a, page, page_rect);
            }
            AnnotationKind::Rectangle => {
                if let Some(c) = a.style.fill {
                    painter.add(egui::Shape::convex_polygon(
                        corners.to_vec(),
                        fill(c),
                        egui::Stroke::NONE,
                    ));
                }
                if let Some(c) = a.style.stroke {
                    painter.add(egui::Shape::closed_line(
                        corners.to_vec(),
                        egui::Stroke::new((a.style.width * scale).max(0.5), fill(c)),
                    ));
                }
            }
            AnnotationKind::Ellipse => {
                let pts = ellipse_pts(self, page, page_rect, l, b, r, t);
                if let Some(c) = a.style.fill {
                    painter.add(egui::Shape::convex_polygon(
                        pts.clone(),
                        fill(c),
                        egui::Stroke::NONE,
                    ));
                }
                if let Some(c) = a.style.stroke {
                    painter.add(egui::Shape::closed_line(
                        pts,
                        egui::Stroke::new((a.style.width * scale).max(0.5), fill(c)),
                    ));
                }
            }
            AnnotationKind::FreeText => {
                if let Some(c) = a.style.fill {
                    painter.add(egui::Shape::convex_polygon(
                        corners.to_vec(),
                        fill(c),
                        egui::Stroke::NONE,
                    ));
                }
                if let Some(c) = a.style.stroke {
                    painter.add(egui::Shape::closed_line(
                        corners.to_vec(),
                        egui::Stroke::new((a.style.width * scale).max(0.5), fill(c)),
                    ));
                }
                // Text glyphs stay horizontal (egui cannot rotate text); anchor
                // them at the mapped top-left corner of the annotation.
                let top_left = self.pdf_to_screen(page, page_rect, l, t);
                let font = egui::FontId::proportional(a.font_size * scale);
                painter.text(
                    top_left,
                    egui::Align2::LEFT_TOP,
                    &a.text,
                    font,
                    egui::Color32::from_rgba_unmultiplied(
                        a.style.stroke.map(|c| c[0]).unwrap_or(0),
                        a.style.stroke.map(|c| c[1]).unwrap_or(0),
                        a.style.stroke.map(|c| c[2]).unwrap_or(0),
                        255,
                    ),
                );
            }
            AnnotationKind::Ink => {
                if a.points.len() >= 2 {
                    let c = a.style.stroke.unwrap_or([0, 0, 0, 255]);
                    let pts: Vec<egui::Pos2> = a
                        .points
                        .iter()
                        .map(|(x, y)| self.pdf_to_screen(page, page_rect, *x, *y))
                        .collect();
                    painter.add(egui::Shape::line(
                        pts,
                        egui::Stroke::new((a.style.width * scale).max(0.5), fill(c)),
                    ));
                }
            }
            AnnotationKind::Stamp => {
                if let Some(path) = a.image_path.as_ref()
                    && let Some(texture) = self.stamp_textures.get(path)
                {
                    // Images cannot be drawn as rotated quads, so use the mapped
                    // bounding box (exact for 0°/180°, approximate for 90°/270°).
                    let rect = self.screen_bbox(page, page_rect, l, b, r, t);
                    painter.image(
                        texture.id(),
                        rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
                }
            }
            AnnotationKind::Redaction => {
                painter.add(egui::Shape::convex_polygon(
                    corners.to_vec(),
                    egui::Color32::from_rgba_unmultiplied(220, 30, 30, 70),
                    egui::Stroke::NONE,
                ));
                painter.add(egui::Shape::closed_line(
                    corners.to_vec(),
                    egui::Stroke::new((1.5 * scale).max(1.0), egui::Color32::from_rgb(220, 30, 30)),
                ));
                let rect = self.screen_bbox(page, page_rect, l, b, r, t);
                if rect.width() > 36.0 && rect.height() > 14.0 {
                    painter.text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "REDACT",
                        egui::FontId::proportional((10.0 * scale).clamp(8.0, 16.0)),
                        egui::Color32::from_rgb(200, 20, 20),
                    );
                }
            }
        }

        if selected {
            painter.add(egui::Shape::closed_line(
                corners.to_vec(),
                egui::Stroke::new(2.0, egui::Color32::from_rgb(80, 160, 255)),
            ));
            for p in corners {
                let handle = egui::Rect::from_center_size(p, egui::vec2(8.0, 8.0));
                painter.rect_filled(handle, 2.0, egui::Color32::WHITE);
                painter.rect_stroke(
                    handle,
                    2.0,
                    egui::Stroke::new(1.0, egui::Color32::from_rgb(80, 160, 255)),
                    egui::StrokeKind::Middle,
                );
            }
        }
    }

    /// Paint a text-markup annotation (highlight/strikeout/underline/squiggly)
    /// using its per-line quads, falling back to the whole rect.
    fn paint_text_markup(
        &self,
        ui: &egui::Ui,
        a: &Annotation,
        page: usize,
        page_rect: egui::Rect,
    ) {
        let painter = ui.painter();
        let fill = |c: [u8; 4]| egui::Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3]);
        let scale = self.effective_scale(page);

        let quads: Vec<(f32, f32, f32, f32, f32, f32, f32, f32)> = if a.quads.is_empty() {
            let (l, b, r, t) = a.bounds_f32();
            vec![(l, b, r, b, r, t, l, t)]
        } else {
            a.quads.clone()
        };

        for (x1, y1, x2, y2, x3, y3, x4, y4) in quads {
            let corners = [
                self.pdf_to_screen(page, page_rect, x1, y1),
                self.pdf_to_screen(page, page_rect, x2, y2),
                self.pdf_to_screen(page, page_rect, x3, y3),
                self.pdf_to_screen(page, page_rect, x4, y4),
            ];
            let min_x = x1.min(x2).min(x3).min(x4);
            let max_x = x1.max(x2).max(x3).max(x4);
            let min_y = y1.min(y2).min(y3).min(y4);
            let max_y = y1.max(y2).max(y3).max(y4);

            match a.kind {
                AnnotationKind::Highlight => {
                    let c = a.style.fill.unwrap_or([255, 220, 0, 110]);
                    painter.add(egui::Shape::convex_polygon(
                        corners.to_vec(),
                        fill(c),
                        egui::Stroke::NONE,
                    ));
                }
                AnnotationKind::Strikeout => {
                    let c = a.style.stroke.unwrap_or([0, 0, 0, 255]);
                    let mid_y = (min_y + max_y) / 2.0;
                    let p1 = self.pdf_to_screen(page, page_rect, min_x, mid_y);
                    let p2 = self.pdf_to_screen(page, page_rect, max_x, mid_y);
                    painter.line_segment(
                        [p1, p2],
                        egui::Stroke::new((1.0 * scale).max(1.0), fill(c)),
                    );
                }
                AnnotationKind::Underline => {
                    let c = a.style.stroke.unwrap_or([0, 0, 0, 255]);
                    let p1 = self.pdf_to_screen(page, page_rect, min_x, min_y);
                    let p2 = self.pdf_to_screen(page, page_rect, max_x, min_y);
                    painter.line_segment(
                        [p1, p2],
                        egui::Stroke::new((1.0 * scale).max(1.0), fill(c)),
                    );
                }
                AnnotationKind::Squiggly => {
                    let c = a.style.stroke.unwrap_or([0, 0, 0, 255]);
                    let step = 3.0_f32.max((max_x - min_x) / 24.0);
                    let mut pts = Vec::new();
                    let mut x = min_x;
                    let mut flip = 1.0_f32;
                    while x < max_x {
                        let y = min_y - if flip > 0.0 { 1.5 } else { 0.0 };
                        pts.push(self.pdf_to_screen(page, page_rect, x, y));
                        x += step;
                        flip = -flip;
                    }
                    pts.push(self.pdf_to_screen(page, page_rect, max_x, min_y));
                    painter.add(egui::Shape::line(
                        pts,
                        egui::Stroke::new((1.0 * scale).max(1.0), fill(c)),
                    ));
                }
                _ => {}
            }
        }
    }

    /// Paint the annotation currently being drawn.
    fn paint_interaction(&self, ui: &egui::Ui, _page: usize, page_rect: egui::Rect) {
        let painter = ui.painter();
        match &self.interaction {
            Interaction::Drawing { kind, drag } => {
                let rect = egui::Rect::from_min_max(drag.start, drag.current);
                let style = self.current_style();
                let fill = |c: [u8; 4]| {
                    egui::Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3])
                };
                let stroke_w = style.width.max(0.5);
                match kind {
                    AnnotationKind::Highlight => {
                        let c = style.fill.unwrap_or([255, 220, 0, 110]);
                        painter.rect_filled(rect, 0.0, fill(c));
                    }
                    AnnotationKind::Redaction => {
                        painter.rect_filled(
                            rect,
                            0.0,
                            egui::Color32::from_rgba_unmultiplied(220, 30, 30, 70),
                        );
                        painter.rect_stroke(
                            rect,
                            0.0,
                            egui::Stroke::new(1.5, egui::Color32::from_rgb(220, 30, 30)),
                            egui::StrokeKind::Middle,
                        );
                    }
                    AnnotationKind::Rectangle => {
                        if let Some(c) = style.fill {
                            painter.rect_filled(rect, 0.0, fill(c));
                        }
                        if let Some(c) = style.stroke {
                            painter.rect_stroke(
                                rect,
                                0.0,
                                egui::Stroke::new(stroke_w, fill(c)),
                                egui::StrokeKind::Middle,
                            );
                        }
                    }
                    AnnotationKind::Ellipse => {
                        let radius = rect.size() / 2.0;
                        if let Some(c) = style.fill {
                            painter.add(egui::Shape::ellipse_filled(rect.center(), radius, fill(c)));
                        }
                        if let Some(c) = style.stroke {
                            painter.add(egui::Shape::ellipse_stroke(
                                rect.center(),
                                radius,
                                egui::Stroke::new(stroke_w, fill(c)),
                            ));
                        }
                    }
                    _ => {}
                }
            }
            Interaction::Inking { page, points } if points.len() >= 2 => {
                let s = self.effective_scale(*page);
                let pts: Vec<egui::Pos2> = points
                    .iter()
                    .map(|(x, y)| self.pdf_to_screen(*page, page_rect, *x, *y))
                    .collect();
                painter.add(egui::Shape::line(
                    pts,
                    egui::Stroke::new(
                        (self.stroke_width * s).max(0.5),
                        egui::Color32::from_rgba_unmultiplied(
                            self.color[0],
                            self.color[1],
                            self.color[2],
                            self.color[3],
                        ),
                    ),
                ));
            }
            _ => {}
        }
    }

    /// Handle mouse interaction on a page.
    /// Handle mouse interaction on a page.
    fn handle_page_click(
        &mut self,
        page: usize,
        page_rect: egui::Rect,
        response: egui::Response,
        ui: &egui::Ui,
    ) {
        if !response.rect.intersects(ui.clip_rect()) {
            return;
        }

        let pos = match ui.input(|i| i.pointer.interact_pos()) {
            Some(p) => p,
            None => return,
        };
        let inside = response.rect.contains(pos);

        // Paint existing links on this page and allow clicking a link with Select
        // (when no annotation was hit) or ⌘+Click.
        let cmd_held = ui.input(|i| i.modifiers.command || i.modifiers.mac_cmd);
        let (pdf_x, pdf_y) = self.screen_to_pdf(page, page_rect, pos);
        let hovered_link = if inside {
            self.page_links.get(page).and_then(|links| {
                links
                    .iter()
                    .find(|lnk| pdf_x >= lnk.l && pdf_x <= lnk.r && pdf_y >= lnk.b && pdf_y <= lnk.t)
                    .cloned()
            })
        } else {
            None
        };
        if let Some(links) = self.page_links.get(page) {
            for lnk in links {
                let rect = self.screen_bbox(page, page_rect, lnk.l, lnk.b, lnk.r, lnk.t);
                let is_hovered = hovered_link.as_ref().is_some_and(|h| h.index == lnk.index);
                if self.tool == Tool::Link || is_hovered {
                    ui.painter().rect_filled(
                        rect,
                        2.0,
                        egui::Color32::from_rgba_unmultiplied(60, 140, 255, if is_hovered { 45 } else { 20 }),
                    );
                    ui.painter().rect_stroke(
                        rect,
                        2.0,
                        egui::Stroke::new(1.0, egui::Color32::from_rgb(60, 140, 255)),
                        egui::StrokeKind::Middle,
                    );
                }
            }
        }
        if let Some(lnk) = &hovered_link {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            if response.clicked() && (cmd_held || (self.tool == Tool::Select && self.selected.is_none())) {
                let target = lnk.target.clone();
                self.follow_link(&target);
                return;
            }
        }

        if let Some((drag_page, start, curr)) = self.link_drag
            && drag_page == page
        {
            let rect = egui::Rect::from_two_pos(start, curr);
            ui.painter().rect_filled(rect, 2.0, egui::Color32::from_rgba_unmultiplied(60, 140, 255, 40));
            ui.painter().rect_stroke(rect, 2.0, egui::Stroke::new(1.5, egui::Color32::from_rgb(60, 140, 255)), egui::StrokeKind::Middle);
        }

        match self.tool {
            Tool::Select => self.handle_select(page, page_rect, response, pos, inside),
            Tool::TextSelect => self.handle_text_select(page, page_rect, response, pos, inside, ui.ctx()),
            Tool::Highlight | Tool::Rectangle | Tool::Ellipse | Tool::Redact => {
                self.handle_shape_drag(response, pos, inside)
            }
            Tool::Ink => self.handle_ink(page, page_rect, response, pos, inside),
            Tool::Text => self.handle_text_click(page, page_rect, response, pos, inside),
            Tool::EditText => self.handle_text_edit_click(page, page_rect, response, pos, inside),
            Tool::Image => {
                if inside && response.clicked() {
                    self.image_dialog();
                }
            }
            Tool::ImageSelect => {
                self.handle_image_select_click(page, page_rect, response, pos, inside)
            }
            Tool::Link => {
                if response.drag_started() && inside {
                    self.link_drag = Some((page, pos, pos));
                } else if let Some((drag_page, start, _)) = self.link_drag
                    && drag_page == page
                {
                    if response.dragged() {
                        self.link_drag = Some((page, start, pos));
                    } else if response.drag_stopped() {
                        self.link_drag = None;
                        let (x1, y1) = self.screen_to_pdf(page, page_rect, start);
                        let (x2, y2) = self.screen_to_pdf(page, page_rect, pos);
                        let (l, r) = (x1.min(x2), x1.max(x2));
                        let (b, t) = (y1.min(y2), y1.max(y2));
                        if r - l >= 4.0 && t - b >= 4.0 {
                            self.pending_link_rect = Some((page, l, b, r, t));
                        }
                    }
                }
            }
        }
    }

    /// Drag-to-select text: track the drag and compute the selection on release.
    fn handle_text_select(
        &mut self,
        page: usize,
        page_rect: egui::Rect,
        response: egui::Response,
        pos: egui::Pos2,
        inside: bool,
        ctx: &egui::Context,
    ) {
        if response.drag_started() && inside {
            self.text_select_drag = Some((page, pos, pos));
        } else if let Some((drag_page, start, _)) = self.text_select_drag
            && drag_page == page
        {
            if response.dragged() {
                self.text_select_drag = Some((page, start, pos));
            } else if response.drag_stopped() {
                self.text_select_drag = None;
                let rect = egui::Rect::from_two_pos(start, pos);
                self.select_text_in_rect(page, page_rect, rect, ctx);
            }
        }
    }

    /// Compute the text selection for a screen-space drag rectangle.
    fn select_text_in_rect(
        &mut self,
        page: usize,
        page_rect: egui::Rect,
        rect: egui::Rect,
        ctx: &egui::Context,
    ) {
        let (x1, y1) = self.screen_to_pdf(page, page_rect, rect.min);
        let (x2, y2) = self.screen_to_pdf(page, page_rect, rect.max);
        let (sl, sr) = (x1.min(x2), x1.max(x2));
        let (sb, st) = (y1.min(y2), y1.max(y2));

        self.ensure_text_runs(page);
        let runs = match self.text_runs.get(page) {
            Some(r) => r.clone(),
            None => {
                self.text_selection = None;
                return;
            }
        };

        let mut words: Vec<(usize, usize)> = Vec::new();
        for (ri, run) in runs.iter().enumerate() {
            for (wi, word) in run.words.iter().enumerate() {
                let intersects =
                    word.right >= sl && word.left <= sr && word.top >= sb && word.bottom <= st;
                if intersects {
                    words.push((ri, wi));
                }
            }
        }
        words.sort_unstable();

        if words.is_empty() {
            self.text_selection = None;
            self.status = "No text selected".into();
            return;
        }

        // Group selected words by run (line), joining in reading order.
        let mut lines: Vec<String> = Vec::new();
        let mut current_run = usize::MAX;
        for (ri, wi) in &words {
            if *ri != current_run {
                lines.push(String::new());
                current_run = *ri;
            }
            let word = &runs[*ri].words[*wi];
            if !lines.last_mut().unwrap().is_empty() {
                lines.last_mut().unwrap().push(' ');
            }
            lines.last_mut().unwrap().push_str(&word.text);
        }
        let text = lines.join("\n");

        ctx.copy_text(text.clone());
        self.text_selection = Some(TextSelection { page, words, text });
        self.status = "Text selected & copied — press H/S/U or use the toolbar".into();
    }

    /// Clear the current text selection.
    fn clear_text_selection(&mut self) {
        self.text_selection = None;
        self.text_select_drag = None;
    }

    /// Turn the current text selection into a text-markup annotation.
    fn apply_text_markup(&mut self, kind: AnnotationKind) {
        let Some(sel) = self.text_selection.take() else {
            return;
        };
        let page = sel.page;

        // Build per-line quads by grouping selected words by run.
        let runs = match self.text_runs.get(page) {
            Some(r) => r,
            None => return,
        };
        let mut by_run: std::collections::BTreeMap<usize, Vec<usize>> = std::collections::BTreeMap::new();
        for (ri, wi) in &sel.words {
            by_run.entry(*ri).or_default().push(*wi);
        }

        let mut quads = Vec::new();
        let (mut min_l, mut min_b, mut max_r, mut max_t) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for (ri, word_indices) in &by_run {
            let run = &runs[*ri];
            let mut l = f32::MAX;
            let mut b = f32::MAX;
            let mut r = f32::MIN;
            let mut t = f32::MIN;
            for wi in word_indices {
                let w = &run.words[*wi];
                l = l.min(w.left);
                b = b.min(w.bottom);
                r = r.max(w.right);
                t = t.max(w.top);
            }
            min_l = min_l.min(l);
            min_b = min_b.min(b);
            max_r = max_r.max(r);
            max_t = max_t.max(t);
            quads.push((l, b, r, b, r, t, l, t));
        }

        let mut a = Annotation::new(0, kind);
        a.rect = PdfRect::new_from_values(min_b, min_l, max_t, max_r);
        a.quads = quads;
        a.text = sel.text.clone();
        a.style = match kind {
            AnnotationKind::Highlight => StrokeStyle::highlight(),
            _ => StrokeStyle {
                stroke: Some(self.color),
                fill: None,
                width: 1.0,
            },
        };

        self.commit_annotation(page, a);
    }

    fn handle_select(
        &mut self,
        page: usize,
        page_rect: egui::Rect,
        response: egui::Response,
        pos: egui::Pos2,
        inside: bool,
    ) {
        let Some(doc) = self.document.as_ref() else {
            return;
        };

        // Resize handles of the current selection take priority.
        if response.drag_started()
            && let Some((sel_page, id)) = self.selected
            && sel_page == page
            && let Some(a) = doc.annotations[page].iter().find(|a| a.id == id).cloned()
        {
            let (l, b, r, t) = a.bounds_f32();
            let corners = self.page_rect_corners(page, page_rect, l, b, r, t);
            for corner in ResizeCorner::all() {
                // corners = [bottom-left, bottom-right, top-right, top-left].
                let p = match corner {
                    ResizeCorner::TopLeft => corners[3],
                    ResizeCorner::TopRight => corners[2],
                    ResizeCorner::BottomLeft => corners[0],
                    ResizeCorner::BottomRight => corners[1],
                };
                let handle = egui::Rect::from_center_size(p, egui::vec2(14.0, 14.0));
                if handle.contains(pos) {
                    self.interaction = Interaction::Resizing {
                        page,
                        corner,
                        original: a.clone(),
                    };
                    return;
                }
            }
        }

        if !inside {
            if response.clicked() {
                self.selected = None;
            }
            return;
        }

        let (x, y) = self.screen_to_pdf(page, page_rect, pos);
        let hit = doc.annotations[page]
            .iter()
            .rev()
            .find(|annotation| annotation.contains_point(x, y, 5.0 / self.effective_scale(page)))
            .map(|annotation| annotation.id);

        if response.clicked() {
            self.selected = hit.map(|id| (page, id));
            self.status = if hit.is_some() {
                "Selected. Drag the center to move; drag a corner to resize.".into()
            } else {
                String::new()
            };
            return;
        }

        if response.drag_started() {
            self.selected = hit.map(|id| (page, id));

            if let Some(id) = hit
                && let Some(a) = doc.annotations[page].iter().find(|a| a.id == id).cloned()
            {
                let (l, b, _, _) = a.bounds_f32();
                self.interaction = Interaction::Moving {
                    page,
                    grab_offset: (x - l, y - b),
                    original: a,
                };
            }
        }
    }

    fn handle_shape_drag(&mut self, response: egui::Response, pos: egui::Pos2, inside: bool) {
        let kind = match self.tool.to_kind() {
            Some(k) => k,
            None => return,
        };
        if !inside {
            return;
        }

        if response.drag_started() {
            self.interaction = Interaction::Drawing {
                kind,
                drag: DragState {
                    start: pos,
                    current: pos,
                },
            };
        }
        if response.dragged()
            && let Interaction::Drawing { drag, .. } = &mut self.interaction
        {
            drag.current = pos;
        }
        if response.drag_stopped()
            && let Interaction::Drawing { kind, drag } = self.interaction
        {
            self.interaction = Interaction::None;
            self.finish_drawing(kind, drag);
        }
        if response.clicked() {
            let drag = match kind {
                AnnotationKind::Highlight => DragState {
                    start: egui::pos2(pos.x, pos.y - 12.0),
                    current: egui::pos2(pos.x + 140.0, pos.y + 12.0),
                },
                _ => DragState {
                    start: egui::pos2(pos.x - 50.0, pos.y - 35.0),
                    current: egui::pos2(pos.x + 50.0, pos.y + 35.0),
                },
            };
            self.finish_drawing(kind, drag);
        }
    }

    fn handle_ink(
        &mut self,
        page: usize,
        page_rect: egui::Rect,
        response: egui::Response,
        pos: egui::Pos2,
        inside: bool,
    ) {
        if response.drag_started() && inside {
            let (x, y) = self.screen_to_pdf(page, page_rect, pos);
            self.interaction = Interaction::Inking {
                page,
                points: vec![(x, y)],
            };
        }
    }

    fn handle_text_click(
        &mut self,
        page: usize,
        page_rect: egui::Rect,
        response: egui::Response,
        pos: egui::Pos2,
        inside: bool,
    ) {
        if !inside || !response.clicked() {
            return;
        }
        let (x, y) = self.screen_to_pdf(page, page_rect, pos);
        let text = self.text_buffer.clone();
        if text.trim().is_empty() {
            self.status = "Type some text in the toolbar first, then click the page".into();
            return;
        }
        let w = (text.len() as f32 * self.font_size * 0.6).max(40.0);
        let h = self.font_size * 1.4;
        let mut a = Annotation::new(0, AnnotationKind::FreeText);
        a.text = text;
        a.font_size = self.font_size;
        a.style = self.current_style();
        a.rect = PdfRect::new_from_values(y, x, y + h, x + w);
        self.commit_annotation(page, a);
    }

    /// Drop cached original-text runs after page operations.
    fn invalidate_text_runs(&mut self) {
        if let Some(doc) = self.document.as_ref() {
            let page_count = doc.page_count;
            self.text_runs = vec![Vec::new(); page_count];
            self.text_runs_valid = vec![false; page_count];
        }
        self.selected_text_run = None;
        self.word_selection.clear();
        self.word_anchor = None;
    }

    /// Make sure the original-text runs for a page are extracted and cached.
    fn ensure_text_runs(&mut self, page: usize) {
        if self.text_runs_valid.get(page).copied().unwrap_or(false) {
            return;
        }

        let runs = match self.document.as_ref().map(|doc| doc.text_runs(page)) {
            Some(Ok(runs)) => runs,
            Some(Err(e)) => {
                self.status = format!("Could not read page text: {e}");
                return;
            }
            None => return,
        };

        if let Some(slot) = self.text_runs.get_mut(page) {
            *slot = runs;
            self.text_runs_valid[page] = true;
        }
    }

    /// Handle clicks while the Edit Text tool is active.
    fn handle_text_edit_click(
        &mut self,
        page: usize,
        page_rect: egui::Rect,
        response: egui::Response,
        pos: egui::Pos2,
        inside: bool,
    ) {
        // Process an in-progress drag first so it keeps working even when the
        // pointer has left the page rectangle mid-gesture. Only update the ghost
        // preview offset here; PDFium mutation happens once on drop.
        if response.dragged()
            && let Some(drag) = self.text_drag.as_ref()
            && drag.page == page
        {
            let delta = response.drag_delta();
            let scale = self.effective_scale(page);
            self.text_drag_offset.0 += delta.x / scale;
            self.text_drag_offset.1 -= delta.y / scale;
            return;
        }

        if response.drag_stopped()
            && let Some(drag) = self.text_drag.take()
        {
            let drag_offset = self.text_drag_offset;
            self.text_drag_offset = (0.0, 0.0);
            let travelled = drag_offset.0.abs() >= 1.0 || drag_offset.1.abs() >= 1.0;

            // Snapshot expected post-move positions so we can keep the moved text
            // selected for immediate arrow-key nudging.
            let (expected_words, expected_line) =
                self.expected_selection_after_move(drag.page, drag.word_drag, drag_offset.0, drag_offset.1);

            if travelled {
                let object_indices: Vec<usize> =
                    drag.targets.iter().map(|t| t.object_index).collect();
                if let Some(doc) = self.document.as_mut()
                    && let Err(e) = doc.translate_text_objects(
                        drag.page,
                        &object_indices,
                        drag_offset.0,
                        drag_offset.1,
                    )
                {
                    self.error = Some(format!("Could not move text: {e}"));
                }
            }

            let mut made_room = false;
            let mut moves = Vec::with_capacity(drag.targets.len());
            if travelled {
                for target in &drag.targets {
                    let after = self
                        .document
                        .as_ref()
                        .and_then(|doc| doc.text_object_matrix(drag.page, target.object_index).ok());
                    if let Some(after_matrix) = after {
                        moves.push(TextObjectMove {
                            object_index: target.object_index,
                            before_matrix: target.before_matrix,
                            after_matrix,
                        });
                    }
                }
            }
            // Every line this gesture moves, with the width it had before the
            // gesture started: the dragged lines, the ones shifted to close the gap,
            // and the ones pushed down to open a slot.
            let mut line_widths: Vec<(usize, f32)> = if travelled {
                drag.widths.clone()
            } else {
                Vec::new()
            };
            for &(index, width) in &drag.reflow_widths {
                if !line_widths.iter().any(|(known, _)| *known == index) {
                    line_widths.push((index, width));
                }
            }

            // The text actually moved: open a slot under it if auto-reflow is enabled.
            if travelled && self.auto_reflow && DROP_SLOTS_ENABLED {
                let (pushed, pushed_widths) = self.make_room_at_drop(drag.page, &moves);
                if !pushed.is_empty() {
                    made_room = true;
                    moves.extend(pushed);
                    for (index, width) in pushed_widths {
                        if !line_widths.iter().any(|(known, _)| *known == index) {
                            line_widths.push((index, width));
                        }
                    }
                }
            }

            // Closing the gap is part of the same edit, so one undo puts the
            // paragraph back together.
            let closed: f32 = drag
                .reflow
                .iter()
                .map(|moved| moved.after_matrix.f() - moved.before_matrix.f())
                .fold(0.0_f32, |largest, dy| largest.max(dy));
            moves.extend(drag.reflow);

            if moves.is_empty() {
                self.status = "Text not moved".into();
                return;
            }

            // Re-author every moved line in ONE call before refreshing the
            // render docs, so background page renders see the repaired text.
            let redrawn: Vec<RedrawnLine> = self
                .document
                .as_mut()
                .and_then(|doc| doc.repair_moved_text(drag.page, &line_widths).ok())
                .unwrap_or_default();

            if let Some(doc) = self.document.as_mut() {
                doc.refresh_render_docs(self.pdfium);
            }
            self.cache.retain(|(p, _)| *p != drag.page);
            self.thumbs.retain(|(p, _)| *p != drag.page);
            self.invalidate_text_runs();
            self.dirty = true;

            self.undo.push(
                "Move text",
                DocumentEdit::TextMove(TextMove {
                    page: drag.page,
                    moves,
                    redrawn,
                }),
            );

            if travelled {
                self.reselect_moved_text(drag.page, expected_words, expected_line);
            }

            let moved_label = if drag.word_drag { "Word moved" } else { "Text moved" };
            self.status = match (closed.abs() >= 0.5, made_room) {
                (true, true) => format!("{moved_label} · gap closed {closed:.1} pt · room made"),
                (true, false) => format!("{moved_label} · gap closed {closed:.1} pt"),
                (false, true) => format!("{moved_label} · room made"),
                (false, false) => format!("{moved_label} — use Arrow keys to nudge"),
            };
            return;
        }

        if !inside {
            return;
        }

        self.ensure_text_runs(page);

        let (x, y) = self.screen_to_pdf(page, page_rect, pos);
        let tolerance = 6.0 / self.effective_scale(page);
        let hit = self.text_runs.get(page).and_then(|runs| {
            runs.iter()
                .enumerate()
                .rev()
                .find(|(_, run)| run.contains_point(x, y, tolerance))
                .map(|(index, _)| index)
        });
        let word_hit = self.text_runs.get(page).and_then(|runs| {
            runs.iter().enumerate().rev().find_map(|(ri, run)| {
                run.words
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, w)| w.contains_point(x, y, tolerance))
                    .map(|(wi, _)| (ri, wi))
            })
        });

        // Drag to move the original text.
        if response.drag_started() {
            let run_index = hit.or_else(|| {
                self.selected_text_run
                    .filter(|(p, _)| *p == page)
                    .map(|(_, i)| i)
            });

            // Grabbing a word that is not already selected starts a fresh
            // single-word selection; grabbing one that is part of the selection
            // drags the whole group.
            if let Some((word_run, word_index)) = word_hit
                && !self
                    .word_selection
                    .iter()
                    .any(|&(p, r, w)| p == page && r == word_run && w == word_index)
            {
                self.select_word(page, word_run, word_index, false);
            }

            // With no text under the pointer, drag whatever is selected, so
            // "select, then drag" keeps working.
            let has_selection =
                !self.word_selection.is_empty() && (word_hit.is_some() || hit.is_none());
            if has_selection && self.start_selection_drag(page) {
                return;
            }

            if let Some((word_run, _)) = word_hit {
                // Could not move just the words (shared or exotic font): fall back
                // to moving the whole line rather than refusing the gesture.
                if self.start_line_drag(page, word_run) {
                    self.status =
                        "Couldn't split these words out — moving their whole line".into();
                }
                return;
            }

            if let Some(run_index) = run_index {
                self.start_line_drag(page, run_index);
            }
            return;
        }

        if response.double_clicked() {
            if let Some((run_index, word_index)) = word_hit {
                self.start_word_edit(page, run_index, word_index);
            } else if let Some(run_index) = hit {
                self.start_text_edit(page, run_index);
            }
            return;
        }

        if response.clicked() {
            let extend = response.ctx.input(|i| i.modifiers.shift);
            if let Some((run_index, word_index)) = word_hit {
                self.select_word(page, run_index, word_index, extend);
                let count = self.word_selection.len();
                self.status = if count > 1 {
                    format!("{count} words selected — drag or use Arrow keys to move")
                } else {
                    "Word selected — drag or Arrow keys to move, shift-click to add".into()
                };
            } else if let Some(run_index) = hit {
                self.selected_text_run = Some((page, run_index));
                self.word_selection.clear();
                self.word_anchor = None;
                self.status =
                    "Text line selected — drag or Arrow keys to move, double-click to edit".into();
            } else {
                self.selected_text_run = None;
                self.word_selection.clear();
                self.word_anchor = None;
            }
        }
    }

    /// Select a word, optionally extending the current selection with shift.
    ///
    /// A shift-click selects the reading-order range between the anchor word and
    /// the clicked word, so "click, shift-click" picks a phrase; a plain click
    /// starts a new selection.
    fn select_word(&mut self, page: usize, run_index: usize, word_index: usize, extend: bool) {
        self.selected_text_run = Some((page, run_index));

        let anchor = self.word_anchor.filter(|(anchored, _, _)| *anchored == page);
        match (extend, anchor) {
            (true, Some(anchor)) => {
                self.word_selection =
                    self.reading_order_range(page, (anchor.1, anchor.2), (run_index, word_index));
            }
            _ => {
                self.word_anchor = Some((page, run_index, word_index));
                self.word_selection = vec![(page, run_index, word_index)];
            }
        }
    }

    /// Words between two positions in reading order, inclusive.
    fn reading_order_range(
        &self,
        page: usize,
        from: (usize, usize),
        to: (usize, usize),
    ) -> Vec<(usize, usize, usize)> {
        // Walk visual lines: column-major, top to bottom, with a line's fragments
        // (bold lead-ins, drop caps) counted as part of that line. Without this a
        // shift-click range can start after a fragment and leave it behind.
        let mut order: Vec<(usize, usize)> = Vec::new();
        if let Some(runs) = self.text_runs.get(page) {
            for line in visual_lines(runs) {
                for &run_index in &line.runs {
                    let Some(run) = runs.get(run_index) else {
                        continue;
                    };
                    for word_index in 0..run.words.len() {
                        order.push((run_index, word_index));
                    }
                }
            }
        }

        let start = order.iter().position(|&word| word == from);
        let end = order.iter().position(|&word| word == to);
        match (start, end) {
            (Some(start), Some(end)) => {
                let (low, high) = if start <= end {
                    (start, end)
                } else {
                    (end, start)
                };
                order[low..=high]
                    .iter()
                    .map(|&(run_index, word_index)| (page, run_index, word_index))
                    .collect()
            }
            _ => vec![(page, to.0, to.1)],
        }
    }

    /// Plan which object groups must slide up to close a gap, without mutating
    /// the PDF yet.
    fn plan_gap_closure(
        &self,
        page: usize,
        removed_runs: &[usize],
    ) -> Vec<(Vec<usize>, f32)> {
        let runs = self.text_runs.get(page).cloned().unwrap_or_default();
        if runs.is_empty() || removed_runs.is_empty() {
            return Vec::new();
        }
        let shifts = reflow_after_removal(&runs, removed_runs);
        if shifts.is_empty() {
            return Vec::new();
        }
        let Some(doc) = self.document.as_ref() else {
            return Vec::new();
        };

        let mut plan = Vec::with_capacity(shifts.len());
        for shift in shifts {
            let chars: Vec<usize> = shift
                .runs
                .iter()
                .filter_map(|&run_index| runs.get(run_index))
                .flat_map(|run| run.chars.iter().copied())
                .collect();
            if let Ok(indices) = doc.text_object_indices_for_chars(page, &chars)
                && !indices.is_empty()
            {
                plan.push((indices, shift.dy));
            }
        }
        plan
    }

    /// Apply a planned gap closure and return the moves + pre-move widths.
    fn apply_gap_closure(
        &mut self,
        page: usize,
        plan: Vec<(Vec<usize>, f32)>,
    ) -> (Vec<TextObjectMove>, Vec<(usize, f32)>) {
        if plan.is_empty() {
            return (Vec::new(), Vec::new());
        }

        let mut moves = Vec::new();
        let mut widths: Vec<(usize, f32)> = Vec::new();
        let Some(doc) = self.document.as_mut() else {
            return (Vec::new(), Vec::new());
        };

        for (indices, dy) in plan {
            let mut before = Vec::with_capacity(indices.len());
            for &index in &indices {
                if let Ok(matrix) = doc.text_object_matrix(page, index) {
                    before.push((index, matrix));
                }
            }
            if before.is_empty() {
                continue;
            }
            widths.extend(doc.object_widths(page, &indices));
            if let Err(e) = doc.translate_text_objects(page, &indices, 0.0, dy) {
                self.error = Some(format!("Could not close the gap: {e}"));
                return (moves, widths);
            }
            for (index, before_matrix) in before {
                let after_matrix = doc
                    .text_object_matrix(page, index)
                    .unwrap_or(before_matrix);
                moves.push(TextObjectMove {
                    object_index: index,
                    before_matrix,
                    after_matrix,
                });
            }
        }

        (moves, widths)
    }

    /// Open a slot where the dragged text was dropped, so it does not land on top
    /// of whatever is already there.
    ///
    /// The mirror of [`Self::close_gap_after_removal`]: the source closes up, the
    /// destination moves its lines down by the dropped block's height. Runs must be
    /// re-read here — the split and the move have both changed the page.
    fn make_room_at_drop(
        &mut self,
        page: usize,
        moved: &[TextObjectMove],
    ) -> (Vec<TextObjectMove>, Vec<(usize, f32)>) {
        if moved.is_empty() {
            return (Vec::new(), Vec::new());
        }
        let dropped: std::collections::HashSet<usize> =
            moved.iter().map(|item| item.object_index).collect();

        let Some(doc) = self.document.as_ref() else {
            return (Vec::new(), Vec::new());
        };
        let Ok(runs) = doc.text_runs(page) else {
            return (Vec::new(), Vec::new());
        };

        // Which of the new runs are the dropped text, and how big is it?
        // If dropped text lands directly on an existing line, `text_runs` can
        // fuse them into one run, so check `any` instead of `all` and collect
        // any stationary objects in that fused run so they get pushed down too.
        let mut dropped_runs: Vec<usize> = Vec::new();
        let mut extra_stationary: Vec<usize> = Vec::new();
        let mut dropped_baseline = f32::MIN;
        let mut extent = (f32::MAX, f32::MIN);
        for (run_index, run) in runs.iter().enumerate() {
            let Ok(objects) = doc.text_object_indices_for_chars(page, &run.chars) else {
                continue;
            };
            if objects.is_empty() || !objects.iter().any(|index| dropped.contains(index)) {
                continue;
            }
            dropped_runs.push(run_index);
            dropped_baseline = dropped_baseline.max(run.baseline_y);
            if objects.iter().all(|index| dropped.contains(index)) {
                extent.0 = extent.0.min(run.left);
                extent.1 = extent.1.max(run.right);
            } else {
                for &index in &objects {
                    if dropped.contains(&index) {
                        if let Ok((left, _, right, _)) = doc.object_bounds(page, index) {
                            extent.0 = extent.0.min(left);
                            extent.1 = extent.1.max(right);
                        }
                    } else {
                        extra_stationary.push(index);
                    }
                }
            }
        }
        if dropped_runs.is_empty() || extent.0 >= extent.1 {
            return (Vec::new(), Vec::new());
        }

        let slot = room_for_drop(&runs, &dropped_runs, dropped_baseline, extent);
        let dy = match &slot {
            Some(s) => s.dy,
            None if !extra_stationary.is_empty() => dropped_runs
                .first()
                .and_then(|&ri| runs.get(ri))
                .map(|r| r.font_size * 1.2)
                .unwrap_or(14.0),
            None => return (Vec::new(), Vec::new()),
        };

        let mut indices: Vec<usize> = extra_stationary;
        if let Some(slot) = slot {
            for run_index in &slot.runs {
                if let Some(run) = runs.get(*run_index)
                    && let Ok(objects) = doc.text_object_indices_for_chars(page, &run.chars)
                {
                    indices.extend(objects);
                }
            }
        }
        indices.retain(|index| !dropped.contains(index));
        indices.sort_unstable();
        indices.dedup();
        if indices.is_empty() {
            return (Vec::new(), Vec::new());
        }

        let Some(doc) = self.document.as_mut() else {
            return (Vec::new(), Vec::new());
        };
        let widths = doc.object_widths(page, &indices);
        let mut moves = Vec::with_capacity(indices.len());
        for &index in &indices {
            if let Ok(matrix) = doc.text_object_matrix(page, index) {
                moves.push(TextObjectMove {
                    object_index: index,
                    before_matrix: matrix,
                    after_matrix: matrix,
                });
            }
        }
        if let Err(e) = doc.translate_text_objects(page, &indices, 0.0, -dy) {
            self.error = Some(format!("Could not make room: {e}"));
            return (moves, Vec::new());
        }
        for item in moves.iter_mut() {
            if let Ok(after) = doc.text_object_matrix(page, item.object_index) {
                item.after_matrix = after;
            }
        }
        (moves, widths)
    }

    /// Runs whose every word is selected: those lines leave a gap behind.
    fn fully_selected_runs(&self, page: usize) -> Vec<usize> {
        let Some(runs) = self.text_runs.get(page) else {
            return Vec::new();
        };
        runs.iter()
            .enumerate()
            .filter(|(run_index, run)| {
                !run.words.is_empty()
                    && run.words.iter().enumerate().all(|(word_index, _)| {
                        self.word_selection
                            .contains(&(page, *run_index, word_index))
                    })
            })
            .map(|(run_index, _)| run_index)
            .collect()
    }

    /// Begin dragging the selected words as one group.
    ///
    /// The selected words are grouped into one character range per line and split
    /// out of their lines, so the rest of each line stays exactly where it is.
    /// Returns `false` when nothing could be split, so the caller can fall back to
    /// moving the whole line.
    fn start_selection_drag(&mut self, page: usize) -> bool {
        if self.word_selection.is_empty() {
            return false;
        }

        // One character set per line: every selected word's characters, in order.
        let mut per_run: Vec<(usize, Vec<usize>)> = Vec::new();
        if let Some(runs) = self.text_runs.get(page) {
            for &(selected_page, run_index, word_index) in &self.word_selection {
                if selected_page != page {
                    continue;
                }
                let Some(word) = runs
                    .get(run_index)
                    .and_then(|run| run.words.get(word_index))
                else {
                    continue;
                };
                match per_run.iter_mut().find(|(run, _)| *run == run_index) {
                    Some(entry) => entry.1.extend(word.chars.iter().copied()),
                    None => per_run.push((run_index, word.chars.clone())),
                }
            }
        }

        let groups: Vec<Vec<usize>> = per_run
            .into_iter()
            .map(|(_, chars)| {
                let mut chars = chars;
                chars.sort_unstable();
                chars.dedup();
                chars
            })
            .collect();
        if groups.is_empty() {
            return false;
        }

        // Plan the gap closure while the cached runs still match the page,
        // then apply it only after the split succeeds (when auto-reflow is on).
        let gap_plan = if self.auto_reflow {
            self.plan_gap_closure(page, &self.fully_selected_runs(page))
        } else {
            Vec::new()
        };

        let splits = self
            .document
            .as_mut()
            .and_then(|doc| doc.split_selection_for_move(page, &groups).ok());
        let Some(splits) = splits else {
            return false;
        };

        let mut targets = Vec::with_capacity(splits.len());
        if let Some(doc) = self.document.as_ref() {
            for split in &splits {
                let Ok(matrix) = doc.text_object_matrix(page, split.span_index) else {
                    continue;
                };
                targets.push(TextObjectMove {
                    object_index: split.span_index,
                    before_matrix: matrix,
                    after_matrix: matrix,
                });
            }
        }
        if targets.is_empty() {
            return false;
        }

        let reflow = self.apply_gap_closure(page, gap_plan);

        let count = self.word_selection.len();
        // The split is visually lossless, so the page caches stay valid; they are
        // refreshed once the drag ends.
        let widths = self
            .document
            .as_ref()
            .map(|doc| doc.object_widths(page, &targets.iter().map(|t| t.object_index).collect::<Vec<_>>()))
            .unwrap_or_default();

        self.text_drag = Some(TextDrag {
            page,
            word_drag: true,
            targets,
            reflow: reflow.0,
            reflow_widths: reflow.1,
            widths,
        });
        self.text_drag_offset = (0.0, 0.0);
        self.status = if count > 1 {
            format!("Moving {count} words")
        } else {
            "Move word — drag it anywhere on the page".into()
        };
        true
    }

    /// Begin dragging a whole visual line.
    ///
    /// Every text object that owns part of the line moves together: some PDFs
    /// split one line into several objects (ligatures, kerning runs, colour
    /// changes), and moving only the object under the pointer would tear the line.
    fn start_line_drag(&mut self, page: usize, run_index: usize) -> bool {
        let Some(runs) = self.text_runs.get(page).cloned() else {
            return false;
        };
        // The whole visual line: a line can be drawn as several fragments, and
        // moving only the run under the pointer tears the line apart.
        let mut line_chars = visual_lines(&runs)
            .into_iter()
            .find(|line| line.runs.contains(&run_index))
            .map(|line| line.chars(&runs))
            .unwrap_or_else(|| {
                runs.get(run_index)
                    .map(|run| run.chars.clone())
                    .unwrap_or_default()
            });
        // A name set at the margin is its own object; take it along.
        if let Some(run) = runs.get(run_index) {
            line_chars.extend(right_margin_fragments(&runs, run));
            line_chars.sort_unstable();
            line_chars.dedup();
        }
        if line_chars.is_empty() {
            return false;
        }

        let targets = self.document.as_ref().and_then(|doc| {
            let indices = doc.text_object_indices_for_chars(page, &line_chars).ok()?;
            let mut targets = Vec::new();
            for object_index in indices {
                if let Ok(matrix) = doc.text_object_matrix(page, object_index) {
                    targets.push(TextObjectMove {
                        object_index,
                        before_matrix: matrix,
                        after_matrix: matrix,
                    });
                }
            }
            (!targets.is_empty()).then_some(targets)
        });

        let Some(targets) = targets else {
            self.status = "Couldn't move this text (no matching text object)".into();
            return false;
        };

        let gap_plan = if self.auto_reflow {
            self.plan_gap_closure(page, &[run_index])
        } else {
            Vec::new()
        };
        let reflow = self.apply_gap_closure(page, gap_plan);

        self.selected_text_run = Some((page, run_index));
        self.word_selection.clear();
        self.word_anchor = None;
        let widths = self
            .document
            .as_ref()
            .map(|doc| doc.object_widths(page, &targets.iter().map(|t| t.object_index).collect::<Vec<_>>()))
            .unwrap_or_default();

        self.text_drag = Some(TextDrag {
            page,
            word_drag: false,
            targets,
            reflow: reflow.0,
            reflow_widths: reflow.1,
            widths,
        });
        self.text_drag_offset = (0.0, 0.0);
        self.status = "Move text".into();
        true
    }

    /// Snapshot the expected post-move word/line centers so we can restore
    /// selection after refreshing text runs.
    fn expected_selection_after_move(
        &self,
        page: usize,
        word_mode: bool,
        dx: f32,
        dy: f32,
    ) -> (Vec<(String, f32, f32)>, Option<(String, f32, f32)>) {
        let mut expected_words = Vec::new();
        let mut expected_line = None;
        let Some(runs) = self.text_runs.get(page) else {
            return (expected_words, expected_line);
        };
        if word_mode {
            for &(sel_page, run_index, word_index) in &self.word_selection {
                if sel_page != page {
                    continue;
                }
                if let Some(word) = runs.get(run_index).and_then(|r| r.words.get(word_index)) {
                    expected_words.push((
                        word.text.clone(),
                        (word.left + word.right) * 0.5 + dx,
                        (word.bottom + word.top) * 0.5 + dy,
                    ));
                }
            }
        }
        if let Some((sel_page, run_index)) = self.selected_text_run
            && sel_page == page
            && let Some(run) = runs.get(run_index)
        {
            expected_line = Some((
                run.text.clone(),
                (run.left + run.right) * 0.5 + dx,
                (run.bottom + run.top) * 0.5 + dy,
            ));
        }
        (expected_words, expected_line)
    }

    /// Re-select moved text after `invalidate_text_runs` so the user can
    /// immediately nudge it further with Arrow keys.
    fn reselect_moved_text(
        &mut self,
        page: usize,
        expected_words: Vec<(String, f32, f32)>,
        expected_line: Option<(String, f32, f32)>,
    ) {
        self.ensure_text_runs(page);
        let Some(runs) = self.text_runs.get(page) else {
            return;
        };

        if !expected_words.is_empty() {
            let mut new_selection = Vec::new();
            for (text, cx, cy) in &expected_words {
                let mut best: Option<(f32, usize, usize)> = None;
                for (ri, run) in runs.iter().enumerate() {
                    for (wi, word) in run.words.iter().enumerate() {
                        if word.text != *text {
                            continue;
                        }
                        let wcx = (word.left + word.right) * 0.5;
                        let wcy = (word.bottom + word.top) * 0.5;
                        let dist = (wcx - cx).hypot(wcy - cy);
                        if dist < 12.0 && best.map(|(d, _, _)| dist < d).unwrap_or(true) {
                            best = Some((dist, ri, wi));
                        }
                    }
                }
                if let Some((_, ri, wi)) = best {
                    new_selection.push((page, ri, wi));
                }
            }
            if let Some(&(p, ri, wi)) = new_selection.first() {
                self.selected_text_run = Some((p, ri));
                self.word_anchor = Some((p, ri, wi));
                self.word_selection = new_selection;
                return;
            }
        }

        if let Some((text, cx, cy)) = expected_line {
            let mut best: Option<(f32, usize)> = None;
            for (ri, run) in runs.iter().enumerate() {
                let rcx = (run.left + run.right) * 0.5;
                let rcy = (run.bottom + run.top) * 0.5;
                let dist = (rcx - cx).hypot(rcy - cy);
                let matches_text = run.text == text || run.text.contains(&text) || text.contains(&run.text);
                let threshold = if matches_text { 20.0 } else { 8.0 };
                if dist < threshold && best.map(|(d, _)| dist < d).unwrap_or(true) {
                    best = Some((dist, ri));
                }
            }
            if let Some((_, ri)) = best {
                self.selected_text_run = Some((page, ri));
            }
        }
    }

    /// Nudge the currently selected original text (word selection or line) by
    /// `(dx, dy)` PDF points, as a single undoable step.
    fn nudge_selected_text(&mut self, dx: f32, dy: f32) {
        let Some(page) = self
            .word_selection
            .first()
            .map(|&(p, _, _)| p)
            .or_else(|| self.selected_text_run.map(|(p, _)| p))
        else {
            return;
        };
        self.ensure_text_runs(page);

        let word_mode = !self.word_selection.is_empty();
        let (expected_words, expected_line) =
            self.expected_selection_after_move(page, word_mode, dx, dy);

        let object_indices: Vec<usize> = if word_mode {
            let mut per_run: Vec<(usize, Vec<usize>)> = Vec::new();
            if let Some(runs) = self.text_runs.get(page) {
                for &(selected_page, run_index, word_index) in &self.word_selection {
                    if selected_page != page {
                        continue;
                    }
                    let Some(word) = runs.get(run_index).and_then(|r| r.words.get(word_index)) else {
                        continue;
                    };
                    match per_run.iter_mut().find(|(r, _)| *r == run_index) {
                        Some(entry) => entry.1.extend(word.chars.iter().copied()),
                        None => per_run.push((run_index, word.chars.clone())),
                    }
                }
            }
            let groups: Vec<Vec<usize>> = per_run
                .into_iter()
                .map(|(_, mut chars)| {
                    chars.sort_unstable();
                    chars.dedup();
                    chars
                })
                .collect();
            if groups.is_empty() {
                return;
            }
            let Some(splits) = self
                .document
                .as_mut()
                .and_then(|doc| doc.split_selection_for_move(page, &groups).ok())
            else {
                return;
            };
            splits.into_iter().map(|s| s.span_index).collect()
        } else if let Some((sel_page, run_index)) = self.selected_text_run
            && sel_page == page
            && let Some(runs) = self.text_runs.get(page)
        {
            let mut line_chars = visual_lines(runs)
                .into_iter()
                .find(|line| line.runs.contains(&run_index))
                .map(|line| line.chars(runs))
                .unwrap_or_else(|| {
                    runs.get(run_index)
                        .map(|run| run.chars.clone())
                        .unwrap_or_default()
                });
            if let Some(run) = runs.get(run_index) {
                line_chars.extend(right_margin_fragments(runs, run));
                line_chars.sort_unstable();
                line_chars.dedup();
            }
            let Some(indices) = self
                .document
                .as_ref()
                .and_then(|doc| doc.text_object_indices_for_chars(page, &line_chars).ok())
            else {
                return;
            };
            indices
        } else {
            return;
        };

        if object_indices.is_empty() {
            return;
        }

        let (before, widths) = match self.document.as_ref() {
            Some(doc) => {
                let mut before = Vec::with_capacity(object_indices.len());
                for &index in &object_indices {
                    if let Ok(matrix) = doc.text_object_matrix(page, index) {
                        before.push((index, matrix));
                    }
                }
                let widths = doc.object_widths(page, &object_indices);
                (before, widths)
            }
            None => return,
        };
        if before.is_empty() {
            return;
        }

        if let Some(doc) = self.document.as_mut()
            && let Err(e) = doc.translate_text_objects(page, &object_indices, dx, dy)
        {
            self.error = Some(format!("Could not nudge text: {e}"));
            return;
        }

        let mut moves = Vec::with_capacity(before.len());
        if let Some(doc) = self.document.as_ref() {
            for (index, before_matrix) in before {
                let after_matrix = doc.text_object_matrix(page, index).unwrap_or(before_matrix);
                moves.push(TextObjectMove {
                    object_index: index,
                    before_matrix,
                    after_matrix,
                });
            }
        }

        let redrawn: Vec<RedrawnLine> = self
            .document
            .as_mut()
            .and_then(|doc| doc.repair_moved_text(page, &widths).ok())
            .unwrap_or_default();

        if let Some(doc) = self.document.as_mut() {
            doc.refresh_render_docs(self.pdfium);
        }
        self.cache.retain(|(p, _)| *p != page);
        self.thumbs.retain(|(p, _)| *p != page);
        self.invalidate_text_runs();
        self.dirty = true;

        self.undo.push(
            "Nudge text",
            DocumentEdit::TextMove(TextMove {
                page,
                moves,
                redrawn,
            }),
        );

        self.reselect_moved_text(page, expected_words, expected_line);
        self.status = "Nudged text (Arrow keys, Shift for 10 pt)".into();
    }

    /// Begin an in-place edit for the given original-text run.
    fn start_text_edit(&mut self, page: usize, run_index: usize) {
        if self
            .text_edits
            .iter()
            .any(|e| e.page == page && e.run_index == run_index)
        {
            self.status = "This line already has a staged edit — undo it first".into();
            return;
        }

        let Some(run) = self
            .text_runs
            .get(page)
            .and_then(|runs| runs.get(run_index))
        else {
            return;
        };
        let scale = self.effective_scale(page);
        self.text_edit_state = Some(TextEditState {
            page,
            run_index,
            word_index: None,
            original: run.text.clone(),
            buffer: run.text.clone(),
            font_size: (run.font_size * scale).max(10.0),
        });
        self.text_edit_should_focus = true;
        self.selected_text_run = Some((page, run_index));
        self.status = "Editing text — Enter to apply, Esc to cancel".into();
    }

    /// Begin an in-place edit for a single word within a run.
    fn start_word_edit(&mut self, page: usize, run_index: usize, word_index: usize) {
        let Some(run) = self
            .text_runs
            .get(page)
            .and_then(|runs| runs.get(run_index))
        else {
            return;
        };
        let Some(word) = run.words.get(word_index) else {
            return;
        };
        let scale = self.effective_scale(page);
        self.text_edit_state = Some(TextEditState {
            page,
            run_index,
            word_index: Some(word_index),
            original: word.text.clone(),
            buffer: word.text.clone(),
            font_size: (run.font_size * scale).max(10.0),
        });
        self.text_edit_should_focus = true;
        self.selected_text_run = Some((page, run_index));
        self.select_word(page, run_index, word_index, false);
        self.status = "Editing word — Enter to apply, Esc to cancel".into();
    }

    /// Finish an in-place edit, staging a dry-run text edit in the UI model.
    fn commit_text_edit(&mut self) {
        let Some(state) = self.text_edit_state.take() else {
            return;
        };
        if state.buffer.trim().is_empty() || state.buffer == state.original {
            self.status = "Text edit cancelled".into();
            return;
        }

        let page = state.page;
        let run_index = state.run_index;
        let word_index = state.word_index;
        let edited_text = state.buffer.clone();

        let Some(run) = self
            .text_runs
            .get(page)
            .and_then(|runs| runs.get(run_index))
            .cloned()
        else {
            self.error = Some("Could not find the text run to edit".into());
            return;
        };

        // A word edit snapshots the full line before/after; a line edit uses
        // `edited_text` directly as the new line text.
        let (run_original, run_new, applied) = match word_index {
            Some(wi) => {
                let Some(word) = run.words.get(wi).cloned() else {
                    self.error = Some("Could not find the word to edit".into());
                    return;
                };
                let run_new = run.text.replacen(&word.text, &edited_text, 1);
                let applied = match self.document.as_mut() {
                    Some(doc) => match doc.apply_word_edit(page, &word, &edited_text) {
                        Ok(applied) => applied,
                        Err(e) => {
                            self.error = Some(format!("Could not edit word: {e}"));
                            return;
                        }
                    },
                    None => return,
                };
                (run.text.clone(), run_new, applied)
            }
            None => {
                let applied = match self.document.as_mut() {
                    Some(doc) => match doc.apply_text_edit(page, &run, &edited_text) {
                        Ok(applied) => applied,
                        Err(e) => {
                            self.error = Some(format!("Could not edit text: {e}"));
                            return;
                        }
                    },
                    None => return,
                };
                (run.text.clone(), edited_text.clone(), applied)
            }
        };

        if let Some(doc) = self.document.as_mut() {
            doc.refresh_render_docs(self.pdfium);
        }

        self.dirty = true;
        self.text_edits.clear();
        self.text_runs_valid[page] = false;
        if let Some(run) = self
            .text_runs
            .get_mut(page)
            .and_then(|runs| runs.get_mut(run_index))
        {
            run.text = run_new.clone();
        }
        self.cache.retain(|(p, _)| *p != page);
        self.thumbs.retain(|(p, _)| *p != page);
        self.selected_text_run = None;
        self.word_selection.clear();
        self.word_anchor = None;
        self.status = "Text edit applied".into();

        self.undo.push(
            "edit text",
            DocumentEdit::Text(TextEdit {
                page,
                run_index,
                run_original,
                run_new,
                object_original: applied.object_original,
                object_new: applied.object_new,
                object_index: applied.object_index,
            }),
        );
    }

    /// Cancel the in-place edit without changing anything.
    fn cancel_text_edit(&mut self) {
        self.text_edit_state = None;
        self.text_edit_should_focus = false;
        self.status = "Text edit cancelled".into();
    }

    /// Delete the currently selected original-text word, undoable.
    fn delete_selected_word(&mut self) {
        let Some((page, run_index, word_index)) =
            self.word_anchor.or_else(|| self.word_selection.first().copied())
        else {
            return;
        };
        let Some(run) = self
            .text_runs
            .get(page)
            .and_then(|r| r.get(run_index))
            .cloned()
        else {
            return;
        };
        let Some(word) = run.words.get(word_index).cloned() else {
            return;
        };

        let run_original = run.text.clone();
        let run_new = run.text.replacen(&word.text, "", 1);

        let applied = match self.document.as_mut() {
            Some(doc) => match doc.apply_word_edit(page, &word, "") {
                Ok(applied) => applied,
                Err(e) => {
                    self.error = Some(format!("Could not delete word: {e}"));
                    return;
                }
            },
            None => return,
        };

        if let Some(doc) = self.document.as_mut() {
            doc.refresh_render_docs(self.pdfium);
        }
        self.dirty = true;
        self.invalidate_text_runs();
        self.cache.retain(|(p, _)| *p != page);
        self.thumbs.retain(|(p, _)| *p != page);
        self.status = "Word deleted".into();

        self.undo.push(
            "delete word",
            DocumentEdit::Text(TextEdit {
                page,
                run_index,
                run_original,
                run_new,
                object_original: applied.object_original,
                object_new: applied.object_new,
                object_index: applied.object_index,
            }),
        );
    }


    /// Draw and drive the in-place original-text editor overlay.
    fn show_text_edit_overlay(&mut self, ui: &mut egui::Ui) {
        let Some(rect) = self.text_edit_rect.take() else {
            return;
        };
        let Some(mut state) = self.text_edit_state.take() else {
            return;
        };

        let should_focus = std::mem::take(&mut self.text_edit_should_focus);
        let field_width = rect.width().max(140.0);

        // `rect` is in global screen points; `ui.put` accepts coordinates in
        // the same global point space, so pass it through unchanged.
        let response = ui.put(
            rect,
            egui::TextEdit::singleline(&mut state.buffer)
                .font(egui::FontId::proportional(state.font_size))
                .desired_width(field_width),
        );
        if should_focus {
            response.request_focus();
        }

        // Keep requesting focus until the overlay actually has it.
        if should_focus && !response.has_focus() {
            self.text_edit_should_focus = true;
        }

        let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
        let click_away = response.lost_focus() && !enter && !escape;

        if escape {
            self.cancel_text_edit();
        } else if enter || click_away {
            self.text_edit_state = Some(state);
            self.commit_text_edit();
        } else {
            self.text_edit_state = Some(state);
        }
    }

    /// Re-read the AcroForm field list from the open document.
    fn refresh_form_fields(&mut self) {
        self.form_fields = self
            .document
            .as_ref()
            .and_then(|d| d.form_fields().ok())
            .unwrap_or_default();
    }

    /// Begin an in-place edit of a text form field.
    fn start_form_edit(&mut self, page: usize, index: usize) {
        let Some(field) = self
            .form_fields
            .iter()
            .find(|f| f.page == page && f.index == index)
            .cloned()
        else {
            return;
        };
        if field.field_type != FormFieldType::Text {
            return;
        }
        self.go_to(page);
        self.selected_form_field = Some((page, index));
        self.form_edit_state = Some(FormEditState {
            page,
            field_index: index,
            original: field.value.clone(),
            buffer: field.value.clone(),
            multiline: field.multiline,
        });
        self.form_edit_should_focus = true;
    }

    /// Toggle a checkbox/radio field immediately (no in-place editor).
    fn toggle_form_checkbox(&mut self, page: usize, index: usize) {
        let Some(field) = self
            .form_fields
            .iter()
            .find(|f| f.page == page && f.index == index)
            .cloned()
        else {
            return;
        };
        let before = field.value.clone();
        let after = if before == "true" {
            "false".to_string()
        } else {
            "true".to_string()
        };
        self.apply_form_value(page, index, before, after);
    }

    /// Write a form-field value into the PDF and refresh the view.
    fn apply_form_value(&mut self, page: usize, index: usize, before: String, after: String) {
        let Some(doc) = self.document.as_mut() else {
            return;
        };
        match doc.set_form_field_value(page, index, &after) {
            Ok(()) => {
                doc.refresh_render_docs(self.pdfium);
                self.cache.retain(|(p, _)| *p != page);
                self.thumbs.retain(|(p, _)| *p != page);
                self.dirty = true;
                self.refresh_form_fields();
                self.undo.push(
                    "fill form",
                    DocumentEdit::FormField(FormFieldEdit {
                        page,
                        index,
                        before,
                        after,
                    }),
                );
                self.status = "Form field updated".into();
            }
            Err(e) => self.error = Some(format!("Could not set form field: {e}")),
        }
    }

    /// Restore a form-field value for undo/redo.
    fn restore_form_field(&mut self, edit: &FormFieldEdit, to_original: bool) {
        let value = if to_original {
            edit.before.clone()
        } else {
            edit.after.clone()
        };
        let Some(doc) = self.document.as_mut() else {
            return;
        };
        if let Err(e) = doc.set_form_field_value(edit.page, edit.index, &value) {
            self.error = Some(format!("Could not restore form field: {e}"));
            return;
        }
        doc.refresh_render_docs(self.pdfium);
        self.cache.retain(|(p, _)| *p != edit.page);
        self.thumbs.retain(|(p, _)| *p != edit.page);
        self.refresh_form_fields();
    }

    fn cancel_form_edit(&mut self) {
        self.form_edit_state = None;
        self.form_edit_should_focus = false;
        self.selected_form_field = None;
        self.status = "Form edit cancelled".into();
    }

    fn commit_form_edit(&mut self) {
        let Some(state) = self.form_edit_state.take() else {
            return;
        };
        if state.buffer == state.original {
            self.cancel_form_edit();
            return;
        }
        let (page, index) = (state.page, state.field_index);
        let before = state.original;
        let after = state.buffer;
        self.apply_form_value(page, index, before, after);
        self.selected_form_field = None;
        self.form_edit_rect = None;
    }

    /// Draw and drive the in-place form-field editor overlay.
    fn show_form_edit_overlay(&mut self, ui: &mut egui::Ui) {
        let Some(rect) = self.form_edit_rect.take() else {
            return;
        };
        let Some(mut state) = self.form_edit_state.take() else {
            return;
        };

        let should_focus = std::mem::take(&mut self.form_edit_should_focus);
        let font_size = (rect.height() * 0.6).clamp(10.0, 48.0);
        let font = egui::FontId::proportional(font_size);
        let response = if state.multiline {
            ui.put(
                rect,
                egui::TextEdit::multiline(&mut state.buffer)
                    .font(font)
                    .desired_width(rect.width().max(140.0)),
            )
        } else {
            ui.put(
                rect,
                egui::TextEdit::singleline(&mut state.buffer)
                    .font(font)
                    .desired_width(rect.width().max(140.0)),
            )
        };
        if should_focus {
            response.request_focus();
        }
        if should_focus && !response.has_focus() {
            self.form_edit_should_focus = true;
        }

        // Escape cancels; losing focus commits. For multi-line fields, Enter
        // inserts a newline instead of committing.
        let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
        let commit = response.lost_focus() && !escape;

        if escape {
            self.cancel_form_edit();
        } else if commit {
            self.form_edit_state = Some(state);
            self.commit_form_edit();
        } else {
            self.form_edit_state = Some(state);
        }
    }

    /// Advance any in-progress interaction using the current pointer state.
    fn update_interaction(&mut self, ctx: &egui::Context) {
        let pointer = ctx.input(|i| i.pointer.clone());
        let pos = pointer.interact_pos();
        let released = pointer.primary_released();

        match self.interaction.clone() {
            Interaction::Drawing { kind, drag } => {
                if let Some(p) = pos {
                    self.interaction = Interaction::Drawing {
                        kind,
                        drag: DragState {
                            start: drag.start,
                            current: p,
                        },
                    };
                }
                if released {
                    let drag = match self.interaction {
                        Interaction::Drawing { drag, .. } => drag,
                        _ => return,
                    };
                    self.interaction = Interaction::None;
                    self.finish_drawing(kind, drag);
                }
            }
            Interaction::Inking { page, mut points } => {
                if let Some(p) = pos
                    && let Some(pr) = self.page_rect_at(page)
                    && pr.contains(p)
                {
                    let (x, y) = self.screen_to_pdf(page, pr, p);
                    let far_enough = points
                        .last()
                        .map(|(lx, ly)| (lx - x).abs() + (ly - y).abs() > 1.5)
                        .unwrap_or(true);
                    if far_enough {
                        points.push((x, y));
                    }
                }
                if released {
                    self.interaction = Interaction::None;
                    if points.len() >= 2 {
                        let mut a = Annotation::new(0, AnnotationKind::Ink);
                        a.points = points.clone();
                        a.style = self.current_style();
                        let l = points.iter().map(|p| p.0).fold(f32::MAX, f32::min);
                        let r = points.iter().map(|p| p.0).fold(f32::MIN, f32::max);
                        let b = points.iter().map(|p| p.1).fold(f32::MAX, f32::min);
                        let t = points.iter().map(|p| p.1).fold(f32::MIN, f32::max);
                        a.rect = PdfRect::new_from_values(b, l, t, r);
                        self.commit_annotation(page, a);
                    }
                } else {
                    self.interaction = Interaction::Inking { page, points };
                }
            }
            Interaction::Moving {
                page,
                grab_offset,
                original,
            } => {
                if let Some(p) = pos
                    && let Some(pr) = self.page_rect_at(page)
                {
                    let (x, y) = self.screen_to_pdf(page, pr, p);
                    let (l, b, r, t) = original.bounds_f32();
                    let w = r - l;
                    let h = t - b;
                    let (pw, ph) = self
                        .document
                        .as_ref()
                        .and_then(|d| d.pages.get(page))
                        .map(|p| (p.width_pts, p.height_pts))
                        .unwrap_or((612.0, 792.0));
                    let nl = (x - grab_offset.0).clamp(0.0, (pw - w).max(0.0));
                    let nb = (y - grab_offset.1).clamp(0.0, (ph - h).max(0.0));
                    let mut updated = original.clone();
                    updated.rect = PdfRect::new_from_values(nb, nl, nb + h, nl + w);
                    if original.kind == AnnotationKind::Ink {
                        let dx = nl - l;
                        let dy = nb - b;
                        updated.points = original
                            .points
                            .iter()
                            .map(|(px, py)| (px + dx, py + dy))
                            .collect();
                    }
                    if let Some(doc) = self.document.as_mut() {
                        let _ = doc.update_annotation(page, updated);
                        self.dirty = true;
                    }
                }
                if released {
                    self.interaction = Interaction::None;
                }
            }
            Interaction::Resizing {
                page,
                corner,
                original,
            } => {
                if let Some(p) = pos
                    && let Some(pr) = self.page_rect_at(page)
                {
                    let (x, y) = self.screen_to_pdf(page, pr, p);
                    let (l, b, r, t) = original.bounds_f32();
                    let (mut nl, mut nb, mut nr, mut nt) = if original.kind == AnnotationKind::Stamp
                    {
                        let original_width = (r - l).max(4.0);
                        let original_height = (t - b).max(4.0);
                        let (anchor_x, anchor_y, direction_x, direction_y) = match corner {
                            ResizeCorner::TopLeft => (r, b, -1.0, 1.0),
                            ResizeCorner::TopRight => (l, b, 1.0, 1.0),
                            ResizeCorner::BottomLeft => (r, t, -1.0, -1.0),
                            ResizeCorner::BottomRight => (l, t, 1.0, -1.0),
                        };
                        let requested_width = ((x - anchor_x) * direction_x).max(4.0);
                        let requested_height = ((y - anchor_y) * direction_y).max(4.0);
                        let scale = (requested_width / original_width)
                            .max(requested_height / original_height)
                            .max(4.0 / original_width)
                            .max(4.0 / original_height);
                        let width = original_width * scale;
                        let height = original_height * scale;
                        let left = if direction_x > 0.0 {
                            anchor_x
                        } else {
                            anchor_x - width
                        };
                        let right = if direction_x > 0.0 {
                            anchor_x + width
                        } else {
                            anchor_x
                        };
                        let bottom = if direction_y > 0.0 {
                            anchor_y
                        } else {
                            anchor_y - height
                        };
                        let top = if direction_y > 0.0 {
                            anchor_y + height
                        } else {
                            anchor_y
                        };
                        (left, bottom, right, top)
                    } else {
                        match corner {
                            ResizeCorner::TopLeft => (x, y, r, t),
                            ResizeCorner::TopRight => (l, y, x, t),
                            ResizeCorner::BottomLeft => (x, b, r, y),
                            ResizeCorner::BottomRight => (l, b, x, y),
                        }
                    };
                    // Keep a minimum size so the annotation can't collapse.
                    if (nr - nl).abs() < 4.0 {
                        nr = nl + 4.0;
                    }
                    if (nt - nb).abs() < 4.0 {
                        nt = nb + 4.0;
                    }
                    (nl, nr) = (nl.min(nr), nl.max(nr));
                    (nb, nt) = (nb.min(nt), nb.max(nt));
                    let mut updated = original.clone();
                    updated.rect = PdfRect::new_from_values(nb, nl, nt, nr);
                    if let Some(doc) = self.document.as_mut() {
                        let _ = doc.update_annotation(page, updated);
                        self.dirty = true;
                    }
                }
                if released {
                    self.interaction = Interaction::None;
                }
            }
            Interaction::None => {}
        }
    }

    fn finish_drawing(&mut self, kind: AnnotationKind, drag: DragState) {
        let rect = egui::Rect::from_min_max(drag.start, drag.current);
        if rect.width() < 3.0 && rect.height() < 3.0 {
            return;
        }
        if let Some(page) = self.page_at_screen_pos(drag.start)
            && let Some(pr) = self.page_rect_at(page)
        {
            let to_pdf = |p: egui::Pos2| self.screen_to_pdf(page, pr, p);
            let mut annotation = annotation_from_drag(
                0,
                kind,
                self.current_style(),
                drag.start,
                drag.current,
                to_pdf,
            );
            annotation.font_size = self.font_size;
            self.commit_annotation(page, annotation);
        }
    }

    /// Which page (if any) contains the given screen position.
    fn page_at_screen_pos(&self, pos: egui::Pos2) -> Option<usize> {
        self.page_rects.iter().position(|rect| rect.contains(pos))
    }

    /// Screen rect of a page, from the last laid-out frame.
    fn page_rect_at(&self, page: usize) -> Option<egui::Rect> {
        self.page_rects.get(page).copied()
    }

}

impl eframe::App for EditorApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_dialogs();
        self.poll_search();
        self.tick_search(&ctx);
        self.update_interaction(&ctx);

        // Intercept window close when there are unsaved changes.
        if ctx.input(|i| i.viewport().close_requested())
            && self.dirty
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm = Some(PendingConfirm::Close);
        }

        // ⌘F focuses the search field; Escape clears the search.
        let cmd_f = ctx.input(|i| {
            i.events.iter().any(|e| {
                matches!(
                    e,
                    egui::Event::Key {
                        key: egui::Key::F,
                        pressed: true,
                        modifiers,
                        ..
                    } if modifiers.command || modifiers.mac_cmd
                )
            })
        });
        if cmd_f && self.document.is_some() {
            self.search_should_focus = true;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) && !self.search_hits.is_empty() {
            self.clear_search();
            if let Some(id) = self.search_field_id {
                ctx.memory_mut(|m| m.surrender_focus(id));
            }
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.selected_text_run = None;
            self.word_selection.clear();
        self.word_anchor = None;
            self.selected_image = None;
            self.image_drag = None;
            self.clear_text_selection();
        }
        // F1 toggles the help window.
        if ctx.input(|i| i.key_pressed(egui::Key::F1)) {
            self.show_help = !self.show_help;
        }

        // Keyboard shortcuts.
        let typing = ctx.memory(|m| m.focused().is_some());
        if !typing {
            if ctx.input(|i| i.key_pressed(egui::Key::Enter))
                && self.text_edit_state.is_none()
                && self.tool == Tool::EditText
                && let Some((page, run_index)) = self.selected_text_run
            {
                self.start_text_edit(page, run_index);
            }
            // Text-selection shortcuts (H/S/U) and re-copy (C).
            if self.text_selection.is_some() {
                if ctx.input(|i| i.key_pressed(egui::Key::H)) {
                    self.apply_text_markup(AnnotationKind::Highlight);
                }
                if ctx.input(|i| i.key_pressed(egui::Key::S) && !ctx.input(|i| i.modifiers.command))
                {
                    self.apply_text_markup(AnnotationKind::Strikeout);
                }
                if ctx.input(|i| i.key_pressed(egui::Key::U)) {
                    self.apply_text_markup(AnnotationKind::Underline);
                }
                if ctx.input(|i| i.key_pressed(egui::Key::C) && !ctx.input(|i| i.modifiers.command))
                    && let Some(sel) = &self.text_selection
                {
                    ctx.copy_text(sel.text.clone());
                    self.status = "Copied selection".into();
                }
            }
            if ctx
                .input(|i| i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace))
            {
                if !self.word_selection.is_empty() {
                    self.delete_selected_word();
                } else if self.selected_text_run.is_some() {
                    self.delete_selected_text_line();
                } else {
                    self.delete_selected();
                }
            }
            if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::C)) && self.can_copy() {
                self.copy_selected_item();
            }
            if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::V)) && self.clipboard.is_some() {
                self.paste_clipboard_item();
            }
            if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::S)) {
                self.save();
            }
            if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::O)) {
                self.open_dialog();
            }
            if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Z)) {
                if ctx.input(|i| i.modifiers.shift) {
                    self.redo();
                } else {
                    self.undo();
                }
            }
            let can_nudge = self.tool == Tool::EditText
                && self.text_edit_state.is_none()
                && (self.selected_text_run.is_some() || !self.word_selection.is_empty());
            if can_nudge {
                let step = if ctx.input(|i| i.modifiers.shift) { 10.0 } else { 1.0 };
                if ctx.input(|i| i.key_pressed(egui::Key::ArrowLeft)) {
                    self.nudge_selected_text(-step, 0.0);
                }
                if ctx.input(|i| i.key_pressed(egui::Key::ArrowRight)) {
                    self.nudge_selected_text(step, 0.0);
                }
                if ctx.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                    self.nudge_selected_text(0.0, step);
                }
                if ctx.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                    self.nudge_selected_text(0.0, -step);
                }
            }
            if ctx.input(|i| {
                i.key_pressed(egui::Key::PageDown)
                    || (!can_nudge && i.key_pressed(egui::Key::ArrowDown))
            }) {
                self.go_to(self.current_page + 1);
            }
            if ctx.input(|i| {
                i.key_pressed(egui::Key::PageUp)
                    || (!can_nudge && i.key_pressed(egui::Key::ArrowUp))
            }) {
                self.go_to(self.current_page.saturating_sub(1));
            }
            if ctx.input(|i| i.key_pressed(egui::Key::Home)) {
                self.go_to(0);
            }
            if ctx.input(|i| i.key_pressed(egui::Key::End))
                && let Some(d) = self.document.as_ref()
            {
                self.go_to(d.page_count.saturating_sub(1));
            }
        }

        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
        egui::Panel::top("tools").show(ui, |ui| self.tool_bar(ui));
        egui::Panel::bottom("bottom").show(ui, |ui| self.bottom_bar(ui));
        if self.show_thumbs && self.document.is_some() {
            egui::Panel::left("thumbs")
                .resizable(true)
                .default_size(180.0)
                .show(ui, |ui| self.sidebar(ui));
        }
        if self.show_bookmarks && self.document.is_some() {
            egui::Panel::left("bookmarks")
                .resizable(true)
                .default_size(220.0)
                .show(ui, |ui| self.bookmarks_panel(ui));
        }
        if self.show_forms && self.document.is_some() {
            egui::Panel::right("forms")
                .resizable(true)
                .default_size(240.0)
                .show(ui, |ui| self.forms_panel(ui));
        }
        egui::CentralPanel::default().show(ui, |ui| self.central_panel(ui));

        // Floating help & tool windows.
        self.help_window(&ctx);
        self.crop_page_window(&ctx);
        self.signatures_window(&ctx);
        self.flatten_window(&ctx);
        self.headers_footers_window(&ctx);
        self.watermark_window(&ctx);
        self.add_link_window(&ctx);
        self.metadata_window(&ctx);
        self.compress_window(&ctx);
        self.security_window(&ctx);
        self.password_prompt_window(&ctx);

        // OCR result preview, if enabled.
        #[cfg(any(feature = "ocr", target_os = "windows"))]
        self.page_text_window(&ctx);

        // Unsaved-changes confirmation, if pending.
        self.show_confirm_window(&ctx);

        // Keep repainting while background work is running.
        let busy = !self.thumb_rendering.lock().unwrap().is_empty()
            || !self.thumb_pending.lock().unwrap().is_empty()
            || !self.rendering.lock().unwrap().is_empty()
            || self.pending.lock().unwrap().is_some()
            || self.search_rx.is_some();
        if busy {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(50));
        }
    }
}

/// Sample a page-space ellipse into screen-space points (32 segments).
fn ellipse_pts(
    app: &EditorApp,
    page: usize,
    page_rect: egui::Rect,
    l: f32,
    b: f32,
    r: f32,
    t: f32,
) -> Vec<egui::Pos2> {
    let cx = (l + r) / 2.0;
    let cy = (b + t) / 2.0;
    let rx = ((r - l) / 2.0).abs();
    let ry = ((t - b) / 2.0).abs();
    const N: usize = 32;
    (0..N)
        .map(|i| {
            let theta = (i as f32 / N as f32) * std::f32::consts::TAU;
            app.pdf_to_screen(page, page_rect, cx + rx * theta.cos(), cy + ry * theta.sin())
        })
        .collect()
}

/// Locate the Tesseract `tessdata` directory, checking the app bundle first
/// (packaged) then Homebrew locations (development).
#[cfg(feature = "ocr")]
fn tessdata_dir() -> Option<std::path::PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        let candidates = vec![
            exe.parent().map(|p| p.join("tessdata")),
            exe.parent()
                .and_then(|p| p.parent())
                .map(|p| p.join("Resources").join("tessdata")),
        ];
        for c in candidates.into_iter().flatten() {
            if c.join("eng.traineddata").exists() {
                return Some(c);
            }
        }
    }
    for p in ["/opt/homebrew/share/tessdata", "/usr/local/share/tessdata"] {
        let path = std::path::PathBuf::from(p);
        if path.join("eng.traineddata").exists() {
            return Some(path);
        }
    }
    None
}

