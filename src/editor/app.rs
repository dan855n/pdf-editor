//! The editor application: UI, canvas interaction, and document operations.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};

use eframe::egui;
use pdf_viewer::annotations::{Annotation, AnnotationKind, StrokeStyle};
use pdf_viewer::text_model::TextRun;
use pdf_viewer::{OpenDocument, SearchHit, render_page, render_thumbnail, search_all_pages};
use pdfium_render::prelude::*;

use crate::tools::{DragState, Interaction, ResizeCorner, Tool, annotation_from_drag};
use crate::undo::{
    DocumentEdit, ImageCrop, ImageMove, ImageReplace, ImageRotate, TextEdit, TextMove, TextStyle,
    UndoStack,
};

mod image_tools;
mod search;
mod ui;

const GAP: f32 = 16.0;

/// A rendered page waiting to be uploaded to the GPU on the UI thread.
type PendingTexture = Arc<Mutex<Option<(usize, f32, egui::ColorImage)>>>;

/// A pending "you have unsaved changes" confirmation.
#[derive(Debug, Clone)]
enum PendingConfirm {
    /// The user asked to quit; confirm before discarding changes.
    Close,
    /// The user asked to open `PathBuf`; confirm before discarding changes.
    Open(PathBuf),
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
    /// Currently selected word within a run: (page, run index, word index).
    selected_word: Option<(usize, usize, usize)>,
    /// Original-text object being dragged: (page, object index).
    text_drag: Option<(usize, usize)>,
    /// Matrix of the dragged text object when the drag started, for move undo.
    text_drag_start_matrix: Option<PdfMatrix>,
    /// Accumulated drag offset in PDF points, for the live ghost preview.
    text_drag_offset: (f32, f32),
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
    undo: UndoStack,
    dirty: bool,
    status: String,

    /// Text being typed for a new free-text annotation.
    text_buffer: String,
    /// Receiver for an in-flight native file dialog.
    open_pending: Option<mpsc::Receiver<Option<PathBuf>>>,
    save_pending: Option<mpsc::Receiver<Option<PathBuf>>>,
    image_pending: Option<mpsc::Receiver<Option<PathBuf>>>,

    /// Set when we want the ScrollArea to jump to a page next frame.
    scroll_target: Option<usize>,
    ignore_next_scroll: bool,

    /// Screen rects of the pages as laid out last frame, used for hit-testing.
    page_rects: Vec<egui::Rect>,

    /// Pending unsaved-changes confirmation, if any.
    confirm: Option<PendingConfirm>,
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
            selected_word: None,
            text_drag: None,
            text_drag_start_matrix: None,
            text_drag_offset: (0.0, 0.0),
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
            undo: UndoStack::default(),
            dirty: false,
            status: "Ready".into(),
            text_buffer: String::new(),
            open_pending: None,
            save_pending: None,
            image_pending: None,
            scroll_target: None,
            ignore_next_scroll: false,
            page_rects: Vec::new(),
            confirm: None,
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
    }

    pub(crate) fn load_document(&mut self, path: &std::path::Path) {
        match OpenDocument::load(self.pdfium, path) {
            Ok(doc) => {
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
                self.selected_word = None;
                self.text_drag = None;
                self.text_drag_start_matrix = None;
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
                self.error = Some(format!("Could not open {}: {e}", path.display()));
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
            Tool::Image => StrokeStyle::default(),
            Tool::Select | Tool::EditText | Tool::ImageSelect => StrokeStyle::default(),
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
                DocumentEdit::ImageRotate(edit) => self.restore_image_rotate(&edit, true),
                DocumentEdit::ImageReplace(edit) => self.restore_image_replace(&edit, true),
                DocumentEdit::ImageCrop(edit) => self.restore_image_crop(&edit, true),
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
                DocumentEdit::ImageRotate(edit) => self.restore_image_rotate(&edit, false),
                DocumentEdit::ImageReplace(edit) => self.restore_image_replace(&edit, false),
                DocumentEdit::ImageCrop(edit) => self.restore_image_crop(&edit, false),
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
        self.selected_word = None;
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
        let matrix = if to_original {
            edit.before_matrix
        } else {
            edit.after_matrix
        };

        if let Some(doc) = self.document.as_mut()
            && let Err(e) = doc.set_text_object_matrix(edit.page, edit.object_index, matrix)
        {
            self.error = Some(format!("Could not restore text move: {e}"));
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

    /// OCR the current page and add an invisible text layer, making a scanned
    /// page searchable/selectable without changing its appearance.
    #[cfg(feature = "ocr")]
    fn ocr_page(&mut self) {
        let Some(tessdata) = tessdata_dir() else {
            self.error = Some(
                "Tesseract language data (eng.traineddata) not found".into(),
            );
            return;
        };
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

                    // ---- selected word highlight ----
                    if let Some((sel_page, run_index, word_index)) = self.selected_word
                        && sel_page == page
                        && let Some(run) = self.text_runs.get(page).and_then(|r| r.get(run_index))
                        && let Some(word) = run.words.get(word_index)
                    {
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
                    if let Some((drag_page, _)) = self.text_drag
                        && drag_page == page
                        && let Some((_, run_index)) = self.selected_text_run
                        && let Some(run) = self.text_runs.get(page).and_then(|r| r.get(run_index))
                    {
                        let s = self.effective_scale(page);
                        let (ox, oy) = self.text_drag_offset;
                        let rect = self.screen_bbox(
                            page,
                            page_rect,
                            run.left + ox,
                            run.bottom + oy,
                            run.right + ox,
                            run.top + oy,
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
                        let top_left =
                            self.pdf_to_screen(page, page_rect, run.left + ox, run.top + oy);
                        ui.painter().text(
                            top_left,
                            egui::Align2::LEFT_TOP,
                            &run.text,
                            egui::FontId::proportional(run.font_size * s),
                            egui::Color32::from_rgba_unmultiplied(60, 120, 220, 160),
                        );
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
            AnnotationKind::Highlight => {
                let c = a.style.fill.unwrap_or([255, 220, 0, 110]);
                painter.add(egui::Shape::convex_polygon(
                    corners.to_vec(),
                    fill(c),
                    egui::Stroke::NONE,
                ));
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

        match self.tool {
            Tool::Select => self.handle_select(page, page_rect, response, pos, inside),
            Tool::Highlight | Tool::Rectangle | Tool::Ellipse => {
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
        }
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
        self.selected_word = None;
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
        // pointer has left the page rectangle mid-gesture.
        if response.dragged()
            && let Some((drag_page, drag_index)) = self.text_drag
            && drag_page == page
        {
            let delta = response.drag_delta();
            let scale = self.effective_scale(page);
            let dx_pt = delta.x / scale;
            let dy_pt = -delta.y / scale;
            self.text_drag_offset.0 += dx_pt;
            self.text_drag_offset.1 += dy_pt;
            if let Some(doc) = self.document.as_mut()
                && let Err(e) = doc.translate_text_object(page, drag_index, dx_pt, dy_pt)
            {
                self.error = Some(format!("Could not move text: {e}"));
            }
            return;
        }

        if response.drag_stopped()
            && let Some((drag_page, drag_index)) = self.text_drag.take()
        {
            let before = self.text_drag_start_matrix.take();
            self.text_drag_offset = (0.0, 0.0);
            let after = self
                .document
                .as_ref()
                .and_then(|doc| doc.text_object_matrix(drag_page, drag_index).ok());
            if let Some(doc) = self.document.as_mut() {
                doc.refresh_render_docs(self.pdfium);
            }
            self.cache.retain(|(p, _)| *p != drag_page);
            self.thumbs.retain(|(p, _)| *p != drag_page);
            self.invalidate_text_runs();
            self.dirty = true;
            if let (Some(before), Some(after)) = (before, after) {
                self.undo.push(
                    "Move text",
                    DocumentEdit::TextMove(TextMove {
                        page: drag_page,
                        object_index: drag_index,
                        before_matrix: before,
                        after_matrix: after,
                    }),
                );
            }
            self.status = "Text moved".into();
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
            if let Some(run_index) = run_index {
                let run = self
                    .text_runs
                    .get(page)
                    .and_then(|r| r.get(run_index))
                    .cloned();
                if let Some(run) = run {
                    let result = self.document.as_ref().and_then(|doc| {
                        let object_index = doc.text_object_index_for_run(page, &run).ok()?;
                        let matrix = doc.text_object_matrix(page, object_index).ok();
                        Some((object_index, matrix))
                    });
                    if let Some((object_index, start_matrix)) = result {
                        self.selected_text_run = Some((page, run_index));
                        self.text_drag = Some((page, object_index));
                        self.text_drag_start_matrix = start_matrix;
                        self.text_drag_offset = (0.0, 0.0);
                        self.status = "Move text".into();
                    } else {
                        self.status =
                            "Couldn't move this text (no matching text object)".into();
                    }
                }
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
            if let Some((run_index, word_index)) = word_hit {
                self.selected_text_run = Some((page, run_index));
                self.selected_word = Some((page, run_index, word_index));
                self.status =
                    "Word selected — drag to move, double-click to edit it".into();
            } else if let Some(run_index) = hit {
                self.selected_text_run = Some((page, run_index));
                self.selected_word = None;
                self.status =
                    "Text line selected — drag to move, double-click to edit".into();
            } else {
                self.selected_text_run = None;
                self.selected_word = None;
            }
        }
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
        self.selected_word = Some((page, run_index, word_index));
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
        self.selected_word = None;
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
        let Some((page, run_index, word_index)) = self.selected_word else {
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
            self.selected_word = None;
            self.selected_image = None;
            self.image_drag = None;
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
            if ctx
                .input(|i| i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace))
            {
                if self.selected_word.is_some() {
                    self.delete_selected_word();
                } else {
                    self.delete_selected();
                }
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
            if ctx.input(|i| {
                i.key_pressed(egui::Key::ArrowDown) || i.key_pressed(egui::Key::PageDown)
            }) {
                self.go_to(self.current_page + 1);
            }
            if ctx.input(|i| i.key_pressed(egui::Key::ArrowUp) || i.key_pressed(egui::Key::PageUp))
            {
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
        egui::CentralPanel::default().show(ui, |ui| self.central_panel(ui));

        // Floating help window.
        self.help_window(&ctx);

        // OCR result preview, if enabled.
        #[cfg(feature = "ocr")]
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

