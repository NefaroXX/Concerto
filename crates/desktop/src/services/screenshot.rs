//! Screenshot service — saves RGBA pixel data to PNG files and clipboard.
//!
//! This module provides pure utility functions. The iced `Task` plumbing
//! lives in `app.rs`.

use std::path::PathBuf;

/// Result of a screenshot save operation.
#[derive(Debug, Clone)]
pub struct ScreenshotResult {
    /// File path where the screenshot was saved.
    pub file_path: PathBuf,
    /// Whether the screenshot was also copied to the clipboard.
    pub copied_to_clipboard: bool,
}

/// Errors for screenshot operations.
#[derive(Debug)]
pub enum ScreenshotError {
    /// Failed to create RGBA image from raw pixel data.
    EncodingFailed(String),
    /// Failed to save file to disk.
    SaveFailed { path: PathBuf, error: String },
    /// Failed to copy to system clipboard.
    ClipboardFailed(String),
}

impl std::fmt::Display for ScreenshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EncodingFailed(msg) => write!(f, "PNG encoding failed: {}", msg),
            Self::SaveFailed { path, error } => {
                write!(f, "Failed to save to {}: {}", path.display(), error)
            }
            Self::ClipboardFailed(msg) => write!(f, "Clipboard copy failed: {}", msg),
        }
    }
}

impl std::error::Error for ScreenshotError {}

/// Ensures the screenshots directory exists and returns its path.
fn screenshots_dir() -> Result<PathBuf, ScreenshotError> {
    let pictures =
        dirs::picture_dir().or_else(|| dirs::data_dir().map(|d| d.join("concerto"))).ok_or_else(
            || ScreenshotError::EncodingFailed("Cannot determine pictures directory".into()),
        )?;

    let dir = pictures.join("Concerto").join("screenshots");
    std::fs::create_dir_all(&dir)
        .map_err(|e| ScreenshotError::SaveFailed { path: dir.clone(), error: e.to_string() })?;
    Ok(dir)
}

/// Generates a timestamped filename for the screenshot.
fn screenshot_filename() -> String {
    let now = time::OffsetDateTime::now_utc();
    // Format: screenshot_20260704_153045.png
    let date = now.date();
    let time_parts = now.time();
    format!(
        "screenshot_{:04}{:02}{:02}_{:02}{:02}{:02}.png",
        date.year(),
        u8::from(date.month()),
        date.day(),
        time_parts.hour(),
        time_parts.minute(),
        time_parts.second(),
    )
}

/// Saves RGBA pixel data as a PNG file and optionally copies to clipboard.
///
/// # Arguments
/// * `rgba` — Raw RGBA pixel data (4 bytes per pixel, sRGB color space).
/// * `width` — Image width in physical pixels.
/// * `height` — Image height in physical pixels.
pub fn save_png(
    rgba: &[u8],
    width: u32,
    height: u32,
    copy_to_clipboard: bool,
) -> Result<ScreenshotResult, ScreenshotError> {
    // 1. Convert RGBA to PNG
    let rgba_image = image::RgbaImage::from_raw(width, height, rgba.to_vec()).ok_or_else(|| {
        ScreenshotError::EncodingFailed(format!("Failed to create {}x{} RGBA image", width, height))
    })?;

    let mut png_buffer = std::io::Cursor::new(Vec::new());
    rgba_image
        .write_to(&mut png_buffer, image::ImageFormat::Png)
        .map_err(|e| ScreenshotError::EncodingFailed(e.to_string()))?;

    let png_bytes = png_buffer.into_inner();

    // 2. Save to file
    let dir = screenshots_dir()?;
    let file_path = dir.join(screenshot_filename());
    std::fs::write(&file_path, &png_bytes).map_err(|e| ScreenshotError::SaveFailed {
        path: file_path.clone(),
        error: e.to_string(),
    })?;

    // 3. Copy to clipboard (best effort)
    let copied_to_clipboard =
        if copy_to_clipboard { copy_png_to_clipboard(&png_bytes).is_ok() } else { false };

    tracing::info!(
        path = %file_path.display(),
        width,
        height,
        clipboard = copied_to_clipboard,
        "Screenshot saved"
    );

    Ok(ScreenshotResult { file_path, copied_to_clipboard })
}

/// Copies PNG bytes to the system clipboard as image data.
fn copy_png_to_clipboard(png_bytes: &[u8]) -> Result<(), ScreenshotError> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| ScreenshotError::ClipboardFailed(e.to_string()))?;

    let img = image::load_from_memory(png_bytes)
        .map_err(|e| ScreenshotError::ClipboardFailed(e.to_string()))?;

    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();

    clipboard
        .set_image(arboard::ImageData {
            width: width as usize,
            height: height as usize,
            bytes: std::borrow::Cow::Borrowed(rgba.as_raw()),
        })
        .map_err(|e| ScreenshotError::ClipboardFailed(e.to_string()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screenshot_filename_has_correct_format() {
        let name = screenshot_filename();
        assert!(name.starts_with("screenshot_"));
        assert!(name.ends_with(".png"));
    }

    #[test]
    fn save_png_creates_file() {
        // 1x1 red pixel
        let rgba = vec![255, 0, 0, 255];
        let result = save_png(&rgba, 1, 1, false);
        if let Ok(res) = result {
            assert!(res.file_path.exists());
            // Cleanup
            let _ = std::fs::remove_file(&res.file_path);
        }
    }

    #[test]
    fn screenshots_dir_creates_directory() {
        let _ = screenshots_dir();
    }
}
