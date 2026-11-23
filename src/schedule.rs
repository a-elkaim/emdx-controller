//! Schedules (the ePaper app's schedule content): the image the display shows changes
//! at set times of day, every day, on its own once it has downloaded the images.
//!
//! Sent like any content (see [`crate::cms`]). The manifest has one
//! block per start time (`ScheduleContent`, see [`crate::content::schedule_manifest`]).
//! As in the app, an entry is a start time and one image, with no duration: it stays
//! until the next entry starts.

use anyhow::Result;

use crate::{
    cms::{self, Image},
    content::{self, ContentType, ScheduleBlock, StartTime},
    mdc::Device,
};

/// The image to show from `start`
#[derive(Debug, Clone)]
pub struct Entry<T = Vec<u8>> {
    pub start: StartTime,
    /// A JPEG when sent; anything (a path, a raw upload) while only being checked
    pub image: T,
}

/// The app's rules: at least one entry, and no two entries start at the same time
pub fn check<T>(entries: &[Entry<T>]) -> Result<(), String> {
    if entries.is_empty() {
        return Err("a schedule needs at least one start time".into());
    }
    let mut seen = std::collections::HashSet::new();
    for e in entries {
        if !seen.insert(e.start) {
            return Err(format!(
                "two entries start at {}: each start time can be used once",
                e.start
            ));
        }
    }
    Ok(())
}

/// Serve the schedule from `local_ip`, give the display its manifest, and wait until
/// it has downloaded every image. Returns `true` when the content player had to be
/// switched to mobile first.
pub async fn send(
    device: &Device,
    local_ip: &str,
    entries: Vec<Entry>,
    name: &str,
) -> Result<bool> {
    check(&entries).map_err(anyhow::Error::msg)?;
    // An image used at several times is sent once (in the app it is one content,
    // with one id and one folder on the display)
    let mut jpegs: Vec<Vec<u8>> = Vec::new();
    let plan: Vec<(StartTime, usize)> = entries
        .into_iter()
        .map(|e| {
            let index = jpegs.iter().position(|j| *j == e.image).unwrap_or_else(|| {
                jpegs.push(e.image);
                jpegs.len() - 1
            });
            (e.start, index)
        })
        .collect();
    let (entries, images) = (plan.len(), jpegs.len());
    let switched = cms::deliver(
        device,
        local_ip,
        jpegs.into_iter().map(Image::jpeg).collect(),
        ContentType::Schedule,
        "schedule",
        |content_id, slides| {
            let blocks: Vec<ScheduleBlock> = plan
                .iter()
                .map(|(start, index)| ScheduleBlock {
                    start: *start,
                    slide: slides[*index].clone(),
                })
                .collect();
            content::schedule_manifest(content_id, name, &blocks, &content::create_time_now())
        },
    )
    .await?;
    crate::say!("✅ Schedule sent: {entries} start times, {images} images");
    Ok(switched)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(at: &str) -> Entry<()> {
        Entry {
            start: StartTime::parse(at).unwrap(),
            image: (),
        }
    }

    #[test]
    fn checks_the_entries() {
        assert!(check(&[entry("8:00"), entry("12:00")]).is_ok());
        assert!(check(&[entry("8:00")]).is_ok());
        assert!(check::<()>(&[]).is_err());
        assert!(check(&[entry("8:00"), entry("08:00")]).is_err());
    }
}
