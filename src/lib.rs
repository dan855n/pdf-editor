//! Shared PDF document model used by both the viewer and the editor.
//!
//! The viewer (`main.rs`) renders pages; the editor (`editor/main.rs`) adds
//! annotation creation, editing, undo/redo and saving on top of this model.

pub mod annotations;
pub mod document;
pub mod pdfium_bindings;

pub use annotations::{Annotation, AnnotationKind, StrokeStyle};
pub use document::{DocumentError, OpenDocument, PageInfo, SaveOptions};
pub use pdfium_bindings::{library_dir, load_pdfium_bindings};
