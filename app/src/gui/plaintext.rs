//! "Behind the pet" text stream renderer (config `text.mode = "behind"`).
//!
//! Instead of a phone bubble, the message lines are drawn directly into the
//! transparent window area BEFORE the pet sprite, aligned to the pet image's
//! own box (pet_w × pet_h): scrolling text starts near the head and, when
//! long (a long DSH response kept at `text.max_chars` per line), extends all
//! the way down past the feet — never protruding past the pet's right edge.
//! The character naturally occludes whatever its opaque pixels cover
//! (被挡住也无所谓). There is no bubble chrome around the glyphs —
//! readability comes from the outline (勾边) the compositor strokes around
//! each glyph (black fill on a white outline), which holds up on any
//! background.
//!
//! The original phone-bubble renderer stays available: switch with
//! `text.mode` in config.json or via the tray menu at runtime.

use super::bubble::scaled;
use super::render::Compositor;
use dshpet::config::{parse_hex_color, TextConfig};

/// Window-relative margins of the text block inside the pet box.
pub const MARGIN_X: u32 = 10;
pub const MARGIN_Y: u32 = 8;
/// Extra height slack so the outline halo and descenders are never clipped.
const H_SLACK: u32 = 6;

#[derive(Debug, Clone)]
pub struct TextOverlay {
    /// Laid-out lines to draw (wrapped, truncated to fit).
    pub lines: Vec<String>,
    /// Target rect in window coordinates.
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    /// Layout cache key: (lines, window w, window h). Layout is not free
    /// (GDI measure calls), so it only reruns when one of these changes.
    key: (Vec<String>, u32, u32),
}

impl Default for TextOverlay {
    fn default() -> Self {
        TextOverlay { lines: Vec::new(), x: 0, y: 0, w: 0, h: 0, key: (Vec::new(), 0, 0) }
    }
}

impl TextOverlay {
    /// Lay out `lines` inside the PET box (pet_w × pet_h at pet_x/pet_y), so
    /// the stream shadows the character itself: text starts below the head
    /// and, when there is a lot of it, extends all the way down past the
    /// feet — never protruding past the pet's right edge. The pet is drawn
    /// on top and occludes whatever it covers; the outline keeps the visible
    /// part readable. Overflow lines are truncated with "…" (at most
    /// `max_lines` visible), and any total height beyond `pet_h` is clipped
    /// at the bottom.
    pub fn layout_if_needed(
        &mut self,
        lines: Vec<String>,
        pet_w: u32,
        pet_h: u32,
        pet_x: i32,
        pet_y: i32,
        max_lines: usize,
        comp: &Compositor,
    ) {
        if self.key.0 == lines && self.key.1 == pet_w && self.key.2 == pet_h {
            return;
        }
        self.key = (lines.clone(), pet_w, pet_h);
        if lines.is_empty() {
            self.lines.clear();
            self.w = 0;
            self.h = 0;
            return;
        }
        let s = comp.dpi_scale();
        let mx = scaled(MARGIN_X, s);
        let my = scaled(MARGIN_Y, s);
        let text_w = pet_w.saturating_sub(mx * 2).max(64);
        let max_h = pet_h.saturating_sub(my * 2).max(32);

        // No fixed reservation for the streaming line here (unlike the
        // bubble): behind the pet the text may grow with the actual content,
        // filling from the head down toward the feet. The per-line char
        // window (text.max_chars) bounds how long a paragraph can get.
        let mut shown = lines;
        let mut text_h = comp.measure_text(text_w, &shown);
        let max_lines = max_lines.max(1);
        if shown.len() > max_lines {
            shown.truncate(max_lines);
            shown.push("…".to_string());
            text_h = comp.measure_text(text_w, &shown);
        }
        if text_h > max_h {
            // drop trailing lines until it fits, then append an ellipsis
            while text_h > max_h && shown.len() > 1 {
                shown.pop();
                text_h = comp.measure_text(text_w, &shown);
            }
            if text_h > max_h {
                // a single over-long line: keep it but accept the cap
                text_h = max_h;
            } else {
                shown.push("…".to_string());
                text_h = comp.measure_text(text_w, &shown).min(max_h);
            }
        }
        self.lines = shown;
        self.x = pet_x + mx as i32;
        self.y = pet_y + my as i32;
        self.w = text_w;
        self.h = (text_h + scaled(H_SLACK, s)).min(pet_h.saturating_sub(my));
    }

    /// Draw the outlined text block into the buffer. Call BEFORE the pet
    /// frame so the sprite covers whatever overlaps it.
    pub fn draw(&self, comp: &mut Compositor, cfg: &TextConfig) {
        if self.lines.is_empty() {
            return;
        }
        let fill = parse_hex_color(&cfg.fill_color).unwrap_or((0, 0, 0));
        let outline = parse_hex_color(&cfg.outline_color).unwrap_or((255, 255, 255));
        comp.draw_text_outlined(
            self.x,
            self.y,
            self.w,
            self.h,
            &self.lines,
            fill,
            outline,
            cfg.outline_width,
        );
    }
}