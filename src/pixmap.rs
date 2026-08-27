//! Bitmap-digit renderer for the tray icon.
//!
//! StatusNotifierItem pixmaps are ARGB32, network byte order (A,R,G,B per
//! pixel). We render the CO2 ppm value as soft, rounded digits on a canvas
//! sized tightly around the text, so the panel icon itself is the reading
//! and as large as the (square) tray slot allows.
//!
//! Softness recipe: draw the 4x7 glyph mask at SS-times supersampling,
//! dilate it with a small disk (bold + rounded convex corners), then
//! downscale with a box filter so edges carry anti-aliased alpha.

use std::fmt::Write as _;

/// Condensed 4x7 bitmap font for digits 0-9. Each glyph: 7 rows of 4 bits
/// (bit 3 = leftmost column). Narrow so three/four digits lose little height
/// when the square tray slot scales the wide pixmap down.
#[rustfmt::skip]
const DIGITS: [[u8; 7]; 10] = [
    // 0
    [0b0110, 0b1001, 0b1001, 0b1001, 0b1001, 0b1001, 0b0110],
    // 1
    [0b0010, 0b0110, 0b0010, 0b0010, 0b0010, 0b0010, 0b0111],
    // 2
    [0b0110, 0b1001, 0b0001, 0b0010, 0b0100, 0b1000, 0b1111],
    // 3
    [0b1110, 0b0001, 0b0001, 0b0110, 0b0001, 0b0001, 0b1110],
    // 4
    [0b0010, 0b0110, 0b1010, 0b1010, 0b1111, 0b0010, 0b0010],
    // 5
    [0b1111, 0b1000, 0b1110, 0b0001, 0b0001, 0b1001, 0b0110],
    // 6
    [0b0110, 0b1000, 0b1000, 0b1110, 0b1001, 0b1001, 0b0110],
    // 7
    [0b1111, 0b0001, 0b0001, 0b0010, 0b0100, 0b0100, 0b0100],
    // 8
    [0b0110, 0b1001, 0b1001, 0b0110, 0b1001, 0b1001, 0b0110],
    // 9
    [0b0110, 0b1001, 0b1001, 0b0111, 0b0001, 0b0001, 0b0110],
];

const FONT_W: i32 = 4;
const FONT_H: i32 = 7;

/// Canvas pixels per font pixel.
const SCALE: i32 = 3;
/// Supersampling factor for the soft-edge render.
const SS: i32 = 6;
/// Disk dilation radius in supersampled pixels. Adds one canvas pixel of
/// stroke (bold) and rounds convex corners.
const DILATE_R: i32 = 3;
/// Gap between digits, in canvas pixels.
const SPACING: i32 = 3;
/// Transparent margin around the text (canvas pixels).
const PAD: i32 = 1;

/// A rendered icon pixmap (ARGB32, network byte order, straight alpha).
pub struct Pixmap {
    pub width: i32,
    pub height: i32,
    pub data: Vec<u8>,
}

/// Digit colors. Stale wins over the CO2 ladder.
pub const FRESH_RGB: (u8, u8, u8) = (250, 250, 250); // white
pub const ELEVATED_RGB: (u8, u8, u8) = (255, 224, 138); // pastel yellow
pub const HIGH_RGB: (u8, u8, u8) = (255, 64, 64); // bright red
pub const STALE_RGB: (u8, u8, u8) = (128, 128, 128); // grey

/// Pick the digit color for a reading.
pub fn digit_rgb(co2_ppm: u16, fresh: bool) -> (u8, u8, u8) {
    if !fresh {
        STALE_RGB
    } else if co2_ppm > 1000 {
        HIGH_RGB
    } else if co2_ppm > 800 {
        ELEVATED_RGB
    } else {
        FRESH_RGB
    }
}

/// Render `value` (clamped to 4 digits) as soft bold digits on a tight
/// canvas. `rgb` is the digit color; alpha carries anti-aliased coverage
/// over a transparent background.
pub fn render_digits(value: u16, rgb: (u8, u8, u8)) -> Pixmap {
    let text: Vec<u8> = format!("{:03}", value.min(9999))
        .bytes()
        .map(|b| b - b'0')
        .collect();

    let n = text.len() as i32;
    let glyph_w = FONT_W * SCALE + 1; // +1 canvas px of bold from dilation
    let glyph_h = FONT_H * SCALE + 1;
    let width = n * glyph_w + (n - 1) * SPACING + 2 * PAD;
    let height = glyph_h + 2 * PAD;

    let iw = width * SS;
    let ih = height * SS;
    let mut mask = vec![false; (iw * ih) as usize];

    let cell = SCALE * SS; // supersampled size of one font pixel
    for (i, &d) in text.iter().enumerate() {
        let glyph = &DIGITS[d as usize];
        let gx0 = (PAD + (i as i32) * (glyph_w + SPACING)) * SS;
        let gy0 = PAD * SS;
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..FONT_W {
                if bits & (1 << (FONT_W - 1 - col)) == 0 {
                    continue;
                }
                let bx = gx0 + col * cell;
                let by = gy0 + (row as i32) * cell;
                for y in by..by + cell {
                    let base = (y * iw + bx) as usize;
                    for x in 0..cell {
                        mask[base + x as usize] = true;
                    }
                }
            }
        }
    }

    // Disk dilation: bold + rounded convex corners.
    let snap = mask.clone();
    for y in 0..ih {
        for x in 0..iw {
            if !snap[(y * iw + x) as usize] {
                continue;
            }
            for dy in -DILATE_R..=DILATE_R {
                let yy = y + dy;
                if !(0..ih).contains(&yy) {
                    continue;
                }
                for dx in -DILATE_R..=DILATE_R {
                    if dx * dx + dy * dy > DILATE_R * DILATE_R {
                        continue;
                    }
                    let xx = x + dx;
                    if (0..iw).contains(&xx) {
                        mask[(yy * iw + xx) as usize] = true;
                    }
                }
            }
        }
    }

    // Box-filter downscale -> anti-aliased straight alpha.
    let mut data = vec![0u8; (width * height * 4) as usize];
    for oy in 0..height {
        for ox in 0..width {
            let mut acc: u32 = 0;
            for sy in 0..SS {
                let base = ((oy * SS + sy) * iw + ox * SS) as usize;
                for sx in 0..SS {
                    if mask[base + sx as usize] {
                        acc += 1;
                    }
                }
            }
            let a = (acc * 255 / (SS * SS) as u32) as u8;
            let o = ((oy * width + ox) * 4) as usize;
            data[o] = a;
            data[o + 1] = rgb.0;
            data[o + 2] = rgb.1;
            data[o + 3] = rgb.2;
        }
    }
    Pixmap { width, height, data }
}

/// Human-readable geometry summary, used in docs/tests.
pub fn geometry(value: u16) -> String {
    let p = render_digits(value, FRESH_RGB);
    let mut s = String::new();
    let _ = write!(s, "{}x{}", p.width, p.height);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha_at(p: &Pixmap, x: i32, y: i32) -> u8 {
        p.data[((y * p.width + x) * 4) as usize]
    }

    #[test]
    fn buffer_matches_canvas() {
        let p = render_digits(643, FRESH_RGB);
        assert_eq!(p.data.len(), (p.width * p.height * 4) as usize);
        // 3 digits: 3*13 + 2*3 + 2 = 47 wide, 22+2 = 24 tall
        assert_eq!((p.width, p.height), (47, 24));
    }

    #[test]
    fn four_digit_values_get_wider_canvas() {
        let p = render_digits(1200, FRESH_RGB);
        assert_eq!((p.width, p.height), (63, 24));
    }

    #[test]
    fn digits_produce_ink_and_transparent_bg() {
        let p = render_digits(888, FRESH_RGB);
        let ink = p.data.chunks(4).filter(|q| q[0] > 0).count();
        assert!(ink > 200, "expected substantial ink, got {ink}");
        // corners stay fully transparent (background is not filled)
        for (x, y) in [(0, 0), (p.width - 1, 0), (0, p.height - 1), (p.width - 1, p.height - 1)] {
            assert_eq!(alpha_at(&p, x, y), 0, "corner ({x},{y}) must be transparent");
        }
    }

    #[test]
    fn strokes_are_bold_but_not_leaking() {
        // "111": middle digit's bar sits at x = 17 + 2*3 = 23 (pre-dilation),
        // dilation grows ~1px each side -> solid core around 22..27.
        let p = render_digits(111, FRESH_RGB);
        let y = (PAD + 3 * SCALE) * 1 + SS / 2; // mid of a glyph row, canvas px
        let row: Vec<u8> = (0..p.width).map(|x| alpha_at(&p, x, y)).collect();
        let solid: Vec<i32> = (0..p.width)
            .filter(|&x| row[x as usize] > 200)
            .collect();
        assert!(
            solid.iter().any(|&x| (22..=27).contains(&x)),
            "expected a solid bar near x 22..27, row={row:?}"
        );
        // the inter-digit gap must stay clear
        assert_eq!(row[15], 0, "gap between digits must be transparent");
    }

    #[test]
    fn edges_are_antialiased() {
        // The soft font must produce intermediate alpha values, not just
        // 0/255 steps.
        let p = render_digits(640, FRESH_RGB);
        let soft = p
            .data
            .chunks(4)
            .filter(|q| q[0] > 10 && q[0] < 245)
            .count();
        assert!(soft > 20, "expected anti-aliased edge pixels, got {soft}");
    }

    #[test]
    fn clamps_to_four_digits() {
        let a = render_digits(65535, (1, 2, 3));
        let b = render_digits(9999, (1, 2, 3));
        assert_eq!(a.width, b.width);
        assert_eq!(a.data, b.data);
    }

    #[test]
    fn color_ladder() {
        assert_eq!(digit_rgb(400, true), FRESH_RGB);
        assert_eq!(digit_rgb(800, true), FRESH_RGB); // "over 800" is strict
        assert_eq!(digit_rgb(801, true), ELEVATED_RGB);
        assert_eq!(digit_rgb(1000, true), ELEVATED_RGB);
        assert_eq!(digit_rgb(1001, true), HIGH_RGB);
        // stale wins over the ladder
        assert_eq!(digit_rgb(1500, false), STALE_RGB);
        assert_eq!(digit_rgb(600, false), STALE_RGB);
    }

    #[test]
    fn stale_window_is_adaptive() {
        use crate::state::stale_window;
        // before the rhythm is known, the floor applies
        assert_eq!(stale_window(150, None), 150);
        // 60 s sampling -> 2*60+30 = 150
        assert_eq!(stale_window(150, Some(60)), 150);
        // 5 min sampling -> 2*300+30 = 630, so grey only after ~2 missed packets
        assert_eq!(stale_window(150, Some(300)), 630);
    }

    /// Manual artifact generator: `cargo test render_color_demo -- --ignored`
    /// writes PPM previews of all four states to /tmp for visual inspection.
    #[test]
    #[ignore]
    fn render_color_demo() {
        let cases = [
            ("white", 640, true),
            ("yellow", 850, true),
            ("red", 1250, true),
            ("gray", 850, false),
        ];
        for (name, co2, fresh) in cases {
            let p = render_digits(co2, digit_rgb(co2, fresh));
            let mut out = format!("P6\n{} {}\n255\n", p.width, p.height).into_bytes();
            // composite straight alpha over a dark panel background
            for px in p.data.chunks(4) {
                let a = px[0] as u32;
                for (i, c) in [px[1], px[2], px[3]].iter().enumerate() {
                    let bg = [35u32, 35, 38][i];
                    out.push(((*c as u32 * a + bg * (255 - a)) / 255) as u8);
                }
            }
            std::fs::write(format!("/tmp/digits-{name}.ppm"), out).unwrap();
        }
    }
}
