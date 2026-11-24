//! Annotation model shared by the editor UI and the PDFium bridge.
//!
//! An [`Annotation`] is a plain-data description of a mark on a page. The
//! editor keeps a list of these per page; the bridge turns them into real PDF
//! annotation objects (and back again when a document is opened).

use pdfium_render::prelude::*;

/// What kind of mark an annotation is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnotationKind {
    /// Text highlight (translucent fill over a rectangle).
    Highlight,
    /// Strikeout line through selected text.
    Strikeout,
    /// Underline beneath selected text.
    Underline,
    /// Squiggly underline beneath selected text.
    Squiggly,
    /// Freehand ink stroke (a polyline of points).
    Ink,
    /// Rectangle outline / fill.
    Rectangle,
    /// Ellipse outline / fill.
    Ellipse,
    /// Free-text note placed at a point.
    FreeText,
    /// A raster image stamped onto the page (signature, logo, ...).
    Stamp,
    /// A pending redaction region (permanently destroys underlying content when applied).
    Redaction,
}

impl AnnotationKind {
    pub const ALL: [AnnotationKind; 10] = [
        AnnotationKind::Highlight,
        AnnotationKind::Strikeout,
        AnnotationKind::Underline,
        AnnotationKind::Squiggly,
        AnnotationKind::Ink,
        AnnotationKind::Rectangle,
        AnnotationKind::Ellipse,
        AnnotationKind::FreeText,
        AnnotationKind::Stamp,
        AnnotationKind::Redaction,
    ];

    pub fn label(self) -> &'static str {
        match self {
            AnnotationKind::Highlight => "Highlight",
            AnnotationKind::Strikeout => "Strikeout",
            AnnotationKind::Underline => "Underline",
            AnnotationKind::Squiggly => "Squiggly",
            AnnotationKind::Ink => "Ink",
            AnnotationKind::Rectangle => "Rectangle",
            AnnotationKind::Ellipse => "Ellipse",
            AnnotationKind::FreeText => "Text",
            AnnotationKind::Stamp => "Image",
            AnnotationKind::Redaction => "Redact",
        }
    }

    /// The PDF annotation subtype string used when creating the annotation.
    pub fn pdf_subtype(self) -> &'static str {
        match self {
            AnnotationKind::Highlight => "Highlight",
            AnnotationKind::Strikeout => "Strikeout",
            AnnotationKind::Underline => "Underline",
            AnnotationKind::Squiggly => "Squiggly",
            AnnotationKind::Ink => "Ink",
            AnnotationKind::Rectangle => "Square",
            AnnotationKind::Ellipse => "Circle",
            AnnotationKind::FreeText => "FreeText",
            AnnotationKind::Stamp => "Stamp",
            AnnotationKind::Redaction => "Square",
        }
    }

    /// Whether this kind marks a region of selected text (has per-line quads).
    pub fn is_text_markup(self) -> bool {
        matches!(
            self,
            AnnotationKind::Highlight
                | AnnotationKind::Strikeout
                | AnnotationKind::Underline
                | AnnotationKind::Squiggly
        )
    }
}

/// Stroke / fill styling for an annotation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StrokeStyle {
    /// Fill colour, `None` for unfilled shapes.
    pub fill: Option<[u8; 4]>,
    /// Outline colour, `None` for unstroked shapes.
    pub stroke: Option<[u8; 4]>,
    /// Outline width in PDF points.
    pub width: f32,
}

impl Default for StrokeStyle {
    fn default() -> Self {
        Self {
            fill: None,
            stroke: Some([255, 0, 0, 255]),
            width: 2.0,
        }
    }
}

impl StrokeStyle {
    pub fn highlight() -> Self {
        Self {
            fill: Some([255, 220, 0, 110]),
            stroke: None,
            width: 0.0,
        }
    }
}

/// A single annotation, in PDF page space (origin bottom-left, units = points).
#[derive(Debug, Clone, PartialEq)]
pub struct Annotation {
    pub id: u64,
    pub kind: AnnotationKind,
    /// Axis-aligned bounds in PDF points. For ink strokes this is the bounding
    /// box of [`Self::points`].
    pub rect: PdfRect,
    /// Freehand points in PDF points; empty for non-ink annotations.
    pub points: Vec<(f32, f32)>,
    /// Text content for free-text annotations.
    pub text: String,
    /// Per-line quads for text-markup annotations, in PDF points, as
    /// `(x1,y1, x2,y2, x3,y3, x4,y4)` in counter-clockwise order starting at
    /// the bottom-left corner. Empty means "use [`Self::rect`]".
    pub quads: Vec<(f32, f32, f32, f32, f32, f32, f32, f32)>,
    pub style: StrokeStyle,
    /// Font size for free-text annotations, in points.
    pub font_size: f32,
    /// Path to the raster image for stamp annotations.
    pub image_path: Option<String>,
    /// Natural size of the stamp image in points (width, height).
    pub image_size: Option<(f32, f32)>,
}

impl Annotation {
    pub fn new(id: u64, kind: AnnotationKind) -> Self {
        Self {
            id,
            kind,
            rect: PdfRect::new_from_values(0.0, 0.0, 0.0, 0.0),
            points: Vec::new(),
            text: String::new(),
            quads: Vec::new(),
            style: StrokeStyle::default(),
            font_size: 14.0,
            image_path: None,
            image_size: None,
        }
    }

    /// Bounds as (left, bottom, right, top) raw floats.
    pub fn bounds_f32(&self) -> (f32, f32, f32, f32) {
        (
            self.rect.left().value,
            self.rect.bottom().value,
            self.rect.right().value,
            self.rect.top().value,
        )
    }

    /// Hit test in PDF space, with a small tolerance for thin marks.
    pub fn contains_point(&self, x: f32, y: f32, tolerance: f32) -> bool {
        let (l, b, r, t) = self.bounds_f32();
        if self.kind == AnnotationKind::Ink {
            // Distance to the polyline, so thin strokes are easy to grab.
            for pair in self.points.windows(2) {
                let (x1, y1) = pair[0];
                let (x2, y2) = pair[1];
                if point_near_segment(x, y, x1, y1, x2, y2, tolerance) {
                    return true;
                }
            }
            false
        } else {
            x >= l - tolerance && x <= r + tolerance && y >= b - tolerance && y <= t + tolerance
        }
    }
}

fn point_near_segment(
    px: f32,
    py: f32,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    tolerance: f32,
) -> bool {
    let dx = x2 - x1;
    let dy = y2 - y1;
    let len_sq = dx * dx + dy * dy;
    let t = if len_sq <= f32::EPSILON {
        0.0
    } else {
        (((px - x1) * dx + (py - y1) * dy) / len_sq).clamp(0.0, 1.0)
    };
    let cx = x1 + t * dx;
    let cy = y1 + t * dy;
    let dist_sq = (px - cx) * (px - cx) + (py - cy) * (py - cy);
    dist_sq <= tolerance * tolerance
}

/// Convert a PDFium colour to an RGBA byte array.
pub fn pdf_color_to_rgba(color: PdfColor) -> [u8; 4] {
    [color.red(), color.green(), color.blue(), color.alpha()]
}

/// Build a [`PdfColor`] from an RGBA byte array.
pub fn rgba_to_pdf_color(rgba: [u8; 4]) -> PdfColor {
    PdfColor::new(rgba[0], rgba[1], rgba[2], rgba[3])
}
