//! Bubble layout & drawing: a vertical "phone screen" bubble anchored at the
//! pet's top-left corner. Modern card style: 1px light-gray micro border +
//! soft floating shadow, translucent white fill so it stays readable on both
//! light and dark backgrounds. It is drawn UNDER the pet sprite, so the
//! enlarged body may occlude part of it (可以被本体遮挡一部分).
//!
//! Content layout: a header block above the divider — "From <client>"
//! (meta row) on top, then the state title below it — then a 1px divider
//! line, then the message stream below the divider. Overflow lines are
//! truncated with "…".

use super::render::Compositor;
use dshpet::bubble_text::BubbleText;
use dshpet::config::BubbleTheme;
use dshpet::state::Mode;

pub const PAD_X: u32 = 7;
pub const PAD_TOP: u32 = 6;
pub const PAD_BOTTOM: u32 = 8;
/// Window-relative margin of the bubble from the window's top edge.
/// Kept ≥ the shadow blur so the halo diffuses evenly from the card's
/// center without being clipped by the window edge.
pub const BUBBLE_MARGIN_Y: u32 = 44;
/// 气泡右缘在动图中的设计锚点:display.scale = 1.0、dpi = 1.0 时右缘位于
/// 800px 动图宽的 25% = 200px。缩放比例变化时气泡右缘始终按该百分比定位
/// (gui::compose),保证"本体只遮挡气泡右缘一小截"的遮挡关系不随 scale
/// 改变——否则缩小后气泡会横在本体下方,看起来像本体站到了气泡正上方。
/// 定位公式在物理空间计算:动图宽(仅按 display.scale 缩放)需再乘 DPI,
/// 与按 DPI 缩放的气泡宽同空间。
pub const BUBBLE_RIGHT_FRACTION: f32 = 0.25;
/// Phone-screen proportions, enlarged 1.5×: a wider/taller portrait panel.
/// It may extend under the pet body, which is fine (部分遮挡).
///pub const MAX_BUBBLE_W: u32 = 178;
///pub const MIN_BUBBLE_W: u32 = 138;
///pub const MAX_BUBBLE_H: u32 = 386;
pub const MAX_BUBBLE_W: u32 = 150;
pub const MIN_BUBBLE_W: u32 = 150;
pub const MAX_BUBBLE_H: u32 = 350;
/// Force a minimum height so even short content keeps the phone-screen
/// (taller-than-wide) silhouette instead of collapsing into a landscape
/// pill.
///pub const MIN_BUBBLE_H: u32 = 374;
pub const MIN_BUBBLE_H: u32 = 350;
/// How transparent the white fill is (0 = invisible, 255 = opaque).
pub const FILL_ALPHA: u8 = 80;
/// Soft floating shadow (CSS box-shadow style): the blurred halo diffuses
/// evenly from the card's center — zero offset in both axes
/// (0px 0px 40px ≈ rgba(0,0,0,.15)). The bubble margins guarantee the halo
/// is never clipped by the window edge.
const SHADOW_OFFSET_X: u32 = 0;
const SHADOW_OFFSET_Y: u32 = 0;
const SHADOW_BLUR: u32 = 20;
const SHADOW_ALPHA: u8 = 38; // ≈ 15% black
/// 1px light-gray micro border (modern card).
const BORDER_RGB: (u8, u8, u8) = (205, 205, 205);
const BORDER_ALPHA: u8 = 190;
/// Divider line under the header row.
const DIVIDER_RGB: (u8, u8, u8) = (196, 196, 196);
const DIVIDER_ALPHA: u8 = 170;
/// Vertical spacing around the divider (above / line height / below).
const DIVIDER_GAP_TOP: u32 = 3;
const DIVIDER_H: u32 = 1;
const DIVIDER_GAP_BOTTOM: u32 = 4;
/// Gap between the "From …" pill and the state title row below it.
const PILL_GAP: u32 = 6;
/// Header text colors: title dark, "From …" muted gray.
const TITLE_RGB: (u8, u8, u8) = (0x26, 0x26, 0x26);
const FROM_RGB: (u8, u8, u8) = (0x8f, 0x8f, 0x8f);

pub(crate) fn scaled(v: u32, s: f32) -> u32 {
    ((v as f32) * s).round().max(1.0) as u32
}

#[derive(Debug, Clone, Default)]
pub struct Bubble {
    /// 解析后的主题(colors/radius/shadow/state accents)。
    pub theme: BubbleTheme,
    pub text: BubbleText,
    pub w: u32,
    pub h: u32,
    /// Height of the whole header block in px (both rows, measured at
    /// layout time).
    pub header_h: u32,
    /// Height of the "From <client>" meta row in px (0 = no From row).
    pub from_h: u32,
}

/// [thinking, working, done, fail, attention, neutral] 的色板索引。
fn state_idx(mode: Mode) -> usize {
    match mode {
        Mode::Thinking => 0,
        Mode::Working => 1,
        Mode::Done => 2,
        Mode::Failed => 3,
        Mode::Attention => 4,
        _ => 5,
    }
}

impl Bubble {
    pub fn visible(&self) -> bool {
        !self.text.title.is_empty()
    }

    /// Measure the bubble for `text`. The box has a UNIFORM fixed size
    /// (the enlarged 1.5× phone screen, capped by the pet frame) that never
    /// changes with the message; the content is laid out inside it — the
    /// header row on top, then the divider, then the stream (wrapped,
    /// trailing lines dropped with "…", and a streaming single line is
    /// tail-fitted: newest chars kept, oldest dropped with a leading "…").
    pub fn layout(&mut self, text: BubbleText, pet_w: u32, pet_h: u32, comp: &Compositor) -> bool {
        let s = comp.dpi_scale();
        // fixed size regardless of the message
        let min_w = scaled(MIN_BUBBLE_W, s);
        let w = scaled(MAX_BUBBLE_W, s).min(pet_w.max(min_w));
        // 高度上限 = 动图高 − 顶部边距:气泡不得越过本体下缘,否则会被
        // 窗口底部裁掉。小 scale / 高 DPI 下可用高度小于设计高时按上限
        // 收缩——原来的 .max(MIN_BUBBLE_H) 会把高度回弹到设计值(350×DPI),
        // 导致气泡下缘 394×DPI 超过动图高 800×scale 而被截断
        // (150% DPI 时 scale ≤ 0.74 即触发,下缘被切掉 (394×1.5−800×s)px)。
        let h = scaled(MAX_BUBBLE_H, s).min(pet_h.saturating_sub(scaled(BUBBLE_MARGIN_Y, s)));
        // 文本与几何都未变才跳过:几何(宠物缩放 / DPI)变化时旧的大气泡
        // 会把下段顶出窗口底部被截断,必须重排内容(重新按新预算截行)。
        if text == self.text && w == self.w && h == self.h {
            return false;
        }
        if text.title.is_empty() {
            let changed = !self.text.title.is_empty() || self.w != 0 || self.h != 0;
            self.text = text;
            self.w = 0;
            self.h = 0;
            self.header_h = 0;
            self.from_h = 0;
            return changed;
        }
        let pad_x = scaled(PAD_X, s);
        let pad_top = scaled(PAD_TOP, s);
        let pad_bottom = scaled(PAD_BOTTOM, s);
        let text_w = w.saturating_sub(pad_x * 2).max(scaled(72, s));
        let text_h = h.saturating_sub(pad_top + pad_bottom).max(scaled(24, s));

        // header block above the divider: "From <client>" meta row on top,
        // the state title row below it; the block height is the sum (both
        // single lines in practice)
        let title_h = comp.measure_text(text_w, &[text.title.clone()]);
        let from_h = text
            .from
            .as_ref()
            .map(|f| comp.measure_text(text_w, &[format!("From {f}")]))
            .unwrap_or(0);
        // 药丸与标题行之间的间距(仅在存在 From 行时计入,保证正文区域同步缩小)
        let pill_gap = if text.from.is_some() { scaled(PILL_GAP, s) } else { 0 };
        let header_h = (title_h + from_h + pill_gap).max(1);

        // stream area = text area minus header row and the divider block
        let gap_top = scaled(DIVIDER_GAP_TOP, s);
        let gap_bot = scaled(DIVIDER_GAP_BOTTOM, s);
        let divider_h = scaled(DIVIDER_H, s);
        let budget = text_h
            .saturating_sub(header_h + gap_top + divider_h + gap_bot)
            .max(scaled(16, s));

        // fit the content into the fixed area without resizing the box
        let mut shown = text.lines;
        let mut measured = comp.measure_text(text_w, &shown);
        if shown.len() == 1 && measured > budget {
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
                if comp.measure_text(text_w, &[suffix]) <= budget {
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
                if comp.measure_text(text_w, &[line.clone()]) > budget && keep > 1 {
                    line = String::from("…");
                    line.extend(chars[n - (keep - 1)..].iter());
                }
                shown[0] = line;
                measured = comp.measure_text(text_w, &shown).min(budget);
            }
        }
        if measured > budget {
            // multi-line message too tall: drop trailing lines, append "…"
            while measured > budget && shown.len() > 1 {
                shown.pop();
                measured = comp.measure_text(text_w, &shown);
            }
            if measured > budget {
                // single over-long line: keep it but accept the clipped cap
            } else {
                shown.push("…".to_string());
            }
        }
        self.text = BubbleText { title: text.title, from: text.from, lines: shown };
        self.w = w;
        self.h = h;
        self.header_h = header_h;
        self.from_h = from_h;
        true
    }

    /// Draw at (x, y): soft floating shadow, 1px light-gray border,
    /// translucent white fill, then the header row (state title left, the
    /// "From …" pill also left-aligned below it), the divider, and the
    /// stream. Drawn before the pet sprite, so the body may cover part of it.
    /// Draw at (x+sx, y) with the resolved theme: soft floating shadow,
    /// micro border, translucent fill, a 4px state accent bar on the left,
    /// the header (state title left — colored for done/fail/attention — and
    /// the "From …" source pill at the right), the divider, then the stream.
    /// `appear` (0..1) fades/slides the whole card in (auto-hide transition
    /// friendly). Drawn before the pet sprite, so the body may cover part.
    pub fn draw(&self, comp: &mut Compositor, x: i32, y: i32, mode: Mode, appear: f32) {
        let appear = appear.clamp(0.0, 1.0);
        let s = comp.dpi_scale();
        let pad_x = scaled(PAD_X, s);
        let pad_top = scaled(PAD_TOP, s);
        let pad_bottom = scaled(PAD_BOTTOM, s);
        let radius = scaled(self.theme.radius, s);
        let border = scaled(1, s);
        let accent = self.theme.state[state_idx(mode)];
        let ta = (appear * 255.0) as u8;
        // soft floating shadow
        let sdx = (SHADOW_OFFSET_X as f32 * s).round() as i32;
        let sdy = (SHADOW_OFFSET_Y as f32 * s).round() as i32;
        comp.soft_shadow(
            x,
            y,
            self.w,
            self.h,
            radius,
            sdx,
            sdy,
            scaled(SHADOW_BLUR, s),
            (self.theme.shadow_alpha as f32 * appear) as u8,
        );
        // micro border
        comp.fill_round_rect(
            x, y, self.w, self.h, radius,
            rgb(self.theme.border),
            (self.theme.border_alpha as f32 * appear) as u8,
        );
        // translucent fill (inset by the border)
        let fill_w = self.w.saturating_sub(border * 2);
        let fill_h = self.h.saturating_sub(border * 2);
        if self.theme.acrylic {
            // 亚克力:截取气泡身后的桌面实时模糊着色(软件实现,Win10/11 + 分层窗口均有效);
            // 捕获失败(安全桌面等)时回退普通半透明填充,避免气泡"没有面板"
            if !comp.draw_acrylic_fill(
                x + border as i32,
                y + border as i32,
                fill_w,
                fill_h,
                radius.saturating_sub(1),
                scaled(14, s),
                rgb(self.theme.fill),
                0.35 * appear,
            ) {
                comp.fill_round_rect(
                    x + border as i32,
                    y + border as i32,
                    fill_w,
                    fill_h,
                    radius.saturating_sub(1),
                    rgb(self.theme.fill),
                    (self.theme.fill_alpha as f32 * appear) as u8,
                );
            }
        } else {
            comp.fill_round_rect(
                x + border as i32,
                y + border as i32,
                fill_w,
                fill_h,
                radius.saturating_sub(1),
                rgb(self.theme.fill),
                (self.theme.fill_alpha as f32 * appear) as u8,
            );
        }
        let tw = self.w.saturating_sub(pad_x * 2);
        let th = self.h.saturating_sub(pad_top + pad_bottom);
        let tx = x + pad_x as i32;
        let ty = y + pad_top as i32;
        let header_h = self.header_h.max(1);

        // header block: "From <client>" pill on top, the state title below
        let mut cy = ty;
        if let Some(f) = &self.text.from {
            let fh = self.from_h.max(1);
            let label = format!("From {f}");
            // source pill: state-colored, white bold text。宽度按**粗体**测量
            // (常规字体测量会让粗体换行截断);2×8px 内边距,rect 高度足够,
            // 文字下缘不会被裁掉。
            let pad = scaled(8, s);
            let pw = comp.text_width_bold(&label).saturating_add(pad * 2);
            let ph = fh.saturating_add(scaled(4, s));
            let px = tx + ((tw as i32 - pw as i32) / 2).max(0); // 水平居中
            let py = cy.saturating_sub(scaled(2, s) as i32);
            comp.fill_round_rect(
                px,
                py,
                pw,
                ph,
                ph / 2,
                rgb(accent),
                (230.0 * appear) as u8,
            );
            comp.draw_text_alpha(
                px + pad as i32,
                py + scaled(2, s) as i32,
                pw.saturating_sub(pad * 2).saturating_add(scaled(4, s)),
                ph.saturating_sub(scaled(2, s)),
                &[label],
                (255, 255, 255),
                false,
                true,
                true, // 单行:Width 测到正好时 GDI 换行会把第二行挤出药丸
                appear,
            );
            cy += fh as i32;
            cy += scaled(PILL_GAP, s) as i32; // 药丸与标题行的间距
        }
        let title_h = header_h.saturating_sub(self.from_h).max(1);
        let title_color = match mode {
            Mode::Done | Mode::Failed | Mode::Attention => accent,
            _ => self.theme.title,
        };
        comp.draw_text_alpha(
            tx,
            cy,
            tw,
            title_h,
            &[self.text.title.clone()],
            rgb(title_color),
            false,
            true,
            false,
            appear,
        );
        let _ = ta;
        if self.text.lines.is_empty() {
            return;
        }
        // divider under the header row
        let gap_top = scaled(DIVIDER_GAP_TOP, s);
        let gap_bot = scaled(DIVIDER_GAP_BOTTOM, s);
        let divider_h = scaled(DIVIDER_H, s);
        let dy = ty + header_h as i32 + gap_top as i32;
        comp.fill_round_rect(
            tx,
            dy,
            tw,
            divider_h,
            0,
            rgb(self.theme.divider),
            (self.theme.divider_alpha as f32 * appear) as u8,
        );
        // message stream below the divider
        let cy = dy + divider_h as i32 + gap_bot as i32;
        let ch = th.saturating_sub(header_h + gap_top + divider_h + gap_bot);
        comp.draw_text_alpha(
            tx,
            cy,
            tw,
            ch,
            &self.text.lines,
            rgb(self.theme.title),
            false,
            false,
            false,
            appear,
        );
    }
}

#[inline]
fn rgb(c: dshpet::config::Rgb) -> (u8, u8, u8) {
    (c.r, c.g, c.b)
}
