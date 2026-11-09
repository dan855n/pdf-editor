//! Stage 4 of a Rust PDF viewer built with egui/eframe + PDFium.
//!
//! New in this stage: full-document text search with highlighted hits and
//! Enter / Shift-Enter navigation between results.

use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};

use eframe::egui;
use pdfium_render::prelude::*;
use pdf_viewer::load_pdfium_bindings;

const GAP: f32 = 16.0; // gap between pages when scrolling

fn main() -> eframe::Result {
    let pdfium = Pdfium::new(load_pdfium_bindings());

    // `PdfDocument<'a>` borrows the `Pdfium` instance. Leaking the single
    // Pdfium keeps the lifetime plumbing simple; documents are still closed
    // properly when replaced (their Drop calls FPDF_CloseDocument).
    let holder = DocumentHolder {
        pdfium: Box::leak(Box::new(pdfium)),
    };

    let mut app = PdfViewerApp::new(holder);

    // Optional third arg: initial search query (for testing / deep links).
    if let Some(q) = std::env::args().nth(2) {
        app.search_query = q;
    }

    // Open from the command line, or fall back to the last session's file.
    if let Some(arg) = std::env::args().nth(1) {
        let path = PathBuf::from(arg);
        if path.exists() {
            app.load_document(&path);
        } else {
            eprintln!("no such file: {}", path.display());
        }
    } else if let Some((last, page)) = read_last_state() {
        app.load_document(&last);
        if page > 0 {
            app.go_to(page);
        }
    }

    let saved_theme = read_theme();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([900.0, 700.0])
            .with_min_inner_size([480.0, 360.0]),
        ..Default::default()
    };

    eframe::run_native(
        "pdf-viewer",
        options,
        Box::new(move |cc| {
            if let Some(t) = saved_theme {
                cc.egui_ctx.set_theme(t);
            }
            Ok(Box::new(app))
        }),
    )
}

// ---------------------------------------------------------------- doc model

struct DocumentHolder {
    pdfium: &'static Pdfium,
}

struct OpenDocument {
    path: PathBuf,
    page_count: usize,
    doc: Arc<PdfDocument<'static>>,
    /// Page sizes in PDF points, index = page number.
    sizes_pts: Vec<(f32, f32)>,
    title: String,
    author: String,
    producer: String,
    file_size: u64,
}

fn meta(doc: &PdfDocument<'_>, tag: PdfDocumentMetadataTagType) -> String {
    doc
        .metadata()
        .get(tag)
        .map(|t| t.value().to_owned())
        .unwrap_or_default()
}

// --------------------------------------------------------------------- app

/// A rendered page waiting to be uploaded to the GPU on the UI thread.
type PendingTexture = Arc<Mutex<Option<(usize, f32, egui::ColorImage)>>>;

/// One search hit: (page, left, bottom, right, top) in PDF points.
type SearchHit = (usize, f32, f32, f32, f32);

#[derive(Clone)]
struct CachedPage {
    scale: f32,
    texture: egui::TextureHandle,
}

struct PdfViewerApp {
    holder: DocumentHolder,
    document: Option<OpenDocument>,
    error: Option<String>,

    current_page: usize, // topmost page in view, 0-based
    zoom: f32,           // pixels per PDF point; 1.0 == 72 dpi
    fit_width: bool,
    last_available_width: f32,

    pending: PendingTexture,
    rendering: Arc<Mutex<Vec<usize>>>, // page indices currently being rendered
    cache: Vec<(usize, CachedPage)>,

    show_thumbs: bool,
    thumb_pending: Arc<Mutex<Vec<(usize, egui::ColorImage)>>>,
    thumb_rendering: Arc<Mutex<Vec<usize>>>,
    thumbs: Vec<(usize, egui::TextureHandle)>,

    page_input: String,

    // ---- search ----
    search_query: String,
    /// Hits as (page, left, bottom, right, top) in PDF points.
    search_hits: Vec<(usize, f32, f32, f32, f32)>,
    /// Index into search_hits of the currently selected hit.
    search_selected: usize,
    /// Set when we want the scroll strip to center on the selected hit.
    pending_hit_scroll: Option<usize>,
    /// True while the search field has focus (Enter navigates instead of jumping pages).
    search_focus_last: bool,
    /// Set by ⌘F; the toolbar picks it up next frame to focus the field.
    search_should_focus: bool,
    /// Remembered so Enter/Shift+Enter can navigate while the field is focused.
    search_field_id: Option<egui::Id>,
    /// Last query we actually ran through pdfium, to avoid re-searching per frame.
    last_searched: String,
    /// Receiver for an in-flight background search, tagged with its generation.
    search_rx: Option<mpsc::Receiver<(u64, Vec<SearchHit>)>>,
    /// Incremented each time a new search starts so stale results are discarded.
    search_gen: u64,

    /// Set when we want the ScrollArea to jump to a page next frame.
    scroll_target: Option<usize>,
    /// Suppress scroll-derived page updates while jumping programmatically.
    ignore_next_scroll: bool,
    /// Receiver for an in-flight native file dialog. The blocking dialog runs
    /// on a worker thread; the UI thread only polls this channel.
    open_pending: Option<mpsc::Receiver<Option<PathBuf>>>,
}

impl PdfViewerApp {
    fn new(holder: DocumentHolder) -> Self {
        Self {
            holder,
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
            page_input: String::new(),
            search_query: String::new(),
            search_hits: Vec::new(),
            search_selected: 0,
            pending_hit_scroll: None,
            search_focus_last: false,
            search_should_focus: false,
            search_field_id: None,
            last_searched: String::new(),
            search_rx: None,
            search_gen: 0,
            scroll_target: None,
            ignore_next_scroll: false,
            open_pending: None,
        }
    }

    // ------------------------------------------------------------- opening

    fn open_dialog(&mut self) {
        // Don't stack multiple dialogs.
        if self.open_pending.is_some() {
            return;
        }

        // rfd's synchronous `pick_file` blocks until the user picks or cancels.
        // On macOS its native backend must present the modal on the main thread,
        // so calling it from inside egui's update would deadlock the event loop.
        // Spawn a worker thread instead: rfd dispatches the modal to the main
        // thread (which is free again once this frame returns), then posts the
        // result back through a channel we poll next frame.
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

    /// Collect the result of a finished native file dialog, if any.
    fn poll_open_dialog(&mut self) {
        let mut ready: Option<Option<PathBuf>> = None;
        if let Some(rx) = &self.open_pending {
            match rx.try_recv() {
                Ok(v) => ready = Some(v),
                Err(mpsc::TryRecvError::Empty) => {}
                // Worker gone without sending: treat as cancelled.
                Err(mpsc::TryRecvError::Disconnected) => ready = Some(None),
            }
        }
        if let Some(result) = ready {
            self.open_pending = None;
            if let Some(path) = result {
                self.load_document(&path);
            }
        }
    }

    fn load_document(&mut self, path: &Path) {
        match self.holder.pdfium.load_pdf_from_file(path, None) {
            Ok(doc) => {
                // SAFETY: the Pdfium this document borrows is leaked and lives
                // for the whole process, so extending 'a to 'static is sound.
                let doc: PdfDocument<'static> = unsafe { std::mem::transmute(doc) };
                let page_count = doc.pages().len().max(0) as usize;
                let sizes_pts = (0..page_count)
                    .filter_map(|i| doc.pages().get(i as i32).ok())
                    .map(|p| (p.width().value, p.height().value))
                    .collect();
                let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                self.document = Some(OpenDocument {
                    title: meta(&doc, PdfDocumentMetadataTagType::Title),
                    author: meta(&doc, PdfDocumentMetadataTagType::Author),
                    producer: meta(&doc, PdfDocumentMetadataTagType::Producer),
                    file_size,
                    path: path.to_path_buf(),
                    page_count,
                    doc: Arc::new(doc),
                    sizes_pts,
                });
                self.current_page = 0;
                self.cache.clear();
                self.thumbs.clear();
                *self.thumb_pending.lock().unwrap() = Vec::new();
                *self.thumb_rendering.lock().unwrap() = Vec::new();
                self.fit_width = true;
                *self.pending.lock().unwrap() = None;
                self.scroll_target = Some(0);
                save_last_file(&path.canonicalize().unwrap_or_else(|_| path.to_path_buf()));
            }
            Err(e) => {
                self.error = Some(format!("Could not open {}: {e}", path.display()));
            }
        }
    }

    // ------------------------------------------------------------ geometry

    /// Pixels per PDF point for the given page.
    fn effective_scale(&self, page: usize) -> f32 {
        let page_w_pts = self
            .document
            .as_ref()
            .and_then(|d| d.sizes_pts.get(page))
            .map(|s| s.0)
            .unwrap_or(612.0);

        if self.fit_width && self.last_available_width > 50.0 {
            ((self.last_available_width - 48.0) / page_w_pts).clamp(0.2, 6.0)
        } else {
            self.zoom.clamp(0.2, 6.0)
        }
    }

    /// On-screen size of a page at the current scale.
    fn page_size_px(&self, page: usize) -> egui::Vec2 {
        let (w, h) = self
            .document
            .as_ref()
            .and_then(|d| d.sizes_pts.get(page))
            .copied()
            .unwrap_or((612.0, 792.0));
        let s = self.effective_scale(page);
        egui::vec2(w * s, h * s)
    }

    // ------------------------------------------------------------ rendering

    const MAX_CONCURRENT_RENDERS: usize = 3;

    /// Spawn background renders for pages that are missing from the cache.
    /// `priority` pages are requested first.
    fn request_missing_renders(&mut self, priority: &[usize], all_pages: &[usize]) {
        let scale_ok = |page: usize| self.effective_scale_cached(page);

        let mut queue: Vec<usize> = Vec::new();
        let rendering = self.rendering.lock().unwrap();
        for page in priority.iter().chain(all_pages.iter()).copied() {
            if rendering.contains(&page) {
                continue;
            }
            if queue.contains(&page) {
                continue;
            }
            if self.cached_scale(page).is_some() {
                continue; // already rendered at the right scale
            }
            queue.push(page);
        }
        drop(rendering);

        let free = Self::MAX_CONCURRENT_RENDERS.saturating_sub(self.rendering.lock().unwrap().len());
        for page in queue.into_iter().take(free) {
            let Some(scale) = scale_ok(page) else { continue };
            let Some(doc) = self.document.as_ref() else { return };
            let doc_arc = Arc::clone(&doc.doc);
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
                // Spin until the previous result was consumed (small backlog
                // is fine; renders are throttled anyway).
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

    /// Queue background thumbnail renders for visible + upcoming pages.
    fn request_thumbnails(&mut self) {
        if !self.show_thumbs {
            return;
        }
        let Some(doc) = self.document.as_ref() else { return };
        let total = doc.page_count;

        const THUMB_W_PX: i32 = 140;
        const MAX_THUMB_JOBS: usize = 4;

        let mut jobs: Vec<usize> = Vec::new();
        {
            let rendering = self.thumb_rendering.lock().unwrap();
            let pending = self.thumb_pending.lock().unwrap();
            // current page first, then forward, then backward
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
            let doc_arc = Arc::clone(&doc.doc);
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
            let texture = ui.ctx().load_texture(
                format!("thumb-{page}"),
                image,
                egui::TextureOptions::LINEAR,
            );
            self.thumbs.retain(|(p, _)| *p != page);
            self.thumbs.push((page, texture));
        }
    }

    /// Kick off a background search of every page for the given query. The
    /// worker runs on its own thread so large documents don't freeze the UI;
    /// results are posted back and applied by [`Self::poll_search`].
    fn start_search(&mut self, query: &str) {
        let Some(doc) = self.document.as_ref() else { return };
        let query = query.trim().to_owned();
        if query.is_empty() {
            self.search_hits.clear();
            self.pending_hit_scroll = None;
            self.search_rx = None;
            return;
        }

        // Bump the generation so any in-flight (now stale) search is discarded.
        self.search_gen = self.search_gen.wrapping_add(1);
        let generation = self.search_gen;
        self.search_hits.clear();
        self.pending_hit_scroll = None;

        let (tx, rx) = mpsc::channel();
        self.search_rx = Some(rx);

        let doc_arc = Arc::clone(&doc.doc);
        let q_clone = query.clone();
        std::thread::spawn(move || {
            eprintln!("[SEARCH] worker started for {q_clone:?}");
            let hits = search_all_pages(&doc_arc, &query);
            eprintln!("[SEARCH] worker done {} hits for {q_clone:?}", hits.len());
            let _ = tx.send((generation, hits));
        });
    }

    /// Apply the result of a finished background search, unless it's stale.
    fn poll_search(&mut self) {
        let mut ready: Option<(u64, Vec<SearchHit>)> = None;
        if let Some(rx) = &self.search_rx {
            match rx.try_recv() {
                Ok(v) => ready = Some(v),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.search_rx = None,
            }
        }
        if let Some((generation, hits)) = ready {
            self.search_rx = None;
            if generation == self.search_gen {
                self.search_hits = hits;
                self.search_selected = 0;
                if !self.search_hits.is_empty() {
                    self.pending_hit_scroll = Some(0);
                }
            }
        }
    }

    fn goto_hit(&mut self, delta: isize) {
        if self.search_hits.is_empty() {
            return;
        }
        let n = self.search_hits.len() as isize;
        let next = (self.search_selected as isize + delta).rem_euclid(n);
        self.search_selected = next as usize;
        self.pending_hit_scroll = Some(next as usize);
    }

    fn clear_search(&mut self) {
        self.search_hits.clear();
        self.search_query.clear();
        self.last_searched.clear();
        self.pending_hit_scroll = None;
        // Invalidate any in-flight search so a stale result can't reappear.
        self.search_gen = self.search_gen.wrapping_add(1);
        self.search_rx = None;
    }

    fn cached_scale(&self, page: usize) -> Option<f32> {
        self.cache
            .iter()
            .find(|(p, _)| *p == page)
            .map(|(_, c)| c.scale)
    }

    fn effective_scale_cached(&self, page: usize) -> Option<f32> {
        Some(self.effective_scale(page))
    }

    fn evict_cache(&mut self, keep: &[usize]) {
        self.cache.retain(|(p, _)| keep.contains(p));
    }

    // ------------------------------------------------------------------ ui

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button(egui::RichText::new("📂 Open").strong()).clicked() {
                self.open_dialog();
            }
            ui.label(egui::RichText::new("(⌘O)").weak());

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
                    ui.label(shown);
                }
                None => {
                    ui.label(egui::RichText::new("No document open").weak());
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Search field + result counter.
                if self.document.is_some() {
                    let n = self.search_hits.len();
                    let sel = if n > 0 { self.search_selected + 1 } else { 0 };
                    let re = ui.add_sized(
                        [170.0, ui.spacing().interact_size.y],
                        egui::TextEdit::singleline(&mut self.search_query)
                            .id(egui::Id::unique("search_field"))
                            .hint_text("Search")
                            .desired_width(170.0)
                            .margin(4.0),
                    );
                    // Visible status so the user always sees progress/results.
                    if self.search_rx.is_some() {
                        ui.label(egui::RichText::new("searching…").weak().small());
                    } else if !self.search_query.trim().is_empty() {
                        if n > 0 {
                            ui.label(egui::RichText::new(format!("{sel}/{n}")).weak().small());
                        } else {
                            ui.label(egui::RichText::new("0 matches").weak().small());
                        }
                    }
                    if re.gained_focus() || re.changed() || re.lost_focus() {
                        eprintln!(
                            "[SEARCH] id={:?} gained={} lost={} changed={} focus={} query={:?}",
                            re.id,
                            re.gained_focus(),
                            re.lost_focus(),
                            re.changed(),
                            re.has_focus(),
                            self.search_query
                        );
                    }
                    // Trigger a search immediately when the text changes (typing).
                    if re.changed() {
                        let q = self.search_query.trim().to_owned();
                        self.last_searched = q.clone();
                        if q.is_empty() {
                            self.search_hits.clear();
                            self.pending_hit_scroll = None;
                            self.search_rx = None;
                        } else {
                            self.start_search(&q);
                        }
                    }
                    // Explicit Search button — deterministic fallback.
                    if ui.button("🔍").on_hover_text("Search").clicked() {
                        let q = self.search_query.trim().to_owned();
                        if !q.is_empty() {
                            self.last_searched = q.clone();
                            self.start_search(&q);
                        }
                    }
                    if std::mem::take(&mut self.search_should_focus) {
                        re.request_focus();
                    }
                    let focused_now = re.has_focus();
                    if focused_now && !self.search_focus_last {
                        // Entering the field: nothing special yet.
                    }
                    if !focused_now && self.search_focus_last {
                        // Left the field with an empty query: clear highlights.
                        if self.search_query.trim().is_empty() {
                            self.clear_search();
                        }
                    }
                    self.search_focus_last = focused_now;
                    self.search_field_id = Some(re.id);

                    // Typing into the field triggers a fresh search immediately.


                    // Enter / Shift+Enter navigate hits while the field is focused.
                    if focused_now {
                        let (enter, shift) = ui.input(|i| {
                            (
                                i.key_pressed(egui::Key::Enter),
                                i.modifiers.shift,
                            )
                        });
                        if enter {
                            let q = self.search_query.trim().to_owned();
                            if q.is_empty() {
                                // no-op
                            } else if shift {
                                self.goto_hit(-1);
                            } else if self.search_hits.is_empty() {
                                // No results yet (still searching, or zero hits):
                                // force a fresh background search.
                                self.last_searched = String::new();
                                self.start_search(&q);
                            } else {
                                self.goto_hit(1);
                            }
                        }
                    }

                    if n > 0 {
                        if ui
                            .add(egui::Button::new("▲").small())
                            .on_hover_text("Previous match (Shift+Enter)")
                            .clicked()
                        {
                            self.goto_hit(-1);
                        }
                        if ui
                            .add(egui::Button::new("▼").small())
                            .on_hover_text("Next match (Enter)")
                            .clicked()
                        {
                            self.goto_hit(1);
                        }
                    }
                    ui.label(egui::RichText::new("⌘F").weak().small());
                }

                let theme = ui.ctx().options(|o| o.theme_preference);
                let icon = match theme {
                    egui::ThemePreference::Dark => "🌙",
                    egui::ThemePreference::Light => "☀️",
                    egui::ThemePreference::System => "🌗",
                };
                if ui
                    .button(icon)
                    .on_hover_text("Cycle dark / light / system theme")
                    .clicked()
                {
                    let next = match theme {
                        egui::ThemePreference::Dark => egui::ThemePreference::Light,
                        egui::ThemePreference::Light => egui::ThemePreference::System,
                        egui::ThemePreference::System => egui::ThemePreference::Dark,
                    };
                    ui.ctx().set_theme(next);
                    save_theme(next);
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

    fn bottom_bar(&mut self, ui: &mut egui::Ui) {
        let total = self.document.as_ref().map(|d| d.page_count).unwrap_or(0);

        ui.add_enabled_ui(total > 0, |ui| {
            ui.horizontal(|ui| {
                if ui.button("◀ Prev").clicked() {
                    self.go_to(self.current_page.saturating_sub(1));
                }

                // Editable page indicator: type a number, press Enter.
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
                        if let Ok(n) = self.page_input.trim().parse::<usize>() {
                            if n >= 1 && n <= total {
                                self.go_to(n - 1);
                            }
                        }
                        self.page_input.clear();
                        re.request_focus();
                    }
                    if !re.has_focus() {
                        // Keep the box showing the current page when idle.
                        if self.page_input.is_empty() || self.page_input.parse::<usize>().is_ok() {
                            self.page_input = (self.current_page + 1).to_string();
                        }
                    } else if self.ignore_next_scroll {
                        self.page_input.clear();
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
        // Auto-run search whenever the committed query differs from what we last
        // searched. This catches both "user finished typing" (idle frames) and
        // programmatic changes without depending on Event consumption order.
        if self.document.is_some() {
            let q = self.search_query.trim().to_owned();
            if q != self.last_searched {
                eprintln!("[SEARCH] trigger q={q:?} last={:?}", self.last_searched);
                self.last_searched = q.clone();
                if q.is_empty() {
                    self.search_hits.clear();
                    self.pending_hit_scroll = None;
                    self.search_rx = None;
                } else {
                    self.start_search(&q);
                }
            }
        }

        let available = ui.available_rect_before_wrap();
        self.last_available_width = available.width();

        self.upload_thumbnails(ui);

        // Upload finished background renders into the texture cache.
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
                ui.heading("PDF Viewer");
                ui.add_space(8.0);
                ui.label("Stage 2 — open a PDF to get started.");
                ui.add_space(16.0);
                if ui.button("📂 Open a PDF…").clicked() {
                    self.open_dialog();
                }
            });
            return;
        }

        if let Some(err) = self.error.take() {
            ui.centered_and_justified(|ui| {
                ui.colored_label(egui::Color32::from_rgb(240, 120, 100), err);
            });
            return;
        }

        let total = self.document.as_ref().unwrap().page_count;

        // ---- continuous scroll strip ----
        let mut sa = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if let Some(target) = self.scroll_target.take() {
            // Jump by offset: pages stacked above the target.
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
            let mut page_rects: Vec<egui::Rect> = Vec::with_capacity(total);
            ui.vertical_centered(|ui| {
                for page in 0..total {
                    let mut last_rect = egui::Rect::NOTHING;
                    let size = self.page_size_px(page);
                    if let Some((_, c)) = self.cache.iter().find(|(p, _)| *p == page) {
                        let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                        let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                        ui.painter().rect_filled(rect.expand(1.0), 2.0, egui::Color32::WHITE);
                        ui.painter().image(c.texture.id(), rect, uv, egui::Color32::WHITE);
                        last_rect = rect;
                    } else {
                        // Grey placeholder sized like the real page.
                        let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                        ui.painter().rect_filled(rect, 2.0, egui::Color32::from_gray(70));
                        if rect.intersects(ui.clip_rect()) {
                            last_rect = rect;
                            let label = format!("{}", page + 1);
                            ui.painter().text(
                                rect.center(),
                                egui::Align2::CENTER_CENTER,
                                label,
                                egui::FontId::proportional(28.0),
                                egui::Color32::GRAY,
                            );
                        }
                    }
                    page_rects.push(last_rect);

                    // Paint this page's search highlights in content space so
                    // they move with the page when scrolling.
                    if last_rect != egui::Rect::NOTHING {
                        let s = self.effective_scale(page);
                        let selected = self.search_selected;
                        for (i, hit) in self.search_hits.iter().enumerate() {
                            if hit.0 != page {
                                continue;
                            }
                            let (_pg, l, b, r_, t) = *hit;
                            let rect = egui::Rect::from_min_max(
                                egui::pos2(last_rect.min.x + l * s, last_rect.max.y - t * s),
                                egui::pos2(last_rect.min.x + r_ * s, last_rect.max.y - b * s),
                            );
                            let fill = if i == selected {
                                egui::Color32::from_rgba_unmultiplied(255, 214, 0, 160)
                            } else {
                                egui::Color32::from_rgba_unmultiplied(255, 214, 0, 70)
                            };
                            ui.painter().rect_filled(rect.expand(1.0), 1.0, fill);
                        }
                    }

                    ui.add_space(GAP);
                }
            });

            // Scroll to the selected hit. This must run against the scroll
            // area's own ui, not the outer panel ui.
            if let Some(idx) = hit_scroll_target
                && let Some(&(p, l, b, r_, t)) = self.search_hits.get(idx)
                && let Some(pr) = page_rects.get(p)
            {
                let s = self.effective_scale(p);
                let _ = (l, r_);
                // PDF y grows upward; screen y grows downward.
                let hit_center_y = pr.max.y - ((b + t) / 2.0) * s;
                ui.scroll_to_rect(
                    egui::Rect::from_min_size(
                        egui::pos2(pr.min.x, hit_center_y),
                        egui::vec2(1.0, 1.0),
                    ),
                    Some(egui::Align::Center),
                );
            }
        });

        // Which page is at the top of the viewport? The scroll offset tells
        // us how far down the content strip we are; walk the cumulative page
        // heights to find which one sits there.
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
            save_last_position(page);
        }

        self.request_thumbnails();

        // Request renders: current page first, then neighbours outward.
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

        // Trim far-away cached textures to bound memory.
        let keep: Vec<usize> = (page.saturating_sub(6)..(page + 12).min(total)).collect();
        self.evict_cache(&keep);
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let Some(doc) = self.document.as_ref() else { return };
        let total = doc.page_count;
        let current = self.current_page;
        let aspects: Vec<f32> = (0..total)
            .map(|p| {
                let (w, h) = doc.sizes_pts.get(p).copied().unwrap_or((612.0, 792.0));
                w / h
            })
            .collect();

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    for page in 0..total {
                        let aspect = aspects[page];
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

                        // Make the whole thumbnail clickable: Frame's own
                        // response only senses hover, so interact over its rect.
                        let inner = frame.show(ui, |ui| {
                            ui.vertical_centered(|ui| {
                                if let Some((_, tex)) =
                                    self.thumbs.iter().find(|(p, _)| *p == page)
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
                                        egui::Color32::from_gray(if page == current { 90 } else { 55 }),
                                    );
                                }
                                ui.label(
                                    egui::RichText::new(format!("{}", page + 1))
                                        .small()
                                        .weak(),
                                );
                            });
                        });

                        let click = ui.interact(inner.response.rect, ui.scope_id().with(("thumb", page)), egui::Sense::click());
                        if click.clicked() {
                            self.go_to(page);
                        }
                        ui.add_space(6.0);
                    }
                });
            });
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        let Some(doc) = self.document.as_ref() else {
            ui.label(egui::RichText::new("Ready").weak().small());
            return;
        };

        ui.horizontal(|ui| {
            let meta = format!(
                "{} · {} pages · {}",
                if doc.author.is_empty() {
                    "Unknown author"
                } else {
                    &doc.author
                },
                doc.page_count,
                human_bytes(doc.file_size),
            );
            ui.label(egui::RichText::new(meta).weak().small());

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let scale = self.effective_scale(self.current_page);
                let (wpts, hpts) = doc
                    .sizes_pts
                    .get(self.current_page)
                    .copied()
                    .unwrap_or((612.0, 792.0));
                ui.label(
                    egui::RichText::new(format!(
                        "{:.0}% · {:.0}×{:.0} pt",
                        scale * 100.0,
                        wpts,
                        hpts
                    ))
                    .weak()
                    .small(),
                );
                if !doc.producer.is_empty() {
                    ui.label(
                        egui::RichText::new(format!("produced by {}", doc.producer))
                            .weak()
                            .small(),
                    );
                }
            });
        });
    }

    // --------------------------------------------------------------- actions

    fn go_to(&mut self, page: usize) {
        let Some(doc) = self.document.as_ref() else { return };
        let page = page.min(doc.page_count.saturating_sub(1));
        self.current_page = page;
        self.scroll_target = Some(page);
        save_last_position(page);
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

    /// After a scale change, drop mismatched textures and re-render what's needed.
    fn request_fit_rebuild(&mut self) {
        // Invalidate every cached page whose scale no longer matches.
        let mut stale: Vec<usize> = Vec::new();
        for (p, c) in &self.cache {
            if (c.scale - self.effective_scale(*p)).abs() > 0.01 {
                stale.push(*p);
            }
        }
        self.cache.retain(|(p, _)| !stale.contains(p));
    }
}

impl eframe::App for PdfViewerApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {

        // Load a PDF if a native file dialog just finished.
        self.poll_open_dialog();

        // Apply a finished background search, if any.
        self.poll_search();

        let typing = ctx.memory(|m| m.focused().is_some());


        // ⌘F / Ctrl-F focuses the search field. Match the raw event so the
        // mac_cmd flag path is covered regardless of Modifiers.command timing.
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
            eprintln!("[SEARCH] cmd-F detected");
            self.search_should_focus = true;
        }
        // Escape clears the search (and blurs the field).
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) && !self.search_hits.is_empty() {
            self.clear_search();
            if let Some(id) = self.search_field_id {
                ctx.memory_mut(|m| m.surrender_focus(id));
            }
        }

        let (open, plus, minus) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::O) && i.modifiers.command,
                i.key_pressed(egui::Key::Plus) || i.key_pressed(egui::Key::Equals),
                i.key_pressed(egui::Key::Minus),
            )
        });
        if open {
            self.open_dialog();
        }
        if !typing {
            // Bare number keys 1-9 jump straight to that page.
            for k in [
                egui::Key::Num1, egui::Key::Num2, egui::Key::Num3, egui::Key::Num4, egui::Key::Num5,
                egui::Key::Num6, egui::Key::Num7, egui::Key::Num8, egui::Key::Num9,
            ] {
                if ctx.input(|i| i.key_pressed(k)) {
                    self.go_to((k as usize) - (egui::Key::Num1 as usize));
                    break;
                }
            }

            let (next, prev, home, end) = ctx.input(|i| {
                (
                    i.key_pressed(egui::Key::ArrowDown) || i.key_pressed(egui::Key::PageDown),
                    i.key_pressed(egui::Key::ArrowUp) || i.key_pressed(egui::Key::PageUp),
                    i.key_pressed(egui::Key::Home),
                    i.key_pressed(egui::Key::End),
                )
            });
            if next {
                self.go_to(self.current_page + 1);
            }
            if prev {
                self.go_to(self.current_page.saturating_sub(1));
            }
            if home {
                self.go_to(0);
            }
            if end {
                if let Some(d) = self.document.as_ref() {
                    self.go_to(d.page_count.saturating_sub(1));
                }
            }
            if plus {
                self.zoom_in();
            }
            if minus {
                self.zoom_out();
            }
        }

        // Ctrl/Cmd + wheel zoom.
        let zoom_delta = ctx.input(|i| {
            if i.modifiers.ctrl || i.modifiers.command {
                i.smooth_scroll_delta.y
            } else {
                0.0
            }
        });
        if zoom_delta.abs() > f32::EPSILON && self.document.is_some() {
            self.fit_width = false;
            self.zoom = (self.zoom * (1.0 + zoom_delta * 0.002)).clamp(0.2, 6.0);
            self.request_fit_rebuild();
        }

        // Keep repainting while background work is running.
        let busy_thumbs = !self.thumb_rendering.lock().unwrap().is_empty()
            || !self.thumb_pending.lock().unwrap().is_empty();
        if busy_thumbs
            || !self.rendering.lock().unwrap().is_empty()
            || self.pending.lock().unwrap().is_some()
            || self.search_rx.is_some()
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::Panel::bottom("bottom").show(ui, |ui| self.bottom_bar(ui));
        if self.show_thumbs && self.document.is_some() {
            egui::Panel::left("thumbs")
                .resizable(true)
                .default_size(180.0)
                .show(ui, |ui| self.sidebar(ui));
        }
        egui::CentralPanel::default().show(ui, |ui| self.central_panel(ui));
    }
}

// -------------------------------------------------------------- persistence

fn state_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let dir = PathBuf::from(home).join(".local/share/pdf-viewer");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("state.txt")
}

/// State file: two lines — last file path (absolute), last page (0-based).
fn read_last_state() -> Option<(PathBuf, usize)> {
    let txt = std::fs::read_to_string(state_path()).ok()?;
    let mut lines = txt.lines();
    let path = PathBuf::from(lines.next()?);
    let page = lines.next().and_then(|l| l.trim().parse().ok()).unwrap_or(0);
    path.exists().then_some((path, page))
}

fn save_state(path: &str, page: usize) {
    // Preserve the third line (theme preference) when rewriting file/page.
    let theme = read_theme().map(|t| match t {
        egui::ThemePreference::Dark => "dark",
        egui::ThemePreference::Light => "light",
        egui::ThemePreference::System => "system",
    });
    let theme_line = theme.map_or_else(String::new, |t| format!("\n{t}"));
    let _ = std::fs::write(state_path(), format!("{path}\n{page}{theme_line}\n"));
}

fn save_last_file(path: &Path) {
    save_state(path.to_str().unwrap_or(""), 0);
}

fn read_theme() -> Option<egui::ThemePreference> {
    let txt = std::fs::read_to_string(state_path()).ok()?;
    match txt.lines().nth(2)?.trim() {
        "dark" => Some(egui::ThemePreference::Dark),
        "light" => Some(egui::ThemePreference::Light),
        "system" => Some(egui::ThemePreference::System),
        _ => None,
    }
}

fn save_theme(theme: egui::ThemePreference) {
    let name = match theme {
        egui::ThemePreference::Dark => "dark",
        egui::ThemePreference::Light => "light",
        egui::ThemePreference::System => "system",
    };
    let path = state_path();
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines = existing.lines();
    let file = lines.next().unwrap_or("");
    let page = lines.next().unwrap_or("0");
    let _ = std::fs::write(&path, format!("{file}\n{page}\n{name}\n"));
}

fn save_last_position(page: usize) {
    if let Ok(txt) = std::fs::read_to_string(state_path())
        && let Some(line) = txt.lines().next()
    {
        save_state(line, page);
    }
}

/// Search every page's text layer for `query`, returning one bounding-box hit
/// per occurrence. Runs off the UI thread; the `thread_safe` feature serializes
/// pdfium access behind a mutex, so it's safe to run alongside page rendering.
fn search_all_pages(doc: &PdfDocument<'static>, query: &str) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    let options = PdfSearchOptions::new();
    let page_count = doc.pages().len().max(0) as usize;

    for p in 0..page_count {
        let Ok(page) = doc.pages().get(p as i32) else { continue };
        let Ok(text) = page.text() else { continue };
        let Ok(search) = text.search(query, &options) else { continue };
        for segments in search.iter(PdfSearchDirection::SearchForward) {
            // Union of the segment rects that make up this hit.
            let mut l = f32::MAX;
            let mut b = f32::MAX;
            let mut r = f32::MIN;
            let mut t = f32::MIN;
            for i in segments.as_range() {
                if let Ok(seg) = segments.get(i) {
                    let br = seg.bounds();
                    l = l.min(br.left().value);
                    b = b.min(br.bottom().value);
                    r = r.max(br.right().value);
                    t = t.max(br.top().value);
                }
            }
            if l < r && b < t {
                hits.push((p, l, b, r, t));
            }
        }
    }

    hits
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
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

// ------------------------------------------------------------------- render

/// Render one page into an RGBA `ColorImage`. Runs off the UI thread; safe
/// because `PdfDocument` is `Send + Sync` under pdfium-render's `thread_safe`
/// feature and we hold it in an `Arc`.
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
