//! Compositor: RGBA frame buffer (premultiplied BGRA) + GDI text,
//! uploaded with UpdateLayeredWindow. Cross-fade and grayscale are applied
//! in the pixel loop; the buffer is never cleared to an opaque color, so
//! animation switches cannot flash (plan §5.6).

use dshpet::anim::Frame;
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

pub struct Compositor {
    pub win_w: u32,
    pub win_h: u32,
    hwnd: HWND,
    dc: HDC,
    dib: HBITMAP,
    bits_ptr: *mut u8,
    bits_len: usize,
    font: HFONT,
    screen_dc: HDC,
    dpi_scale: f32,
    /// Offscreen DIB used to rasterize bubble text with a proper alpha
    /// channel (GDI leaves alpha=0, so we extract coverage from a
    /// white-on-black pass and premultiplied color from a color-on-black
    /// pass, then composite with the "over" operator).
    text_dc: HDC,
    text_dib: HBITMAP,
    text_bits: *mut u8,
    text_w: u32,
    text_h: u32,
}

fn premul(bg: u8, a: u8) -> u8 {
    ((bg as u32 * a as u32 + 127) / 255) as u8
}

/// (r, g, b) -> COLORREF (0x00BBGGRR).
fn colorref(c: (u8, u8, u8)) -> u32 {
    ((c.2 as u32) << 16) | ((c.1 as u32) << 8) | (c.0 as u32)
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
        let font = unsafe {
            CreateFontW(
                -((14.0 * dpi_scale * font_scale).round() as i32).max(9),
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
        Compositor {
            win_w: 0,
            win_h: 0,
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

    /// Draw one RGBA frame at (x, y) with global alpha and optional grayscale.
    /// Composites "over" the premultiplied BGRA buffer: opaque sprite pixels
    /// cover what is underneath (the behind-the-pet text), transparent /
    /// antialiased pixels keep it — nothing below the sprite is erased.
    pub fn draw_frame(&mut self, f: &Frame, x: i32, y: i32, alpha: f32, grayscale: bool) {
        if f.w == 0 || f.h == 0 {
            return;
        }
        let alpha = alpha.clamp(0.0, 1.0);
        let mut src_idx = 0usize;
        for row in 0..f.h as i32 {
            let dst_row = y + row;
            if dst_row < 0 || dst_row >= self.win_h as i32 {
                src_idx += f.w as usize * 4;
                continue;
            }
            let mut dst_idx = ((dst_row as u32) * self.win_w * 4) as usize;
            for col in 0..f.w as i32 {
                let dst_col = x + col;
                if dst_col >= 0 && dst_col < self.win_w as i32 {
                    let (mut r, mut g, mut b, a) = (
                        f.rgba[src_idx] as u32,
                        f.rgba[src_idx + 1] as u32,
                        f.rgba[src_idx + 2] as u32,
                        f.rgba[src_idx + 3] as u32,
                    );
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
                src_idx += 4;
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

    /// (Re)create the offscreen text DIB big enough for (w, h).
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

    /// Draw one text pass into the text DIB: `color` on black, in
    /// DIB-relative coordinates (the offscreen DIB is only w×h, so the
    /// origin maps to the target rect's top-left). With GDI grayscale
    /// antialiasing, a white-on-black pass yields per-pixel glyph coverage
    /// in the gray level, and a color-on-black pass yields the premultiplied
    /// glyph color (pixel = coverage * color).
    ///
    /// `dx`/`dy` shift the drawn glyphs by a few pixels; the shifted copies
    /// are used to build an expanded coverage mask for the outline halo (and
    /// GDI clips shifted draws at the DIB bounds, so no buffer overrun).
    ///
    /// Each line is measured first and drawn with a 2px bottom slack, so the
    /// last line's descenders are never clipped by the area boundary.
    fn text_pass(&self, color: u32, w: u32, h: u32, lines: &[String], dx: i32, dy: i32) {
        unsafe {
            let _ = SetBkMode(self.text_dc, TRANSPARENT);
            let _ = SetTextColor(self.text_dc, COLORREF(color)); // 0x00BBGGRR
            let _ = SelectObject(self.text_dc, self.font);
        }
        let area_h = h as i32;
        let mut cy = 0i32;
        for line in lines {
            let mut text: Vec<u16> = line.encode_utf16().collect();
            // measure this line's wrapped height at the same width
            let mut mrc = RECT { left: 0, top: 0, right: w as i32, bottom: 0 };
            unsafe {
                let _ = DrawTextW(self.text_dc, &mut text, &mut mrc, DT_WORDBREAK | DT_NOPREFIX | DT_CALCRECT);
            }
            let line_h = (mrc.bottom - mrc.top).max(1);
            // draw fully, plus a little slack so the final line is not cut
            let mut rc = RECT {
                left: dx,
                top: cy + dy,
                right: w as i32 + dx,
                bottom: (cy + line_h + 2 + dy).min(area_h),
            };
            let mut text: Vec<u16> = line.encode_utf16().collect();
            unsafe {
                let _ = DrawTextW(self.text_dc, &mut text, &mut rc, DT_WORDBREAK | DT_NOPREFIX | DT_LEFT | DT_TOP);
            }
            cy += line_h;
            if cy >= area_h {
                break;
            }
        }
    }

    /// Draw bubble text lines inside (x, y, w, h).
    ///
    /// GDI writes alpha=0 on 32bpp DIBs, so a naive DrawText would be
    /// invisible (or need a crude "promote every edge pixel to opaque"
    /// fixup that destroys antialiasing and makes glyphs look jagged and
    /// thick). Instead we rasterize twice offscreen and composite with
    /// correct per-pixel coverage:
    ///   1. white text on black  -> gray level == glyph coverage
    ///   2. text color on black  -> premultiplied color
    ///   out = src + dst * (1 - src_a)   (both premultiplied, "over")
    pub fn draw_text(&mut self, x: i32, y: i32, w: u32, h: u32, lines: &[String]) {
        if w == 0 || h == 0 {
            return;
        }
        self.ensure_text_dib(w, h);
        let tw = self.text_w as usize;
        let th = self.text_h as usize;
        let tbits = unsafe { std::slice::from_raw_parts_mut(self.text_bits, tw * th * 4) };

        // pass 1: coverage mask (white on black); gray level == coverage
        tbits.fill(0);
        self.text_pass(0xFF_FF_FF, w, h, lines, 0, 0);
        let mut coverage: Vec<u8> = Vec::with_capacity(tw * th);
        for row in 0..th {
            let mut i = row * tw * 4;
            for _ in 0..tw {
                coverage.push(tbits[i]); // R channel of the gray pixel
                i += 4;
            }
        }

        // pass 2: premultiplied glyph color (dark text on black)
        tbits.fill(0);
        self.text_pass(0x00_26_26_26, w, h, lines, 0, 0);

        // composite: out = src + dst * (1 - src_a), both premultiplied
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w as i32).min(self.win_w as i32);
        let y1 = (y + h as i32).min(self.win_h as i32);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let win_w = self.win_w as usize;
        let bits = self.bits_mut();
        for row in y0..y1 {
            let trow = (row - y) as usize;
            let drow = row as usize;
            for col in x0..x1 {
                let tcol = (col - x) as usize;
                let ci = trow * tw + tcol;
                let a = coverage[ci] as u32;
                if a == 0 {
                    continue;
                }
                let ti = ci * 4;
                let di = (drow * win_w + col as usize) * 4;
                let inv = 255 - a;
                let d = &mut bits[di..di + 4];
                d[0] = (tbits[ti] as u32 + (d[0] as u32 * inv + 127) / 255).min(255) as u8;
                d[1] = (tbits[ti + 1] as u32 + (d[1] as u32 * inv + 127) / 255).min(255) as u8;
                d[2] = (tbits[ti + 2] as u32 + (d[2] as u32 * inv + 127) / 255).min(255) as u8;
                d[3] = (a + (d[3] as u32 * inv + 127) / 255).min(255) as u8;
            }
        }
    }

    /// Draw outlined (勾边) text lines inside (x, y, w, h) on top of the
    /// current buffer — the "behind the pet" renderer: no bubble chrome,
    /// just glyphs with a hard outline so they stay readable on any
    /// background (transparent desktop, light or dark apps, the pet body).
    ///
    /// Technique: the same two-pass GDI rasterization as [`draw_text`], plus
    /// an *expanded* coverage mask built by drawing the white glyphs again
    /// at every integer offset in the `r×r` square ring around the origin
    /// (8 draws for a 1px outline, 24 for 2px, 48 for 3px). The outline
    /// color is composited with that expanded coverage, then the fill color
    /// with the true coverage — the fill covers the glyph interior and the
    /// halo survives around the edges, with antialiased blends on both sides.
    pub fn draw_text_outlined(
        &mut self,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        lines: &[String],
        fill: (u8, u8, u8),
        outline: (u8, u8, u8),
        outline_w: u32,
    ) {
        if w == 0 || h == 0 || lines.is_empty() {
            return;
        }
        let r = outline_w.clamp(1, 3) as i32;
        // pad the offscreen raster by the halo radius so the outline is
        // never clipped at the text-rect edges (GDI would clip at the DIB
        // bounds otherwise)
        let pw = w + 2 * r as u32;
        let ph = h + 2 * r as u32;
        self.ensure_text_dib(pw, ph);
        let tw = self.text_w as usize;
        let th = self.text_h as usize;
        let tbits = unsafe { std::slice::from_raw_parts_mut(self.text_bits, tw * th * 4) };

        // pass 1: EXPANDED coverage mask — the white glyphs drawn once per
        // square-ring offset, shifted into the padded DIB; the accumulated
        // gray level at a pixel is the maximum glyph coverage of every
        // offset copy (they overwrite, and the values are close enough for a
        // halo mask).
        tbits.fill(0);
        for dy in -r..=r {
            for dx in -r..=r {
                if dx == 0 && dy == 0 {
                    continue;
                }
                self.text_pass(0xFF_FF_FF, pw, ph, lines, dx + r, dy + r);
            }
        }
        let mut ocov: Vec<u8> = Vec::with_capacity(tw * th);
        for row in 0..th {
            let mut i = row * tw * 4;
            for _ in 0..tw {
                ocov.push(tbits[i]); // R channel of the gray pixel
                i += 4;
            }
        }

        // pass 2: true coverage mask (glyph interior + antialiased edge)
        tbits.fill(0);
        self.text_pass(0xFF_FF_FF, pw, ph, lines, r, r);
        let mut fcov: Vec<u8> = Vec::with_capacity(tw * th);
        for row in 0..th {
            let mut i = row * tw * 4;
            for _ in 0..tw {
                fcov.push(tbits[i]);
                i += 4;
            }
        }

        // pass 3: premultiplied fill color (fill on black)
        tbits.fill(0);
        self.text_pass(colorref(fill), pw, ph, lines, r, r);

        // composite: first the outline shade with the expanded coverage,
        // then the fill with the true coverage ("over" both times)
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w as i32).min(self.win_w as i32);
        let y1 = (y + h as i32).min(self.win_h as i32);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let win_w = self.win_w as usize;
        let bits = self.bits_mut();
        for row in y0..y1 {
            let trow = (row - y) as usize + r as usize;
            let drow = row as usize;
            for col in x0..x1 {
                let tcol = (col - x) as usize + r as usize;
                let ci = trow * tw + tcol;
                let oa = ocov[ci] as u32;
                if oa == 0 {
                    continue;
                }
                let di = (drow * win_w + col as usize) * 4;
                let d = &mut bits[di..di + 4];
                let inv_o = 255 - oa;
                d[0] = (premul(outline.2, oa as u8) as u32 + (d[0] as u32 * inv_o + 127) / 255).min(255) as u8;
                d[1] = (premul(outline.1, oa as u8) as u32 + (d[1] as u32 * inv_o + 127) / 255).min(255) as u8;
                d[2] = (premul(outline.0, oa as u8) as u32 + (d[2] as u32 * inv_o + 127) / 255).min(255) as u8;
                d[3] = (oa + (d[3] as u32 * inv_o + 127) / 255).min(255) as u8;
                let fa = fcov[ci] as u32;
                if fa > 0 {
                    let inv_f = 255 - fa;
                    let ti = ci * 4;
                    d[0] = (tbits[ti] as u32 + (d[0] as u32 * inv_f + 127) / 255).min(255) as u8;
                    d[1] = (tbits[ti + 1] as u32 + (d[1] as u32 * inv_f + 127) / 255).min(255) as u8;
                    d[2] = (tbits[ti + 2] as u32 + (d[2] as u32 * inv_f + 127) / 255).min(255) as u8;
                    d[3] = (fa + (d[3] as u32 * inv_f + 127) / 255).min(255) as u8;
                }
            }
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
    pub fn present(&self) {
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
            let _ = DeleteDC(self.dc);
            let _ = DeleteDC(self.text_dc);
            let _ = ReleaseDC(HWND::default(), self.screen_dc);
        }
    }
}

/// Cheap frame clone helper for cross-fade storage.
pub fn frame_clone(f: &Frame) -> Frame {
    Frame { w: f.w, h: f.h, rgba: f.rgba.clone() }
}
