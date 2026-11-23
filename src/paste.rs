//! What a paste sends to the shell when the clipboard holds more than text.
//!
//! Terminals only carry text, so images are handed over the way drag & drop
//! does it: as a file path. A copied image (screenshot, browser "Copy image")
//! is saved as a PNG in the temp directory and its path is pasted; files
//! copied in Explorer / Finder paste their paths. AI agents such as Claude
//! Code and Codex turn a pasted image path into an image attachment.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Pasted images older than this are deleted when a new one is saved.
const KEEP_IMAGES_FOR: Duration = Duration::from_secs(24 * 60 * 60);

/// Clipboard content as the text to paste. Copied files win over text (Finder
/// also offers the bare file name as text), then text, then an image.
pub fn clipboard_text(cb: &mut arboard::Clipboard) -> Option<String> {
    if let Some(paths) = clipboard_files(cb) {
        return Some(join_paths(&paths));
    }
    if let Ok(text) = cb.get_text()
        && !text.is_empty()
    {
        return Some(text);
    }
    clipboard_image(cb).map(|p| quote_path(&p))
}

/// Paths for a clipboard that holds files or an image but no text, or `None`
/// (so a plain Ctrl+V still reaches the program when there's text to paste).
pub fn clipboard_non_text(cb: &mut arboard::Clipboard) -> Option<String> {
    if let Some(paths) = clipboard_files(cb) {
        return Some(join_paths(&paths));
    }
    if cb.get_text().is_ok_and(|t| !t.is_empty()) {
        return None;
    }
    clipboard_image(cb).map(|p| quote_path(&p))
}

fn clipboard_files(cb: &mut arboard::Clipboard) -> Option<Vec<PathBuf>> {
    cb.get().file_list().ok().filter(|paths| !paths.is_empty())
}

fn clipboard_image(cb: &mut arboard::Clipboard) -> Option<PathBuf> {
    let image = cb.get_image().ok()?;
    let dir = std::env::temp_dir().join("termy-paste");
    match save_png(&dir, image.width, image.height, &image.bytes) {
        Ok(path) => Some(path),
        Err(e) => {
            log::warn!("could not save pasted image: {e}");
            None
        }
    }
}

/// Writes RGBA8 pixels as `termy-paste-<ms>.png` in `dir`, removing old ones.
pub fn save_png(dir: &Path, width: usize, height: usize, rgba: &[u8]) -> std::io::Result<PathBuf> {
    if width == 0 || height == 0 || rgba.len() < width * height * 4 {
        return Err(std::io::Error::other("empty or truncated image"));
    }
    std::fs::create_dir_all(dir)?;
    prune_old(dir);
    let ms = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let path = dir.join(format!("termy-paste-{ms}.png"));
    let file = std::io::BufWriter::new(std::fs::File::create(&path)?);
    let mut encoder = png::Encoder::new(file, width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(std::io::Error::other)?;
    writer.write_image_data(&rgba[..width * height * 4]).map_err(std::io::Error::other)?;
    writer.finish().map_err(std::io::Error::other)?;
    Ok(path)
}

fn prune_old(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let ours = name.to_str().is_some_and(|n| n.starts_with("termy-paste-") && n.ends_with(".png"));
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| now.duration_since(t).unwrap_or_default() > KEEP_IMAGES_FOR);
        if ours && old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

pub fn join_paths(paths: &[PathBuf]) -> String {
    paths.iter().map(|p| quote_path(p)).collect::<Vec<_>>().join(" ")
}

/// Quotes a path for the shell only when it needs it, like drag & drop does.
pub fn quote_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    let plain = !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || "/\\:._-+~@,".contains(c));
    if plain {
        s.into_owned()
    } else if cfg!(windows) {
        format!("\"{s}\"")
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_a_readable_png() {
        let dir = tempfile::tempdir().unwrap();
        let rgba = [255u8, 0, 0, 255, 0, 255, 0, 255];
        let path = save_png(dir.path(), 2, 1, &rgba).unwrap();
        let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&path).unwrap()));
        let reader = decoder.read_info().unwrap();
        assert_eq!((reader.info().width, reader.info().height), (2, 1));
    }

    #[test]
    fn rejects_truncated_images() {
        let dir = tempfile::tempdir().unwrap();
        assert!(save_png(dir.path(), 2, 2, &[0; 4]).is_err());
    }

    #[test]
    fn quotes_only_when_needed() {
        assert_eq!(quote_path(Path::new("/tmp/termy-paste-1.png")), "/tmp/termy-paste-1.png");
        let spaced = quote_path(Path::new("/tmp/my shot.png"));
        if cfg!(windows) {
            assert_eq!(spaced, "\"/tmp/my shot.png\"");
        } else {
            assert_eq!(spaced, "'/tmp/my shot.png'");
        }
    }
}
