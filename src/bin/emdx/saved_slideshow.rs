//! The slideshow prepared for each saved display, kept on disk so it can be edited
//! again later (in `emdx serve`): `<settings folder>/slideshows/<display>/`, with
//! `slideshow.json` (name, interval, image order) and the images as JPEG files.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use emdx_controller::{content::SlideInterval, imaging};

const MANIFEST: &str = "slideshow.json";

/// What is saved: images are listed by id, in the order shown
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Saved {
    pub name: String,
    /// Minutes between images (5 to 1440)
    pub interval: u32,
    pub images: Vec<String>,
}

/// One slide of a save: an image already saved (by id), or a new upload
pub(crate) enum Slide {
    Keep(String),
    New(Vec<u8>),
}

/// `<settings folder>/slideshows`
pub(crate) fn root(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .map(|p| p.join("slideshows"))
        .unwrap_or_else(|| PathBuf::from("slideshows"))
}

/// Display names are 1-32 of `A-Z a-z 0-9 _ -` (config::validate_name), so they are
/// safe folder names
fn dir(root: &Path, display: &str) -> Result<PathBuf> {
    crate::config::validate_name(display)?;
    Ok(root.join(display))
}

/// Image ids are UUIDs: nothing else is read, so no path can escape the folder
fn valid_id(id: &str) -> bool {
    id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

pub(crate) fn load(root: &Path, display: &str) -> Result<Option<Saved>> {
    let path = dir(root, display)?.join(MANIFEST);
    match fs::read(&path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes)
                .with_context(|| format!("reading {}", path.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// One saved image (a JPEG)
pub(crate) fn image(root: &Path, display: &str, id: &str) -> Result<Vec<u8>> {
    if !valid_id(id) {
        bail!("not an image id: {id:?}");
    }
    let path = dir(root, display)?.join(format!("{id}.jpg"));
    fs::read(&path).with_context(|| format!("reading {}", path.display()))
}

/// Replace the saved slideshow: new uploads are converted to JPEG, images no longer
/// listed are deleted. Returns what was saved.
pub(crate) fn save(
    root: &Path,
    display: &str,
    name: &str,
    interval: SlideInterval,
    slides: Vec<Slide>,
) -> Result<Saved> {
    let dir = dir(root, display)?;
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut images = Vec::new();
    for slide in slides {
        match slide {
            Slide::Keep(id) => {
                if !valid_id(&id) || !dir.join(format!("{id}.jpg")).is_file() {
                    bail!("no saved image {id:?}: reload the page");
                }
                images.push(id);
            }
            Slide::New(bytes) => {
                let jpeg = imaging::normalize_jpeg(&bytes, imaging::QUALITY_DEFAULT)
                    .context("reading an added image")?;
                let id = uuid::Uuid::new_v4().to_string();
                fs::write(dir.join(format!("{id}.jpg")), jpeg)
                    .with_context(|| format!("saving an image in {}", dir.display()))?;
                images.push(id);
            }
        }
    }
    let saved = Saved {
        name: name.trim().chars().take(64).collect(),
        interval: interval.minutes(),
        images,
    };
    // Write then rename: a crash never leaves a half-written list
    let tmp = dir.join(format!("{MANIFEST}.tmp"));
    fs::write(&tmp, serde_json::to_vec_pretty(&saved)?)?;
    fs::rename(&tmp, dir.join(MANIFEST))?;
    // Images that are no longer part of it
    for entry in fs::read_dir(&dir)?.flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        if let Some(id) = file.strip_suffix(".jpg") {
            if valid_id(id) && !saved.images.iter().any(|i| i == id) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    Ok(saved)
}

/// The JPEGs to send, in order
pub(crate) fn jpegs(root: &Path, display: &str, saved: &Saved) -> Result<Vec<Vec<u8>>> {
    saved
        .images
        .iter()
        .map(|id| image(root, display, id))
        .collect()
}

/// Follow a display rename
pub(crate) fn rename(root: &Path, from: &str, to: &str) -> Result<()> {
    let (from, to) = (dir(root, from)?, dir(root, to)?);
    if from.exists() && !to.exists() {
        fs::rename(&from, &to).with_context(|| format!("moving {}", from.display()))?;
    }
    Ok(())
}

/// The display was removed from emdx
pub(crate) fn remove(root: &Path, display: &str) -> Result<()> {
    let dir = dir(root, display)?;
    match fs::remove_dir_all(&dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", dir.display()))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png() -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        image::RgbImage::new(8, 4)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    #[test]
    fn saves_reorders_and_cleans_up() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let every = SlideInterval::from_minutes(30).unwrap();
        assert_eq!(load(root, "lobby").unwrap(), None);

        let first = save(
            root,
            "lobby",
            "Menu",
            every,
            vec![Slide::New(png()), Slide::New(png())],
        )
        .unwrap();
        assert_eq!(first.images.len(), 2);
        assert_eq!(load(root, "lobby").unwrap(), Some(first.clone()));
        assert!(imaging::dimensions(&image(root, "lobby", &first.images[0]).unwrap()).is_ok());

        // Reorder, drop the second, add one
        let (a, b) = (first.images[0].clone(), first.images[1].clone());
        let next = save(
            root,
            "lobby",
            "Menu",
            every,
            vec![Slide::New(png()), Slide::Keep(a.clone())],
        )
        .unwrap();
        assert_eq!(next.images[1], a);
        assert!(image(root, "lobby", &b).is_err(), "dropped image deleted");
        assert_eq!(jpegs(root, "lobby", &next).unwrap().len(), 2);

        // Follows a rename, goes away with the display
        rename(root, "lobby", "hall").unwrap();
        assert_eq!(load(root, "hall").unwrap(), Some(next));
        remove(root, "hall").unwrap();
        assert_eq!(load(root, "hall").unwrap(), None);
    }

    #[test]
    fn refuses_unknown_ids_and_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let every = SlideInterval::from_minutes(30).unwrap();
        assert!(image(tmp.path(), "lobby", "../config").is_err());
        assert!(image(tmp.path(), "../x", "00000000-0000-0000-0000-000000000000").is_err());
        assert!(save(
            tmp.path(),
            "lobby",
            "",
            every,
            vec![Slide::Keep("nope".into())]
        )
        .is_err());
    }
}
