//! Image decoding and JPEG encoding.

use std::io::Cursor;

use anyhow::{bail, Context, Result};
use image::{codecs::jpeg::JpegEncoder, DynamicImage, ImageDecoder, ImageReader, RgbImage};

pub const QUALITY_DEFAULT: u8 = 88;

/// Decode any supported format, honoring EXIF orientation.
pub fn decode(bytes: &[u8]) -> Result<DynamicImage> {
    let mut decoder = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()?
        .into_decoder()
        .context("unsupported or corrupt image")?;
    let orientation = decoder.orientation().ok();
    let mut img = DynamicImage::from_decoder(decoder).context("unsupported or corrupt image")?;
    if let Some(o) = orientation {
        img.apply_orientation(o);
    }
    Ok(img)
}

pub fn encode_jpeg_rgb(img: &RgbImage, quality: u8) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, quality).encode_image(img)?;
    Ok(out)
}

/// Encode as JPEG; transparent pixels are flattened onto black.
pub fn encode_jpeg(img: &DynamicImage, quality: u8) -> Result<Vec<u8>> {
    encode_jpeg_rgb(&img.to_rgb8(), quality)
}

/// Normalize an upload (any supported format) to an upright JPEG.
pub fn normalize_jpeg(bytes: &[u8], quality: u8) -> Result<Vec<u8>> {
    encode_jpeg(&decode(bytes)?, quality)
}

/// Rotate by a clockwise quarter turn (`0`, `90`, `180` or `270`) and encode as JPEG.
/// `90` mirrors what the ePaper app does to a portrait panel's thumbnail
/// (`CoreImage.imageRotation90`).
pub fn rotate_jpeg(bytes: &[u8], degrees: u16, quality: u8) -> Result<Vec<u8>> {
    let img = decode(bytes)?;
    let img = match degrees {
        0 => img,
        90 => img.rotate90(),
        180 => img.rotate180(),
        270 => img.rotate270(),
        _ => bail!("rotation must be 0, 90, 180 or 270 degrees"),
    };
    encode_jpeg(&img, quality)
}

pub fn dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
    let img = decode(bytes)?;
    Ok((img.width(), img.height()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    #[test]
    fn round_trips_a_jpeg() {
        let img = RgbImage::from_pixel(64, 32, Rgb([200, 10, 10]));
        let jpeg = encode_jpeg_rgb(&img, 90).unwrap();
        assert_eq!(dimensions(&jpeg).unwrap(), (64, 32));
        assert_eq!(
            dimensions(&normalize_jpeg(&jpeg, 80).unwrap()).unwrap(),
            (64, 32)
        );
        assert!(decode(b"not an image").is_err());
    }

    #[test]
    fn rotates_quarter_turns() {
        // A tall image becomes wide after a 90° turn
        let jpeg = encode_jpeg_rgb(&RgbImage::from_pixel(20, 40, Rgb([1, 2, 3])), 90).unwrap();
        assert_eq!(
            dimensions(&rotate_jpeg(&jpeg, 0, 90).unwrap()).unwrap(),
            (20, 40)
        );
        assert_eq!(
            dimensions(&rotate_jpeg(&jpeg, 90, 90).unwrap()).unwrap(),
            (40, 20)
        );
        assert_eq!(
            dimensions(&rotate_jpeg(&jpeg, 270, 90).unwrap()).unwrap(),
            (40, 20)
        );
        assert!(rotate_jpeg(&jpeg, 45, 90).is_err());
    }

    #[test]
    fn rotates_half_a_turn() {
        // Left half red, right half blue → after 180°, the left half is blue
        let mut img = RgbImage::from_pixel(64, 32, Rgb([220, 0, 0]));
        for x in 32..64 {
            for y in 0..32 {
                img.put_pixel(x, y, Rgb([0, 0, 220]));
            }
        }
        let out = decode(&rotate_jpeg(&encode_jpeg_rgb(&img, 95).unwrap(), 180, 95).unwrap())
            .unwrap()
            .to_rgb8();
        assert_eq!(out.dimensions(), (64, 32));
        let p = out.get_pixel(8, 16).0;
        assert!(p[2] > 150 && p[0] < 80, "left is now blue: {p:?}");
    }
}
