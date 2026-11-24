//! Shared PDF document model used by both the viewer and the editor.
//!
//! The viewer (`main.rs`) renders pages; the editor (`editor/main.rs`) adds
//! annotation creation, editing, undo/redo and saving on top of this model.

pub mod annotations;
pub mod appearance;
pub mod document;
pub mod engine;
#[cfg(any(feature = "ocr", target_os = "windows"))]
pub mod ocr;
pub mod pdfium_bindings;
pub mod security;
pub mod text_model;

pub use annotations::{Annotation, AnnotationKind, StrokeStyle};
pub use document::{
    BookmarkNode, CompressionOptions, CompressionPreset, CompressionStats, DocumentError,
    FormFieldInfo, FormFieldType, HeaderFooterOptions, LinkTarget, OpenDocument, PageInfo,
    PageLinkInfo, PdfMetadata, RedrawnLine, StampPosition, TextFontFamily, WatermarkOptions,
};
pub use engine::{SearchHit, display_size_pts, render_page, render_thumbnail, search_all_pages};
#[cfg(any(feature = "ocr", target_os = "windows"))]
pub use ocr::{OcrWord, ocr_page_words, words_to_text};
pub use pdfium_bindings::{library_dir, load_pdfium_bindings};
pub use security::EncryptionOptions;
