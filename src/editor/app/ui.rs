//! Editor UI: top bar, tool bar, status bar, sidebar, help and dialogs.

use super::*;

impl EditorApp {
    pub(super) fn sidebar(&mut self, ui: &mut egui::Ui) {
        let Some(doc) = self.document.as_ref() else {
            return;
        };
        let total = doc.page_count;
        let current = self.current_page;
        let aspects: Vec<f32> = (0..total)
            .map(|p| {
                let (w, h) = self.page_display_pts(p);
                w / h
            })
            .collect();

        ui.horizontal_wrapped(|ui| {
            if ui
                .small_button("📎 Merge")
                .on_hover_text("Append another PDF to this document")
                .clicked()
            {
                self.merge_pdf_dialog();
            }
            if ui
                .small_button("📤 Extract")
                .on_hover_text("Save selected pages to a new PDF (⌘/⇧-click thumbnails to multi-select)")
                .clicked()
            {
                self.extract_pages_dialog();
            }
            if ui
                .small_button("⧉ Dup")
                .on_hover_text("Duplicate current page")
                .clicked()
            {
                self.duplicate_page();
            }
        });
        let sel_count = self.selected_pages.len();
        if sel_count > 1 {
            ui.label(
                egui::RichText::new(format!("{sel_count} pages selected"))
                    .small()
                    .weak(),
            );
        }
        ui.separator();

        let pointer_pos = ui.input(|i| i.pointer.interact_pos());
        let pointer_released = ui.input(|i| i.pointer.primary_released());
        let mut drag_target: Option<usize> = None;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    for (page, &aspect) in aspects.iter().enumerate().take(total) {
                        let thumb_w = (ui.available_width() - 8.0).min(150.0);
                        let size = egui::vec2(thumb_w, thumb_w / aspect);

                        let is_selected = self.selected_pages.contains(&page);
                        let frame_fill = if page == current {
                            ui.style().visuals.selection.bg_fill
                        } else if is_selected {
                            egui::Color32::from_rgba_unmultiplied(70, 130, 220, 90)
                        } else {
                            ui.style().visuals.widgets.inactive.bg_fill
                        };
                        let frame_stroke = if is_selected && page != current {
                            egui::Stroke::new(1.5, egui::Color32::from_rgb(90, 170, 255))
                        } else {
                            egui::Stroke::NONE
                        };

                        let frame = egui::Frame::new()
                            .fill(frame_fill)
                            .stroke(frame_stroke)
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

                        let rect = inner.response.rect;
                        let click = ui.interact(
                            rect,
                            ui.scope_id().with(("thumb", page)),
                            egui::Sense::click_and_drag(),
                        );
                        if click.drag_started() {
                            self.page_drag_source = Some(page);
                        }
                        if click.clicked() {
                            let (cmd, shift) = ui.input(|i| {
                                (i.modifiers.command || i.modifiers.mac_cmd, i.modifiers.shift)
                            });
                            if cmd {
                                if self.selected_pages.contains(&page) {
                                    if self.selected_pages.len() > 1 {
                                        self.selected_pages.remove(&page);
                                    }
                                } else {
                                    self.selected_pages.insert(page);
                                }
                            } else if shift {
                                let lo = current.min(page);
                                let hi = current.max(page);
                                self.selected_pages.clear();
                                for p in lo..=hi {
                                    self.selected_pages.insert(p);
                                }
                            } else {
                                self.selected_pages.clear();
                                self.selected_pages.insert(page);
                            }
                            self.go_to(page);
                        }

                        if self.page_drag_source.is_some()
                            && let Some(pos) = pointer_pos
                            && rect.expand2(egui::vec2(0.0, 4.0)).contains(pos)
                        {
                            let before = pos.y < rect.center().y;
                            let y = if before { rect.min.y - 2.0 } else { rect.max.y + 2.0 };
                            ui.painter().line_segment(
                                [egui::pos2(rect.min.x, y), egui::pos2(rect.max.x, y)],
                                egui::Stroke::new(2.5, egui::Color32::from_rgb(90, 170, 255)),
                            );
                            drag_target = Some(if before { page } else { page + 1 });
                        }

                        ui.add_space(6.0);
                    }
                });
            });

        if pointer_released
            && let Some(from) = self.page_drag_source.take()
            && let Some(raw_to) = drag_target
        {
            let to = if raw_to > from {
                raw_to - 1
            } else {
                raw_to
            }
            .min(total.saturating_sub(1));
            if to != from {
                self.reorder_page(from, to);
            }
        }
    }

    /// Right-side panel listing AcroForm fields for filling.
    pub(super) fn forms_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Form fields");

        let list: Vec<(usize, usize, String, FormFieldType, String)> = self
            .form_fields
            .iter()
            .map(|f| (f.page, f.index, f.name.clone(), f.field_type, f.value.clone()))
            .collect();

        if list.is_empty() {
            ui.label(egui::RichText::new("No fillable fields in this PDF.").weak());
            return;
        }

        ui.label(
            egui::RichText::new(format!("{} field(s)", list.len()))
                .weak()
                .small(),
        );
        ui.separator();

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (page, index, name, ftype, value) in list {
                    ui.horizontal(|ui| {
                        let label = if name.is_empty() {
                            format!("p{} · {}", page + 1, ftype.label())
                        } else {
                            name.clone()
                        };
                        match ftype {
                            FormFieldType::Checkbox | FormFieldType::RadioButton => {
                                let mut checked = value == "true";
                                if ui.checkbox(&mut checked, "").changed() {
                                    self.toggle_form_checkbox(page, index);
                                }
                                ui.label(label);
                            }
                            FormFieldType::Text => {
                                if ui.button(format!("✏️ {label}")).clicked() {
                                    self.start_form_edit(page, index);
                                }
                                ui.add(egui::Label::new(egui::RichText::new(value).weak()).truncate());
                            }
                            _ => {
                                ui.label(format!("{label} ({})", ftype.label()));
                                ui.add(egui::Label::new(egui::RichText::new(value).weak()).truncate());
                            }
                        }
                    });
                }
            });
    }

    /// The floating help window describing how to use the editor.
    pub(super) fn help_window(&mut self, ctx: &egui::Context) {
        egui::Window::new("❓ Help — how to use this editor")
            .open(&mut self.show_help)
            .collapsible(false)
            .resizable(true)
            .default_width(520.0)
            .vscroll(true)
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(
                        "This is an annotation editor: it adds marks on top of a PDF. \
                         It does not rewrite text already embedded in the document.",
                    )
                    .italics(),
                );
                ui.add_space(6.0);

                ui.strong("Tools");
                let tool_rows: [(&str, &str); 8] = [
                    (
                        "⬚ Select",
                        "Click to select; drag the middle to move; drag a corner to resize.",
                    ),
                    (
                        "🖍 Highlight",
                        "Drag across text to highlight, or click for a default-sized mark.",
                    ),
                    ("✏️ Ink", "Drag to draw a freehand line."),
                    (
                        "▭ Rect",
                        "Drag to draw a rectangle outline, or click for a default size.",
                    ),
                    (
                        "◯ Ellipse",
                        "Drag to draw an ellipse, or click for a default size.",
                    ),
                    (
                        "🅣 Text",
                        "Type in the Text box first, then click where the note should go.",
                    ),
                    (
                        "🖼 Image",
                        "Pick an image file; it is placed at the centre of the page.",
                    ),
                    (
                        "🔤 Edit text",
                        "Click to select; drag to move smoothly (or nudge with Arrow keys, Shift+Arrow for 10 pt); double-click to rewrite.",
                    ),
                ];
                for (tool, desc) in tool_rows {
                    ui.horizontal(|ui| {
                        ui.monospace(tool);
                        ui.label(desc);
                    });
                }

                ui.add_space(6.0);
                ui.strong("Keyboard shortcuts");
                let keys: [(&str, &str); 15] = [
                    ("⌘O", "Open a PDF"),
                    ("⌘S", "Save"),
                    ("⌘Z / ⌘⇧Z", "Undo / redo"),
                    ("⌘F", "Focus the search field"),
                    ("Enter / ⇧Enter", "Next / previous search match"),
                    ("Esc", "Clear the search"),
                    ("Delete", "Delete the selected annotation"),
                    ("↑ ↓  or  PgUp PgDn", "Previous / next page"),
                    ("Home / End", "First / last page"),
                    ("+ / −", "Zoom in / out"),
                    ("⌘ + scroll", "Zoom with the mouse wheel"),
                    ("1–9", "Jump straight to that page"),
                    ("F1", "Toggle this help window"),
                    ("Click thumbnail", "Jump to that page"),
                    ("Number + Enter", "Go to that page (bottom bar)"),
                ];
                for (key, desc) in keys {
                    ui.horizontal(|ui| {
                        ui.monospace(key);
                        ui.label(desc);
                    });
                }

                ui.add_space(6.0);
                ui.strong("Pages");
                ui.label("➕ Insert page — add a blank page after the current one.");
                ui.label("🗑 Delete page — remove the current page.");
                ui.label("⟳ Rotate page — turn the page 90° clockwise.");

                ui.add_space(6.0);
                ui.strong("Saving");
                ui.label(
                    "Save (⌘S) writes your annotations back into the PDF as real \
                     PDF annotations, so other viewers can see them. Reopen the file \
                     here to keep moving or resizing them.",
                );
            });
    }

    /// Show the recognized text (OCR result) for the current page, editable so
    /// mistakes can be corrected, with a copy-to-clipboard button.
    #[cfg(any(feature = "ocr", target_os = "windows"))]
    pub(super) fn page_text_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_page_text;
        let page = self.current_page;
        let mut copied = false;

        egui::Window::new("OCR result")
            .open(&mut open)
            .collapsible(true)
            .resizable(true)
            .default_width(440.0)
            .show(ctx, |ui| {
                match self.ocr_results.get_mut(&page) {
                    Some(text) if !text.is_empty() => {
                        ui.label(
                            egui::RichText::new(format!("Page {} — edit, then copy", page + 1))
                                .weak()
                                .small(),
                        );
                        ui.separator();
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            ui.add(
                                egui::TextEdit::multiline(text)
                                    .desired_rows(20)
                                    .desired_width(f32::INFINITY)
                                    .font(egui::TextStyle::Monospace),
                            );
                        });
                        ui.separator();
                        if ui.button("📋 Copy text").clicked() {
                            ui.ctx().copy_text(text.clone());
                            copied = true;
                        }
                    }
                    _ => {
                        ui.label(egui::RichText::new("No OCR text for this page yet.").weak());
                        ui.label("Click “🔍 OCR page” to recognize it.");
                    }
                }
            });

        if copied {
            self.status = "OCR text copied to clipboard".into();
        }
        self.show_page_text = open;
    }

    /// Show the "unsaved changes" confirmation and act on the user's choice.
    pub(super) fn show_confirm_window(&mut self, ctx: &egui::Context) {
        let Some(confirm) = self.confirm.clone() else {
            return;
        };
        match confirm {
            PendingConfirm::Close | PendingConfirm::Open(_) => {
                let title = match &confirm {
                    PendingConfirm::Close => "Quit without saving?",
                    PendingConfirm::Open(_) => "Open another document?",
                    _ => unreachable!(),
                };
                let mut save_first = false;
                let mut discard = false;
                let mut cancelled = false;
                egui::Window::new(title)
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        ui.label("This document has unsaved changes.");
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button("💾 Save & continue").clicked() {
                                save_first = true;
                            }
                            if ui.button("Discard changes").clicked() {
                                discard = true;
                            }
                            if ui.button("Cancel").clicked() {
                                cancelled = true;
                            }
                        });
                    });
                if cancelled {
                    self.confirm = None;
                    return;
                }
                if !save_first && !discard {
                    return;
                }
                if save_first {
                    self.save();
                    if self.dirty {
                        return;
                    }
                }
                match self.confirm.take() {
                    Some(PendingConfirm::Close) => {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    Some(PendingConfirm::Open(path)) => self.load_document(&path),
                    _ => {}
                }
            }
            PendingConfirm::ApplyRedactions => {
                let mut proceed = false;
                let mut cancelled = false;
                egui::Window::new("Apply Permanent Redactions?")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        ui.label("This will permanently destroy all text, images, and graphics under the marked redaction boxes and cannot be undone.");
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button(egui::RichText::new("🛡 Apply Permanently").color(egui::Color32::from_rgb(230, 60, 60)).strong()).clicked() {
                                proceed = true;
                            }
                            if ui.button("Cancel").clicked() {
                                cancelled = true;
                            }
                        });
                    });
                if cancelled {
                    self.confirm = None;
                } else if proceed {
                    self.confirm = None;
                    self.apply_redactions_action();
                }
            }
            PendingConfirm::FlattenPage | PendingConfirm::FlattenAll => {
                let all_pages = matches!(confirm, PendingConfirm::FlattenAll);
                let title = if all_pages {
                    "Flatten All Pages?"
                } else {
                    "Flatten Current Page?"
                };
                let desc = if all_pages {
                    "This will permanently merge all annotations and form fields across every page into static PDF content. This cannot be undone."
                } else {
                    "This will permanently merge all annotations and form fields on this page into static PDF content. This cannot be undone."
                };
                let mut proceed = false;
                let mut cancelled = false;
                egui::Window::new(title)
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        ui.label(desc);
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button("📄 Flatten Permanently").clicked() {
                                proceed = true;
                            }
                            if ui.button("Cancel").clicked() {
                                cancelled = true;
                            }
                        });
                    });
                if cancelled {
                    self.confirm = None;
                } else if proceed {
                    self.confirm = None;
                    self.flatten_action(all_pages);
                }
            }
        }
    }
    // ------------------------------------------------------------------ ui

    pub(super) fn top_bar(&mut self, ui: &mut egui::Ui) {
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

            if ui
                .add_enabled(self.can_copy(), egui::Button::new("📋 Copy"))
                .on_hover_text("Copy selected annotation, text object, or image (⌘C)")
                .clicked()
            {
                self.copy_selected_item();
            }
            if ui
                .add_enabled(self.clipboard.is_some(), egui::Button::new("📌 Paste"))
                .on_hover_text("Paste onto current page (⌘V)")
                .clicked()
            {
                self.paste_clipboard_item();
            }

            ui.separator();

            if ui
                .button("🗑 Delete annotation")
                .on_hover_text("Delete the selected annotation (Delete key)")
                .clicked()
            {
                self.delete_selected();
            }

            if ui
                .selectable_label(self.show_forms, "📝 Forms")
                .on_hover_text("Show / fill AcroForm fields")
                .clicked()
            {
                self.show_forms = !self.show_forms;
            }

            if ui
                .selectable_label(self.show_signatures, "✍ Signatures")
                .on_hover_text("Create, save, and stamp reusable signatures")
                .clicked()
            {
                self.show_signatures = !self.show_signatures;
            }

            if ui
                .selectable_label(self.show_flatten, "📄 Flatten")
                .on_hover_text("Flatten annotations and form fields into static page content")
                .clicked()
            {
                self.show_flatten = !self.show_flatten;
            }

            if ui
                .selectable_label(self.show_bookmarks, "🔖 Bookmarks")
                .on_hover_text("Show document bookmarks / outline")
                .clicked()
            {
                self.show_bookmarks = !self.show_bookmarks;
            }

            if ui
                .add_enabled(
                    self.document.is_some(),
                    egui::Button::new("🏷 Metadata").selected(self.show_metadata),
                )
                .on_hover_text("View and edit PDF metadata (Title, Author, Subject, Keywords)")
                .clicked()
            {
                self.show_metadata = !self.show_metadata;
                if self.show_metadata && let Some(doc) = self.document.as_ref() {
                    self.meta_draft = doc.metadata();
                }
            }

            if ui
                .add_enabled(
                    self.document.is_some(),
                    egui::Button::new("🗜 Compress").selected(self.show_compress),
                )
                .on_hover_text("Downsample and JPEG-compress embedded images to shrink PDF size")
                .clicked()
            {
                self.show_compress = !self.show_compress;
            }

            if ui
                .add_enabled(
                    self.document.is_some(),
                    egui::Button::new("🔒 Security").selected(self.show_security),
                )
                .on_hover_text("Add or remove PDF password encryption and permissions")
                .clicked()
            {
                self.show_security = !self.show_security;
            }

            if ui
                .button("❓ Help")
                .on_hover_text("How to use this editor (F1)")
                .clicked()
            {
                self.show_help = true;
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
                    ui.add(
                        egui::Label::new(egui::RichText::new(format!("{shown}{suffix}")).strong())
                            .truncate(),
                    );
                }
                None => {
                    ui.label(egui::RichText::new("No document open").weak());
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Search field + result counter (rightmost).
                if self.document.is_some() {
                    let n = self.search_hits.len();
                    let sel = if n > 0 { self.search_selected + 1 } else { 0 };
                    let re = ui.add_sized(
                        [170.0, ui.spacing().interact_size.y],
                        egui::TextEdit::singleline(&mut self.search_query)
                            .id(egui::Id::unique("editor_search_field"))
                            .hint_text("Search")
                            .desired_width(170.0)
                            .margin(4.0),
                    );
                    if self.search_rx.is_some() || self.search_pending_query.is_some() {
                        ui.label(egui::RichText::new("searching…").weak().small());
                    } else if !self.search_query.trim().is_empty() {
                        if n > 0 {
                            ui.label(egui::RichText::new(format!("{sel}/{n}")).weak().small());
                        } else {
                            ui.label(egui::RichText::new("0 matches").weak().small());
                        }
                    }
                    // Debounce: don't spawn a scan per keystroke.
                    if re.changed() {
                        let q = self.search_query.trim().to_owned();
                        self.debounce_search(q);
                    }
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
                    self.search_field_id = Some(re.id);

                    // Enter / Shift+Enter navigate hits. A singleline TextEdit
                    // surrenders focus on Enter, so detect it via `lost_focus`
                    // and re-focus to keep cycling with repeated Enter presses.
                    if re.lost_focus() {
                        let (enter, shift) =
                            ui.input(|i| (i.key_pressed(egui::Key::Enter), i.modifiers.shift));
                        if enter {
                            let q = self.search_query.trim().to_owned();
                            if q.is_empty() {
                                // no-op
                            } else if shift {
                                self.goto_hit(-1);
                            } else if self.search_hits.is_empty() {
                                self.last_searched = String::new();
                                self.start_search(&q);
                            } else {
                                self.goto_hit(1);
                            }
                            // Keep focus so the next Enter navigates too.
                            re.request_focus();
                        } else if self.search_query.trim().is_empty() {
                            // Left the field some other way with an empty query.
                            self.clear_search();
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

                if !self.status.is_empty() {
                    ui.add(
                        egui::Label::new(egui::RichText::new(&self.status).weak().small())
                            .truncate(),
                    );
                    ui.separator();
                }
                if self.document.is_some() {
                    let mut show = self.show_thumbs;
                    if ui.toggle_value(&mut show, "Thumbnails").changed() {
                        self.show_thumbs = show;
                        self.request_thumbnails();
                    }
                }

                // Theme cycle: dark → light → system.
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
                    crate::persist::save_theme(next);
                }
            });
        });
    }

    pub(super) fn tool_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label("Tool:");
            for tool in Tool::ALL {
                let text = format!("{} {}", tool.icon(), tool.label());
                if ui.selectable_label(self.tool == tool, text).clicked() {
                    self.tool = tool;
                    self.interaction = Interaction::None;
                    self.selected = None;
                    self.selected_text_run = None;
                    self.word_selection.clear();
                    self.word_anchor = None;
                    self.text_drag = None;
                    self.text_drag_offset = (0.0, 0.0);
                    self.selected_image = None;
                    self.image_drag = None;
                    self.image_drag_start_matrix = None;
                    self.crop_mode = false;
                    self.crop_start = None;
                    self.crop_current = None;
                    if self.tool == Tool::Image {
                        self.image_dialog();
                    }
                }
            }

            ui.separator();

            if self.text_selection.is_some() {
                ui.label("Selection:");
                if ui.button("🖍 Highlight").clicked() {
                    self.apply_text_markup(AnnotationKind::Highlight);
                }
                if ui.button("S̶ Strike").clicked() {
                    self.apply_text_markup(AnnotationKind::Strikeout);
                }
                if ui.button("U̲ Underline").clicked() {
                    self.apply_text_markup(AnnotationKind::Underline);
                }
                if ui.button("✖ Clear").clicked() {
                    self.clear_text_selection();
                }
                ui.separator();
            }

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

            // A selected free-text annotation gets its own size and colour.
            if let Some((fs, col)) = self.selected_free_text() {
                ui.separator();
                ui.label("Selected text:");
                let mut size = fs;
                if ui
                    .add(egui::Slider::new(&mut size, 6.0..=72.0).step_by(1.0))
                    .changed()
                {
                    self.edit_selected_text(size, col);
                }
                let mut c =
                    egui::Color32::from_rgba_unmultiplied(col[0], col[1], col[2], col[3]);
                if ui.color_edit_button_srgba(&mut c).changed() {
                    self.edit_selected_text(fs, c.to_srgba_unmultiplied());
                }
            }

            // A selected original-text word/line gets size, bold, font, colour, and delete controls.
            if let Some((_, _, size, mode)) = self.selected_text_style() {
                ui.separator();
                ui.label("Original text:");
                let mut new_size = size;
                if ui
                    .add(egui::Slider::new(&mut new_size, 4.0..=72.0).step_by(1.0))
                    .changed()
                {
                    self.edit_selected_text_style(new_size, mode);
                }
                let bold = matches!(mode, PdfPageTextRenderMode::FilledThenStroked);
                let mut want_bold = bold;
                if ui.toggle_value(&mut want_bold, "Bold").changed() {
                    let new_mode = if want_bold {
                        PdfPageTextRenderMode::FilledThenStroked
                    } else {
                        PdfPageTextRenderMode::FilledUnstroked
                    };
                    self.edit_selected_text_style(size, new_mode);
                }
                if let Some((col, family)) = self.selected_text_color_and_font() {
                    let mut c = egui::Color32::from_rgba_unmultiplied(col[0], col[1], col[2], col[3]);
                    if ui.color_edit_button_srgba(&mut c).changed() {
                        self.edit_selected_text_color(c.to_srgba_unmultiplied());
                    }
                    let mut selected_fam = family;
                    egui::ComboBox::from_id_salt("orig_text_font")
                        .selected_text(selected_fam.label())
                        .show_ui(ui, |ui| {
                            for fam in TextFontFamily::ALL {
                                if ui.selectable_value(&mut selected_fam, fam, fam.label()).changed() {
                                    self.edit_selected_text_font(fam);
                                }
                            }
                        });
                }
                if ui
                    .button("🗑 Delete line")
                    .on_hover_text("Delete this entire text line (Delete key)")
                    .clicked()
                {
                    self.delete_selected_text_line();
                }
            }

            if self.tool == Tool::EditText {
                ui.separator();
                ui.checkbox(&mut self.auto_reflow, "Auto-reflow")
                    .on_hover_text("Automatically shift surrounding lines when moving text");
            }

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
            if ui
                .button("⧉ Duplicate page")
                .on_hover_text("Insert a copy of the current page right after it")
                .clicked()
            {
                self.duplicate_page();
            }
            if ui
                .button("📎 Merge PDF…")
                .on_hover_text("Append another PDF to the end of this document")
                .clicked()
            {
                self.merge_pdf_dialog();
            }
            if ui
                .button("📤 Extract pages…")
                .on_hover_text("Save selected pages to a new PDF (⌘/⇧-click thumbnails to multi-select)")
                .clicked()
            {
                self.extract_pages_dialog();
            }
            if ui
                .selectable_label(self.show_crop_page, "✂ Crop page")
                .on_hover_text("Crop page margins (CropBox)")
                .clicked()
            {
                self.show_crop_page = !self.show_crop_page;
            }

            if ui
                .selectable_label(self.show_headers_footers, "🔢 Headers/Numbers")
                .on_hover_text("Add page numbers, headers, or footers")
                .clicked()
            {
                self.show_headers_footers = !self.show_headers_footers;
            }

            if ui
                .selectable_label(self.show_watermark, "💧 Watermark")
                .on_hover_text("Stamp a text watermark across pages")
                .clicked()
            {
                self.show_watermark = !self.show_watermark;
            }

            if self.has_pending_redactions()
                && ui
                    .button(
                        egui::RichText::new("🛡 Apply Redactions")
                            .color(egui::Color32::from_rgb(230, 60, 60))
                            .strong(),
                    )
                    .on_hover_text("Permanently remove underlying content under redaction boxes")
                    .clicked()
            {
                self.confirm = Some(PendingConfirm::ApplyRedactions);
            }
            #[cfg(any(feature = "ocr", target_os = "windows"))]
            if ui
                .button("🔍 OCR page")
                .on_hover_text("Recognize text on this page so it becomes searchable")
                .clicked()
            {
                self.ocr_page();
            }
            #[cfg(any(feature = "ocr", target_os = "windows"))]
            if ui
                .button("📄 OCR text")
                .on_hover_text("Show the recognized text for the current page")
                .clicked()
            {
                self.show_page_text = !self.show_page_text;
            }

            ui.separator();

            if ui
                .add_enabled(
                    self.selected_image.is_some(),
                    egui::Button::new("💾 Extract image"),
                )
                .on_disabled_hover_text("Select an image first")
                .clicked()
            {
                self.extract_selected_image();
            }

            if ui
                .add_enabled(
                    self.selected_image.is_some(),
                    egui::Button::new("🖼 Replace image…"),
                )
                .on_disabled_hover_text("Select an image first")
                .clicked()
            {
                self.replace_selected_image();
            }

            if ui
                .add_enabled(
                    self.selected_image.is_some(),
                    egui::Button::new("↺ Rotate left"),
                )
                .on_disabled_hover_text("Select an image first")
                .clicked()
            {
                self.rotate_selected_image(-90.0);
            }

            if ui
                .add_enabled(
                    self.selected_image.is_some(),
                    egui::Button::new("↻ Rotate right"),
                )
                .on_disabled_hover_text("Select an image first")
                .clicked()
            {
                self.rotate_selected_image(90.0);
            }

            let crop_label = if self.crop_mode {
                "✂ Crop image (click to cancel)"
            } else {
                "✂ Crop image"
            };
            if ui
                .add_enabled(self.selected_image.is_some(), egui::Button::new(crop_label))
                .on_disabled_hover_text("Select an image first")
                .clicked()
            {
                if self.crop_mode {
                    self.crop_mode = false;
                    self.crop_start = None;
                    self.crop_current = None;
                    self.status = "Crop cancelled".into();
                } else {
                    self.crop_mode = true;
                    self.crop_start = None;
                    self.crop_current = None;
                    self.tool = Tool::ImageSelect;
                    self.status = "Drag a rectangle over the selected image to crop".into();
                }
            }
        });
    }

    pub(super) fn bottom_bar(&mut self, ui: &mut egui::Ui) {
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

    pub(super) fn crop_page_window(&mut self, ctx: &egui::Context) {
        if !self.show_crop_page || self.document.is_none() {
            return;
        }
        let mut open = self.show_crop_page;
        let mut apply = false;
        let mut reset = false;
        let page_num = self.current_page + 1;
        let (pw, ph) = self.page_size_pts(self.current_page);

        egui::Window::new("✂ Crop page margins")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(300.0)
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(format!("Current page {page_num} ({pw:.0} × {ph:.0} pt)"))
                        .small()
                        .weak(),
                );
                ui.add_space(4.0);

                ui.horizontal(|ui| {
                    ui.label("Presets:");
                    if ui.small_button("18 pt").clicked() {
                        self.crop_margins = [18.0; 4];
                    }
                    if ui.small_button("36 pt (0.5 in)").clicked() {
                        self.crop_margins = [36.0; 4];
                    }
                    if ui.small_button("72 pt (1 in)").clicked() {
                        self.crop_margins = [72.0; 4];
                    }
                });
                ui.add_space(4.0);

                let labels = ["Left (pt):", "Bottom (pt):", "Right (pt):", "Top (pt):"];
                for (i, label) in labels.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(*label);
                        ui.add(
                            egui::Slider::new(&mut self.crop_margins[i], 0.0..=200.0)
                                .step_by(1.0),
                        );
                    });
                }

                ui.add_space(4.0);
                ui.checkbox(&mut self.crop_all_pages, "Apply to all pages");
                ui.add_space(6.0);

                ui.horizontal(|ui| {
                    if ui.button("Apply crop").clicked() {
                        apply = true;
                    }
                    if ui.button("Reset crop").clicked() {
                        reset = true;
                    }
                });
            });

        self.show_crop_page = open;
        if apply {
            self.apply_page_crop();
        } else if reset {
            self.reset_page_crop();
        }
    }

    pub(super) fn signatures_window(&mut self, ctx: &egui::Context) {
        if !self.show_signatures {
            return;
        }
        self.ensure_signature_textures(ctx);
        let mut open = self.show_signatures;
        let mut place_sig: Option<(PathBuf, u32, u32)> = None;
        let mut delete_id: Option<String> = None;
        let mut trigger_import = false;
        let mut save_drawn = false;
        let mut save_typed = false;

        egui::Window::new("✍ Reusable Signatures")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(380.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.sig_tab, SignatureTab::Saved, "Saved");
                    ui.selectable_value(&mut self.sig_tab, SignatureTab::Draw, "Draw");
                    ui.selectable_value(&mut self.sig_tab, SignatureTab::Type, "Type");
                });
                ui.separator();

                match self.sig_tab {
                    SignatureTab::Saved => {
                        ui.horizontal(|ui| {
                            if ui.button("🖼 Import Image…").clicked() {
                                trigger_import = true;
                            }
                            ui.label(egui::RichText::new("Click Place to stamp on current page").small().weak());
                        });
                        ui.add_space(6.0);
                        if self.saved_signatures.is_empty() {
                            ui.label(egui::RichText::new("No saved signatures yet. Draw, type, or import one.").weak());
                        } else {
                            egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                                for sig in &self.saved_signatures {
                                    ui.group(|ui| {
                                        ui.horizontal(|ui| {
                                            if let Some(tex) = self.sig_textures.get(&sig.id) {
                                                ui.add(egui::Image::from_texture(tex).max_size(egui::vec2(150.0, 50.0)).maintain_aspect_ratio(true));
                                            }
                                            ui.vertical(|ui| {
                                                ui.strong(&sig.name);
                                                ui.horizontal(|ui| {
                                                    if ui.add_enabled(self.document.is_some(), egui::Button::new("Place")).clicked() {
                                                        place_sig = Some((sig.path.clone(), sig.width, sig.height));
                                                    }
                                                    if ui.button("🗑").on_hover_text("Delete saved signature").clicked() {
                                                        delete_id = Some(sig.id.clone());
                                                    }
                                                });
                                            });
                                        });
                                    });
                                }
                            });
                        }
                    }
                    SignatureTab::Draw => {
                        ui.horizontal(|ui| {
                            ui.label("Name:");
                            ui.add(egui::TextEdit::singleline(&mut self.sig_name).hint_text("My Signature").desired_width(140.0));
                            let mut c = egui::Color32::from_rgba_unmultiplied(self.sig_color[0], self.sig_color[1], self.sig_color[2], self.sig_color[3]);
                            if ui.color_edit_button_srgba(&mut c).changed() {
                                self.sig_color = c.to_srgba_unmultiplied();
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("Width:");
                            ui.add(egui::Slider::new(&mut self.sig_stroke_width, 1.0..=8.0).step_by(0.5));
                            if ui.button("Clear").clicked() {
                                self.sig_draw_strokes.clear();
                                self.sig_draw_active = None;
                            }
                        });
                        let canvas_size = egui::vec2(340.0, 120.0);
                        let (rect, resp) = ui.allocate_exact_size(canvas_size, egui::Sense::drag());
                        ui.painter().rect_filled(rect, 4.0, egui::Color32::WHITE);
                        ui.painter().rect_stroke(rect, 4.0, egui::Stroke::new(1.0, egui::Color32::GRAY), egui::StrokeKind::Middle);
                        let pointer_pos = ui.input(|i| i.pointer.interact_pos());
                        if resp.drag_started() && let Some(pos) = pointer_pos {
                            self.sig_draw_active = Some(vec![(pos.x - rect.min.x, pos.y - rect.min.y)]);
                        }
                        if resp.dragged() && let Some(pos) = pointer_pos {
                            let pt = ((pos.x - rect.min.x).clamp(0.0, rect.width()), (pos.y - rect.min.y).clamp(0.0, rect.height()));
                            if let Some(active) = self.sig_draw_active.as_mut() {
                                active.push(pt);
                            }
                        }
                        if resp.drag_stopped() && let Some(active) = self.sig_draw_active.take() && !active.is_empty() {
                            self.sig_draw_strokes.push(active);
                        }
                        let stroke_c = egui::Color32::from_rgba_unmultiplied(self.sig_color[0], self.sig_color[1], self.sig_color[2], self.sig_color[3]);
                        for stroke in self.sig_draw_strokes.iter().chain(self.sig_draw_active.iter()) {
                            if stroke.len() >= 2 {
                                let pts: Vec<egui::Pos2> = stroke.iter().map(|&(x, y)| egui::pos2(rect.min.x + x, rect.min.y + y)).collect();
                                ui.painter().add(egui::Shape::line(pts, egui::Stroke::new(self.sig_stroke_width, stroke_c)));
                            }
                        }
                        ui.add_space(6.0);
                        if ui.add_enabled(!self.sig_draw_strokes.is_empty(), egui::Button::new("💾 Save & Place")).clicked() {
                            save_drawn = true;
                        }
                    }
                    SignatureTab::Type => {
                        ui.horizontal(|ui| {
                            ui.label("Text:");
                            ui.add(egui::TextEdit::singleline(&mut self.sig_type_text).hint_text("Jane Doe").desired_width(180.0));
                        });
                        ui.horizontal(|ui| {
                            ui.label("Name:");
                            ui.add(egui::TextEdit::singleline(&mut self.sig_name).hint_text("optional label").desired_width(140.0));
                            let mut c = egui::Color32::from_rgba_unmultiplied(self.sig_color[0], self.sig_color[1], self.sig_color[2], self.sig_color[3]);
                            if ui.color_edit_button_srgba(&mut c).changed() {
                                self.sig_color = c.to_srgba_unmultiplied();
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("Style:");
                            ui.selectable_value(&mut self.sig_type_style, 0, "Serif Italic");
                            ui.selectable_value(&mut self.sig_type_style, 1, "Sans Oblique");
                            ui.selectable_value(&mut self.sig_type_style, 2, "Mono Oblique");
                        });
                        ui.add_space(6.0);
                        if ui.add_enabled(!self.sig_type_text.trim().is_empty(), egui::Button::new("💾 Save & Place")).clicked() {
                            save_typed = true;
                        }
                    }
                }
            });

        self.show_signatures = open;
        if trigger_import {
            self.import_signature_dialog();
        }
        if let Some(id) = delete_id {
            match crate::persist::delete_signature(&id) {
                Ok(()) => {
                    self.refresh_signatures();
                    self.status = "Signature deleted".into();
                }
                Err(e) => self.error = Some(format!("Could not delete signature: {e}")),
            }
        }
        if save_drawn {
            let name = if self.sig_name.trim().is_empty() { "Drawn Signature" } else { self.sig_name.trim() };
            match crate::persist::save_signature_from_strokes(name, &self.sig_draw_strokes, self.sig_color, self.sig_stroke_width) {
                Ok(saved) => {
                    self.sig_draw_strokes.clear();
                    self.sig_name.clear();
                    self.refresh_signatures();
                    self.sig_tab = SignatureTab::Saved;
                    if self.document.is_some() {
                        self.place_signature_at_center(&saved.path, saved.width, saved.height);
                    }
                    self.status = format!("Saved signature '{}'", saved.name);
                }
                Err(e) => self.error = Some(format!("Could not save signature: {e}")),
            }
        }
        if save_typed {
            let text = self.sig_type_text.trim().to_string();
            let name = if self.sig_name.trim().is_empty() { text.as_str() } else { self.sig_name.trim() };
            match pdf_viewer::document::render_typed_signature_rgba(self.pdfium, &text, self.sig_type_style, self.sig_color) {
                Ok(rgba) => match crate::persist::save_signature_from_rgba(name, &rgba) {
                    Ok(saved) => {
                        self.sig_type_text.clear();
                        self.sig_name.clear();
                        self.refresh_signatures();
                        self.sig_tab = SignatureTab::Saved;
                        if self.document.is_some() {
                            self.place_signature_at_center(&saved.path, saved.width, saved.height);
                        }
                        self.status = format!("Saved signature '{}'", saved.name);
                    }
                    Err(e) => self.error = Some(format!("Could not save signature: {e}")),
                },
                Err(e) => self.error = Some(format!("Could not render typed signature: {e}")),
            }
        }
        if let Some((path, w, h)) = place_sig {
            self.place_signature_at_center(&path, w, h);
        }
    }

    pub(super) fn flatten_window(&mut self, ctx: &egui::Context) {
        if !self.show_flatten || self.document.is_none() {
            return;
        }
        let mut open = self.show_flatten;
        let page_num = self.current_page + 1;
        let page_count = self.document.as_ref().map(|d| d.page_count).unwrap_or(0);

        egui::Window::new("📄 Flatten Annotations & Forms")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.label("Flattening permanently merges annotations, signatures, and filled form fields into static page content.");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button(format!("Flatten Page {page_num}")).clicked() {
                        self.confirm = Some(PendingConfirm::FlattenPage);
                    }
                    if ui.button(format!("Flatten All {page_count} Pages")).clicked() {
                        self.confirm = Some(PendingConfirm::FlattenAll);
                    }
                });
            });
        self.show_flatten = open;
    }

    pub(super) fn bookmarks_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Bookmarks");
        if self.bookmarks.is_empty() {
            ui.label(egui::RichText::new("No bookmarks in this PDF.").weak());
            return;
        }
        ui.separator();
        let nodes = self.bookmarks.clone();
        let mut target_page: Option<usize> = None;
        fn draw_nodes(ui: &mut egui::Ui, nodes: &[BookmarkNode], target_page: &mut Option<usize>, id_prefix: &str) {
            for (idx, node) in nodes.iter().enumerate() {
                let label = if let Some(p) = node.page {
                    format!("{} (p.{})", node.title, p + 1)
                } else {
                    node.title.clone()
                };
                if node.children.is_empty() {
                    if ui.selectable_label(false, label).clicked() && let Some(p) = node.page {
                        *target_page = Some(p);
                    }
                } else {
                    let id = ui.make_persistent_id(format!("{id_prefix}_{idx}"));
                    egui::CollapsingHeader::new(&label)
                        .id_salt(id)
                        .default_open(true)
                        .show(ui, |ui| {
                            if let Some(p) = node.page && ui.small_button(format!("Go to page {}", p + 1)).clicked() {
                                *target_page = Some(p);
                            }
                            draw_nodes(ui, &node.children, target_page, &format!("{id_prefix}_{idx}"));
                        });
                }
            }
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            draw_nodes(ui, &nodes, &mut target_page, "bm");
        });
        if let Some(p) = target_page {
            self.go_to(p);
        }
    }

    pub(super) fn headers_footers_window(&mut self, ctx: &egui::Context) {
        if !self.show_headers_footers || self.document.is_none() {
            return;
        }
        let mut open = self.show_headers_footers;
        let mut apply = false;
        let total = self.document.as_ref().map(|d| d.page_count).unwrap_or(1).max(1);

        egui::Window::new("🔢 Page Numbers & Headers/Footers")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(360.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Presets:");
                    if ui.small_button("Page {page} of {total}").clicked() {
                        self.hf_text = "Page {page} of {total}".into();
                    }
                    if ui.small_button("{page}").clicked() {
                        self.hf_text = "{page}".into();
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Text:");
                    ui.add(egui::TextEdit::singleline(&mut self.hf_text).desired_width(240.0));
                });
                ui.label(egui::RichText::new("Use {page} (or {n}) and {total} as placeholders.").small().weak());
                ui.add_space(4.0);

                ui.horizontal_wrapped(|ui| {
                    ui.label("Position:");
                    for pos in StampPosition::ALL {
                        ui.selectable_value(&mut self.hf_position, pos, pos.label());
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Size:");
                    ui.add(egui::Slider::new(&mut self.hf_font_size, 6.0..=36.0).step_by(1.0));
                    let mut c = egui::Color32::from_rgba_unmultiplied(self.hf_color[0], self.hf_color[1], self.hf_color[2], self.hf_color[3]);
                    if ui.color_edit_button_srgba(&mut c).changed() {
                        self.hf_color = c.to_srgba_unmultiplied();
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Margin X:");
                    ui.add(egui::DragValue::new(&mut self.hf_margin_x).range(0.0..=200.0));
                    ui.label("Y:");
                    ui.add(egui::DragValue::new(&mut self.hf_margin_y).range(0.0..=200.0));
                    ui.label("Start #:");
                    ui.add(egui::DragValue::new(&mut self.hf_start_number).range(1..=9999));
                });
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.hf_all_pages, "All pages");
                    if !self.hf_all_pages {
                        ui.label("From:");
                        ui.add(egui::DragValue::new(&mut self.hf_from_page).range(1..=total));
                        ui.label("To:");
                        ui.add(egui::DragValue::new(&mut self.hf_to_page).range(1..=total));
                    }
                });
                ui.add_space(6.0);
                if ui.add_enabled(!self.hf_text.trim().is_empty(), egui::Button::new("Apply")).clicked() {
                    apply = true;
                }
            });
        self.show_headers_footers = open;
        if apply {
            self.apply_headers_footers_action();
        }
    }

    pub(super) fn watermark_window(&mut self, ctx: &egui::Context) {
        if !self.show_watermark || self.document.is_none() {
            return;
        }
        let mut open = self.show_watermark;
        let mut apply = false;

        egui::Window::new("💧 Add Watermark")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(340.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Presets:");
                    for preset in ["CONFIDENTIAL", "DRAFT", "COPY"] {
                        if ui.small_button(preset).clicked() {
                            self.wm_text = preset.into();
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Text:");
                    ui.add(egui::TextEdit::singleline(&mut self.wm_text).desired_width(220.0));
                });
                ui.horizontal(|ui| {
                    ui.label("Size:");
                    ui.add(egui::Slider::new(&mut self.wm_font_size, 16.0..=140.0).step_by(2.0));
                });
                ui.horizontal(|ui| {
                    ui.label("Angle°:");
                    ui.add(egui::Slider::new(&mut self.wm_angle, -90.0..=90.0).step_by(5.0));
                });
                ui.horizontal(|ui| {
                    ui.label("Colour:");
                    let mut c = egui::Color32::from_rgba_unmultiplied(self.wm_color[0], self.wm_color[1], self.wm_color[2], self.wm_color[3]);
                    if ui.color_edit_button_srgba(&mut c).changed() {
                        self.wm_color = c.to_srgba_unmultiplied();
                    }
                    let mut alpha = self.wm_color[3];
                    ui.label("Opacity:");
                    if ui.add(egui::Slider::new(&mut alpha, 15..=255)).changed() {
                        self.wm_color[3] = alpha;
                    }
                });
                ui.checkbox(&mut self.wm_all_pages, "Apply to all pages (unchecked = current page)");
                ui.add_space(6.0);
                if ui.add_enabled(!self.wm_text.trim().is_empty(), egui::Button::new("Apply Watermark")).clicked() {
                    apply = true;
                }
            });
        self.show_watermark = open;
        if apply {
            self.apply_watermark_action();
        }
    }

    pub(super) fn add_link_window(&mut self, ctx: &egui::Context) {
        if self.pending_link_rect.is_none() || self.document.is_none() {
            return;
        }
        let total = self.document.as_ref().map(|d| d.page_count).unwrap_or(1).max(1);
        let mut commit = false;
        let mut cancel = false;

        egui::Window::new("🔗 Add Hyperlink")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.link_target_is_page, false, "Web URL");
                    ui.selectable_value(&mut self.link_target_is_page, true, "Go to Page");
                });
                ui.add_space(4.0);
                if self.link_target_is_page {
                    ui.horizontal(|ui| {
                        ui.label("Target page:");
                        ui.add(egui::DragValue::new(&mut self.link_page_input).range(1..=total));
                    });
                } else {
                    ui.horizontal(|ui| {
                        ui.label("URL:");
                        ui.add(egui::TextEdit::singleline(&mut self.link_url_input).desired_width(240.0));
                    });
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let valid = self.link_target_is_page || !self.link_url_input.trim().is_empty();
                    if ui.add_enabled(valid, egui::Button::new("Add Link")).clicked() {
                        commit = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if cancel {
            self.pending_link_rect = None;
        } else if commit {
            self.commit_pending_link();
        }
    }

    pub(super) fn metadata_window(&mut self, ctx: &egui::Context) {
        if !self.show_metadata || self.document.is_none() {
            return;
        }
        let mut open = self.show_metadata;
        let mut apply = false;
        let producer = self.document.as_ref().map(|d| d.producer.clone()).unwrap_or_default();
        let file_size = self.document.as_ref().map(|d| d.file_size).unwrap_or(0);

        egui::Window::new("🏷 PDF Metadata")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(360.0)
            .show(ctx, |ui| {
                egui::Grid::new("meta_grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                    ui.label("Title:");
                    ui.add(egui::TextEdit::singleline(&mut self.meta_draft.title).desired_width(250.0));
                    ui.end_row();
                    ui.label("Author:");
                    ui.add(egui::TextEdit::singleline(&mut self.meta_draft.author).desired_width(250.0));
                    ui.end_row();
                    ui.label("Subject:");
                    ui.add(egui::TextEdit::singleline(&mut self.meta_draft.subject).desired_width(250.0));
                    ui.end_row();
                    ui.label("Keywords:");
                    ui.add(egui::TextEdit::singleline(&mut self.meta_draft.keywords).desired_width(250.0));
                    ui.end_row();
                    if !producer.is_empty() {
                        ui.label("Producer:");
                        ui.label(egui::RichText::new(producer).weak());
                        ui.end_row();
                    }
                    ui.label("File size:");
                    ui.label(egui::RichText::new(format!("{:.1} KB", file_size as f64 / 1024.0)).weak());
                    ui.end_row();
                });
                ui.add_space(6.0);
                if ui.button("Apply Metadata").clicked() {
                    apply = true;
                }
            });
        self.show_metadata = open;
        if apply && let Some(doc) = self.document.as_mut() {
            doc.set_metadata(self.meta_draft.clone());
            self.dirty = true;
            self.status = "Metadata updated (save to write to disk)".into();
        }
    }

    pub(super) fn compress_window(&mut self, ctx: &egui::Context) {
        if !self.show_compress || self.document.is_none() {
            return;
        }
        let mut open = self.show_compress;
        let mut run_compress = false;

        egui::Window::new("🗜 Compress PDF")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(340.0)
            .show(ctx, |ui| {
                ui.label("Downsamples and JPEG-recompresses embedded images across all pages.");
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("Preset:");
                    for preset in [
                        CompressionPreset::Screen,
                        CompressionPreset::Balanced,
                        CompressionPreset::High,
                    ] {
                        if ui
                            .selectable_value(&mut self.compress_preset, preset, preset.label())
                            .clicked()
                        {
                            let (dpi, q) = preset.defaults();
                            self.compress_max_dpi = dpi;
                            self.compress_quality = q;
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Max DPI:");
                    ui.add(egui::Slider::new(&mut self.compress_max_dpi, 50.0..=300.0).step_by(10.0));
                });
                ui.horizontal(|ui| {
                    ui.label("JPEG quality:");
                    let mut q = self.compress_quality;
                    if ui.add(egui::Slider::new(&mut q, 20..=95)).changed() {
                        self.compress_quality = q;
                    }
                });
                if let Some(stats) = &self.compress_stats {
                    ui.add_space(4.0);
                    let before_kb = stats.bytes_before as f64 / 1024.0;
                    let after_kb = stats.bytes_after as f64 / 1024.0;
                    let pct = if stats.bytes_before > 0 && stats.bytes_after < stats.bytes_before {
                        (1.0 - (stats.bytes_after as f64 / stats.bytes_before as f64)) * 100.0
                    } else {
                        0.0
                    };
                    ui.strong(format!(
                        "Processed {} image(s): {before_kb:.0} KB -> {after_kb:.0} KB ({pct:.0}% smaller)",
                        stats.images_processed
                    ));
                }
                ui.add_space(6.0);
                if ui.button("Compress Now").clicked() {
                    run_compress = true;
                }
            });
        self.show_compress = open;
        if run_compress {
            self.compress_action();
        }
    }

    pub(super) fn security_window(&mut self, ctx: &egui::Context) {
        if !self.show_security || self.document.is_none() {
            return;
        }
        let mut open = self.show_security;
        let mut apply_pw = false;
        let mut remove_pw = false;
        let is_protected = self
            .document
            .as_ref()
            .is_some_and(|d| d.is_password_protected() || d.is_encrypted);

        egui::Window::new("🔒 PDF Security & Password")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(340.0)
            .show(ctx, |ui| {
                if is_protected {
                    ui.colored_label(
                        egui::Color32::from_rgb(40, 150, 80),
                        "🔒 Password protection is active (applied on Save).",
                    );
                } else {
                    ui.label("🔓 Document is not password-protected.");
                }
                ui.add_space(6.0);

                egui::Grid::new("security_grid")
                    .num_columns(2)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Open password:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.security_user_pw)
                                .password(true)
                                .desired_width(200.0),
                        );
                        ui.end_row();

                        ui.label("Owner password:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.security_owner_pw)
                                .password(true)
                                .hint_text("Optional (defaults to open)")
                                .desired_width(200.0),
                        );
                        ui.end_row();
                    });

                ui.add_space(6.0);
                ui.label("Permissions:");
                ui.checkbox(&mut self.security_allow_print, "Allow printing");
                ui.checkbox(&mut self.security_allow_copy, "Allow copying text & images");
                ui.checkbox(&mut self.security_allow_modify, "Allow modifying document");
                ui.checkbox(&mut self.security_allow_annotations, "Allow annotations & forms");

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let can_apply = !self.security_user_pw.is_empty()
                        || !self.security_owner_pw.is_empty();
                    if ui
                        .add_enabled(can_apply, egui::Button::new("Apply Password"))
                        .clicked()
                    {
                        apply_pw = true;
                    }
                    if ui
                        .add_enabled(is_protected, egui::Button::new("Remove Password"))
                        .clicked()
                    {
                        remove_pw = true;
                    }
                });
            });

        self.show_security = open;
        if apply_pw && let Some(doc) = self.document.as_mut() {
            doc.set_encryption(Some(EncryptionOptions {
                user_password: self.security_user_pw.clone(),
                owner_password: self.security_owner_pw.clone(),
                allow_print: self.security_allow_print,
                allow_copy: self.security_allow_copy,
                allow_modify: self.security_allow_modify,
                allow_annotations: self.security_allow_annotations,
            }));
            self.dirty = true;
            self.status = "Password encryption set — save to write encrypted PDF".into();
        } else if remove_pw && let Some(doc) = self.document.as_mut() {
            if let Err(e) = doc.strip_encryption(self.pdfium) {
                self.error = Some(format!("Could not remove encryption: {e}"));
            } else {
                self.security_user_pw.clear();
                self.security_owner_pw.clear();
                self.dirty = true;
                self.status = "Password encryption removed — save to write unencrypted PDF".into();
            }
        }
    }

    pub(super) fn password_prompt_window(&mut self, ctx: &egui::Context) {
        let Some(path) = self.password_prompt_path.clone() else {
            return;
        };
        let mut submit = false;
        let mut cancel = false;

        egui::Window::new("🔑 Password Required")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("PDF");
                ui.label(format!("Enter password to open \"{name}\":"));
                ui.add_space(4.0);
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.password_prompt_input)
                        .password(true)
                        .desired_width(240.0),
                );
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    submit = true;
                }
                if let Some(err) = &self.password_prompt_error {
                    ui.colored_label(egui::Color32::from_rgb(220, 70, 70), err);
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Unlock").clicked() {
                        submit = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });

        if submit {
            let pw = self.password_prompt_input.clone();
            self.load_document_with_password(&path, Some(&pw));
        } else if cancel {
            self.password_prompt_path = None;
            self.password_prompt_input.clear();
            self.password_prompt_error = None;
        }
    }
}
