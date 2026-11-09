//! The editor application: UI, canvas interaction, and document operations.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};

use eframe::egui;
use pdf_viewer::annotations::{Annotation, AnnotationKind, StrokeStyle};
use pdf_viewer::{OpenDocument, SaveOptions};
use pdfium_render::prelude::*;

use crate::tools::{DragState, Interaction, ResizeCorner, Tool, annotation_from_drag};
use crate::undo::UndoStack;

const GAP: f32 = 16.0;

/// A rendered page waiting to be uploaded to the GPU on the UI thread.
type PendingTexture = Arc<Mutex<Option<(usize, f32, egui::ColorImage)>>>;

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
    thumb_pending: Arc<Mutex<Vec<(usize, egui::ColorImage)>>>,
    thumb_rendering: Arc<Mutex<Vec<usize>>>,
    thumbs: Vec<(usize, egui::TextureHandle)>,
    stamp_textures: HashMap<String, egui::TextureHandle>,

    page_input: String,

    // ---- editing ----
    tool: Tool,
    color: [u8; 4],
    fill: Option<[u8; 4]>,
    stroke_width: f32,
    font_size: f32,
    interaction: Interaction,
    selected: Option<(usize, u64)>,
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
            thumb_pending: Arc::new(Mutex::new(Vec::new())),
            thumb_rendering: Arc::new(Mutex::new(Vec::new())),
            thumbs: Vec::new(),
            stamp_textures: HashMap::new(),
            page_input: String::new(),
            tool: Tool::Select,
            color: [230, 30, 30, 255],
            fill: None,
            stroke_width: 2.0,
            font_size: 14.0,
            interaction: Interaction::None,
            selected: None,
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
                self.load_document(&path);
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
                self.interaction = Interaction::None;
                self.undo.clear();
                self.dirty = false;
                self.status = "Document opened".into();
            }
            Err(e) => {
                self.error = Some(format!("Could not open {}: {e}", path.display()));
            }
        }
    }

    // ------------------------------------------------------------ geometry

    fn effective_scale(&self, page: usize) -> f32 {
        let page_w_pts = self
            .document
            .as_ref()
            .and_then(|d| d.pages.get(page))
            .map(|p| p.width_pts)
            .unwrap_or(612.0);

        if self.fit_width && self.last_available_width > 50.0 {
            ((self.last_available_width - 48.0) / page_w_pts).clamp(0.2, 6.0)
        } else {
            self.zoom.clamp(0.2, 6.0)
        }
    }

    fn page_size_px(&self, page: usize) -> egui::Vec2 {
        let (w, h) = self
            .document
            .as_ref()
            .and_then(|d| d.pages.get(page))
            .map(|p| (p.width_pts, p.height_pts))
            .unwrap_or((612.0, 792.0));
        let s = self.effective_scale(page);
        egui::vec2(w * s, h * s)
    }

    /// Convert a screen point to PDF page coordinates for the given page rect.
    fn screen_to_pdf(&self, page: usize, page_rect: egui::Rect, pos: egui::Pos2) -> (f32, f32) {
        let s = self.effective_scale(page);
        let (w, _h) = self
            .document
            .as_ref()
            .and_then(|d| d.pages.get(page))
            .map(|p| (p.width_pts, p.height_pts))
            .unwrap_or((612.0, 792.0));
        // PDF y grows upward; screen y grows downward.
        let x = (pos.x - page_rect.min.x) / s;
        let y = (page_rect.max.y - pos.y) / s;
        let _ = w;
        (x, y)
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
            let Some(scale) = self
                .cached_scale(page)
                .map(|_| ())
                .map_or_else(|| Some(self.effective_scale(page)), |_| None)
            else {
                continue;
            };
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
                let image = render_page(&doc_arc, page, scale);
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
                let image = render_thumbnail(&doc_arc, page, THUMB_W_PX);
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
            Tool::Select => StrokeStyle::default(),
        }
    }

    fn commit_annotation(&mut self, mut annotation: Annotation) {
        if annotation.kind == AnnotationKind::Highlight {
            annotation.style = StrokeStyle::highlight();
        }
        annotation.font_size = self.font_size;

        let page = self.current_page;
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
                self.undo
                    .push("add annotation", before, doc.annotations.clone());
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
                self.undo
                    .push("delete annotation", before, doc.annotations.clone());
                let _ = annotation;
            }
            Err(e) => self.error = Some(format!("Could not delete annotation: {e}")),
        }
    }

    /// Restore an annotation snapshot, re-applying it to the PDF objects.
    fn restore_snapshot(&mut self, annotations: Vec<Vec<Annotation>>) {
        let Some(doc) = self.document.as_mut() else {
            return;
        };

        // Remove every existing annotation from the PDF, then re-create the
        // ones in the snapshot.
        let page_count = doc.page_count;
        for page in 0..page_count {
            let Ok(mut pdf_page) = doc.doc.pages_mut().get(page as i32) else {
                continue;
            };
            loop {
                let annotations = pdf_page.annotations_mut();
                if annotations.is_empty() {
                    break;
                }
                let Ok(annotation) = annotations.get(0) else {
                    break;
                };
                if annotations.delete_annotation(annotation).is_err() {
                    break;
                }
            }
        }

        doc.annotations = annotations;
        let snapshot = doc.annotations.clone();
        for page in 0..page_count {
            let list = snapshot.get(page).cloned().unwrap_or_default();
            for a in list {
                let _ = doc.apply_annotation_public(page, &a);
            }
        }
        doc.read_annotations();
    }

    fn undo(&mut self) {
        if let Some(step) = self.undo.take_undo() {
            self.restore_snapshot(step.before);
            self.dirty = true;
            self.status = format!("Undid {}", step.label);
        }
    }

    fn redo(&mut self) {
        if let Some(step) = self.undo.take_redo() {
            self.restore_snapshot(step.after);
            self.dirty = true;
            self.status = format!("Redid {}", step.label);
        }
    }

    fn save(&mut self) {
        let Some(doc) = self.document.as_mut() else {
            return;
        };
        match doc.save(SaveOptions::default()) {
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

        self.commit_annotation(a);
    }

    // ------------------------------------------------------------ actions

    fn go_to(&mut self, page: usize) {
        let Some(doc) = self.document.as_ref() else {
            return;
        };
        let page = page.min(doc.page_count.saturating_sub(1));
        self.current_page = page;
        self.scroll_target = Some(page);
        self.selected = None;
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
                self.status = "Page rotated 90°".into();
            }
            Err(e) => self.error = Some(format!("Could not rotate page: {e}")),
        }
    }

    // ------------------------------------------------------------------ ui

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button(egui::RichText::new("📂 Open").strong()).clicked() {
                self.open_dialog();
            }
            ui.label(egui::RichText::new("(⌘O)").weak());

            ui.separator();

            if ui.button("💾 Save").clicked() {
                self.save();
            }
            if ui.button("Save As…").clicked() {
                self.save_as_dialog();
            }

            ui.separator();

            if ui
                .add_enabled(self.undo.can_undo(), egui::Button::new("↩ Undo"))
                .on_disabled_hover_text("Nothing to undo")
                .on_hover_text(
                    self.undo
                        .next_undo_label()
                        .map(|l| format!("Undo {l}"))
                        .unwrap_or_default(),
                )
                .clicked()
            {
                self.undo();
            }
            if ui
                .add_enabled(self.undo.can_redo(), egui::Button::new("↪ Redo"))
                .on_disabled_hover_text("Nothing to redo")
                .on_hover_text(
                    self.undo
                        .next_redo_label()
                        .map(|l| format!("Redo {l}"))
                        .unwrap_or_default(),
                )
                .clicked()
            {
                self.redo();
            }

            ui.separator();

            if ui
                .button("🗑 Delete annotation")
                .on_hover_text("Delete the selected annotation (Delete key)")
                .clicked()
            {
                self.delete_selected();
            }

            ui.separator();

            match self.document.as_ref() {
                Some(doc) => {
                    let shown = if doc.title.is_empty() {
                        doc.path
                            .file_stem()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default()
                    } else {
                        doc.title.clone()
                    };
                    let suffix = if self.dirty { " *" } else { "" };
                    ui.label(egui::RichText::new(format!("{shown}{suffix}")).strong());
                }
                None => {
                    ui.label(egui::RichText::new("No document open").weak());
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !self.status.is_empty() {
                    ui.label(egui::RichText::new(&self.status).weak().small());
                    ui.separator();
                }
                if self.document.is_some() {
                    let mut show = self.show_thumbs;
                    if ui.toggle_value(&mut show, "Thumbnails").changed() {
                        self.show_thumbs = show;
                        self.request_thumbnails();
                    }
                }
            });
        });
    }

    fn tool_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Tool:");
            for tool in Tool::ALL {
                let text = format!("{} {}", tool.icon(), tool.label());
                if ui.selectable_label(self.tool == tool, text).clicked() {
                    self.tool = tool;
                    self.interaction = Interaction::None;
                    self.selected = None;
                    if self.tool == Tool::Image {
                        self.image_dialog();
                    }
                }
            }

            ui.separator();

            ui.label("Text:");
            ui.add_sized(
                [140.0, ui.spacing().interact_size.y],
                egui::TextEdit::singleline(&mut self.text_buffer)
                    .hint_text("note text…")
                    .desired_width(140.0),
            );

            ui.separator();

            ui.label("Colour:");
            let mut color = egui::Color32::from_rgba_unmultiplied(
                self.color[0],
                self.color[1],
                self.color[2],
                self.color[3],
            );
            if ui.color_edit_button_srgba(&mut color).changed() {
                self.color = color.to_srgba_unmultiplied();
            }

            ui.separator();

            ui.label("Fill:");
            let mut has_fill = self.fill.is_some();
            if ui.checkbox(&mut has_fill, "").changed() {
                self.fill = if has_fill { Some(self.color) } else { None };
            }
            if has_fill {
                let fill = self.fill.unwrap_or(self.color);
                let mut fill_color =
                    egui::Color32::from_rgba_unmultiplied(fill[0], fill[1], fill[2], fill[3]);
                if ui.color_edit_button_srgba(&mut fill_color).changed() {
                    self.fill = Some(fill_color.to_srgba_unmultiplied());
                }
            }

            ui.separator();

            ui.label("Width:");
            ui.add(egui::Slider::new(&mut self.stroke_width, 0.5..=12.0).step_by(0.5));

            ui.separator();

            ui.label("Font:");
            ui.add(egui::Slider::new(&mut self.font_size, 6.0..=72.0).step_by(1.0));

            ui.separator();

            if ui.button("🗑 Delete page").clicked() {
                self.delete_page();
            }
            if ui.button("➕ Insert page").clicked() {
                self.insert_page();
            }
            if ui.button("⟳ Rotate page").clicked() {
                self.rotate_page();
            }
        });
    }

    fn bottom_bar(&mut self, ui: &mut egui::Ui) {
        let total = self.document.as_ref().map(|d| d.page_count).unwrap_or(0);

        ui.add_enabled_ui(total > 0, |ui| {
            ui.horizontal(|ui| {
                if ui.button("◀ Prev").clicked() {
                    self.go_to(self.current_page.saturating_sub(1));
                }

                if total > 0 {
                    ui.label("Page");
                    let re = ui.add_sized(
                        [56.0, ui.spacing().interact_size.y],
                        egui::TextEdit::singleline(&mut self.page_input)
                            .hint_text(egui::RichText::new("go to…").weak())
                            .align(egui::Align2::LEFT_CENTER)
                            .desired_width(56.0),
                    );
                    ui.label(format!("of {total}"));
                    let commit = re.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if commit {
                        if let Ok(n) = self.page_input.trim().parse::<usize>()
                            && n >= 1
                            && n <= total
                        {
                            self.go_to(n - 1);
                        }
                        self.page_input.clear();
                        re.request_focus();
                    }
                    if !re.has_focus()
                        && (self.page_input.is_empty() || self.page_input.parse::<usize>().is_ok())
                    {
                        self.page_input = (self.current_page + 1).to_string();
                    }
                }

                if ui.button("Next ▶").clicked() {
                    self.go_to(self.current_page + 1);
                }

                ui.separator();

                if ui.button("➖").clicked() {
                    self.zoom_out();
                }
                let pct = self.effective_scale(self.current_page) * 100.0;
                ui.monospace(format!("{:>4.0}%", pct));
                if ui.button("➕").clicked() {
                    self.zoom_in();
                }

                ui.separator();

                if ui.toggle_value(&mut self.fit_width, "Fit width").changed() {
                    self.request_fit_rebuild();
                }
            });
        });
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

        let out = sa.show(ui, |ui| {
            let mut laid_out: Vec<egui::Rect> = Vec::with_capacity(total);
            ui.vertical_centered(|ui| {
                for page in 0..total {
                    let size = self.page_size_px(page);
                    let (page_rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
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

                    // ---- existing annotations ----
                    if let Some(doc) = self.document.as_ref() {
                        let s = self.effective_scale(page);
                        for a in &doc.annotations[page] {
                            let (l, b, r, t) = a.bounds_f32();
                            let rect = egui::Rect::from_min_max(
                                egui::pos2(page_rect.min.x + l * s, page_rect.max.y - t * s),
                                egui::pos2(page_rect.min.x + r * s, page_rect.max.y - b * s),
                            );
                            let is_selected = self.selected == Some((page, a.id));
                            self.paint_annotation(ui, a, rect, s, is_selected);
                        }
                    }

                    // ---- in-progress drawing ----
                    self.paint_interaction(ui, page, page_rect);

                    // ---- click handling ----
                    self.handle_page_click(page, page_rect, response, ui);

                    laid_out.push(page_rect);
                    ui.add_space(GAP);
                }
            });
            // Return the laid-out rects through the scroll area's output.
            laid_out
        });

        self.page_rects = out.inner;

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

    /// Paint one annotation onto the page.
    fn paint_annotation(
        &self,
        ui: &egui::Ui,
        a: &Annotation,
        rect: egui::Rect,
        scale: f32,
        selected: bool,
    ) {
        let painter = ui.painter();
        let fill = |c: [u8; 4]| egui::Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3]);

        match a.kind {
            AnnotationKind::Highlight => {
                let c = a.style.fill.unwrap_or([255, 220, 0, 110]);
                painter.rect_filled(rect, 0.0, fill(c));
            }
            AnnotationKind::Rectangle => {
                if let Some(c) = a.style.fill {
                    painter.rect_filled(rect, 0.0, fill(c));
                }
                if let Some(c) = a.style.stroke {
                    painter.rect_stroke(
                        rect,
                        0.0,
                        egui::Stroke::new((a.style.width * scale).max(0.5), fill(c)),
                        egui::StrokeKind::Middle,
                    );
                }
            }
            AnnotationKind::Ellipse => {
                if let Some(c) = a.style.fill {
                    painter.circle_filled(
                        rect.center(),
                        rect.width().min(rect.height()) / 2.0,
                        fill(c),
                    );
                }
                if let Some(c) = a.style.stroke {
                    painter.circle_stroke(
                        rect.center(),
                        rect.width().min(rect.height()) / 2.0,
                        egui::Stroke::new((a.style.width * scale).max(0.5), fill(c)),
                    );
                }
            }
            AnnotationKind::FreeText => {
                if let Some(c) = a.style.fill {
                    painter.rect_filled(rect, 2.0, fill(c));
                }
                if let Some(c) = a.style.stroke {
                    painter.rect_stroke(
                        rect,
                        2.0,
                        egui::Stroke::new((a.style.width * scale).max(0.5), fill(c)),
                        egui::StrokeKind::Middle,
                    );
                }
                let font = egui::FontId::proportional(a.font_size * scale);
                painter.text(
                    rect.left_top(),
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
                        .map(|(x, y)| egui::pos2(rect.min.x + x * scale, rect.max.y - y * scale))
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
            painter.rect_stroke(
                rect,
                0.0,
                egui::Stroke::new(2.0, egui::Color32::from_rgb(80, 160, 255)),
                egui::StrokeKind::Middle,
            );
            // Corner handles.
            for corner in ResizeCorner::all() {
                let p = match corner {
                    ResizeCorner::TopLeft => rect.left_top(),
                    ResizeCorner::TopRight => rect.right_top(),
                    ResizeCorner::BottomLeft => rect.left_bottom(),
                    ResizeCorner::BottomRight => rect.right_bottom(),
                };
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
    fn paint_interaction(&self, ui: &egui::Ui, page: usize, page_rect: egui::Rect) {
        let painter = ui.painter();
        match &self.interaction {
            Interaction::Drawing { kind, drag } => {
                let rect = egui::Rect::from_min_max(drag.start, drag.current);
                let style = self.current_style();
                let s = self.effective_scale(page);
                let fake = Annotation {
                    id: 0,
                    kind: *kind,
                    rect: PdfRect::new_from_values(0.0, 0.0, 0.0, 0.0),
                    points: Vec::new(),
                    text: self.text_buffer.clone(),
                    style,
                    font_size: self.font_size,
                    image_path: None,
                    image_size: None,
                };
                let _ = s;
                self.paint_annotation(ui, &fake, rect, 1.0, false);
            }
            Interaction::Inking { page, points } if points.len() >= 2 => {
                let s = self.effective_scale(*page);
                let pts: Vec<egui::Pos2> = points
                    .iter()
                    .map(|(x, y)| egui::pos2(page_rect.min.x + x * s, page_rect.max.y - y * s))
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
            Tool::Image => {
                if inside && response.clicked() {
                    self.image_dialog();
                }
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
        if let Some((sel_page, id)) = self.selected
            && sel_page == page
            && let Some(a) = doc.annotations[page].iter().find(|a| a.id == id).cloned()
        {
            let s = self.effective_scale(page);
            let (l, b, r, t) = a.bounds_f32();
            let rect = egui::Rect::from_min_max(
                egui::pos2(page_rect.min.x + l * s, page_rect.max.y - t * s),
                egui::pos2(page_rect.min.x + r * s, page_rect.max.y - b * s),
            );
            for corner in ResizeCorner::all() {
                let p = match corner {
                    ResizeCorner::TopLeft => rect.left_top(),
                    ResizeCorner::TopRight => rect.right_top(),
                    ResizeCorner::BottomLeft => rect.left_bottom(),
                    ResizeCorner::BottomRight => rect.right_bottom(),
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

        if response.drag_started() {
            // Pick the topmost annotation under the pointer.
            let mut hit = None;
            for a in doc.annotations[page].iter().rev() {
                if a.contains_point(x, y, 5.0 / self.effective_scale(page)) {
                    hit = Some(a.id);
                    break;
                }
            }

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

        if response.drag_started() && inside {
            self.interaction = Interaction::Drawing {
                kind,
                drag: DragState {
                    start: pos,
                    current: pos,
                },
            };
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
        self.commit_annotation(a);
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
                    let rect = egui::Rect::from_min_max(drag.start, drag.current);
                    if rect.width() < 3.0 && rect.height() < 3.0 {
                        return; // accidental click
                    }
                    if let Some(page) = self.page_at_screen_pos(drag.start)
                        && let Some(pr) = self.page_rect_at(page)
                    {
                        let to_pdf = |p: egui::Pos2| self.screen_to_pdf(page, pr, p);
                        let mut a = annotation_from_drag(
                            0,
                            kind,
                            self.current_style(),
                            drag.start,
                            drag.current,
                            to_pdf,
                        );
                        a.font_size = self.font_size;
                        self.commit_annotation(a);
                    }
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
                        self.commit_annotation(a);
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
                    let (mut nl, mut nb, mut nr, mut nt) = match corner {
                        ResizeCorner::TopLeft => (x, y, r, t),
                        ResizeCorner::TopRight => (l, y, x, t),
                        ResizeCorner::BottomLeft => (x, b, r, y),
                        ResizeCorner::BottomRight => (l, b, x, y),
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

    /// Which page (if any) contains the given screen position.
    fn page_at_screen_pos(&self, pos: egui::Pos2) -> Option<usize> {
        self.page_rects.iter().position(|rect| rect.contains(pos))
    }

    /// Screen rect of a page, from the last laid-out frame.
    fn page_rect_at(&self, page: usize) -> Option<egui::Rect> {
        self.page_rects.get(page).copied()
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let Some(doc) = self.document.as_ref() else {
            return;
        };
        let total = doc.page_count;
        let current = self.current_page;
        let aspects: Vec<f32> = (0..total)
            .map(|p| {
                let p = doc.pages.get(p).copied().unwrap_or(pdf_viewer::PageInfo {
                    width_pts: 612.0,
                    height_pts: 792.0,
                    rotation: PdfPageRenderRotation::None,
                });
                p.width_pts / p.height_pts
            })
            .collect();

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    for (page, &aspect) in aspects.iter().enumerate().take(total) {
                        let thumb_w = (ui.available_width() - 8.0).min(150.0);
                        let size = egui::vec2(thumb_w, thumb_w / aspect);

                        let frame_fill = if page == current {
                            ui.style().visuals.selection.bg_fill
                        } else {
                            ui.style().visuals.widgets.inactive.bg_fill
                        };

                        let frame = egui::Frame::new()
                            .fill(frame_fill)
                            .corner_radius(4.0)
                            .inner_margin(4.0);

                        let inner = frame.show(ui, |ui| {
                            ui.vertical_centered(|ui| {
                                if let Some((_, tex)) = self.thumbs.iter().find(|(p, _)| *p == page)
                                {
                                    let img = egui::Image::from_texture(tex)
                                        .max_size(size)
                                        .maintain_aspect_ratio(true);
                                    ui.add(img);
                                } else {
                                    let (rect, _) =
                                        ui.allocate_exact_size(size, egui::Sense::hover());
                                    ui.painter().rect_filled(
                                        rect,
                                        2.0,
                                        egui::Color32::from_gray(if page == current {
                                            90
                                        } else {
                                            55
                                        }),
                                    );
                                }
                                ui.label(
                                    egui::RichText::new(format!("{}", page + 1)).small().weak(),
                                );
                            });
                        });

                        let click = ui.interact(
                            inner.response.rect,
                            ui.scope_id().with(("thumb", page)),
                            egui::Sense::click(),
                        );
                        if click.clicked() {
                            self.go_to(page);
                        }
                        ui.add_space(6.0);
                    }
                });
            });
    }
}

impl eframe::App for EditorApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_dialogs();
        self.update_interaction(&ctx);

        // Keyboard shortcuts.
        let typing = ctx.memory(|m| m.focused().is_some());
        if !typing {
            if ctx
                .input(|i| i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace))
            {
                self.delete_selected();
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

        // Keep repainting while background work is running.
        let busy = !self.thumb_rendering.lock().unwrap().is_empty()
            || !self.thumb_pending.lock().unwrap().is_empty()
            || !self.rendering.lock().unwrap().is_empty()
            || self.pending.lock().unwrap().is_some();
        if busy {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(50));
        }
    }
}

/// Render one page as a small thumbnail, width-clamped to `target_w` pixels.
fn render_thumbnail(
    doc: &PdfDocument<'static>,
    page_index: usize,
    target_w: i32,
) -> egui::ColorImage {
    let blank = |w: i32, h: i32| {
        egui::ColorImage::new(
            [w.max(1) as usize, h.max(1) as usize],
            vec![egui::Color32::from_gray(60); (w.max(1) * h.max(1)) as usize],
        )
    };

    let Ok(page) = doc.pages().get(page_index as i32) else {
        return blank(target_w, target_w);
    };
    let w_pts = page.width().value.max(1.0);
    let h_pts = page.height().value.max(1.0);
    let scale = (target_w as f32 / w_pts).clamp(0.05, 4.0);
    let width = (w_pts * scale).max(1.0) as i32;
    let height = (h_pts * scale).max(1.0) as i32;

    let config = PdfRenderConfig::new()
        .set_target_width(width)
        .set_target_height(height)
        .set_reverse_byte_order(true);

    match page.render_with_config(&config) {
        Ok(bitmap) => {
            let rgba = bitmap.as_rgba_bytes();
            egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], &rgba)
        }
        Err(_) => blank(width, height),
    }
}

/// Render one page into an RGBA image for the editor canvas.
fn render_page(doc: &PdfDocument<'static>, page_index: usize, scale: f32) -> egui::ColorImage {
    let placeholder = |w: f32, h: f32| {
        let w = w.clamp(1.0, 4000.0) as usize;
        let h = h.clamp(1.0, 4000.0) as usize;
        egui::ColorImage::new([w, h], vec![egui::Color32::from_gray(200); w * h])
    };

    let Ok(page) = doc.pages().get(page_index as i32) else {
        return placeholder(612.0 * scale, 792.0 * scale);
    };
    let width = (page.width().value * scale).max(1.0) as i32;
    let height = (page.height().value * scale).max(1.0) as i32;

    let config = PdfRenderConfig::new()
        .set_target_width(width)
        .set_target_height(height)
        .set_reverse_byte_order(true);

    let Ok(bitmap) = page.render_with_config(&config) else {
        return placeholder(width as f32, height as f32);
    };

    let rgba = bitmap.as_rgba_bytes();
    egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], &rgba)
}
