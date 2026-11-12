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
                        "Click a word to select it; double-click to rewrite the word; drag to move the line.",
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
    #[cfg(feature = "ocr")]
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
        if self.confirm.is_none() {
            return;
        }
        let (title, _is_close) = match &self.confirm {
            Some(PendingConfirm::Close) => ("Quit without saving?", true),
            Some(PendingConfirm::Open(_)) => ("Open another document?", false),
            None => return,
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
                // Save failed (error already surfaced); keep the dialog open.
                return;
            }
        }

        match self.confirm.take() {
            Some(PendingConfirm::Close) => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Some(PendingConfirm::Open(path)) => self.load_document(&path),
            None => {}
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

            ui.separator();

            if ui
                .button("🗑 Delete annotation")
                .on_hover_text("Delete the selected annotation (Delete key)")
                .clicked()
            {
                self.delete_selected();
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
                    self.selected_word = None;
                    self.text_drag = None;
                    self.text_drag_start_matrix = None;
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

            // A selected original-text word/line gets size + bold controls.
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
            #[cfg(feature = "ocr")]
            if ui
                .button("🔍 OCR page")
                .on_hover_text("Recognize text on this page so it becomes searchable")
                .clicked()
            {
                self.ocr_page();
            }
            #[cfg(feature = "ocr")]
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

}
