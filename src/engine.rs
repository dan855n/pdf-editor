//! Shared PDF rendering and search helpers used by both the viewer and the
//! editor binaries.
//!
//! Rendering here is rotation-aware: the page's intrinsic `/Rotate` is applied
//! so a rotated page renders upright, with output dimensions swapped for
//! 90°/270° rotations. Callers are expected to lay pages out using the same
//! rotated dimensions (see the binaries' `page_size_px`).

use eframe::egui;
use pdfium_render::prelude::*;

/// One search hit: (page, left, bottom, right, top) in PDF points.
pub type SearchHit = (usize, f32, f32, f32, f32);

/// On-screen (rotation-adjusted) page dimensions in PDF points.
pub fn display_size_pts(
    width_pts: f32,
    height_pts: f32,
    rotation: PdfPageRenderRotation,
) -> (f32, f32) {
    match rotation {
        PdfPageRenderRotation::Degrees90 | PdfPageRenderRotation::Degrees270 => {
            (height_pts, width_pts)
        }
        _ => (width_pts, height_pts),
    }
}

/// Render one page as a small thumbnail, width-clamped to `target_w` pixels.
///
/// `render_annotations` controls whether the PDF's own annotations are baked
/// into the raster: the viewer sets it to `true` (annotations should show),
/// while the editor sets it to `false` (annotations are drawn as overlays).
pub fn render_thumbnail(
    doc: &PdfDocument<'static>,
    page_index: usize,
    target_w: i32,
    render_annotations: bool,
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
    let pw = page.width().value.max(1.0);
    let ph = page.height().value.max(1.0);
    let rotation = page.rotation().unwrap_or(PdfPageRenderRotation::None);
    let (dw, dh) = display_size_pts(pw, ph, rotation);
    let scale = (target_w as f32 / dw).clamp(0.05, 4.0);
    let width = (dw * scale).max(1.0) as i32;
    let height = (dh * scale).max(1.0) as i32;

    let config = PdfRenderConfig::new()
        .set_target_width(width)
        .set_target_height(height)
        .set_reverse_byte_order(true)
        .rotate(rotation, false)
        .render_annotations(render_annotations);

    match page.render_with_config(&config) {
        Ok(bitmap) => {
            let rgba = bitmap.as_rgba_bytes();
            egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], &rgba)
        }
        Err(_) => blank(width, height),
    }
}

/// Render one page into an RGBA image.
///
/// `render_annotations` controls whether the PDF's own annotations are baked
/// into the raster (see [`render_thumbnail`]).
pub fn render_page(
    doc: &PdfDocument<'static>,
    page_index: usize,
    scale: f32,
    render_annotations: bool,
) -> egui::ColorImage {
    let placeholder = |w: f32, h: f32| {
        let w = w.clamp(1.0, 4000.0) as usize;
        let h = h.clamp(1.0, 4000.0) as usize;
        egui::ColorImage::new([w, h], vec![egui::Color32::from_gray(200); w * h])
    };

    let Ok(page) = doc.pages().get(page_index as i32) else {
        return placeholder(612.0 * scale, 792.0 * scale);
    };
    let pw = page.width().value.max(1.0);
    let ph = page.height().value.max(1.0);
    let rotation = page.rotation().unwrap_or(PdfPageRenderRotation::None);
    let (dw, dh) = display_size_pts(pw, ph, rotation);
    let width = (dw * scale).max(1.0) as i32;
    let height = (dh * scale).max(1.0) as i32;

    let config = PdfRenderConfig::new()
        .set_target_width(width)
        .set_target_height(height)
        .set_reverse_byte_order(true)
        .rotate(rotation, false)
        .render_annotations(render_annotations);

    let Ok(bitmap) = page.render_with_config(&config) else {
        return placeholder(width as f32, height as f32);
    };

    let rgba = bitmap.as_rgba_bytes();
    egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], &rgba)
}

/// Search every page's text layer for `query`, returning one bounding-box hit
/// per occurrence.
///
/// Runs off the UI thread; the `thread_safe` feature serializes pdfium access,
/// so it is safe to run alongside page rendering. Scanning is capped to keep
/// very large documents from blocking a worker thread indefinitely.
pub fn search_all_pages(doc: &PdfDocument<'static>, query: &str) -> Vec<SearchHit> {
    const MAX_SEARCH_PAGES: usize = 500;

    let mut hits = Vec::new();
    let options = PdfSearchOptions::new();
    let page_count = doc.pages().len().max(0) as usize;

    for p in 0..page_count.min(MAX_SEARCH_PAGES) {
        let Ok(page) = doc.pages().get(p as i32) else {
            continue;
        };
        let Ok(text) = page.text() else { continue };
        let Ok(search) = text.search(query, &options) else {
            continue;
        };
        for segments in search.iter(PdfSearchDirection::SearchForward) {
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
