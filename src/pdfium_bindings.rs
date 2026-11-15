//! Locating and loading the vendored PDFium library.

use std::path::PathBuf;

use pdfium_render::prelude::*;

/// The PDFium shared-library filename for the current platform.
#[cfg(target_os = "macos")]
pub const PDFIUM_LIB_NAME: &str = "libpdfium.dylib";
#[cfg(target_os = "linux")]
pub const PDFIUM_LIB_NAME: &str = "libpdfium.so";
#[cfg(target_os = "windows")]
pub const PDFIUM_LIB_NAME: &str = "pdfium.dll";

/// Where to find the vendored pdfium library.
pub fn library_dir() -> &'static str {
    concat!(env!("CARGO_MANIFEST_DIR"), "/vendor/pdfium/lib")
}

/// Try several ways of locating pdfium: vendored copy next to the running
/// executable, vendored copy in the source tree, then a system-wide install.
pub fn load_pdfium_bindings() -> Box<dyn PdfiumLibraryBindings> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    // 1. Next to the executable (works for relocated / bundled builds).
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join(PDFIUM_LIB_NAME));
        candidates.push(dir.join("deps").join(PDFIUM_LIB_NAME));
    }
    // 2. Vendored inside the source tree.
    candidates.push(PathBuf::from(library_dir()).join(PDFIUM_LIB_NAME));

    for path in &candidates {
        if path.exists() {
            match Pdfium::bind_to_library(path) {
                Ok(b) => return b,
                Err(e) => eprintln!("pdfium: failed to bind {}: {e}", path.display()),
            }
        }
    }

    // 3. System library search.
    match Pdfium::bind_to_system_library() {
        Ok(b) => b,
        Err(e) => panic!(
            "Could not load pdfium.\nExpected it at {} or on the system library path.: {e}",
            PathBuf::from(library_dir())
                .join(PDFIUM_LIB_NAME)
                .display()
        ),
    }
}
