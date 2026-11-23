//! The identification image shown on a display right after setup: its name in
//! large type and a few details, so several new displays can be told apart.

use ab_glyph::{point, Font, FontRef, PxScale, ScaleFont};
use anyhow::Result;
use image::{Rgb, RgbImage};

use crate::imaging;

/// DejaVu Sans (Bitstream Vera licence, see `assets/fonts/LICENSE-DejaVu.txt`).
static REGULAR: &[u8] = include_bytes!("../assets/fonts/DejaVuSans.ttf");
static BOLD: &[u8] = include_bytes!("../assets/fonts/DejaVuSans-Bold.ttf");

const INK: [u8; 3] = [0, 0, 0];
const MUTED: [u8; 3] = [90, 90, 90];
const PAPER: [u8; 3] = [255, 255, 255];

/// What to print. Rows with an empty value are skipped.
#[derive(Debug, Clone, Default)]
pub struct Info {
    /// The name the display was saved under, in large type.
    pub title: String,
    /// A second line under the title (e.g. the display's own name).
    pub subtitle: Option<String>,
    /// `(label, value)` pairs.
    pub rows: Vec<(String, String)>,
    /// Small line at the bottom.
    pub footer: Option<String>,
}

struct Fonts {
    regular: FontRef<'static>,
    bold: FontRef<'static>,
}

fn fonts() -> Fonts {
    Fonts {
        regular: FontRef::try_from_slice(REGULAR).expect("embedded font is valid"),
        bold: FontRef::try_from_slice(BOLD).expect("embedded font is valid"),
    }
}

/// Width of `text` in pixels at `size`.
fn text_width(font: &FontRef, size: f32, text: &str) -> f32 {
    let f = font.as_scaled(PxScale::from(size));
    let mut width = 0.0;
    let mut prev = None;
    for c in text.chars() {
        let id = f.glyph_id(c);
        if let Some(p) = prev {
            width += f.kern(p, id);
        }
        width += f.h_advance(id);
        prev = Some(id);
    }
    width
}

/// The largest size up to `size` at which `text` fits in `max_width`.
fn fit(font: &FontRef, size: f32, text: &str, max_width: f32) -> f32 {
    let w = text_width(font, size, text);
    if w > max_width && w > 0.0 {
        (size * max_width / w).floor()
    } else {
        size
    }
}

/// Draw `text` with its baseline at `baseline`, starting at `x`.
fn draw_text(
    img: &mut RgbImage,
    font: &FontRef,
    size: f32,
    x: f32,
    baseline: f32,
    text: &str,
    color: [u8; 3],
) {
    let scale = PxScale::from(size);
    let f = font.as_scaled(scale);
    let (w, h) = img.dimensions();
    let mut caret = x;
    let mut prev = None;
    for c in text.chars() {
        let id = f.glyph_id(c);
        if let Some(p) = prev {
            caret += f.kern(p, id);
        }
        let glyph = id.with_scale_and_position(scale, point(caret, baseline));
        caret += f.h_advance(id);
        prev = Some(id);
        let Some(outline) = font.outline_glyph(glyph) else {
            continue;
        };
        let bounds = outline.px_bounds();
        outline.draw(|gx, gy, coverage| {
            let px = bounds.min.x as i64 + i64::from(gx);
            let py = bounds.min.y as i64 + i64::from(gy);
            if px < 0 || py < 0 || px >= i64::from(w) || py >= i64::from(h) {
                return;
            }
            let p = img.get_pixel_mut(px as u32, py as u32);
            let a = coverage.clamp(0.0, 1.0);
            for (i, c) in color.iter().enumerate() {
                p.0[i] = (f32::from(p.0[i]) * (1.0 - a) + f32::from(*c) * a).round() as u8;
            }
        });
    }
}

fn fill_rect(img: &mut RgbImage, x: u32, y: u32, w: u32, h: u32, color: [u8; 3]) {
    let (iw, ih) = img.dimensions();
    for py in y..(y + h).min(ih) {
        for px in x..(x + w).min(iw) {
            img.put_pixel(px, py, Rgb(color));
        }
    }
}

/// Render the image at the display's frame size (e.g. 1440×2560 portrait).
pub fn render(info: &Info, (w, h): (u32, u32)) -> RgbImage {
    let fonts = fonts();
    let mut img = RgbImage::from_pixel(w, h, Rgb(PAPER));
    // Everything is sized from the short side (1440 on an EM32DX)
    let s = w.min(h) as f32 / 1440.0;
    let margin = 120.0 * s;
    let inner = w as f32 - 2.0 * margin;
    let landscape = w > h;

    // Title, centred
    let mut y = margin + 40.0 * s;
    let title_size = fit(&fonts.bold, 210.0 * s, &info.title, inner);
    y += title_size * 0.95;
    let tw = text_width(&fonts.bold, title_size, &info.title);
    draw_text(
        &mut img,
        &fonts.bold,
        title_size,
        (w as f32 - tw) / 2.0,
        y,
        &info.title,
        INK,
    );
    if let Some(sub) = info.subtitle.as_deref().filter(|s| !s.is_empty()) {
        let size = fit(&fonts.regular, 72.0 * s, sub, inner);
        y += size * 1.6;
        let sw = text_width(&fonts.regular, size, sub);
        draw_text(
            &mut img,
            &fonts.regular,
            size,
            (w as f32 - sw) / 2.0,
            y,
            sub,
            MUTED,
        );
    }

    // Divider
    y += 90.0 * s;
    fill_rect(
        &mut img,
        margin as u32,
        y as u32,
        inner as u32,
        (8.0 * s).max(2.0) as u32,
        INK,
    );
    y += 70.0 * s;

    // Detail rows: label above value in portrait, side by side in landscape
    let label_size = 52.0 * s;
    let value_size = 76.0 * s;
    let rows = info.rows.iter().filter(|(_, v)| !v.is_empty());
    for (label, value) in rows {
        if landscape {
            let col = margin + inner * 0.32;
            let vs = fit(&fonts.bold, value_size, value, w as f32 - margin - col);
            y += value_size;
            draw_text(
                &mut img,
                &fonts.regular,
                label_size,
                margin,
                y,
                label,
                MUTED,
            );
            draw_text(&mut img, &fonts.bold, vs, col, y, value, INK);
            y += value_size * 0.55;
        } else {
            y += label_size;
            draw_text(
                &mut img,
                &fonts.regular,
                label_size,
                margin,
                y,
                label,
                MUTED,
            );
            let vs = fit(&fonts.bold, value_size, value, inner);
            y += vs * 1.15;
            draw_text(&mut img, &fonts.bold, vs, margin, y, value, INK);
            y += value_size * 0.75;
        }
        if y > h as f32 - margin - 120.0 * s {
            break;
        }
    }

    if let Some(footer) = info.footer.as_deref().filter(|s| !s.is_empty()) {
        let size = fit(&fonts.regular, 44.0 * s, footer, inner);
        let fw = text_width(&fonts.regular, size, footer);
        draw_text(
            &mut img,
            &fonts.regular,
            size,
            (w as f32 - fw) / 2.0,
            h as f32 - margin,
            footer,
            MUTED,
        );
    }
    img
}

/// [`render`] encoded as JPEG, ready to send.
pub fn render_jpeg(info: &Info, frame: (u32, u32)) -> Result<Vec<u8>> {
    imaging::encode_jpeg_rgb(&render(info, frame), 92)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Info {
        Info {
            title: "devteam".into(),
            subtitle: Some("Samsung EMDX 743D".into()),
            rows: vec![
                ("IP address".into(), "192.168.1.34".into()),
                ("Model".into(), "EM32DX".into()),
                ("Empty".into(), String::new()),
            ],
            footer: Some("Set up with emdx".into()),
        }
    }

    fn ink_rows(img: &RgbImage) -> usize {
        (0..img.height())
            .filter(|&y| (0..img.width()).any(|x| img.get_pixel(x, y).0[0] < 128))
            .count()
    }

    #[test]
    fn renders_at_frame_size_with_text() {
        for frame in [(1440, 2560), (2560, 1440)] {
            let img = render(&sample(), frame);
            assert_eq!(img.dimensions(), frame);
            assert!(ink_rows(&img) > 200, "text was drawn");
        }
    }

    #[test]
    fn long_titles_shrink_to_fit() {
        let size = fit(&fonts().bold, 210.0, &"W".repeat(60), 1200.0);
        assert!(size < 210.0);
        assert!(text_width(&fonts().bold, size, &"W".repeat(60)) <= 1200.5);
        assert_eq!(fit(&fonts().bold, 210.0, "ab", 1200.0), 210.0);
    }

    #[test]
    fn encodes_a_jpeg() {
        let jpeg = render_jpeg(&sample(), (1440, 2560)).unwrap();
        assert_eq!(imaging::dimensions(&jpeg).unwrap(), (1440, 2560));
    }
}
