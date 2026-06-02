//! Cover retrieval: image file in the folder or embedded image from the tags.

use lofty::file::TaggedFileExt;
use std::path::{Path, PathBuf};

const COVER_STEMS: &[&str] = &["cover", "folder", "front", "albumart", "album", "artist"];
const IMG_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "bmp", "gif"];

fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMG_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Searches the folder for a cover image file (preferring known names).
pub fn find_cover_file(dir: &Path) -> Option<PathBuf> {
    let images: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_image(p))
        .collect();

    for stem in COVER_STEMS {
        if let Some(p) = images.iter().find(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_ascii_lowercase() == *stem)
                .unwrap_or(false)
        }) {
            return Some(p.clone());
        }
    }

    let mut images = images;
    images.sort();
    images.into_iter().next()
}

/// Reads an embedded cover from the tags of an audio file.
pub fn embedded_cover(file: &Path) -> Option<Vec<u8>> {
    let tagged = lofty::read_from_path(file).ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    let pic = tag.pictures().first()?;
    Some(pic.data().to_vec())
}
