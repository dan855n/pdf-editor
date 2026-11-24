//! Small persistence helpers: last-opened file/page and theme preference.
//!
//! State lives in a plain text file — `~/.local/share/pdf-editor/state.txt`
//! on macOS/Linux, `%LOCALAPPDATA%\pdf-editor\state.txt` on Windows — with
//! three lines: last file path, last page (0-based), theme preference.

use std::path::{Path, PathBuf};

use eframe::egui;

fn state_path() -> PathBuf {
    let dir = if cfg!(target_os = "windows") {
        std::env::var("LOCALAPPDATA")
            .or_else(|_| std::env::var("APPDATA"))
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
            })
            .join("pdf-editor")
    } else {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join(".local/share/pdf-editor")
    };
    let _ = std::fs::create_dir_all(&dir);
    dir.join("state.txt")
}

/// Last file path (absolute) and last page (0-based), if the file still exists.
pub fn read_last_state() -> Option<(PathBuf, usize)> {
    let txt = std::fs::read_to_string(state_path()).ok()?;
    let mut lines = txt.lines();
    let path = PathBuf::from(lines.next()?);
    let page = lines
        .next()
        .and_then(|l| l.trim().parse().ok())
        .unwrap_or(0);
    path.exists().then_some((path, page))
}

/// Record the file just opened, resetting the saved page to 0.
pub fn save_last_file(path: &Path) {
    save_state(&path.to_string_lossy(), 0);
}

/// Record the current page, preserving the saved file and theme.
pub fn save_last_position(page: usize) {
    let path = std::fs::read_to_string(state_path())
        .ok()
        .and_then(|t| t.lines().next().map(str::to_owned))
        .unwrap_or_default();
    save_state(&path, page);
}

pub fn read_theme() -> Option<egui::ThemePreference> {
    let txt = std::fs::read_to_string(state_path()).ok()?;
    match txt.lines().nth(2)?.trim() {
        "dark" => Some(egui::ThemePreference::Dark),
        "light" => Some(egui::ThemePreference::Light),
        "system" => Some(egui::ThemePreference::System),
        _ => None,
    }
}

pub fn save_theme(theme: egui::ThemePreference) {
    let name = match theme {
        egui::ThemePreference::Dark => "dark",
        egui::ThemePreference::Light => "light",
        egui::ThemePreference::System => "system",
    };
    let existing = std::fs::read_to_string(state_path()).unwrap_or_default();
    let mut lines = existing.lines();
    let file = lines.next().unwrap_or("");
    let page = lines.next().unwrap_or("0");
    let _ = std::fs::write(state_path(), format!("{file}\n{page}\n{name}\n"));
}

/// Rewrite the file/page lines, preserving the theme line if present.
fn save_state(path: &str, page: usize) {
    let theme = read_theme().map(|t| match t {
        egui::ThemePreference::Dark => "dark",
        egui::ThemePreference::Light => "light",
        egui::ThemePreference::System => "system",
    });
    let theme_line = theme.map_or_else(String::new, |t| format!("\n{t}"));
    let _ = std::fs::write(state_path(), format!("{path}\n{page}{theme_line}\n"));
}

/// Directory storing reusable user signatures (`<app_data_dir>/signatures/`).
pub fn signatures_dir() -> PathBuf {
    let dir = state_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("signatures");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// A saved reusable signature stored in [`signatures_dir()`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedSignature {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
}

/// List all saved signatures in [`signatures_dir()`], sorted by name.
pub fn list_signatures() -> Vec<SavedSignature> {
    list_signatures_in(&signatures_dir())
}

fn list_signatures_in(dir: &Path) -> Vec<SavedSignature> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("png") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let id = stem.to_string();
        let name_path = dir.join(format!("{id}.txt"));
        let name = std::fs::read_to_string(&name_path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| id.clone());
        let (width, height) = image::image_dimensions(&path).unwrap_or((240, 80));
        out.push(SavedSignature {
            id,
            name,
            path,
            width,
            height,
        });
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

fn next_signature_id(dir: &Path) -> String {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut candidate = format!("sig-{nonce}");
    let mut counter = 0usize;
    while dir.join(format!("{candidate}.png")).exists() {
        counter += 1;
        candidate = format!("sig-{nonce}-{counter}");
    }
    candidate
}

/// Save a transparent RGBA image as a reusable signature.
pub fn save_signature_from_rgba(
    name: &str,
    rgba: &image::RgbaImage,
) -> Result<SavedSignature, String> {
    save_signature_from_rgba_in(&signatures_dir(), name, rgba)
}

fn save_signature_from_rgba_in(
    dir: &Path,
    name: &str,
    rgba: &image::RgbaImage,
) -> Result<SavedSignature, String> {
    let _ = std::fs::create_dir_all(dir);
    let id = next_signature_id(dir);
    let png_path = dir.join(format!("{id}.png"));
    let txt_path = dir.join(format!("{id}.txt"));
    let display_name = if name.trim().is_empty() {
        "Signature".to_string()
    } else {
        name.trim().to_string()
    };
    rgba.save(&png_path)
        .map_err(|e| format!("could not save signature PNG: {e}"))?;
    let _ = std::fs::write(&txt_path, format!("{display_name}\n"));
    let (width, height) = rgba.dimensions();
    Ok(SavedSignature {
        id,
        name: display_name,
        path: png_path,
        width,
        height,
    })
}

/// Render freehand signature strokes (in canvas pixel coordinates) onto a
/// tightly cropped transparent RGBA PNG and save it to [`signatures_dir()`].
pub fn save_signature_from_strokes(
    name: &str,
    strokes: &[Vec<(f32, f32)>],
    color: [u8; 4],
    stroke_width: f32,
) -> Result<SavedSignature, String> {
    save_signature_from_strokes_in(&signatures_dir(), name, strokes, color, stroke_width)
}

fn save_signature_from_strokes_in(
    dir: &Path,
    name: &str,
    strokes: &[Vec<(f32, f32)>],
    color: [u8; 4],
    stroke_width: f32,
) -> Result<SavedSignature, String> {
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    let mut has_point = false;
    for stroke in strokes {
        for &(x, y) in stroke {
            has_point = true;
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
    }
    if !has_point {
        return Err("Draw a signature first before saving.".into());
    }

    let pad = (stroke_width * 2.0).max(8.0);
    let scale = 2.0f32;
    let raw_w = ((max_x - min_x) + pad * 2.0).max(16.0);
    let raw_h = ((max_y - min_y) + pad * 2.0).max(16.0);
    let img_w = (raw_w * scale).ceil().min(2048.0) as u32;
    let img_h = (raw_h * scale).ceil().min(2048.0) as u32;
    let mut rgba = image::RgbaImage::from_pixel(img_w, img_h, image::Rgba([0, 0, 0, 0]));

    let radius = (stroke_width * 0.5 * scale).max(1.0);
    let r_ceil = radius.ceil() as i32 + 1;
    let pixel = image::Rgba([color[0], color[1], color[2], 255]);

    let mut stamp_disc = |cx: f32, cy: f32| {
        let x0 = (cx.floor() as i32 - r_ceil).max(0);
        let x1 = (cx.ceil() as i32 + r_ceil).min(img_w as i32 - 1);
        let y0 = (cy.floor() as i32 - r_ceil).max(0);
        let y1 = (cy.ceil() as i32 + r_ceil).min(img_h as i32 - 1);
        for py in y0..=y1 {
            for px in x0..=x1 {
                let dx = px as f32 + 0.5 - cx;
                let dy = py as f32 + 0.5 - cy;
                let dist = (dx * dx + dy * dy).sqrt();
                if dist <= radius + 0.75 {
                    let alpha = ((radius + 0.75 - dist).clamp(0.0, 1.0) * 255.0) as u8;
                    let existing = rgba.get_pixel_mut(px as u32, py as u32);
                    if alpha > existing[3] {
                        *existing = image::Rgba([pixel[0], pixel[1], pixel[2], alpha]);
                    }
                }
            }
        }
    };

    for stroke in strokes {
        if stroke.is_empty() {
            continue;
        }
        if stroke.len() == 1 {
            let cx = (stroke[0].0 - min_x + pad) * scale;
            let cy = (stroke[0].1 - min_y + pad) * scale;
            stamp_disc(cx, cy);
            continue;
        }
        for pair in stroke.windows(2) {
            let x0 = (pair[0].0 - min_x + pad) * scale;
            let y0 = (pair[0].1 - min_y + pad) * scale;
            let x1 = (pair[1].0 - min_x + pad) * scale;
            let y1 = (pair[1].1 - min_y + pad) * scale;
            let dist = ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt();
            let steps = (dist / 0.75).ceil().max(1.0) as usize;
            for s in 0..=steps {
                let t = s as f32 / steps as f32;
                stamp_disc(x0 + (x1 - x0) * t, y0 + (y1 - y0) * t);
            }
        }
    }

    save_signature_from_rgba_in(dir, name, &rgba)
}

/// Import an image file (PNG/JPG) into the reusable signature library.
pub fn save_signature_from_image_file(
    name: &str,
    src_path: &Path,
) -> Result<SavedSignature, String> {
    let img = image::open(src_path)
        .map_err(|e| format!("could not open signature image {}: {e}", src_path.display()))?;
    let (w, h) = (img.width(), img.height());
    let scaled = if w > 1200 || h > 600 {
        img.resize(1200, 600, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };
    let rgba = scaled.to_rgba8();
    let fallback_name = src_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Imported Signature");
    let display = if name.trim().is_empty() {
        fallback_name
    } else {
        name
    };
    save_signature_from_rgba(display, &rgba)
}

/// Delete a saved signature by id.
pub fn delete_signature(id: &str) -> Result<(), String> {
    delete_signature_in(&signatures_dir(), id)
}

fn delete_signature_in(dir: &Path, id: &str) -> Result<(), String> {
    let png = dir.join(format!("{id}.png"));
    let txt = dir.join(format!("{id}.txt"));
    if png.exists() {
        std::fs::remove_file(&png).map_err(|e| format!("could not delete signature: {e}"))?;
    }
    let _ = std::fs::remove_file(&txt);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_store_save_list_and_delete() {
        let dir = std::env::temp_dir().join(format!(
            "pdf-editor-sig-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);

        let strokes = vec![vec![(10.0, 20.0), (80.0, 25.0), (140.0, 15.0)]];
        let saved = save_signature_from_strokes_in(
            &dir,
            "My Signature",
            &strokes,
            [10, 20, 120, 255],
            3.0,
        )
        .unwrap();
        assert_eq!(saved.name, "My Signature");
        assert!(saved.path.exists());
        assert!(saved.width > 0 && saved.height > 0);

        let list = list_signatures_in(&dir);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, saved.id);
        assert_eq!(list[0].name, "My Signature");

        delete_signature_in(&dir, &saved.id).unwrap();
        assert!(list_signatures_in(&dir).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
