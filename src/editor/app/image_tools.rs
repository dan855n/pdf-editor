//! Image selection and editing tools.

use super::*;

impl EditorApp {
    /// Handle clicks and drags while the Image Select tool is active.
    pub(super) fn handle_image_select_click(
        &mut self,
        page: usize,
        page_rect: egui::Rect,
        response: egui::Response,
        pos: egui::Pos2,
        inside: bool,
    ) {
        if !inside {
            return;
        }

        // Crop mode: drag a rectangle over the selected image and release to
        // crop. While cropping, selection and move drags are disabled.
        if self.crop_mode {
            if response.drag_started() {
                if let Some((sel_page, sel_index)) = self.selected_image
                    && sel_page == page
                {
                    self.crop_start = Some((sel_page, sel_index, pos));
                    self.crop_current = None;
                    self.status = "Crop rectangle…".into();
                }
                return;
            }

            if response.dragged()
                && let Some((crop_page, crop_index, start)) = self.crop_start
                && crop_page == page
            {
                let _ = crop_index;
                let _ = start;
                self.crop_current = Some(pos);
                return;
            }

            if response.drag_stopped()
                && let Some((crop_page, crop_index, start)) = self.crop_start.take()
                && crop_page == page
            {
                let end = self.crop_current.take().unwrap_or(pos);
                self.crop_mode = false;
                self.crop_start = None;
                self.crop_current = None;
                if (end - start).length() > 4.0 {
                    self.crop_selected_image_to_rect(
                        crop_page,
                        crop_index,
                        egui::Rect::from_two_pos(start, end),
                        page_rect,
                    );
                } else {
                    self.status = "Crop cancelled (drag too small)".into();
                }
                return;
            }

            if response.clicked() {
                return;
            }

            return;
        }

        let (x, y) = self.screen_to_pdf(page, page_rect, pos);
        let tolerance = 4.0 / self.effective_scale(page);

        let mut hit = None;
        if let Some(doc) = self.document.as_ref()
            && let Ok(pdf_page) = doc.doc.pages().get(page as i32)
        {
            for (index, obj) in pdf_page.objects().iter().enumerate() {
                let Some(image) = obj.as_image_object() else {
                    continue;
                };
                let Ok(bounds) = image.bounds() else {
                    continue;
                };
                let l = bounds.left().value;
                let r = bounds.right().value;
                let b = bounds.bottom().value;
                let t = bounds.top().value;
                if x >= l - tolerance
                    && x <= r + tolerance
                    && y >= b - tolerance
                    && y <= t + tolerance
                {
                    hit = Some(index);
                    break;
                }
            }
        }

        if response.drag_started() {
            if let Some(index) = hit {
                let start_matrix = self
                    .document
                    .as_ref()
                    .and_then(|doc| doc.image_object_matrix(page, index).ok());
                self.selected_image = Some((page, index));
                self.selected_text_run = None;
                self.image_drag = Some((page, index));
                self.image_drag_start_matrix = start_matrix;
                self.status = "Move image".into();
            }
            return;
        }

        if response.dragged()
            && let Some((drag_page, drag_index)) = self.image_drag
            && drag_page == page
        {
            // egui's drag_delta() is the pointer movement since the previous
            // frame, so apply it directly rather than accumulating.
            let delta = response.drag_delta();
            let scale = self.effective_scale(page);
            let dx_pt = delta.x / scale;
            let dy_pt = -delta.y / scale;

            if let Some(doc) = self.document.as_mut()
                && let Err(e) = doc.translate_image_object(page, drag_index, dx_pt, dy_pt)
            {
                self.error = Some(format!("Could not move image: {e}"));
            }
            return;
        }

        if response.drag_stopped()
            && let Some((drag_page, drag_index)) = self.image_drag.take()
        {
            let before = self.image_drag_start_matrix.take();
            let after = self
                .document
                .as_ref()
                .and_then(|doc| doc.image_object_matrix(drag_page, drag_index).ok());

            if let Some(doc) = self.document.as_mut() {
                doc.refresh_render_docs(self.pdfium);
            }
            self.cache.clear();
            self.thumbs.clear();
            self.dirty = true;

            if let (Some(before), Some(after)) = (before, after) {
                self.undo.push(
                    "Move image",
                    DocumentEdit::ImageMove(ImageMove {
                        page: drag_page,
                        object_index: drag_index,
                        before_matrix: before,
                        after_matrix: after,
                    }),
                );
            }
            self.status = "Image moved".into();
            return;
        }

        if response.clicked() {
            match hit {
                Some(index) => {
                    self.selected_image = Some((page, index));
                    self.selected_text_run = None;
                    self.status = self.image_info_status(page, index);
                }
                None => {
                    self.selected_image = None;
                }
            }
        }
    }

    /// Build a short status string describing the selected image.
    pub(super) fn image_info_status(&self, page: usize, object_index: usize) -> String {
        let Some(doc) = self.document.as_ref() else {
            return "Image selected".into();
        };
        let Ok(pdf_page) = doc.doc.pages().get(page as i32) else {
            return "Image selected".into();
        };
        let Ok(object) = pdf_page.objects().get(object_index) else {
            return "Image selected".into();
        };
        let Some(image) = object.as_image_object() else {
            return "Image selected".into();
        };

        let width = image.width().unwrap_or(0);
        let height = image.height().unwrap_or(0);
        let dpi = image.horizontal_dpi().unwrap_or(0.0);
        let bits = image.bits_per_pixel().unwrap_or(0);
        format!("Image selected: {width}×{height} px, {dpi:.0} dpi, {bits} bpc")
    }

    /// Open a save dialog and write the selected image to the chosen path.
    pub(super) fn extract_selected_image(&mut self) {
        if self.image_extract_pending.is_some() {
            return;
        }
        let Some((page, object_index)) = self.selected_image else {
            self.status = "No image selected".into();
            return;
        };

        let Some(doc) = self.document.as_ref() else {
            return;
        };
        let Ok(pdf_page) = doc.doc.pages().get(page as i32) else {
            return;
        };
        let Ok(object) = pdf_page.objects().get(object_index) else {
            return;
        };
        let Some(image) = object.as_image_object() else {
            return;
        };
        let Ok(raw_image) = image.get_raw_image() else {
            self.error = Some("Could not read the selected image".into());
            return;
        };

        let (tx, rx) = mpsc::channel();
        self.image_extract_pending = Some(rx);

        std::thread::spawn(move || {
            let picked = rfd::FileDialog::new()
                .set_title("Extract image")
                .set_file_name("image.png")
                .save_file();

            let result = match picked {
                Some(path) if raw_image.save(&path).is_ok() => Some(path),
                _ => None,
            };
            let _ = tx.send(result);
        });
    }

    /// Rotate the selected image clockwise by `degrees_cw` (negative is
    /// counter-clockwise), then refresh the render caches.
    pub(super) fn rotate_selected_image(&mut self, degrees_cw: f32) {
        let Some((page, object_index)) = self.selected_image else {
            self.status = "No image selected".into();
            return;
        };

        let Some(doc) = self.document.as_mut() else {
            return;
        };

        if let Err(e) = doc.rotate_image_object(page, object_index, degrees_cw) {
            self.error = Some(format!("Could not rotate image: {e}"));
            return;
        }

        doc.refresh_render_docs(self.pdfium);
        self.cache.clear();
        self.thumbs.clear();
        self.dirty = true;

        self.undo.push(
            if degrees_cw < 0.0 {
                "Rotate image left"
            } else {
                "Rotate image right"
            },
            DocumentEdit::ImageRotate(ImageRotate {
                page,
                object_index,
                degrees_cw,
            }),
        );

        self.status = if degrees_cw < 0.0 {
            "Image rotated left".into()
        } else {
            "Image rotated right".into()
        };
    }

    /// Open a file dialog and replace the selected image with the chosen file.
    pub(super) fn replace_selected_image(&mut self) {
        if self.image_replace_pending.is_some() {
            return;
        }
        let Some((page, object_index)) = self.selected_image else {
            self.status = "No image selected".into();
            return;
        };

        let Some(doc) = self.document.as_ref() else {
            return;
        };
        let before_bytes = match doc.image_object_png_bytes(page, object_index) {
            Ok(bytes) => bytes,
            Err(e) => {
                self.error = Some(format!("Could not snapshot image: {e}"));
                return;
            }
        };

        let (tx, rx) = mpsc::channel();
        self.image_replace_pending = Some(rx);

        std::thread::spawn(move || {
            let picked = rfd::FileDialog::new()
                .set_title("Replace image")
                .add_filter("Images", &["png", "jpg", "jpeg"])
                .pick_file();

            let result = picked.and_then(|path| {
                std::fs::read(path)
                    .ok()
                    .map(|bytes| (page, object_index, before_bytes, bytes))
            });
            let _ = tx.send(result);
        });
    }

    /// Crop the selected image to the user-drawn screen rectangle.
    pub(super) fn crop_selected_image_to_rect(
        &mut self,
        page: usize,
        object_index: usize,
        screen_rect: egui::Rect,
        page_rect: egui::Rect,
    ) {
        let (x0, y0) = self.screen_to_pdf(page, page_rect, screen_rect.min);
        let (x1, y1) = self.screen_to_pdf(page, page_rect, screen_rect.max);
        let left = x0.min(x1);
        let right = x0.max(x1);
        let bottom = y0.min(y1);
        let top = y0.max(y1);
        let crop_rect = PdfRect::new_from_values(bottom, left, top, right);

        // Clamp the crop rectangle to the selected image's visible bounds so
        // dragging partly outside the image cannot replace it with blank page.
        let crop_rect = {
            let Some(doc) = self.document.as_ref() else {
                return;
            };
            let Ok(pdf_page) = doc.doc.pages().get(page as i32) else {
                return;
            };
            let Ok(object) = pdf_page.objects().get(object_index) else {
                return;
            };
            let Some(image) = object.as_image_object() else {
                return;
            };
            let Ok(image_rect) = image.bounds().map(|b| b.to_rect()) else {
                return;
            };
            let left = crop_rect.left().value.max(image_rect.left().value);
            let right = crop_rect.right().value.min(image_rect.right().value);
            let bottom = crop_rect.bottom().value.max(image_rect.bottom().value);
            let top = crop_rect.top().value.min(image_rect.top().value);
            if left >= right || bottom >= top {
                self.status = "Crop cancelled (rectangle outside image)".into();
                return;
            }
            PdfRect::new_from_values(bottom, left, top, right)
        };

        let (before_bytes, before_matrix) = {
            let Some(doc) = self.document.as_ref() else {
                return;
            };
            match (
                doc.image_object_png_bytes(page, object_index),
                doc.image_object_matrix(page, object_index),
            ) {
                (Ok(bytes), Ok(matrix)) => (bytes, matrix),
                (Err(e), _) | (_, Err(e)) => {
                    self.error = Some(format!("Could not snapshot image: {e}"));
                    return;
                }
            }
        };

        if let Some(doc) = self.document.as_mut()
            && let Err(e) = doc.crop_image_object(page, object_index, crop_rect)
        {
            self.error = Some(format!("Could not crop image: {e}"));
            return;
        }

        let (after_bytes, after_matrix) = {
            let Some(doc) = self.document.as_ref() else {
                return;
            };
            match (
                doc.image_object_png_bytes(page, object_index),
                doc.image_object_matrix(page, object_index),
            ) {
                (Ok(bytes), Ok(matrix)) => (bytes, matrix),
                (Err(e), _) | (_, Err(e)) => {
                    self.error = Some(format!("Could not snapshot cropped image: {e}"));
                    return;
                }
            }
        };

        if let Some(doc) = self.document.as_mut() {
            doc.refresh_render_docs(self.pdfium);
        }
        self.cache.clear();
        self.thumbs.clear();
        self.dirty = true;

        self.undo.push(
            "Crop image",
            DocumentEdit::ImageCrop(ImageCrop {
                page,
                object_index,
                before_bytes,
                before_matrix,
                after_bytes,
                after_matrix,
            }),
        );
        self.status = "Image cropped".into();
    }

}
