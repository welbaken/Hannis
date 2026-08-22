//! Compositor: RGBA frame buffer (premultiplied BGRA) + text, uploaded
//! with UpdateLayeredWindow. Cross-fade and grayscale are applied in the
//! pixel loop; the buffer is never cleared to an opaque color, so
//! animation switches cannot flash (plan §5.6).
//!
//! Text quality: glyphs are rasterized by GDI at 2× resolution and
//! box-filtered back down to 1× (the "render big, shrink" supersampling
//! trick) — edges come out smoother and more solid than a single native
//! pass, with no extra fonts or dependencies (the system font stack via
//! GDI font linking stays: any CJK/emoji glyph the OS has renders fine).
//! The finished text block is cached and only re-rasterized when content,
//! size or colors change. ClearType subpixel AA is deliberately NOT used:
//! on a transparent layered window the RGB subpixel fringes would show as
//! color halos over the varying desktop behind the pet, so grayscale AA
//! is the correct mode here.

use dshpet::anim::Frame;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

pub struct Compositor {
    pub win_w: u32,
    pub win_h: u32,
    /// Rows at the bottom of the buffer that draw_frame must skip (auto-hide
    /// 收起时:被任务栏盖住的身体部分留透明,让任务栏透出;0 = 不裁剪)。
    pub clip_bottom: i32,
    hwnd: HWND,
    dc: HDC,
    dib: HBITMAP,
    bits_ptr: *mut u8,
    bits_len: usize,
    font: HFONT,
    screen_dc: HDC,
    dpi_scale: f32,
    /// 2× supersampled offscreen rasterization for text. GDI leaves
    /// alpha=0 on 32bpp DIBs, so glyph coverage comes from a white-on-
    /// black pass and premultiplied color from a color-on-black pass;
    /// both are rendered at 2× and box-filtered down to 1× in
    /// [`Compositor::raster_text`].
    text_dc: HDC,
    text_dib: HBITMAP,
    text_bits: *mut u8,
    text_w: u32,
    text_h: u32,
    /// 2×-size font for the supersampled text passes (same face and
    /// quality as `font`, double height).
    font2: HFONT,
    /// Bold variants (weight 700) for the bubble header row above the
    /// divider — same sizes, regular and 2×.
    font_bold: HFONT,
    font2_bold: HFONT,
    /// Cached rasterized text block (1×, premultiplied BGRA with alpha ==
    /// coverage). Rebuilt only when the key changes, composited per frame.
    text_cache: Option<TextCache>,
    /// Scratch buffers reused across re-rasterizations (no realloc on
    /// every typewriter tick).
    cov2: Vec<u8>,
    col2: Vec<u8>,
    /// Scratch buffers for the soft-shadow blur.
    shadow_cov: Vec<u8>,
    shadow_tmp: Vec<u8>,
    /// Global alpha applied to the next composite_block(draw_text_alpha 用)。
    raster_alpha: f32,
    /// 窗口屏幕原点(软件亚克力截屏坐标用;compose 时更新)。
    screen_pos: (i32, i32),
    /// 亚克力缓存:(w, h, BGRA)上次截屏+模糊+着色结果。
    acrylic_cache: Option<(u32, u32, Vec<u8>)>,
    /// 上次截屏时刻(内部 ~150ms 节流)。
    acrylic_last: Option<std::time::Instant>,
    /// 上次 present 的完整帧(预乘 BGRA)+ 当时的窗口屏幕原点。
    /// 亚克力截屏得到的是"桌面 + 宠物自己上一帧"的混合画面,逐像素
    /// 反混合即可还原桌面原色——否则反馈循环会在几帧内把亚克力洗成
    /// 一片均匀平色(看起来就是普通半透明,而非模糊玻璃)。
    last_frame: Option<Vec<u8>>,
    last_pos: (i32, i32),
}

fn premul(bg: u8, a: u8) -> u8 {
    ((bg as u32 * a as u32 + 127) / 255) as u8
}

/// (r, g, b) -> COLORREF (0x00BBGGRR).
fn colorref(c: (u8, u8, u8)) -> u32 {
    ((c.2 as u32) << 16) | ((c.1 as u32) << 8) | (c.0 as u32)
}

/// Sliding-window box blur, horizontal pass: dst[x] = mean of src over
/// [x-r, x+r] clamped to the row bounds. O(w) per row.
fn box_blur_h(src: &[u8], w: usize, h: usize, r: usize, dst: &mut [u8]) {
    for y in 0..h {
        let row = &src[y * w..(y + 1) * w];
        let drow = &mut dst[y * w..(y + 1) * w];
        let mut sum: u32 = 0;
        let mut count: u32 = 0;
        let hi0 = r.min(w - 1);
        for i in 0..=hi0 {
            sum += row[i] as u32;
            count += 1;
        }
        for x in 0..w {
            drow[x] = (sum / count) as u8;
            // slide the window right by one
            if x >= r {
                sum -= row[x - r] as u32;
                count -= 1;
            }
            if x + r + 1 < w {
                sum += row[x + r + 1] as u32;
                count += 1;
            }
        }
    }
}

/// Sliding-window box blur, vertical pass (column-wise analogue).
fn box_blur_v(src: &[u8], w: usize, h: usize, r: usize, dst: &mut [u8]) {
    for x in 0..w {
        let mut sum: u32 = 0;
        let mut count: u32 = 0;
        let hi0 = r.min(h - 1);
        for i in 0..=hi0 {
            sum += src[i * w + x] as u32;
            count += 1;
        }
        for y in 0..h {
            dst[y * w + x] = (sum / count) as u8;
            if y >= r {
                sum -= src[(y - r) * w + x] as u32;
                count -= 1;
            }
            if y + r + 1 < h {
                sum += src[(y + r + 1) * w + x] as u32;
                count += 1;
            }
        }
    }
}

/// Cache key for a rasterized text block: content, target size, style.
#[derive(Clone, PartialEq, Eq, Debug)]
struct CacheKey {
    lines: Vec<String>,
    w: u32,
    h: u32,
    style: CacheStyle,
}

/// Text style: fill color + alignment + weight (the outlined behind-the-pet
/// renderer was removed; only bubble text remains). `bold` selects the
/// weight-700 font for the header row above the divider; `single` draws the
/// line non-wrapping (DT_SINGLELINE), used by the "From …" pill so GDI can
/// never force a word wrap at the exact-fit width.
#[derive(Clone, PartialEq, Eq, Debug)]
enum CacheStyle {
    Plain { fill: (u8, u8, u8), right: bool, bold: bool, single: bool },
}

/// Rasterized text block: 1× premultiplied BGRA, alpha == glyph coverage.
struct TextCache {
    key: CacheKey,
    w: u32,
    h: u32,
    bgra: Vec<u8>,
}

impl Compositor {
    pub fn new(hwnd: HWND, font_scale: f32) -> Compositor {
        let screen_dc = unsafe { GetDC(HWND::default()) };
        let dc = unsafe { CreateCompatibleDC(screen_dc) };
        let text_dc = unsafe { CreateCompatibleDC(screen_dc) };
        // follow the system display scaling (100% = 96 DPI): the pet window
        // is drawn in physical pixels, so the bubble font must scale with DPI
        let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
        let dpi_scale = dpi as f32 / 96.0;
        let font_scale = font_scale.clamp(0.5, 2.5);
        let h1 = ((14.0 * dpi_scale * font_scale).round() as i32).max(9);
        let font = unsafe {
            CreateFontW(
                -h1,
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                OUT_DEFAULT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32,
                ANTIALIASED_QUALITY.0 as u32,
                (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
                w!("Microsoft YaHei UI"),
            )
        };
        // 2× font for the supersampled text passes
        let font2 = unsafe {
            CreateFontW(
                -(h1 * 2),
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                OUT_DEFAULT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32,
                ANTIALIASED_QUALITY.0 as u32,
                (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
                w!("Microsoft YaHei UI"),
            )
        };
        // bold variants (header row above the divider), 1× and 2×
        let font_bold = unsafe {
            CreateFontW(
                -h1,
                0,
                0,
                0,
                700,
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                OUT_DEFAULT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32,
                ANTIALIASED_QUALITY.0 as u32,
                (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
                w!("Microsoft YaHei UI"),
            )
        };
        let font2_bold = unsafe {
            CreateFontW(
                -(h1 * 2),
                0,
                0,
                0,
                700,
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                OUT_DEFAULT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32,
                ANTIALIASED_QUALITY.0 as u32,
                (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
                w!("Microsoft YaHei UI"),
            )
        };
        Compositor {
            win_w: 0,
            win_h: 0,
            clip_bottom: 0,
            hwnd,
            dc,
            dib: HBITMAP::default(),
            bits_ptr: std::ptr::null_mut(),
            bits_len: 0,
            font,
            screen_dc,
            dpi_scale,
            text_dc,
            text_dib: HBITMAP::default(),
            text_bits: std::ptr::null_mut(),
            text_w: 0,
            text_h: 0,
            font2,
            font_bold,
            font2_bold,
            text_cache: None,
            cov2: Vec::new(),
            col2: Vec::new(),
            shadow_cov: Vec::new(),
            shadow_tmp: Vec::new(),
            raster_alpha: 1.0,
            screen_pos: (0, 0),
            acrylic_cache: None,
            acrylic_last: None,
            last_frame: None,
            last_pos: (0, 0),
        }
    }

    /// System display scaling factor (1.0 at 100% DPI).
    pub fn dpi_scale(&self) -> f32 {
        self.dpi_scale
    }

    fn bits_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.bits_ptr, self.bits_len) }
    }

    fn bits(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.bits_ptr, self.bits_len) }
    }

    /// (Re)create the DIB backing store for the given window size.
    pub fn resize(&mut self, w: u32, h: u32) {
        if w == self.win_w && h == self.win_h && !self.bits().is_empty() {
            return;
        }
        self.win_w = w.max(1);
        self.win_h = h.max(1);
        if !self.dib.is_invalid() {
            unsafe {
                let _ = SelectObject(self.dc, GetStockObject(WHITE_BRUSH));
                let _ = DeleteObject(HGDIOBJ(self.dib.0));
            }
            self.dib = HBITMAP::default();
            self.bits_ptr = std::ptr::null_mut();
            self.bits_len = 0;
        }
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = self.win_w as i32;
        bmi.bmiHeader.biHeight = -(self.win_h as i32); // top-down
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB.0;
        let mut ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        unsafe {
            self.dib = CreateDIBSection(self.dc, &bmi, DIB_RGB_COLORS, &mut ptr, None, 0)
                .unwrap_or(HBITMAP::default());
            self.bits_ptr = ptr as *mut u8;
            self.bits_len = (self.win_w * self.win_h * 4) as usize;
            let _ = SelectObject(self.dc, self.dib);
        }
    }

    pub fn clear(&mut self) {
        self.bits_mut().fill(0);
    }

    /// Draw one RGBA or indexed frame at (x, y) with global alpha and
    /// optional grayscale. Composites "over" the premultiplied BGRA buffer:
    /// opaque sprite pixels cover what is underneath (the behind-the-pet
    /// text), transparent / antialiased pixels keep it — nothing below the
    /// sprite is erased.
    ///
    /// The pixel loop is scoped to the frame's cached alpha bbox: pixels
    /// outside it are fully transparent and cannot change the buffer, so
    /// skipping them is bit-identical and much cheaper for sprites with
    /// large transparent margins (the 800x800 canvas vs. the character).
    pub fn draw_frame(&mut self, f: &Frame, x: i32, y: i32, alpha: f32, grayscale: bool) {
        if f.w == 0 || f.h == 0 {
            return;
        }
        let alpha = alpha.clamp(0.0, 1.0);
        let indexed = f.rgba.is_empty();
        let (bx, by, bw, bh) = f.alpha_bbox().unwrap_or((0, 0, f.w as i32, f.h as i32));
        let fw = f.w as i32;
        let clip_top = (self.win_h as i32 - self.clip_bottom).max(0);
        for row in by..by + bh {
            let dst_row = y + row;
            if dst_row < 0 || dst_row >= self.win_h as i32 {
                continue;
            }
            if self.clip_bottom > 0 && dst_row >= clip_top {
                continue; // 收起状态:被任务栏盖住的部分留透明
            }
            let mut src_off = (row as usize) * fw as usize + bx as usize;
            // NOTE: bx must be scaled by 4 bytes/pixel — a raw `+ bx` here
            // shifts the whole draw left by (bx - bx/4) px per frame (each
            // frame's bbox differs, so the pet visibly jitters/drifts as the
            // animation plays).
            let mut dst_idx = ((dst_row as u32) * self.win_w * 4 + (bx.max(0) as u32) * 4) as usize;
            for col in bx..bx + bw {
                let dst_col = x + col;
                if dst_col >= 0 && dst_col < self.win_w as i32 {
                    let (r, g, b, a) = if indexed {
                        let pi = f.idx[src_off] as usize;
                        let a = if f.alpha.is_empty() {
                            f.pal_alpha[pi] as u32
                        } else {
                            f.alpha[src_off] as u32
                        };
                        (
                            f.palette[pi * 3] as u32,
                            f.palette[pi * 3 + 1] as u32,
                            f.palette[pi * 3 + 2] as u32,
                            a,
                        )
                    } else {
                        let o = src_off * 4;
                        (
                            f.rgba[o] as u32,
                            f.rgba[o + 1] as u32,
                            f.rgba[o + 2] as u32,
                            f.rgba[o + 3] as u32,
                        )
                    };
                    let (mut r, mut g, mut b, a) = (r, g, b, a);
                    if grayscale {
                        let l = (r * 2126 + g * 7152 + b * 722 + 5000) / 10000;
                        r = l;
                        g = l;
                        b = l;
                    }
                    let aa = ((a as f32 * alpha).round() as u32).min(255);
                    let sa = aa as u32;
                    let inv = 255 - sa;
                    let d = &mut self.bits_mut()[dst_idx..dst_idx + 4];
                    let a0 = d[3] as u32;
                    let out_a = sa + (a0 * inv + 127) / 255;
                    if out_a > 0 {
                        // proper "over" composite: opaque pet pixels cover the
                        // destination (e.g. the behind-the-pet text), while
                        // transparent / antialiased pixels blend over it —
                        // nothing below the sprite is erased
                        d[0] = (premul(b as u8, sa as u8) as u32 + (d[0] as u32 * inv + 127) / 255).min(255) as u8;
                        d[1] = (premul(g as u8, sa as u8) as u32 + (d[1] as u32 * inv + 127) / 255).min(255) as u8;
                        d[2] = (premul(r as u8, sa as u8) as u32 + (d[2] as u32 * inv + 127) / 255).min(255) as u8;
                        d[3] = out_a as u8;
                    }
                }
                src_off += 1;
                dst_idx += 4;
            }
        }
    }

    /// Fill a rounded rectangle with a translucent color (bubble background).
    /// Safe for any rect size: when the rect is smaller than 2x the radius
    /// the clamp bounds would invert (min > max) and `.clamp` panics, which
    /// crashed the app on the first bubble draw (small notch pill) — so the
    /// inner bounds are coerced to never invert.
    pub fn fill_round_rect(&mut self, x: i32, y: i32, w: u32, h: u32, radius: u32, color: (u8, u8, u8), alpha: u8) {
        if w == 0 || h == 0 {
            return;
        }
        let (x0, y0) = (x as i64, y as i64);
        let (x1, y1) = (x as i64 + w as i64 - 1, y as i64 + h as i64 - 1);
        let r = radius as i64;
        let r2 = r * r;
        let (cr, cg, cb) = color;
        let cx_lo = x0 + r;
        let cx_hi = (x1 - r).max(cx_lo);
        let cy_lo = y0 + r;
        let cy_hi = (y1 - r).max(cy_lo);
        for row in y.max(0)..(y + h as i32).min(self.win_h as i32) {
            let yy = row as i64;
            let mut dst = (row as u32 * self.win_w * 4) as usize + x.max(0) as usize * 4;
            for col in x.max(0)..(x + w as i32).min(self.win_w as i32) {
                let xx = col as i64;
                // rounded-corner SDF: inside if not in a corner region beyond radius
                let cx = xx.clamp(cx_lo, cx_hi);
                let cy = yy.clamp(cy_lo, cy_hi);
                let dx = xx - cx;
                let dy = yy - cy;
                let inside = dx * dx + dy * dy <= r2;
                if inside {
                    let d = &mut self.bits_mut()[dst..dst + 4];
                    let a0 = d[3] as u32;
                    let na = (alpha as u32).min(255);
                    let out_a = a0 + (na * (255 - a0) + 127) / 255;
                    if out_a > 0 {
                        d[0] = premul(cb, out_a as u8);
                        d[1] = premul(cg, out_a as u8);
                        d[2] = premul(cr, out_a as u8);
                        d[3] = out_a as u8;
                    }
                }
                dst += 4;
            }
        }
    }

    /// Draw a soft, blurred shadow of a rounded rect that spreads out in
    /// every direction — the "floating card" look (CSS `box-shadow:
    /// <dx> <dy> <blur> rgba(0,0,0,alpha)`). The shape is rasterized into a
    /// padded coverage buffer, blurred with two separable box-blur
    /// iterations (≈ Gaussian), then composited "over" the window buffer
    /// as black at `alpha` (0..=255). The halo extends `blur` px beyond
    /// the shape on every side, so it visibly diffuses up/left too when
    /// the blur exceeds the offset.
    pub fn soft_shadow(
        &mut self,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        radius: u32,
        dx: i32,
        dy: i32,
        blur: u32,
        alpha: u8,
    ) {
        if w == 0 || h == 0 || alpha == 0 {
            return;
        }
        let blur = blur.max(1) as i64;
        let pad = (blur + dx.max(0) as i64 + dy.max(0) as i64) as usize;
        let bw = w as usize + pad * 2;
        let bh = h as usize + pad * 2;
        self.shadow_cov.resize(bw * bh, 0);
        self.shadow_tmp.resize(bw * bh, 0);
        let cov = &mut self.shadow_cov;
        // rasterize the rounded-rect shape (hard edge; the blur softens it).
        // Every pixel is written, so stale data from a previous size never
        // leaks into the halo.
        let (sx0, sy0) = (pad as i64 + dx as i64, pad as i64 + dy as i64);
        let r = radius as i64;
        let r2 = r * r;
        let cx_lo = sx0 + r;
        let cx_hi = (sx0 + w as i64 - 1 - r).max(cx_lo);
        let cy_lo = sy0 + r;
        let cy_hi = (sy0 + h as i64 - 1 - r).max(cy_lo);
        for row in 0..bh {
            let yy = row as i64;
            let cy = yy.clamp(cy_lo, cy_hi);
            let dyy = yy - cy;
            for col in 0..bw {
                let xx = col as i64;
                let cx = xx.clamp(cx_lo, cx_hi);
                let dxx = xx - cx;
                cov[row * bw + col] = if dxx * dxx + dyy * dyy <= r2 { 255 } else { 0 };
            }
        }
        // two separable box-blur iterations ≈ Gaussian falloff
        let br = (blur / 2).max(1) as usize;
        {
            let (cov, tmp) = (&mut self.shadow_cov, &mut self.shadow_tmp);
            for _ in 0..2 {
                box_blur_h(cov, bw, bh, br, tmp);
                box_blur_v(tmp, bw, bh, br, cov);
            }
        }
        // composite: black at cov*alpha over the window buffer (clipped).
        // The blurred coverage is read through a raw pointer so the window
        // buffer can be borrowed mutably below (same pattern as the text
        // cache composite).
        let cov_ptr = self.shadow_cov.as_ptr();
        let a = alpha as u32;
        let ox = x - pad as i32;
        let oy = y - pad as i32;
        let x0 = ox.max(0);
        let y0 = oy.max(0);
        let x1 = (ox + bw as i32).min(self.win_w as i32);
        let y1 = (oy + bh as i32).min(self.win_h as i32);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let win_w = self.win_w as usize;
        let bits = self.bits_mut();
        for row in y0..y1 {
            let brow = (row - oy) as usize;
            let drow = row as usize;
            for col in x0..x1 {
                let bcol = (col - ox) as usize;
                let cv = unsafe { *cov_ptr.add(brow * bw + bcol) } as u32;
                if cv == 0 {
                    continue;
                }
                let sa = (cv * a + 127) / 255; // shadow alpha at this pixel
                let di = (drow * win_w + col as usize) * 4;
                let inv = 255 - sa;
                let d = &mut bits[di..di + 4];
                d[0] = ((d[0] as u32 * inv + 127) / 255) as u8;
                d[1] = ((d[1] as u32 * inv + 127) / 255) as u8;
                d[2] = ((d[2] as u32 * inv + 127) / 255) as u8;
                d[3] = (sa + (d[3] as u32 * inv + 127) / 255).min(255) as u8;
            }
        }
    }

    /// (Re)create the 2× text DIB big enough for (w, h).
    fn ensure_text_dib(&mut self, w: u32, h: u32) {
        if w <= self.text_w && h <= self.text_h && !self.text_dib.is_invalid() {
            return;
        }
        if !self.text_dib.is_invalid() {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(self.text_dib.0));
            }
            self.text_dib = HBITMAP::default();
            self.text_bits = std::ptr::null_mut();
        }
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = w.max(1) as i32;
        bmi.bmiHeader.biHeight = -(h.max(1) as i32); // top-down
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB.0;
        let mut ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        unsafe {
            self.text_dib = CreateDIBSection(self.text_dc, &bmi, DIB_RGB_COLORS, &mut ptr, None, 0)
                .unwrap_or(HBITMAP::default());
            self.text_bits = ptr as *mut u8;
            self.text_w = w.max(1);
            self.text_h = h.max(1);
            let _ = SelectObject(self.text_dc, self.text_dib);
        }
    }

    /// Rasterize one text pass into the 2× text DIB: `color` on black, in
    /// DIB-relative coordinates (the offscreen DIB is only w×h, so the
    /// origin maps to the target rect's top-left). With GDI grayscale
    /// antialiasing, a white-on-black pass yields per-pixel glyph coverage
    /// in the gray level, and a color-on-black pass yields the premultiplied
    /// glyph color (pixel = coverage * color).
    ///
    /// `dc`/`font` are the 2× rasterization DC and font; `dx`/`dy` shift
    /// the drawn glyphs by a few pixels (the 2× padding offset for the
    /// outlined halo; GDI clips shifted draws at the DIB bounds, so no
    /// buffer overrun). `right` right-aligns the lines inside the rect
    /// (DT_RIGHT) instead of left-aligning them.
    ///
    /// Each line is measured first and drawn with a 2px bottom slack, so the
    /// last line's descenders are never clipped by the area boundary.
    /// `single` = DT_SINGLELINE (no word wrap; for the "From …" pill, whose
    /// rect is sized to the text: a wrap would push the second line below
    /// the pill and show only the first word).
    fn text_pass(dc: HDC, font: HFONT, color: u32, w: u32, h: u32, lines: &[String], dx: i32, dy: i32, right: bool, single: bool) {
        unsafe {
            let _ = SetBkMode(dc, TRANSPARENT);
            let _ = SetTextColor(dc, COLORREF(color)); // 0x00BBGGRR
            let _ = SelectObject(dc, font);
        }
        let area_h = h as i32;
        let mut cy = 0i32;
        for line in lines {
            let mut text: Vec<u16> = line.encode_utf16().collect();
            // measure this line's wrapped height at the same width
            let mut mrc = RECT { left: 0, top: 0, right: w as i32, bottom: 0 };
            unsafe {
                let _ = DrawTextW(dc, &mut text, &mut mrc, DT_WORDBREAK | DT_NOPREFIX | DT_CALCRECT);
            }
            let line_h = if single {
                // 单行:一行即全部高度(不换行,测高用 DT_SINGLELINE)
                let mut src: Vec<u16> = line.encode_utf16().collect();
                let mut rc2 = RECT { left: 0, top: 0, right: w as i32, bottom: 0 };
                unsafe {
                    let _ = DrawTextW(dc, &mut src, &mut rc2, DT_SINGLELINE | DT_NOPREFIX | DT_CALCRECT);
                }
                (rc2.bottom - rc2.top).max(1)
            } else {
                (mrc.bottom - mrc.top).max(1)
            };
            // draw fully, plus a little slack so the final line is not cut
            let mut rc = RECT {
                left: dx,
                top: cy + dy,
                right: w as i32 + dx,
                bottom: (cy + line_h + 2 + dy).min(area_h),
            };
            let mut text: Vec<u16> = line.encode_utf16().collect();
            let flags = if single {
                DT_SINGLELINE | DT_NOPREFIX | DT_LEFT
            } else {
                DT_WORDBREAK | DT_NOPREFIX | (if right { DT_RIGHT } else { DT_LEFT })
            };
            unsafe {
                let _ = DrawTextW(dc, &mut text, &mut rc, flags);
            }
            cy += line_h;
            if cy >= area_h {
                break;
            }
        }
    }

    /// Draw text lines inside (x, y, w, h): 2× supersampled GDI
    /// rasterization, cached, composited with correct per-pixel coverage
    /// (see [`Compositor::raster_text`]). `fill` is the text color
    /// (r, g, b); `right` right-aligns the lines inside the rect; `bold`
    /// renders with the weight-700 font (bubble header row).
    pub fn draw_text(&mut self, x: i32, y: i32, w: u32, h: u32, lines: &[String], fill: (u8, u8, u8), right: bool, bold: bool) {
        self.draw_text_alpha(x, y, w, h, lines, fill, right, bold, false, 1.0)
    }

    /// `alpha`(0..1)整体缩放文字的预乘颜色与覆盖度(气泡淡入/淡出用)。
    /// `single` = 不换行绘制(来源药丸用;DT_SINGLELINE,避免 GDI 在
    /// 恰好等宽的边界上把词组换行)。
    pub fn draw_text_alpha(&mut self, x: i32, y: i32, w: u32, h: u32, lines: &[String], fill: (u8, u8, u8), right: bool, bold: bool, single: bool, alpha: f32) {
        if w == 0 || h == 0 || lines.is_empty() {
            self.text_cache = None;
            return;
        }
        let style = CacheStyle::Plain { fill, right, bold, single };
        self.raster_text(w, h, lines, style);
        let scale = alpha.clamp(0.0, 1.0);
        self.raster_alpha = scale;
        let (ptr, cw, ch) = {
            let c = self.text_cache.as_ref().unwrap();
            (c.bgra.as_ptr(), c.w, c.h)
        };
        self.composite_block(x, y, ptr, cw, ch);
    }

    /// Rasterize `lines` into the cached 1× text block (premultiplied
    /// BGRA, alpha == coverage) when the cache key changed.
    ///
    /// Supersampling: every GDI pass runs at 2× resolution into the 2×
    /// text DIB, then the coverage/color planes are box-filtered down to
    /// 1×. The 2×→1× average is a 4-tap AA filter, so glyph edges come
    /// out smoother and more solid than a single native pass — the cheap
    /// "render big, shrink" contrast trick.
    fn raster_text(&mut self, w: u32, h: u32, lines: &[String], style: CacheStyle) {
        let key = CacheKey { lines: lines.to_vec(), w, h, style };
        if let Some(c) = &self.text_cache {
            if c.key == key {
                return;
            }
        }
        let right = match &key.style {
            CacheStyle::Plain { right, .. } => *right,
        };
        let bold = match &key.style {
            CacheStyle::Plain { bold, .. } => *bold,
        };
        let single = match &key.style {
            CacheStyle::Plain { single, .. } => *single,
        };
        let font2 = if bold { self.font2_bold } else { self.font2 };
        let w2 = w as usize * 2;
        let h2 = h as usize * 2;
        self.ensure_text_dib(w2 as u32, h2 as u32);
        let tw = self.text_w as usize;
        let th = self.text_h as usize;
        let tbits = unsafe { std::slice::from_raw_parts_mut(self.text_bits, tw * th * 4) };

        // pass 1: true coverage mask at 2× (white on black)
        tbits.fill(0);
        Self::text_pass(self.text_dc, font2, 0xFF_FF_FF, w2 as u32, h2 as u32, lines, 0, 0, right, single);
        self.cov2.resize(tw * th, 0);
        for row in 0..th {
            let mut i = row * tw * 4;
            for col in 0..tw {
                self.cov2[row * tw + col] = tbits[i]; // R channel of the gray pixel
                i += 4;
            }
        }

        // pass 2: premultiplied glyph color at 2× (fill on black)
        let fill = match &key.style {
            CacheStyle::Plain { fill, .. } => *fill,
        };
        tbits.fill(0);
        Self::text_pass(self.text_dc, font2, colorref(fill), w2 as u32, h2 as u32, lines, 0, 0, right, single);
        self.col2.resize(tw * th * 3, 0);
        for row in 0..th {
            let mut i = row * tw * 4;
            for col in 0..tw {
                let o = (row * tw + col) * 3;
                self.col2[o] = tbits[i];
                self.col2[o + 1] = tbits[i + 1];
                self.col2[o + 2] = tbits[i + 2];
                i += 4;
            }
        }

        // box-filter 2×→1× and build the premultiplied block
        let cw = w as usize;
        let ch = h as usize;
        let mut block: Vec<u8> = Vec::with_capacity(cw * ch * 4);
        for j in 0..ch {
            let sy0 = j * 2;
            let sy1 = (sy0 + 1).min(th - 1);
            for i in 0..cw {
                let sx0 = i * 2;
                let sx1 = (sx0 + 1).min(tw - 1);
                let i00 = sy0 * tw + sx0;
                let i01 = sy0 * tw + sx1;
                let i10 = sy1 * tw + sx0;
                let i11 = sy1 * tw + sx1;
                let avg = |a: usize, b: usize, c: usize, d: usize| (a + b + c + d + 2) / 4;
                let fa = avg(
                    self.cov2[i00] as usize,
                    self.cov2[i01] as usize,
                    self.cov2[i10] as usize,
                    self.cov2[i11] as usize,
                ) as u8;
                let cb = avg(
                    self.col2[i00 * 3] as usize,
                    self.col2[i01 * 3] as usize,
                    self.col2[i10 * 3] as usize,
                    self.col2[i11 * 3] as usize,
                ) as u8;
                let cg = avg(
                    self.col2[i00 * 3 + 1] as usize,
                    self.col2[i01 * 3 + 1] as usize,
                    self.col2[i10 * 3 + 1] as usize,
                    self.col2[i11 * 3 + 1] as usize,
                ) as u8;
                let cr = avg(
                    self.col2[i00 * 3 + 2] as usize,
                    self.col2[i01 * 3 + 2] as usize,
                    self.col2[i10 * 3 + 2] as usize,
                    self.col2[i11 * 3 + 2] as usize,
                ) as u8;
                block.extend_from_slice(&[cb, cg, cr, fa]);
            }
        }
        self.text_cache = Some(TextCache { key, w, h, bgra: block });
    }

    /// Composite the cached text block (premultiplied BGRA, alpha ==
    /// coverage) at (x, y) with the "over" operator.
    fn composite_block(&mut self, x: i32, y: i32, src: *const u8, w: u32, h: u32) {
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w as i32).min(self.win_w as i32);
        let y1 = (y + h as i32).min(self.win_h as i32);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let win_w = self.win_w as usize;
        let cw = w as usize;
        let scale = self.raster_alpha.clamp(0.0, 1.0);
        let bits = self.bits_mut();
        let sa = (scale * 255.0) as u32;
        for row in y0..y1 {
            let srow = (row - y) as usize;
            let drow = row as usize;
            for col in x0..x1 {
                let scol = (col - x) as usize;
                let si = (srow * cw + scol) * 4;
                let a = unsafe { *src.add(si + 3) } as u32 * sa / 255;
                if a == 0 {
                    continue;
                }
                let di = (drow * win_w + col as usize) * 4;
                let inv = 255 - a;
                let d = &mut bits[di..di + 4];
                let sc = |v: u8| (v as u32 * sa / 255) as u8;
                d[0] = (sc(unsafe { *src.add(si) }) as u32 + (d[0] as u32 * inv + 127) / 255).min(255) as u8;
                d[1] = (sc(unsafe { *src.add(si + 1) }) as u32 + (d[1] as u32 * inv + 127) / 255).min(255) as u8;
                d[2] = (sc(unsafe { *src.add(si + 2) }) as u32 + (d[2] as u32 * inv + 127) / 255).min(255) as u8;
                d[3] = (a + (d[3] as u32 * inv + 127) / 255).min(255) as u8;
            }
        }
    }

    /// 软件亚克力填充:截取**气泡身后的屏幕**(窗口坐标 → 屏幕坐标),盒式模糊
    /// RGB 三通道,与 tint 按 tint_alpha 混合,再按圆角遮罩 "over" 合成进窗口
    /// 缓冲。内部 ~150ms 节流截屏(静止期复用缓存),因此对 Win10/11 与
    /// UpdateLayeredWindow 分层窗口都有效(未文档化的 DWM accent 在 Win11
    /// 分层窗口上大多静默失效)。
    ///
    /// 截屏得到的是"桌面 + 宠物上一帧"的混合,捕获后先按上一帧逐像素
    /// 反混合还原桌面,再模糊——没有反馈循环,玻璃质感才能一直保持
    /// (否则几帧内收敛成纯平色)。
    ///
    /// 返回 false = 捕获失败(安全桌面等),本帧未绘制;调用方应回退到
    /// 普通半透明填充。
    pub fn draw_acrylic_fill(
        &mut self,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        radius: u32,
        blur: u32,
        tint: (u8, u8, u8),
        tint_alpha: f32,
    ) -> bool {
        if w == 0 || h == 0 {
            return false;
        }
        let now = std::time::Instant::now();
        let refresh = self
            .acrylic_last
            .map(|t| now.duration_since(t) >= std::time::Duration::from_millis(150))
            .unwrap_or(true);
        if refresh {
            if let Some((cw, ch, px)) = self.capture_acrylic(x, y, w, h, blur, tint, tint_alpha) {
                self.acrylic_cache = Some((cw, ch, px));
            } else {
                self.acrylic_last = Some(now);
                return false;
            }
            self.acrylic_last = Some(now);
        }
        let Some((cw, ch, px)) = self
            .acrylic_cache
            .as_ref()
            .map(|(w, h, p)| (*w, *h, p.as_ptr()))
        else {
            return false;
        };
        // 圆角遮罩 + over 合成(corners 与 fill_round_rect 一样为 SDF 硬边,
        // 1px 边框盖在边缘上)
        let r = radius.max(1) as i64;
        let r2 = r * r;
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + cw as i32).min(self.win_w as i32);
        let y1 = (y + ch as i32).min(self.win_h as i32);
        if x1 <= x0 || y1 <= y0 {
            return false;
        }
        let cx_lo = x0 as i64 + r;
        let cx_hi = (x1 as i64 - 1 - r).max(cx_lo);
        let cy_lo = y0 as i64 + r;
        let cy_hi = (y1 as i64 - 1 - r).max(cy_lo);
        let win_w = self.win_w as usize;
        let bits = self.bits_mut();
        let scl = tint_alpha.clamp(0.0, 1.0);
        let sa = (scl * 255.0) as u32;
        for row in y0..y1 {
            let yy = row as i64;
            let cy2 = yy.clamp(cy_lo, cy_hi);
            let dyy = yy - cy2;
            let prow = (row - y) as usize;
            for col in x0..x1 {
                let xx = col as i64;
                let cx2 = xx.clamp(cx_lo, cx_hi);
                let dxx = xx - cx2;
                if dxx * dxx + dyy * dyy <= r2 {
                    let pcol = (col - x) as usize;
                    let si = (prow * cw as usize + pcol) * 4;
                    let di = (row as usize * win_w + col as usize) * 4;
                    let d = &mut bits[di..di + 4];
                    let a0 = d[3] as u32;
                    let out_a = sa + (a0 * (255 - sa) + 127) / 255;
                    if out_a > 0 {
                        let sc = |v: u32| (v * sa + 127) / 255;
                        let r = unsafe { *px.add(si) as u32 };
                        let g = unsafe { *px.add(si + 1) as u32 };
                        let b = unsafe { *px.add(si + 2) as u32 };
                        d[0] = (sc(b) + (d[0] as u32 * (255 - sa) + 127) / 255).min(255) as u8;
                        d[1] = (sc(g) + (d[1] as u32 * (255 - sa) + 127) / 255).min(255) as u8;
                        d[2] = (sc(r) + (d[2] as u32 * (255 - sa) + 127) / 255).min(255) as u8;
                        d[3] = out_a as u8;
                    }
                }
            }
        }
        true
    }

    /// 截取屏幕区域 → 分通道盒式模糊 → tint 混合;返回 (w, h, BGRA 不透明)。
    fn capture_acrylic(
        &mut self,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        blur: u32,
        tint: (u8, u8, u8),
        tint_alpha: f32,
    ) -> Option<(u32, u32, Vec<u8>)> {
        unsafe {
            let (ox, oy) = self.screen_pos;
            let sx = ox + x;
            let sy = oy + y;
            let screen_dc = GetDC(HWND::default());
            let mem_dc = CreateCompatibleDC(screen_dc);
            let mut bmi = BITMAPINFO::default();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = w as i32;
            bmi.bmiHeader.biHeight = -(h as i32);
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB.0;
            let mut ptr: *mut core::ffi::c_void = std::ptr::null_mut();
            let hbmp = CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut ptr, None, 0)
                .unwrap_or(HBITMAP::default());
            let _ = SelectObject(mem_dc, hbmp);
            let ok = BitBlt(mem_dc, 0, 0, w as i32, h as i32, screen_dc, sx, sy, SRCCOPY);
            if ok.is_err() || ptr.is_null() {
                let _ = DeleteObject(HGDIOBJ(hbmp.0));
                let _ = DeleteDC(mem_dc);
                let _ = ReleaseDC(HWND::default(), screen_dc);
                return None;
            }
            let n = (w as usize) * (h as usize);
            let raw = std::slice::from_raw_parts(ptr as *const u8, n * 4);
            // ---- 反混合:屏幕像素 = 桌面×(1-a) + 宠物上一帧(预乘色) ----
            // 逐像素解出被宠物盖住的桌面原色,消除"截到自己"的反馈。
            // 上一帧随窗口移动:屏幕坐标直接映射回上一帧的窗口坐标。
            let mut data = raw.to_vec();
            if let Some(prev) = self.last_frame.as_ref() {
                let win_w = self.win_w as i64;
                let win_h = self.win_h as i64;
                let (pxo, pyo) = self.last_pos;
                for row in 0..h as usize {
                    let py = sy as i64 + row as i64 - pyo as i64;
                    if py < 0 || py >= win_h {
                        continue;
                    }
                    let prow = py as usize;
                    for col in 0..w as usize {
                        let px = sx as i64 + col as i64 - pxo as i64;
                        if px < 0 || px >= win_w {
                            continue;
                        }
                        let pi = (prow * win_w as usize + px as usize) * 4;
                        let a = prev[pi + 3] as i64;
                        if a <= 0 || a >= 254 {
                            continue; // a≈255 无法反解:被本体完全盖住,反正不可见
                        }
                        let inv = 255 - a;
                        let di = (row * w as usize + col) * 4;
                        for c in 0..3 {
                            let s2 = raw[di + c] as i64;
                            let p2 = prev[pi + c] as i64;
                            let d2 = ((s2 - p2) * 255) / inv;
                            data[di + c] = if d2 < 0 { 0 } else if d2 > 255 { 255 } else { d2 as u8 };
                        }
                    }
                }
            }
            let mut planes = [vec![0u8; n], vec![0u8; n], vec![0u8; n]];
            let mut tmp = vec![0u8; n];
            for (ci, plane) in planes.iter_mut().enumerate() {
                for i in 0..n {
                    plane[i] = data[i * 4 + ci];
                }
                let r = (blur.max(1) / 2) as usize;
                box_blur_h(plane, w as usize, h as usize, r, &mut tmp);
                box_blur_v(&tmp, w as usize, h as usize, r, plane);
                box_blur_h(plane, w as usize, h as usize, r, &mut tmp);
                box_blur_v(&tmp, w as usize, h as usize, r, plane);
            }
            let t = tint_alpha.clamp(0.0, 1.0);
            let mut out = vec![0u8; n * 4];
            for i in 0..n {
                out[i * 4] = ((planes[0][i] as f32) * (1.0 - t) + tint.2 as f32 * t) as u8;
                out[i * 4 + 1] = ((planes[1][i] as f32) * (1.0 - t) + tint.1 as f32 * t) as u8;
                out[i * 4 + 2] = ((planes[2][i] as f32) * (1.0 - t) + tint.0 as f32 * t) as u8;
                out[i * 4 + 3] = 255;
            }
            let _ = DeleteObject(HGDIOBJ(hbmp.0));
            let _ = DeleteDC(mem_dc);
            let _ = ReleaseDC(HWND::default(), screen_dc);
            Some((w, h, out))
        }
    }

    /// 设置窗口屏幕原点(compose 时更新;亚克力截屏坐标用)。
    pub fn set_screen_pos(&mut self, x: i32, y: i32) {
        self.screen_pos = (x, y);
    }

    /// Measure the pixel width of one line (1× font; for pills/tags).
    pub fn text_width(&self, text: &str) -> u32 {
        unsafe {
            let _ = SelectObject(self.dc, self.font);
            let wide: Vec<u16> = text.encode_utf16().collect();
            let mut size = SIZE::default();
            let _ = GetTextExtentPoint32W(self.dc, &wide, &mut size);
            size.cx.max(0) as u32
        }
    }

    /// 与 `text_width` 相同但用粗体(来源胶囊等粗体文字的宽度测量)。
    pub fn text_width_bold(&self, text: &str) -> u32 {
        unsafe {
            let _ = SelectObject(self.dc, self.font_bold);
            let wide: Vec<u16> = text.encode_utf16().collect();
            let mut size = SIZE::default();
            let _ = GetTextExtentPoint32W(self.dc, &wide, &mut size);
            size.cx.max(0) as u32
        }
    }

    /// Measure total text height for lines wrapped at `w` px.
    pub fn measure_text(&self, w: u32, lines: &[String]) -> u32 {
        unsafe {
            let _ = SelectObject(self.dc, self.font);
        }
        let mut total = 0u32;
        for line in lines {
            let mut rc = RECT { left: 0, top: 0, right: w as i32, bottom: 0 };
            let mut text: Vec<u16> = line.encode_utf16().collect();
            unsafe {
                let _ = DrawTextW(self.dc, &mut text, &mut rc, DT_WORDBREAK | DT_NOPREFIX | DT_CALCRECT);
            }
            total += (rc.bottom - rc.top).max(1) as u32;
        }
        total
    }

    /// Upload the buffer with UpdateLayeredWindow (per-pixel alpha).
    pub fn present(&mut self) {
        if self.win_w == 0 || self.win_h == 0 {
            return;
        }
        unsafe {
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let size = SIZE { cx: self.win_w as i32, cy: self.win_h as i32 };
            let origin = POINT { x: 0, y: 0 };
            let _ = UpdateLayeredWindow(
                self.hwnd,
                self.screen_dc,
                None,
                Some(&size),
                self.dc,
                Some(&origin),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
        }
        // 记住"屏幕上正在显示的帧"与它的位置(亚克力反混合用)
        self.last_frame = Some(unsafe {
            std::slice::from_raw_parts(self.bits_ptr, self.bits_len).to_vec()
        });
        self.last_pos = self.screen_pos;
    }
}

impl Drop for Compositor {
    fn drop(&mut self) {
        unsafe {
            if !self.dib.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(self.dib.0));
            }
            if !self.text_dib.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(self.text_dib.0));
            }
            let _ = DeleteObject(HGDIOBJ(self.font.0));
            let _ = DeleteObject(HGDIOBJ(self.font2.0));
            let _ = DeleteObject(HGDIOBJ(self.font_bold.0));
            let _ = DeleteObject(HGDIOBJ(self.font2_bold.0));
            let _ = DeleteDC(self.dc);
            let _ = DeleteDC(self.text_dc);
            let _ = ReleaseDC(HWND::default(), self.screen_dc);
        }
    }
}

/// Cheap frame clone helper for cross-fade storage.
pub fn frame_clone(f: &Frame) -> Frame {
    Frame {
        w: f.w,
        h: f.h,
        idx: f.idx.clone(),
        palette: f.palette.clone(),
        pal_alpha: f.pal_alpha.clone(),
        alpha: f.alpha.clone(),
        rgba: f.rgba.clone(),
        bbox: f.bbox,
    }
}
