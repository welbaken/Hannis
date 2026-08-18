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
pub const MAX_BUBBLE_H: u32 = 600;
/// Force a minimum height so even short content keeps the phone-screen
/// (taller-than-wide) silhouette instead of collapsing into a landscape
/// pill.
pub const MIN_BUBBLE_H: u32 = 390;
/// How transparent the white fill is (0 = invisible, 255 = opaque).
pub const FILL_ALPHA: u8 = 10;

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

    /// Measure the bubble for `lines`, constrained to the pet frame and a
    /// max height; truncates overflow with "…". Text wraps at the phone
    /// width, and total height is capped at MAX_BUBBLE_H / the pet height.
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
        let min_w = scaled(MIN_BUBBLE_W, s);
        let max_w = scaled(MAX_BUBBLE_W, s).min(pet_w.max(min_w));
        let text_w = (max_w - pad_x * 2).max(scaled(72, s));
        let max_h = scaled(MAX_BUBBLE_H, s).min(pet_h.saturating_sub(scaled(BUBBLE_MARGIN_Y, s)));
        let max_text_h = max_h.saturating_sub(pad_top + pad_bottom).max(scaled(24, s));

        let mut shown = lines;
        let mut text_h = comp.measure_text(text_w, &shown);
        if shown.len() == 1 {
            // The streaming single line owns the FULL text area (fixed
            // height -> the bubble never jumps while text streams in) and
            // its content is TAIL-FITTED: keep the newest chars that fit in
            // max_text_h, dropping the oldest with a leading "…". This is
            // what lets the enlarged box actually show the growing stream
            // instead of a small fixed window.
            let chars: Vec<char> = shown[0].chars().collect();
            let n = chars.len();
            if n > 0 && text_h > max_text_h {
                // largest suffix (in chars) whose wrapped height fits
                let mut lo = 1usize;
                let mut hi = n;
                let mut keep = n;
                while lo <= hi {
                    let mid = (lo + hi) / 2;
                    let suffix: String = chars[n - mid..].iter().collect();
                    if comp.measure_text(text_w, &[suffix]) <= max_text_h {
                        keep = mid;
                        lo = mid + 1;
                    } else {
                        hi = mid - 1;
                    }
                }
                if keep < n {
                    let mut line = String::from("…");
                    line.extend(chars[n - keep..].iter());
                    // the leading ellipsis may push it over by a row; drop
                    // one more char if needed
                    if comp.measure_text(text_w, &[line.clone()]) > max_text_h && keep > 1 {
                        line = String::from("…");
                        line.extend(chars[n - (keep - 1)..].iter());
                    }
                    shown[0] = line;
                }
            }
            text_h = max_text_h; // full box height for the streaming view
        }
        if text_h > max_text_h {
            // drop trailing lines until it fits, then append an ellipsis
            while text_h > max_text_h && shown.len() > 1 {
                shown.pop();
                text_h = comp.measure_text(text_w, &shown);
            }
            if text_h > max_text_h {
                // a single over-long line: keep it but accept the cap
                text_h = max_text_h;
            } else {
                shown.push("…".to_string());
                text_h = comp.measure_text(text_w, &shown).min(max_text_h);
            }
        }
        self.lines = shown;
        self.w = (text_w + pad_x * 2).max(min_w);
        self.h = (text_h + pad_top + pad_bottom).max(scaled(MIN_BUBBLE_H, s));
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
        comp.fill_round_rect(x, y, self.w, self.h, radius, (70, 70, 70), 210);
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
