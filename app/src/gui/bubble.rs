//! Bubble layout & drawing: a vertical "phone screen" bubble anchored at the
//! pet's top-left corner. Translucent (半透明) white fill with a dark border +
//! soft shadow so it stays readable on both light and dark backgrounds. It is
//! drawn UNDER the pet sprite, so the enlarged body may occlude part of it
//! (可以被本体遮挡一部分). No speaker notch — just text inside padding.
//! Overflow lines are truncated with "…".

use super::render::Compositor;

pub const PAD_X: u32 = 7;
pub const PAD_TOP: u32 = 6;
pub const PAD_BOTTOM: u32 = 8;
/// Window-relative margin of the bubble from the pet's top-left corner.
pub const BUBBLE_MARGIN_X: u32 = 6;
pub const BUBBLE_MARGIN_Y: u32 = 10;
/// Phone-screen proportions, enlarged 1.5×: a wider/taller portrait panel.
/// It may extend under the pet body, which is fine (部分遮挡).
pub const MAX_BUBBLE_W: u32 = 186;
pub const MIN_BUBBLE_W: u32 = 144;
pub const MAX_BUBBLE_H: u32 = 400;
/// Force a minimum height so even short content keeps the phone-screen
/// (taller-than-wide) silhouette instead of collapsing into a landscape
/// pill.
pub const MIN_BUBBLE_H: u32 = 390;
/// How transparent the white fill is (0 = invisible, 255 = opaque).
pub const FILL_ALPHA: u8 = 50;

pub(crate) fn scaled(v: u32, s: f32) -> u32 {
    ((v as f32) * s).round().max(1.0) as u32
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bubble {
    pub lines: Vec<String>,
    pub w: u32,
    pub h: u32,
}

impl Default for Bubble {
    fn default() -> Self {
        Bubble { lines: Vec::new(), w: 0, h: 0 }
    }
}

impl Bubble {
    pub fn visible(&self) -> bool {
        !self.lines.is_empty()
    }

    /// Measure the bubble for `lines`. The box has a UNIFORM fixed size
    /// (the enlarged 1.5× phone screen, capped by the pet frame) that never
    /// changes with the message; the content is laid out inside it — wrapped,
    /// trailing lines dropped with "…", and a streaming single line is
    /// tail-fitted (newest chars kept, oldest dropped with a leading "…").
    pub fn layout(&mut self, lines: Vec<String>, pet_w: u32, pet_h: u32, comp: &Compositor) -> bool {
        if lines == self.lines {
            return false;
        }
        if lines.is_empty() {
            self.lines.clear();
            self.w = 0;
            self.h = 0;
            return true;
        }
        let s = comp.dpi_scale();
        let pad_x = scaled(PAD_X, s);
        let pad_top = scaled(PAD_TOP, s);
        let pad_bottom = scaled(PAD_BOTTOM, s);
        // fixed size regardless of the message
        let min_w = scaled(MIN_BUBBLE_W, s);
        let w = scaled(MAX_BUBBLE_W, s).min(pet_w.max(min_w));
        let h = scaled(MAX_BUBBLE_H, s)
            .min(pet_h.saturating_sub(scaled(BUBBLE_MARGIN_Y, s)))
            .max(scaled(MIN_BUBBLE_H, s));
        let text_w = w.saturating_sub(pad_x * 2).max(scaled(72, s));
        let text_h = h.saturating_sub(pad_top + pad_bottom).max(scaled(24, s));

        // fit the content into the fixed area without resizing the box
        let mut shown = lines;
        let mut measured = comp.measure_text(text_w, &shown);
        if shown.len() == 1 && measured > text_h {
            // streaming single line: TAIL-FIT — keep the newest chars that
            // fit, drop the oldest with a leading "…"
            let chars: Vec<char> = shown[0].chars().collect();
            let n = chars.len();
            // largest suffix (in chars) whose wrapped height fits
            let mut lo = 1usize;
            let mut hi = n;
            let mut keep = n;
            while lo <= hi {
                let mid = (lo + hi) / 2;
                let suffix: String = chars[n - mid..].iter().collect();
                if comp.measure_text(text_w, &[suffix]) <= text_h {
                    keep = mid;
                    lo = mid + 1;
                } else {
                    hi = mid - 1;
                }
            }
            if keep < n {
                let mut line = String::from("…");
                line.extend(chars[n - keep..].iter());
                // the leading ellipsis may push it over by a row; drop one
                // more char if needed
                if comp.measure_text(text_w, &[line.clone()]) > text_h && keep > 1 {
                    line = String::from("…");
                    line.extend(chars[n - (keep - 1)..].iter());
                }
                shown[0] = line;
                measured = comp.measure_text(text_w, &shown).min(text_h);
            }
        }
        if measured > text_h {
            // multi-line message too tall: drop trailing lines, append "…"
            while measured > text_h && shown.len() > 1 {
                shown.pop();
                measured = comp.measure_text(text_w, &shown);
            }
            if measured > text_h {
                // single over-long line: keep it but accept the clipped cap
            } else {
                shown.push("…".to_string());
            }
        }
        self.lines = shown;
        self.w = w;
        self.h = h;
        true
    }

    /// Draw at (x, y): soft shadow, dark border, semi-transparent white
    /// fill, text. No speaker notch. Drawn before the pet sprite, so the
    /// body may cover part of it.
    pub fn draw(&self, comp: &mut Compositor, x: i32, y: i32) {
        let s = comp.dpi_scale();
        let pad_x = scaled(PAD_X, s);
        let pad_top = scaled(PAD_TOP, s);
        let pad_bottom = scaled(PAD_BOTTOM, s);
        let radius = scaled(12, s);
        let border = scaled(2, s);
        // soft shadow (visible on light backgrounds)
        comp.fill_round_rect(x + border as i32 + 1, y + border as i32 + 3, self.w, self.h, radius, (0, 0, 0), 46);
        // dark border ring
        comp.fill_round_rect(x, y, self.w, self.h, radius, (70, 70, 70), 50);
        // semi-transparent white fill
        comp.fill_round_rect(
            x + border as i32,
            y + border as i32,
            self.w.saturating_sub(border * 2),
            self.h.saturating_sub(border * 2),
            radius.saturating_sub(1),
            (255, 255, 255),
            FILL_ALPHA,
        );
        comp.draw_text(
            x + pad_x as i32,
            y + pad_top as i32,
            self.w - pad_x * 2,
            self.h - pad_top - pad_bottom,
            &self.lines,
        );
    }
}
