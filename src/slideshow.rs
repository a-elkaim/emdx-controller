//! Slideshows (the ePaper app's playlists): several images the display shows in
//! turn, each for the same interval, on its own once it has downloaded them.
//!
//! Sent like any content (see [`crate::cms`]); the manifest lists one entry per
//! slide (`PlaylistContent`, see [`crate::content::playlist_manifest`]).

use anyhow::Result;

use crate::{
    cms::{self, Image},
    content::{self, ContentType, SlideInterval},
    mdc::Device,
};

/// Serve the slideshow from `local_ip`, give the display its manifest, and wait until
/// it has downloaded every image. `jpegs` are shown in this order. Returns `true`
/// when the content player had to be switched to mobile first.
pub async fn send(
    device: &Device,
    local_ip: &str,
    jpegs: Vec<Vec<u8>>,
    interval: SlideInterval,
    name: &str,
) -> Result<bool> {
    let count = jpegs.len();
    let images = jpegs.into_iter().map(Image::jpeg).collect();
    let switched = cms::deliver(
        device,
        local_ip,
        images,
        ContentType::Playlist,
        "slideshow",
        |content_id, slides| {
            content::playlist_manifest(
                content_id,
                name,
                slides,
                interval,
                &content::create_time_now(),
            )
        },
    )
    .await?;
    crate::say!("✅ Slideshow sent: {count} images, each shown for {interval}");
    Ok(switched)
}
