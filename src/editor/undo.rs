//! Undo/redo for editor operations.
//!
//! The editor's document is a PDFium handle plus plain-data models, so undo
//! is implemented as snapshots of those models. Annotation operations store
//! full per-page annotation lists; text edits store the before/after strings
//! for a single line.

use pdf_viewer::annotations::Annotation;
use pdfium_render::prelude::{PdfMatrix, PdfPageTextRenderMode};

/// One move-image edit. Undo/redo restores the object's exact matrix.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageMove {
    pub page: usize,
    pub object_index: usize,
    pub before_matrix: PdfMatrix,
    pub after_matrix: PdfMatrix,
}

/// One move-text edit. Undo/redo restores the text object's exact matrix.
#[derive(Debug, Clone, PartialEq)]
pub struct TextMove {
    pub page: usize,
    pub object_index: usize,
    pub before_matrix: PdfMatrix,
    pub after_matrix: PdfMatrix,
}

/// One restyle-text edit (font size + render mode). Undo/redo restores both.
#[derive(Debug, Clone, PartialEq)]
pub struct TextStyle {
    pub page: usize,
    pub object_index: usize,
    pub before_size: f32,
    pub after_size: f32,
    pub before_mode: PdfPageTextRenderMode,
    pub after_mode: PdfPageTextRenderMode,
}

/// One rotate-image edit.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageRotate {
    pub page: usize,
    pub object_index: usize,
    /// Clockwise rotation applied by the original edit. Undo applies the
    /// negative of this value; redo applies it again.
    pub degrees_cw: f32,
}

/// One replace-image edit. The object matrix is unchanged.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageReplace {
    pub page: usize,
    pub object_index: usize,
    pub before_bytes: Vec<u8>,
    pub after_bytes: Vec<u8>,
}

/// One crop-image edit. Crop changes both the bitmap and the on-page matrix.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageCrop {
    pub page: usize,
    pub object_index: usize,
    pub before_bytes: Vec<u8>,
    pub before_matrix: PdfMatrix,
    pub after_bytes: Vec<u8>,
    pub after_matrix: PdfMatrix,
}

/// One original-text edit to a single visual line.
#[derive(Debug, Clone, PartialEq)]
pub struct TextEdit {
    pub page: usize,
    pub run_index: usize,
    /// The visual line text before and after the edit (UI model).
    pub run_original: String,
    pub run_new: String,
    /// The full PDF text-object contents before and after the edit. These can
    /// differ from the visual line by leading/trailing whitespace.
    pub object_original: String,
    pub object_new: String,
    /// Index of the original PDF text object in the page's object collection.
    pub object_index: usize,
}

/// One undoable document edit.
#[derive(Debug, Clone, PartialEq)]
pub enum DocumentEdit {
    Annotations {
        before: Vec<Vec<Annotation>>,
        after: Vec<Vec<Annotation>>,
    },
    Text(TextEdit),
    ImageMove(ImageMove),
    TextMove(TextMove),
    TextStyle(TextStyle),
    ImageRotate(ImageRotate),
    ImageReplace(ImageReplace),
    ImageCrop(ImageCrop),
}

/// One undoable step: a label plus the edit itself.
pub struct Step {
    pub label: String,
    pub edit: DocumentEdit,
}

pub struct UndoStack {
    past: Vec<Step>,
    future: Vec<Step>,
    limit: usize,
}

impl Default for UndoStack {
    fn default() -> Self {
        Self::new(100)
    }
}

impl UndoStack {
    pub fn new(limit: usize) -> Self {
        Self {
            past: Vec::new(),
            future: Vec::new(),
            limit: limit.max(1),
        }
    }

    /// Record an operation.
    pub fn push(&mut self, label: impl Into<String>, edit: DocumentEdit) {
        if self.past.len() >= self.limit {
            self.past.remove(0);
        }
        self.past.push(Step {
            label: label.into(),
            edit,
        });
        self.future.clear();
    }

    pub fn can_undo(&self) -> bool {
        !self.past.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.future.is_empty()
    }

    /// Take the step that would be undone, leaving it on the redo stack.
    pub fn take_undo(&mut self) -> Option<Step> {
        let step = self.past.pop()?;
        self.future.push(Step {
            label: step.label.clone(),
            edit: step.edit.clone(),
        });
        Some(step)
    }

    /// Take the step that would be redone, leaving it on the undo stack.
    pub fn take_redo(&mut self) -> Option<Step> {
        let step = self.future.pop()?;
        self.past.push(Step {
            label: step.label.clone(),
            edit: step.edit.clone(),
        });
        Some(step)
    }

    pub fn clear(&mut self) {
        self.past.clear();
        self.future.clear();
    }

    /// Label of the step that [`Self::take_undo`] would revert, if any.
    pub fn next_undo_label(&self) -> Option<&str> {
        self.past.last().map(|s| s.label.as_str())
    }

    /// Label of the step that [`Self::take_redo`] would re-apply, if any.
    pub fn next_redo_label(&self) -> Option<&str> {
        self.future.last().map(|s| s.label.as_str())
    }
}
