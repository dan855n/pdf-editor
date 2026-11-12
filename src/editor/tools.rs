//! Editor tool definitions and shared drawing state.

use pdf_viewer::annotations::{Annotation, AnnotationKind, StrokeStyle};

/// Which editing tool is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// Select, move and resize existing annotations.
    Select,
    /// Drag a rectangle to highlight text.
    Highlight,
    /// Freehand drawing.
    Ink,
    /// Click to select and edit the original text in the PDF.
    EditText,
    /// Drag a rectangle outline.
    Rectangle,
    /// Drag an ellipse outline.
    Ellipse,
    /// Click to place a text note.
    Text,
    /// Click to place an image (signature, stamp, ...).
    Image,
    /// Click an existing image in the PDF to select it.
    ImageSelect,
}

impl Tool {
    pub const ALL: [Tool; 9] = [
        Tool::Select,
        Tool::Highlight,
        Tool::Ink,
        Tool::EditText,
        Tool::Rectangle,
        Tool::Ellipse,
        Tool::Text,
        Tool::Image,
        Tool::ImageSelect,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Tool::Select => "Select",
            Tool::Highlight => "Highlight",
            Tool::Ink => "Ink",
            Tool::EditText => "Edit text",
            Tool::Rectangle => "Rect",
            Tool::Ellipse => "Ellipse",
            Tool::Text => "Text",
            Tool::Image => "Image",
            Tool::ImageSelect => "Img select",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Tool::Select => "⬚",
            Tool::Highlight => "🖍",
            Tool::Ink => "✏️",
            Tool::EditText => "🔤",
            Tool::Rectangle => "▭",
            Tool::Ellipse => "◯",
            Tool::Text => "🅣",
            Tool::Image => "🖼",
            Tool::ImageSelect => "🖼️🔍",
        }
    }

    pub fn to_kind(self) -> Option<AnnotationKind> {
        match self {
            Tool::Select => None,
            Tool::Highlight => Some(AnnotationKind::Highlight),
            Tool::Ink => Some(AnnotationKind::Ink),
            Tool::EditText => None,
            Tool::Rectangle => Some(AnnotationKind::Rectangle),
            Tool::Ellipse => Some(AnnotationKind::Ellipse),
            Tool::Text => Some(AnnotationKind::FreeText),
            Tool::Image => Some(AnnotationKind::Stamp),
            Tool::ImageSelect => None,
        }
    }
}

/// A drag in progress, in screen space.
#[derive(Debug, Clone, Copy)]
pub struct DragState {
    pub start: egui::Pos2,
    pub current: egui::Pos2,
}

/// What the user is currently doing on the canvas.
#[derive(Debug, Clone)]
pub enum Interaction {
    None,
    /// Drawing a new annotation of the given kind.
    Drawing {
        kind: AnnotationKind,
        drag: DragState,
    },
    /// Freehand ink in progress; points are in PDF space.
    Inking {
        page: usize,
        points: Vec<(f32, f32)>,
    },
    /// Moving an existing annotation.
    Moving {
        page: usize,
        /// Offset from the annotation's origin to the grab point, PDF points.
        grab_offset: (f32, f32),
        original: Annotation,
    },
    /// Resizing an existing annotation from a corner.
    Resizing {
        page: usize,
        corner: ResizeCorner,
        original: Annotation,
    },
}

/// Which corner handle is being dragged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeCorner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl ResizeCorner {
    pub fn all() -> [ResizeCorner; 4] {
        [
            ResizeCorner::TopLeft,
            ResizeCorner::TopRight,
            ResizeCorner::BottomLeft,
            ResizeCorner::BottomRight,
        ]
    }
}

/// Build a fresh annotation of `kind` from a completed drag rectangle.
pub fn annotation_from_drag(
    id: u64,
    kind: AnnotationKind,
    style: StrokeStyle,
    start: egui::Pos2,
    end: egui::Pos2,
    to_pdf: impl Fn(egui::Pos2) -> (f32, f32),
) -> Annotation {
    let (x1, y1) = to_pdf(start);
    let (x2, y2) = to_pdf(end);
    let (l, r) = (x1.min(x2), x1.max(x2));
    let (b, t) = (y1.min(y2), y1.max(y2));

    let mut a = Annotation::new(id, kind);
    a.rect = pdfium_render::prelude::PdfRect::new_from_values(b, l, t, r);
    a.style = style;
    a
}
