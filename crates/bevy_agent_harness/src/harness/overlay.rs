//! The mouse overlay a recording can draw onto its frames: a cursor arrow, a
//! highlight while a button is held, and an expanding ring where a button was
//! pressed. It is painted onto the captured pixels on the encoder thread, so
//! nothing is added to the game's world and screenshots stay clean.

use image::RgbaImage;

/// How long a click ripple stays visible, in seconds of video.
pub(crate) const RIPPLE_SECS: f32 = 0.5;

/// A click ripple. `at` is a fraction of the frame size (0..1, origin top-left).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Ripple {
    pub at: [f32; 2],
    /// Seconds of video since the press.
    pub age: f32,
}

/// What to draw on one frame. Positions are fractions of the frame size.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Overlay {
    pub cursor: Option<[f32; 2]>,
    /// A mouse button is down.
    pub held: bool,
    pub ripples: Vec<Ripple>,
}

const WHITE: [f32; 3] = [255.0, 255.0, 255.0];
const BLACK: [f32; 3] = [0.0, 0.0, 0.0];

/// The arrow, tip at the origin, in pixels of a 720-pixel-high frame.
const ARROW: [[f32; 2]; 7] = [
    [0.0, 0.0],
    [0.0, 17.0],
    [4.2, 13.2],
    [7.2, 20.0],
    [9.8, 18.9],
    [6.8, 12.2],
    [12.2, 12.2],
];

fn blend(image: &mut RgbaImage, x: i64, y: i64, color: [f32; 3], alpha: f32) {
    if alpha <= 0.0 || x < 0 || y < 0 || x >= image.width() as i64 || y >= image.height() as i64 {
        return;
    }
    let alpha = alpha.min(1.0);
    let pixel = image.get_pixel_mut(x as u32, y as u32);
    for c in 0..3 {
        pixel[c] = (pixel[c] as f32 * (1.0 - alpha) + color[c] * alpha).round() as u8;
    }
    pixel[3] = pixel[3].max((alpha * 255.0) as u8);
}

/// Paint every pixel within `reach` of `center`, with the coverage `shade` gives
/// for its distance from the centre.
fn radial(
    image: &mut RgbaImage,
    center: [f32; 2],
    reach: f32,
    color: [f32; 3],
    shade: impl Fn(f32) -> f32,
) {
    let (x0, x1) = (
        (center[0] - reach).floor() as i64,
        (center[0] + reach).ceil() as i64,
    );
    let (y0, y1) = (
        (center[1] - reach).floor() as i64,
        (center[1] + reach).ceil() as i64,
    );
    for y in y0..=y1 {
        for x in x0..=x1 {
            let (dx, dy) = (x as f32 + 0.5 - center[0], y as f32 + 0.5 - center[1]);
            blend(image, x, y, color, shade((dx * dx + dy * dy).sqrt()));
        }
    }
}

fn disc(image: &mut RgbaImage, center: [f32; 2], radius: f32, color: [f32; 3], alpha: f32) {
    radial(image, center, radius + 1.0, color, |d| {
        (radius + 0.5 - d).clamp(0.0, 1.0) * alpha
    });
}

fn ring(
    image: &mut RgbaImage,
    center: [f32; 2],
    radius: f32,
    thickness: f32,
    color: [f32; 3],
    alpha: f32,
) {
    radial(image, center, radius + thickness, color, |d| {
        (thickness / 2.0 + 0.5 - (d - radius).abs()).clamp(0.0, 1.0) * alpha
    });
}

fn distance_to_segment(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let (abx, aby) = (b[0] - a[0], b[1] - a[1]);
    let (apx, apy) = (p[0] - a[0], p[1] - a[1]);
    let len2 = abx * abx + aby * aby;
    let t = if len2 > 0.0 {
        ((apx * abx + apy * aby) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (dx, dy) = (apx - abx * t, apy - aby * t);
    (dx * dx + dy * dy).sqrt()
}

/// A white arrow with a dark outline, its tip at `tip`.
fn arrow(image: &mut RgbaImage, tip: [f32; 2], scale: f32) {
    let points: Vec<[f32; 2]> = ARROW
        .iter()
        .map(|p| [tip[0] + p[0] * scale, tip[1] + p[1] * scale])
        .collect();
    let outline = 1.1 * scale;
    let margin = outline.ceil() as i64 + 1;
    let (x0, y0) = (
        tip[0].floor() as i64 - margin,
        tip[1].floor() as i64 - margin,
    );
    let x1 = (tip[0] + 12.2 * scale).ceil() as i64 + margin;
    let y1 = (tip[1] + 20.0 * scale).ceil() as i64 + margin;
    const SUB: usize = 3;
    for y in y0..=y1 {
        for x in x0..=x1 {
            let (mut white, mut black) = (0.0, 0.0);
            for sy in 0..SUB {
                for sx in 0..SUB {
                    let p = [
                        x as f32 + (sx as f32 + 0.5) / SUB as f32,
                        y as f32 + (sy as f32 + 0.5) / SUB as f32,
                    ];
                    let mut inside = false;
                    let mut nearest = f32::MAX;
                    for i in 0..points.len() {
                        let (a, b) = (points[i], points[(i + 1) % points.len()]);
                        nearest = nearest.min(distance_to_segment(p, a, b));
                        if (a[1] > p[1]) != (b[1] > p[1])
                            && p[0] < a[0] + (p[1] - a[1]) / (b[1] - a[1]) * (b[0] - a[0])
                        {
                            inside = !inside;
                        }
                    }
                    if inside && nearest >= outline * 0.5 {
                        white += 1.0;
                    } else if inside || nearest < outline * 0.5 {
                        black += 1.0;
                    }
                }
            }
            let samples = (SUB * SUB) as f32;
            blend(image, x, y, BLACK, black / samples);
            // What is left after the outline took its share.
            let rest = 1.0 - black / samples;
            if rest > 0.0 {
                blend(image, x, y, WHITE, (white / samples / rest).min(1.0));
            }
        }
    }
}

impl Overlay {
    /// The same overlay `seconds` of video later.
    pub fn advanced(&self, seconds: f32) -> Self {
        let mut later = self.clone();
        for ripple in &mut later.ripples {
            ripple.age += seconds;
        }
        later
    }

    /// Whether it looks different from one video frame to the next.
    pub fn is_animated(&self) -> bool {
        !self.ripples.is_empty()
    }

    pub fn draw(&self, image: &mut RgbaImage) {
        let (width, height) = (image.width() as f32, image.height() as f32);
        // Sized for a 720-pixel-high frame and scaled with the frame.
        let scale = (height / 720.0).clamp(0.6, 4.0);
        let pixels = |at: [f32; 2]| [at[0] * width, at[1] * height];

        for ripple in &self.ripples {
            let t = ripple.age / RIPPLE_SECS;
            if !(0.0..1.0).contains(&t) {
                continue;
            }
            let center = pixels(ripple.at);
            let grown = 1.0 - (1.0 - t).powi(3);
            let radius = (7.0 + 30.0 * grown) * scale;
            let fade = (1.0 - t).powf(1.5);
            disc(
                image,
                center,
                8.0 * scale,
                WHITE,
                (1.0 - t * 3.0).max(0.0) * 0.5,
            );
            // A dark ring under the white one keeps it visible on light scenes.
            ring(image, center, radius, 5.5 * scale, BLACK, fade * 0.4);
            ring(image, center, radius, 2.6 * scale, WHITE, fade * 0.95);
        }
        if let Some(cursor) = self.cursor {
            let tip = pixels(cursor);
            if self.held {
                disc(image, tip, 14.0 * scale, WHITE, 0.3);
                ring(image, tip, 14.0 * scale, 1.6 * scale, BLACK, 0.4);
            }
            arrow(image, tip, 1.25 * scale);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    fn frame() -> RgbaImage {
        RgbaImage::from_pixel(400, 300, Rgba([40, 80, 120, 255]))
    }

    fn changed(image: &RgbaImage) -> usize {
        let base = Rgba([40, 80, 120, 255]);
        image.pixels().filter(|p| **p != base).count()
    }

    #[test]
    fn cursor_is_a_white_arrow_with_a_dark_edge() {
        let mut image = frame();
        Overlay {
            cursor: Some([0.5, 0.5]),
            ..Default::default()
        }
        .draw(&mut image);
        // Inside the arrow body, just below the tip.
        assert_eq!(*image.get_pixel(202, 158), Rgba([255, 255, 255, 255]));
        // The tip itself is outline.
        let tip = image.get_pixel(200, 150);
        assert!(tip[0] < 60 && tip[1] < 60, "{tip:?}");
        // Nothing above or left of the tip, and nothing far away.
        assert_eq!(*image.get_pixel(196, 146), Rgba([40, 80, 120, 255]));
        assert_eq!(*image.get_pixel(260, 210), Rgba([40, 80, 120, 255]));
        assert!(changed(&image) < 400);
    }

    #[test]
    fn ripple_grows_and_fades_out() {
        let at = |age: f32| {
            let mut image = frame();
            Overlay {
                ripples: vec![Ripple {
                    at: [0.5, 0.5],
                    age,
                }],
                ..Default::default()
            }
            .draw(&mut image);
            image
        };
        let reach = |image: &RgbaImage| {
            (0..200u32)
                .rev()
                .find(|d| *image.get_pixel(200 + d, 150) != Rgba([40, 80, 120, 255]))
                .unwrap_or(0)
        };
        let (early, late) = (at(0.02), at(0.3));
        assert!(reach(&early) > 2, "visible at once");
        assert!(reach(&late) > reach(&early) + 4, "the ring expands");
        assert_eq!(changed(&at(RIPPLE_SECS + 0.01)), 0, "gone afterwards");
        // The same ripple reached by advancing an earlier overlay.
        let advanced = Overlay {
            ripples: vec![Ripple {
                at: [0.5, 0.5],
                age: 0.1,
            }],
            ..Default::default()
        }
        .advanced(0.2);
        assert!((advanced.ripples[0].age - 0.3).abs() < 1e-6);
    }

    #[test]
    fn drawing_at_the_edge_stays_in_bounds() {
        let mut image = frame();
        Overlay {
            cursor: Some([1.0, 1.0]),
            held: true,
            ripples: vec![Ripple {
                at: [0.0, 0.0],
                age: 0.2,
            }],
        }
        .draw(&mut image);
        assert!(changed(&image) > 0);
    }
}
