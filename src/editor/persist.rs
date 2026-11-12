//! Small persistence helpers: last-opened file/page and theme preference.
//!
//! State lives in a plain text file (`~/.local/share/pdf-editor/state.txt`)
//! with three lines: last file path, last page (0-based), theme preference.

use std::path::{Path, PathBuf};

use eframe::egui;

fn state_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let dir = PathBuf::from(home).join(".local/share/pdf-editor");
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
