//! Bubble layout & drawing: a vertical "phone screen" bubble anchored at the
//! pet's top-left corner. Translucent white fill with a dark border + soft
//! shadow so it stays readable on both light and dark backgrounds. The width
//! is capped narrow and the height is capped so the bottom edge ends inside
//! the pet canvas's transparent top-left area (the character is centered, so
//! that corner is empty). Overflow lines are truncated with "…".

use super::render::Compositor;

pub const PAD_X: u32 = 7;
pub const PAD_TOP: u32 = 6;
pub const PAD_BOTTOM: u32 = 8;
/// Window-relative margin of the bubble from the pet's top-left corner.
pub const BUBBLE_MARGIN_X: u32 = 6;
pub const BUBBLE_MARGIN_Y: u32 = 10;
/// Phone-screen proportions: narrow enough to stay clear of the head (the
/// head's top-left contour starts ~x=160), tall enough to look like a
/// portrait screen while its bottom edge stays in the transparent band.
pub const MAX_BUBBLE_W: u32 = 124;
pub const MIN_BUBBLE_W: u32 = 96;
pub const MAX_BUBBLE_H: u32 = 400;
/// Force a minimum height so even short content keeps the phone-screen
/// (taller-than-wide) silhouette instead of collapsing into a landscape
/// pill.
pub const MIN_BUBBLE_H: u32 = 260;
/// "Speaker notch" pill near the top of the screen, like a phone.
pub const NOTCH_W: u32 = 44;
pub const NOTCH_H: u32 = 6;
pub const NOTCH_GAP: u32 = 9;

const MAX_LINE_CHARS: usize = 120;

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
    /// width, and total height is capped so the bottom edge lands inside the
    /// pet's transparent top-left area.
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
        let notch_gap = scaled(NOTCH_GAP, s)
            .max(pad_top + scaled(NOTCH_H, s) + 2); // room for the notch pill
        let min_w = scaled(MIN_BUBBLE_W, s);
        let max_w = scaled(MAX_BUBBLE_W, s).min(pet_w.max(min_w));
        let text_w = (max_w - pad_x * 2).max(scaled(72, s));
        let max_h = scaled(MAX_BUBBLE_H, s).min(pet_h.saturating_sub(scaled(BUBBLE_MARGIN_Y, s) + scaled(NOTCH_H, s)));
        let max_text_h = max_h.saturating_sub(pad_top + pad_bottom + notch_gap).max(scaled(24, s));

        let mut shown = lines;
        let mut text_h = comp.measure_text(text_w, &shown);
        if shown.len() == 1 {
            // stable height: the streaming single line changes length every
            // update; allocate a FIXED height (up to 4 rows) so the bubble
            // never resizes and the pet never jumps
            let dummy: String = "…".to_string() + &"字".repeat(MAX_LINE_CHARS);
            let one_row = comp.measure_text(text_w, &["字".to_string()]).max(1);
            let dummy_h = comp.measure_text(text_w, &[dummy]);
            text_h = dummy_h.min(one_row * 4).min(max_text_h).max(one_row);
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
        self.h = (text_h + pad_top + pad_bottom + notch_gap).max(scaled(MIN_BUBBLE_H, s));
        true
    }

    /// Draw at (x, y): soft shadow, dark border, translucent white fill,
    /// speaker notch, text.
    pub fn draw(&self, comp: &mut Compositor, x: i32, y: i32) {
        let s = comp.dpi_scale();
        let pad_x = scaled(PAD_X, s);
        let pad_top = scaled(PAD_TOP, s);
        let pad_bottom = scaled(PAD_BOTTOM, s);
        let notch_gap = scaled(NOTCH_GAP, s)
            .max(pad_top + scaled(NOTCH_H, s) + 2);
        let radius = scaled(12, s);
        let border = scaled(2, s);
        // soft shadow (visible on light backgrounds)
        comp.fill_round_rect(x + border as i32 + 1, y + border as i32 + 3, self.w, self.h, radius, (0, 0, 0), 46);
        // dark border ring
        comp.fill_round_rect(x, y, self.w, self.h, radius, (70, 70, 70), 210);
        // translucent white fill
        comp.fill_round_rect(
            x + border as i32,
            y + border as i32,
            self.w.saturating_sub(border * 2),
            self.h.saturating_sub(border * 2),
            radius.saturating_sub(1),
            (255, 255, 255),
            240,
        );
        // phone speaker notch: centered near the top of the screen
        let notch_w = scaled(NOTCH_W, s);
        let notch_h = scaled(NOTCH_H, s);
        let nx = x + (self.w as i32 - notch_w as i32) / 2;
        let ny = y + pad_top as i32;
        // small radius so the pill stays pill-shaped even on tiny scales
        let nradius = (notch_h / 2).min(3).max(1);
        comp.fill_round_rect(nx, ny, notch_w, notch_h, nradius, (110, 110, 110), 200);
        comp.draw_text(
            x + pad_x as i32,
            y + notch_gap as i32,
            self.w - pad_x * 2,
            self.h - notch_gap - pad_bottom,
            &self.lines,
        );
    }
}
