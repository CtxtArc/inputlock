//! Software rendering into an ARGB8888 (premultiplied) shm buffer.
//!
//! The whole surface is transparent (or lightly dimmed) so everything below
//! keeps rendering; only a small status pill is drawn at the bottom.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Idle,
    Typing,
    Verifying,
    Wrong,
    /// PAM refused for a reason other than a typo (e.g. account locked).
    Blocked,
}

#[derive(Clone, Copy)]
pub struct Style {
    /// 0.0 = fully transparent, 1.0 = black.
    pub dim: f32,
    pub indicator: bool,
}

#[derive(Clone, Copy)]
struct Rgba(f32, f32, f32, f32);

impl Rgba {
    fn hex(rgb: u32, a: f32) -> Self {
        Rgba(
            ((rgb >> 16) & 0xff) as f32 / 255.0,
            ((rgb >> 8) & 0xff) as f32 / 255.0,
            (rgb & 0xff) as f32 / 255.0,
            a,
        )
    }
}

const BG: u32 = 0x1e1e2e;
const IDLE: u32 = 0x7f849c;
const TYPING: u32 = 0x89b4fa;
const VERIFY: u32 = 0xf9e2af;
const WRONG: u32 = 0xf38ba8;
const BLOCKED: u32 = 0xfab387;

/// Draws a full frame. `w`/`h` are buffer pixels, `scale` the buffer scale.
pub fn draw(canvas: &mut [u8], w: u32, h: u32, scale: u32, style: Style, phase: Phase, len: usize) {
    // Background: black at `dim` alpha, premultiplied => (0,0,0,a).
    let a = (style.dim.clamp(0.0, 1.0) * 255.0).round() as u8;
    let px = u32::from_le_bytes([0, 0, 0, a]).to_le_bytes();
    for c in canvas.chunks_exact_mut(4) {
        c.copy_from_slice(&px);
    }

    if !style.indicator {
        return;
    }

    let s = scale as f32;
    let pw = 260.0 * s;
    let ph = 44.0 * s;
    let x0 = (w as f32 - pw) / 2.0;
    let y0 = h as f32 - ph - (h as f32 * 0.08).max(24.0 * s);

    let accent = match phase {
        Phase::Idle => IDLE,
        Phase::Typing => TYPING,
        Phase::Verifying => VERIFY,
        Phase::Wrong => WRONG,
        Phase::Blocked => BLOCKED,
    };

    let mut cv = Canvas { buf: canvas, w, h };

    // Pill: accent outline then dark fill.
    let r = ph / 2.0;
    cv.rounded_rect(x0, y0, pw, ph, r, Rgba::hex(accent, 0.95));
    let bw = 2.0 * s;
    cv.rounded_rect(x0 + bw, y0 + bw, pw - 2.0 * bw, ph - 2.0 * bw, r - bw, Rgba::hex(BG, 0.92));

    // Lock glyph on the left: body + shackle.
    let lx = x0 + 18.0 * s;
    let ly = y0 + ph / 2.0;
    let col = Rgba::hex(accent, 1.0);
    cv.rounded_rect(lx, ly - 2.0 * s, 14.0 * s, 11.0 * s, 2.0 * s, col);
    cv.ring(lx + 7.0 * s, ly - 3.0 * s, 4.5 * s, 1.6 * s, true, col);

    // Dots for typed characters (length is capped visually, never revealed exactly).
    let dot_r = 4.0 * s;
    let gap = 14.0 * s;
    let max_dots = 12usize;
    let area_x = lx + 30.0 * s;
    let area_w = x0 + pw - 20.0 * s - area_x;
    match phase {
        Phase::Typing | Phase::Verifying => {
            let n = if len > max_dots { (len - 1) % max_dots + 1 } else { len };
            for i in 0..n {
                let cx = area_x + dot_r + i as f32 * gap;
                if cx + dot_r > area_x + area_w {
                    break;
                }
                cv.circle(cx, ly, dot_r, col);
            }
        }
        Phase::Wrong | Phase::Blocked => {
            // A small "×" made of dots across the area center.
            let cx = area_x + area_w / 2.0;
            for i in -3i32..=3 {
                let o = i as f32 * 3.0 * s;
                cv.circle(cx + o, ly + o, 1.8 * s, col);
                cv.circle(cx + o, ly - o, 1.8 * s, col);
            }
        }
        Phase::Idle => {
            for i in 0..3 {
                let cx = area_x + dot_r + i as f32 * gap;
                cv.circle(cx, ly, 2.0 * s, Rgba::hex(IDLE, 0.6));
            }
        }
    }
}

struct Canvas<'a> {
    buf: &'a mut [u8],
    w: u32,
    h: u32,
}

impl Canvas<'_> {
    /// Composites `c` with coverage `cov` over the pixel (premultiplied "over").
    #[inline]
    fn blend(&mut self, x: u32, y: u32, c: Rgba, cov: f32) {
        let a = c.3 * cov;
        if a <= 0.0 {
            return;
        }
        let i = ((y * self.w + x) * 4) as usize;
        let p = &mut self.buf[i..i + 4]; // B G R A
        let inv = 1.0 - a;
        let mix = |src: f32, dst: u8| ((src * a * 255.0) + dst as f32 * inv).round().min(255.0) as u8;
        p[0] = mix(c.2, p[0]);
        p[1] = mix(c.1, p[1]);
        p[2] = mix(c.0, p[2]);
        p[3] = ((a * 255.0) + p[3] as f32 * inv).round().min(255.0) as u8;
    }

    /// Iterates pixels in a bounding box, calling `sdf` for a signed distance
    /// (negative inside) and blending with 1px anti-aliasing.
    fn fill_sdf(&mut self, bx: f32, by: f32, bw: f32, bh: f32, c: Rgba, sdf: impl Fn(f32, f32) -> f32) {
        let xa = bx.floor().max(0.0) as u32;
        let ya = by.floor().max(0.0) as u32;
        let xb = ((bx + bw).ceil() as i64).clamp(0, self.w as i64) as u32;
        let yb = ((by + bh).ceil() as i64).clamp(0, self.h as i64) as u32;
        for y in ya..yb {
            for x in xa..xb {
                let d = sdf(x as f32 + 0.5, y as f32 + 0.5);
                let cov = (0.5 - d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.blend(x, y, c, cov);
                }
            }
        }
    }

    fn rounded_rect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, c: Rgba) {
        let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
        let (cx, cy) = (x + w / 2.0, y + h / 2.0);
        let (hx, hy) = (w / 2.0 - r, h / 2.0 - r);
        self.fill_sdf(x - 1.0, y - 1.0, w + 2.0, h + 2.0, c, |px, py| {
            let qx = (px - cx).abs() - hx;
            let qy = (py - cy).abs() - hy;
            let ox = qx.max(0.0);
            let oy = qy.max(0.0);
            (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
        });
    }

    fn circle(&mut self, cx: f32, cy: f32, r: f32, c: Rgba) {
        self.fill_sdf(cx - r - 1.0, cy - r - 1.0, 2.0 * r + 2.0, 2.0 * r + 2.0, c, |px, py| {
            ((px - cx).powi(2) + (py - cy).powi(2)).sqrt() - r
        });
    }

    /// Ring of radius `r` and thickness `t`; `upper_only` keeps the top half.
    fn ring(&mut self, cx: f32, cy: f32, r: f32, t: f32, upper_only: bool, c: Rgba) {
        let o = r + t;
        self.fill_sdf(cx - o - 1.0, cy - o - 1.0, 2.0 * o + 2.0, 2.0 * o + 2.0, c, |px, py| {
            let d = (((px - cx).powi(2) + (py - cy).powi(2)).sqrt() - r).abs() - t / 2.0;
            if upper_only && py > cy {
                d.max(py - cy)
            } else {
                d
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(dim: f32, indicator: bool, phase: Phase) -> Vec<u8> {
        let (w, h) = (400u32, 300u32);
        let mut buf = vec![0xAAu8; (w * h * 4) as usize];
        draw(&mut buf, w, h, 1, Style { dim, indicator }, phase, 5);
        buf
    }

    #[test]
    fn transparent_when_no_dim_and_no_indicator() {
        assert!(frame(0.0, false, Phase::Idle).iter().all(|&b| b == 0));
    }

    #[test]
    fn dim_sets_alpha_only() {
        let buf = frame(0.5, false, Phase::Idle);
        for px in buf.chunks_exact(4) {
            assert_eq!(&px[..3], &[0, 0, 0]);
            assert_eq!(px[3], 128);
        }
    }

    #[test]
    fn premultiplied_invariant_holds() {
        for phase in [Phase::Idle, Phase::Typing, Phase::Verifying, Phase::Wrong, Phase::Blocked] {
            for px in frame(0.2, true, phase).chunks_exact(4) {
                assert!(px[0] <= px[3] && px[1] <= px[3] && px[2] <= px[3], "{px:?}");
            }
        }
    }

    #[test]
    fn indicator_draws_something() {
        assert!(frame(0.0, true, Phase::Typing).iter().any(|&b| b != 0));
    }
}
