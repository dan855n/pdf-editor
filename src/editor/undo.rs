//! Undo/redo for editor operations.
//!
//! The editor's document is a PDFium handle plus a plain-data annotation list,
//! so undo is implemented as a snapshot of the annotation model: each step
//! stores the full per-page annotation lists from before and after the
//! operation. Restoring a snapshot re-applies the model to the PDF objects.
//!
//! Snapshots are small (a few KB for typical pages) and the stack is bounded,
//! so a long editing session can't grow memory without limit.

use pdf_viewer::annotations::Annotation;

/// One undoable step: the annotation model before and after the operation.
pub struct Step {
    pub label: String,
    pub before: Vec<Vec<Annotation>>,
    pub after: Vec<Vec<Annotation>>,
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
    pub fn push(
        &mut self,
        label: impl Into<String>,
        before: Vec<Vec<Annotation>>,
        after: Vec<Vec<Annotation>>,
    ) {
        if self.past.len() >= self.limit {
            self.past.remove(0);
        }
        self.past.push(Step {
            label: label.into(),
            before,
            after,
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
            before: step.before.clone(),
            after: step.after.clone(),
        });
        Some(step)
    }

    /// Take the step that would be redone, leaving it on the undo stack.
    pub fn take_redo(&mut self) -> Option<Step> {
        let step = self.future.pop()?;
        self.past.push(Step {
            label: step.label.clone(),
            before: step.before.clone(),
            after: step.after.clone(),
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
