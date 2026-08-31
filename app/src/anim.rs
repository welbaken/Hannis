//! Animation loading (sprite sheets only: `resource/<state>.sheet.{png,json}`)
//! and playback scheduling: non-idle states play the full pass once, then
//! loop the tail (~1s); idle loops the full animation (plan §6).
//!
//! Every frame plays for the uniform `frame_ms` duration (config
//! `display.frame_ms`, default 42) — sheet manifests carry geometry only.

use image::imageops::FilterType;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Frame {
    pub w: u32,
    pub h: u32,
    /// Palette indices (1 byte/pixel) for compact frames. Empty
    /// (`idx`/`palette`/`pal_alpha`/`alpha` all empty) = RGBA mode.
    pub idx: Vec<u8>,
    /// Palette RGB triples (len = entries*3), compact mode only.
    pub palette: Vec<u8>,
    /// Per-palette-entry alpha (from a paletted PNG's tRNS chunk; 255 when
    /// absent). Used when the compact frame came from a paletted sheet.
    pub pal_alpha: Vec<u8>,
    /// Exact per-pixel alpha for compact frames quantized from an RGBA
    /// sheet (palette colors are quantized, alpha stays lossless). Empty
    /// when the frame uses `pal_alpha`.
    pub alpha: Vec<u8>,
    /// Straight RGBA8. Filled for downscaled / unquantized frames; the
    /// compositor prefers this over the compact fields when non-empty.
    pub rgba: Vec<u8>,
    /// Bounding box of non-transparent pixels, computed once at load.
    /// Scopes both the avoid-mode trigger and the per-draw pixel loop.
    pub bbox: Option<(i32, i32, i32, i32)>,
}

impl Frame {
    /// Bounding box of non-transparent (alpha != 0) pixels, in frame-local
    /// coordinates: (x, y, w, h). `None` when the frame is fully transparent.
    ///
    /// Used by the GUI to scope the 回避模式 (avoid) trigger to the pet's
    /// visible body instead of the whole layered window rect (which also spans
    /// the transparent bubble gutter and any clear margins around the sprite).
    pub fn alpha_bbox(&self) -> Option<(i32, i32, i32, i32)> {
        if let Some(b) = self.bbox {
            return Some(b);
        }
        self.scan_bbox()
    }

    /// Per-pixel alpha at a linear pixel offset (all representations).
    #[inline]
    pub fn pixel_alpha(&self, off: usize) -> u8 {
        if !self.rgba.is_empty() {
            self.rgba[off * 4 + 3]
        } else if !self.alpha.is_empty() {
            self.alpha[off]
        } else {
            self.pal_alpha[self.idx[off] as usize]
        }
    }

    /// Full scan for the alpha bbox (load-time; result cached in `bbox`).
    fn scan_bbox(&self) -> Option<(i32, i32, i32, i32)> {
        let w = self.w as i32;
        let h = self.h as i32;
        if w <= 0 || h <= 0 {
            return None;
        }
        let n = w as usize * h as usize;
        if self.rgba.is_empty() && self.idx.len() != n {
            return None;
        }
        let mut min_x = w;
        let mut min_y = h;
        let mut max_x = -1i32;
        let mut max_y = -1i32;
        for y in 0..h {
            let row = (y as usize) * (w as usize);
            for x in 0..w {
                if self.pixel_alpha(row + x as usize) != 0 {
                    if x < min_x {
                        min_x = x;
                    }
                    if x > max_x {
                        max_x = x;
                    }
                    if y < min_y {
                        min_y = y;
                    }
                    if y > max_y {
                        max_y = y;
                    }
                }
            }
        }
        if max_x < min_x {
            None
        } else {
            Some((min_x, min_y, max_x - min_x + 1, max_y - min_y + 1))
        }
    }
}

#[derive(Debug)]
pub struct Animation {
    pub name: String,
    pub frames: Vec<Frame>,
    /// Per-frame duration in ms (from ANMF or manifest).
    pub durations_ms: Vec<u32>,
}

impl Animation {
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    pub fn frame(&self, idx: usize) -> &Frame {
        &self.frames[idx.min(self.frames.len() - 1)]
    }

    pub fn total_ms(&self) -> u64 {
        self.durations_ms.iter().map(|&d| d as u64).sum()
    }

    /// Tail start index covering `tail_ms` from the end.
    pub fn tail_start(&self, tail_ms: u64, tail_frames: Option<u32>) -> usize {
        let n = self.frames.len();
        if let Some(f) = tail_frames {
            return (n as i64 - f as i64).max(0) as usize;
        }
        if n == 0 {
            return 0;
        }
        let mut acc = 0u64;
        let mut start = n;
        for (i, d) in self.durations_ms.iter().enumerate().rev() {
            acc += *d as u64;
            if acc >= tail_ms {
                start = i;
                break;
            }
        }
        if start == n {
            0 // tail covers the whole animation -> effectively full loop
        } else {
            start
        }
    }
}

/// Load the animation for `state` from the sprite sheet
/// `resource/<state>.sheet.{png,json}` (single-file decode, zero per-frame
/// decode cost at startup). Frames are downscaled to `scale` (1.0 = native)
/// and play at the uniform `frame_ms` duration per frame.
pub fn load_animation(resource_dir: &Path, state: &str, scale: f32, frame_ms: u32) -> std::io::Result<Animation> {
    load_sheet(resource_dir, state, scale, frame_ms)
}

/// Load the optional separate loop animation for `state` (plan: play the
/// action once, then loop `resource/<state>_loop.sheet.{png,json}`). Returns
/// None when no loop sheet exists (caller falls back to tail-looping the
/// action).
pub fn load_loop_animation(
    resource_dir: &Path,
    state: &str,
    scale: f32,
    frame_ms: u32,
) -> Option<Animation> {
    let mut anim = load_sheet(resource_dir, &format!("{state}_loop"), scale, frame_ms).ok();
    if let Some(mut a) = anim.take() {
        a.name = state.to_string(); // display name = state asset name
        anim = Some(a);
    }
    anim
}

/// Clamp the configured per-frame duration to a sane playback range
/// (0 would make the player spin forever on a frame).
fn frame_ms_safe(frame_ms: u32) -> u32 {
    frame_ms.clamp(1, 2000)
}

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub frame_count: usize,
    /// Sprite-sheet layout: frames per row.
    #[serde(default)]
    pub frames_per_row: usize,
}

/// Load frames from a sprite sheet: resource/<state>.sheet.png + .sheet.json.
/// Frames are laid out row-major in a grid of `frames_per_row` columns,
/// each cell `<width>x<height>` (see tools/make_sheets.js / split_webp.py).
/// The manifest carries only geometry (width/height/frame_count/frames_per_
/// row); playback timing comes from the uniform `frame_ms`.
pub fn load_sheet(resource_dir: &Path, state: &str, scale: f32, frame_ms: u32) -> std::io::Result<Animation> {
    let frame_ms = frame_ms_safe(frame_ms);
    let json_path = resource_dir.join(format!("{state}.sheet.json"));
    let png_path = resource_dir.join(format!("{state}.sheet.png"));
    let manifest: Manifest = {
        let s = std::fs::read_to_string(&json_path)?;
        serde_json::from_str(&s)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?
    };
    let n = manifest.frame_count;
    let (fw, fh) = (manifest.width, manifest.height);
    let fpr = manifest.frames_per_row.max(1) as u32;
    if n == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("sheet frame_count missing in {json_path:?}"),
        ));
    }
    if fw == 0 || fh == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("bad sheet cell size {fw}x{fh} in {json_path:?}"),
        ));
    }
    let rows = (n as u32 + fpr - 1) / fpr;
    let (w, h) = scaled_dims(fw, fh, scale);
    // Memory-saving fast path: at native scale the sheet is converted to a
    // compact representation instead of 4 bytes/pixel RGBA:
    // - paletted sheets (the shipped quantized ones) decode straight to
    //   1 byte/pixel palette indices + the tRNS alpha table;
    // - RGBA sheets are quantized to a ≤256-color palette at load time with
    //   the per-pixel alpha kept lossless (2 bytes/pixel).
    // e.g. the 100-frame idle sheet drops from ~244 MiB to ~61 MiB.
    if w == fw && h == fh {
        if let Some(frames) = load_compact_sheet(&png_path, n, fpr, fw, fh, rows) {
            return Ok(Animation { name: state.to_string(), frames, durations_ms: vec![frame_ms; n] });
        }
    }
    let mut img = image::open(&png_path)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("{png_path:?}: {e}")))?
        .to_rgba8();
    if img.width() < fpr * fw || img.height() < rows * fh {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "sheet too small: {}x{} < needed {}x{}",
                img.width(),
                img.height(),
                fpr * fw,
                rows * fh
            ),
        ));
    }
    let mut frames = Vec::with_capacity(n);
    for i in 0..n {
        let iu = i as u32;
        let x = (iu % fpr) * fw;
        let y = (iu / fpr) * fh;
        let cell = image::imageops::crop(&mut img, x, y, fw, fh).to_image();
        let mut frame = if w == fw && h == fh {
            Frame { w, h, idx: Vec::new(), palette: Vec::new(), pal_alpha: Vec::new(), alpha: Vec::new(), rgba: cell.into_raw(), bbox: None }
        } else {
            downscale(&cell, w, h)
        };
        frame.bbox = frame.scan_bbox();
        frames.push(frame);
    }
    if frames.is_empty() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "no frames"));
    }
    let durations_ms = vec![frame_ms; n];
    Ok(Animation { name: state.to_string(), frames, durations_ms })
}

/// Decode a sheet at native scale into compact frames (1 byte/pixel index +
/// alpha tables, or 2 bytes/pixel when the source is RGBA):
///
/// 1. Paletted (color-type-3) PNGs decode straight to palette indices, with
///    the per-index alpha taken from the tRNS chunk — byte-identical to the
///    RGBA decode, ~4x less memory.
/// 2. RGBA sheets are quantized at load time to a ≤256-color palette
///    (median cut, count-weighted), with the per-pixel alpha kept EXACT —
///    soft edges stay smooth. 2 bytes/pixel instead of 4.
///
/// Returns None when the sheet cannot be compacted (caller falls back to
/// the RGBA path). Every frame gets a copy of the palette (≈1 KiB —
/// negligible) so a cross-fade frame can outlive the animation it was
/// cloned from.
fn load_compact_sheet(
    png_path: &Path,
    n: usize,
    fpr: u32,
    fw: u32,
    fh: u32,
    rows: u32,
) -> Option<Vec<Frame>> {
    let file = std::fs::File::open(png_path).ok()?;
    let mut reader = png::Decoder::new(std::io::BufReader::new(file)).read_info().ok()?;
    // copy the palette/tRNS/geometry out first: next_frame needs &mut reader
    let (sheet_w, sheet_h, color_type, bit_depth, palette, trns) = {
        let info = reader.info();
        (
            info.width,
            info.height,
            info.color_type,
            info.bit_depth,
            info.palette.as_ref().map(|p| p.to_vec()),
            info.trns.clone(),
        )
    };
    if sheet_w < fpr * fw || sheet_h < rows * fh {
        return None;
    }
    if color_type == png::ColorType::Indexed && bit_depth == png::BitDepth::Eight {
        // ---- paletted sheet: indices + tRNS alpha (1 byte/pixel) ----
        let palette = palette?;
        let mut pal_alpha = vec![255u8; palette.len() / 3];
        if let Some(trns) = &trns {
            for (i, a) in trns.iter().enumerate() {
                if let Some(slot) = pal_alpha.get_mut(i) {
                    *slot = *a;
                }
            }
        }
        let mut raw = vec![0u8; reader.output_buffer_size()?];
        reader.next_frame(&mut raw).ok()?;
        let stride = sheet_w as usize; // indexed: one byte per pixel
        let mut frames = Vec::with_capacity(n);
        for i in 0..n {
            let iu = i as u32;
            let sx = (iu % fpr) * fw;
            let sy = (iu / fpr) * fh;
            let mut idx = vec![0u8; (fw * fh) as usize];
            for row in 0..fh {
                let src = ((sy + row) as usize) * stride + sx as usize;
                let dst = (row * fw) as usize;
                idx[dst..dst + fw as usize].copy_from_slice(&raw[src..src + fw as usize]);
            }
            let mut frame = Frame {
                w: fw,
                h: fh,
                idx,
                palette: palette.clone(),
                pal_alpha: pal_alpha.clone(),
                alpha: Vec::new(),
                rgba: Vec::new(),
                bbox: None,
            };
            frame.bbox = frame.scan_bbox();
            frames.push(frame);
        }
        return Some(frames);
    } else if color_type == png::ColorType::Rgba && bit_depth == png::BitDepth::Eight {
        // ---- RGBA sheet: quantize colors, keep exact per-pixel alpha ----
        let mut raw = vec![0u8; reader.output_buffer_size()?];
        let out = reader.next_frame(&mut raw).ok()?;
        if out.width != sheet_w || out.height != sheet_h {
            return None;
        }
        // histogram of (r,g,b); alpha is handled per pixel, not per color
        let stride = sheet_w as usize * 4;
        let mut hist: std::collections::HashMap<(u8, u8, u8), u32> = std::collections::HashMap::new();
        let mut alpha_of: Vec<u8> = Vec::with_capacity(sheet_w as usize * sheet_h as usize);
        for row in 0..sheet_h as usize {
            for col in 0..sheet_w as usize {
                let o = row * stride + col * 4;
                let key = (raw[o], raw[o + 1], raw[o + 2]);
                *hist.entry(key).or_insert(0) += 1;
                alpha_of.push(raw[o + 3]);
            }
        }
        let palette = quantize_palette(&hist, 256);
        let lookup: std::collections::HashMap<(u8, u8, u8), u8> = hist
            .keys()
            .map(|c| (*c, nearest_palette_index(c, &palette)))
            .collect();
        let mut frames = Vec::with_capacity(n);
        let (iw, ih) = (fw as usize, fh as usize);
        for i in 0..n {
            let iu = i as u32;
            let sx = (iu % fpr) * fw;
            let sy = (iu / fpr) * fh;
            let mut idx = vec![0u8; iw * ih];
            let mut alpha = vec![0u8; iw * ih];
            for row in 0..ih {
                for col in 0..iw {
                    let src = (sy as usize + row) * sheet_w as usize + sx as usize + col;
                    let key = (raw[src * 4], raw[src * 4 + 1], raw[src * 4 + 2]);
                    let dst = row * iw + col;
                    idx[dst] = lookup[&key];
                    alpha[dst] = alpha_of[src];
                }
            }
            let mut frame = Frame {
                w: fw,
                h: fh,
                idx,
                palette: palette.clone(),
                pal_alpha: Vec::new(),
                alpha,
                rgba: Vec::new(),
                bbox: None,
            };
            frame.bbox = frame.scan_bbox();
            frames.push(frame);
        }
        return Some(frames);
    }
    None
}

/// Median-cut palette: repeatedly split the box with the largest
/// count-weighted channel range at its weighted median until `max` boxes.
/// Each box's color is the count-weighted average of its members.
fn quantize_palette(hist: &std::collections::HashMap<(u8, u8, u8), u32>, max: usize) -> Vec<u8> {
    type C = (u8, u8, u8, u32); // r, g, b, count
    let chan = |c: &C, ch: usize| match ch {
        0 => c.0,
        1 => c.1,
        _ => c.2,
    };
    let mut boxes: Vec<Vec<C>> = vec![hist.iter().map(|(c, n)| (c.0, c.1, c.2, *n)).collect()];
    while boxes.len() < max {
        // pick the box with the largest channel range
        let mut best: Option<(usize, usize, i32)> = None; // (box idx, channel, range)
        for (bi, b) in boxes.iter().enumerate() {
            if b.len() < 2 {
                continue;
            }
            for ch in 0..3usize {
                let mut lo = 255i32;
                let mut hi = 0i32;
                for c in b {
                    let v = chan(c, ch) as i32;
                    lo = lo.min(v);
                    hi = hi.max(v);
                }
                let r = hi - lo;
                if r > 0 && best.map(|(_, _, br)| r > br).unwrap_or(true) {
                    best = Some((bi, ch, r));
                }
            }
        }
        let Some((bi, ch, _)) = best else { break };
        let b = boxes.remove(bi);
        if b.len() == 1 {
            boxes.push(b);
            break;
        }
        // sort by the channel and split at the weighted median
        let mut sorted = b.clone();
        sorted.sort_by_key(|c| chan(c, ch));
        let total: u64 = sorted.iter().map(|c| c.3 as u64).sum();
        let mut acc = 0u64;
        let mut split = 1usize;
        for (i, c) in sorted.iter().enumerate() {
            acc += c.3 as u64;
            if acc * 2 >= total {
                split = (i + 1).clamp(1, sorted.len() - 1);
                break;
            }
        }
        let (a, b2) = sorted.split_at(split);
        boxes.push(a.to_vec());
        boxes.push(b2.to_vec());
    }
    let mut palette = Vec::with_capacity(boxes.len() * 3);
    for b in &boxes {
        let mut sum = [0u64; 3];
        let mut cnt = 0u64;
        for c in b {
            sum[0] += c.0 as u64 * c.3 as u64;
            sum[1] += c.1 as u64 * c.3 as u64;
            sum[2] += c.2 as u64 * c.3 as u64;
            cnt += c.3 as u64;
        }
        let n = cnt.max(1);
        palette.push((sum[0] / n) as u8);
        palette.push((sum[1] / n) as u8);
        palette.push((sum[2] / n) as u8);
    }
    palette
}

fn nearest_palette_index(c: &(u8, u8, u8), palette: &[u8]) -> u8 {
    let mut best = 0u8;
    let mut best_d = i64::MAX;
    for (i, e) in palette.chunks(3).enumerate() {
        let dr = e[0] as i64 - c.0 as i64;
        let dg = e[1] as i64 - c.1 as i64;
        let db = e[2] as i64 - c.2 as i64;
        let d = dr * dr + dg * dg + db * db;
        if d < best_d {
            best_d = d;
            best = i as u8;
        }
    }
    best
}

fn scaled_dims(w: u32, h: u32, scale: f32) -> (u32, u32) {
    let s = scale.clamp(0.05, 4.0);
    ((w as f32 * s).round().max(1.0) as u32, (h as f32 * s).round().max(1.0) as u32)
}

fn downscale(img: &image::RgbaImage, w: u32, h: u32) -> Frame {
    if w == img.width() && h == img.height() {
        return Frame { w, h, idx: Vec::new(), palette: Vec::new(), pal_alpha: Vec::new(), alpha: Vec::new(), rgba: img.as_raw().clone(), bbox: None };
    }
    let scaled = image::imageops::resize(img, w, h, FilterType::Triangle);
    Frame { w, h, idx: Vec::new(), palette: Vec::new(), pal_alpha: Vec::new(), alpha: Vec::new(), rgba: scaled.into_raw(), bbox: None }
}

/// Playback schedule: full pass once then tail loop (non-idle), or full loop (idle).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Full,
    TailLoop,
}

pub struct Player {
    pub mode_name: String,
    pub full_loop: bool,
    pub tail_start: usize,
    pub tail_end: usize, // exclusive
    pub idx: usize,
    pub remaining_ms: u64,
    /// Incremented each time the initial full pass wraps into the tail.
    /// Lets the caller switch to a separate loop animation exactly once.
    pub full_passes: u32,
}

impl Player {
    pub fn new(anim: &Animation, full_loop: bool, tail_ms: u64, tail_frames: Option<u32>) -> Self {
        let tail_start = anim.tail_start(tail_ms, tail_frames);
        Player {
            mode_name: anim.name.clone(),
            full_loop,
            tail_start,
            tail_end: anim.frame_count(),
            idx: 0,
            remaining_ms: anim.durations_ms.first().copied().unwrap_or(42) as u64,
            full_passes: 0,
        }
    }

    /// Advance by dt ms; returns the new frame index.
    pub fn advance(&mut self, anim: &Animation, dt_ms: u64) -> usize {
        let mut remaining = dt_ms;
        while remaining > 0 {
            if remaining < self.remaining_ms {
                self.remaining_ms -= remaining;
                remaining = 0;
            } else {
                remaining -= self.remaining_ms;
                self.idx += 1;
                let n = anim.frame_count();
                if self.idx >= n {
                    if self.full_loop {
                        self.idx = 0;
                    } else {
                        self.idx = self.tail_start;
                        self.full_passes = self.full_passes.saturating_add(1);
                    }
                }
                self.remaining_ms = anim.durations_ms[self.idx.min(n - 1)] as u64;
            }
        }
        self.idx
    }
}

/// Convenience: current frame index + restart flag when a new mode starts.
pub fn restart_player(anim: &Animation, full_loop: bool, tail_ms: u64, tail_frames: Option<u32>) -> Player {
    Player::new(anim, full_loop, tail_ms, tail_frames)
}

/// Locate the resource dir next to the executable, or given override.
pub fn resource_dir(exe_dir: &Path, override_dir: Option<&Path>) -> PathBuf {
    override_dir
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| exe_dir.join("resource"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_anim(n: usize, d: u32) -> Animation {
        Animation {
            name: "test".into(),
            frames: (0..n)
                .map(|i| Frame {
                    w: 4,
                    h: 4,
                    idx: Vec::new(),
                    palette: Vec::new(),
                    pal_alpha: Vec::new(),
                    alpha: Vec::new(),
                    rgba: vec![i as u8; 16],
                    bbox: None,
                })
                .collect(),
            durations_ms: vec![d; n],
        }
    }

    #[test]
    fn tail_start_derived_from_ms() {
        let a = fake_anim(10, 100); // 1s total, tail 300ms -> last 3 frames
        assert_eq!(a.tail_start(300, None), 7);
    }

    #[test]
    fn tail_start_whole_animation() {
        let a = fake_anim(10, 100);
        assert_eq!(a.tail_start(5000, None), 0);
    }

    #[test]
    fn tail_frames_override() {
        let a = fake_anim(10, 100);
        assert_eq!(a.tail_start(300, Some(5)), 5);
        assert_eq!(a.tail_start(300, Some(20)), 0);
    }

    #[test]
    fn non_idle_plays_full_then_tail_loop() {
        let a = fake_anim(10, 100);
        let mut p = Player::new(&a, false, 300, None);
        assert_eq!(p.idx, 0);
        // full pass: frames 1..9 displayed over 9*100ms
        for i in 1..10 {
            assert_eq!(p.advance(&a, 100), i);
        }
        // 10th advance wraps into tail [7..10)
        assert_eq!(p.advance(&a, 100), 7);
        assert_eq!(p.advance(&a, 100), 8);
        assert_eq!(p.advance(&a, 100), 9);
        assert_eq!(p.advance(&a, 100), 7); // loops
    }

    #[test]
    fn idle_loops_full() {
        let a = fake_anim(10, 100);
        let mut p = Player::new(&a, true, 300, None);
        for _ in 0..5 {
            for i in 1..10 {
                assert_eq!(p.advance(&a, 100), i);
            }
            assert_eq!(p.advance(&a, 100), 0);
        }
    }

    #[test]
    fn full_passes_counter_increments_on_tail_wrap() {
        let a = fake_anim(10, 100);
        let mut p = Player::new(&a, false, 300, None);
        for _ in 0..9 {
            p.advance(&a, 100);
        }
        assert_eq!(p.full_passes, 0); // still in the full pass
        p.advance(&a, 100); // wraps into tail
        assert_eq!(p.full_passes, 1);
        p.advance(&a, 100); // tail loop does not increment again
        assert_eq!(p.full_passes, 1);
        let mut full = Player::new(&a, true, 300, None);
        for _ in 0..25 {
            full.advance(&a, 100);
        }
        assert_eq!(full.full_passes, 0); // full-loop players never wrap
    }

    #[test]
    fn loop_animation_from_sheet_and_fallback() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../resource");
        // no loop sheet -> None (caller falls back to tail-looping the action)
        assert!(load_loop_animation(&dir, "idle", 0.5, 42).is_none()); // no idle_loop sheet
        if dir.join("think_loop.sheet.json").exists() {
            let l = load_loop_animation(&dir, "think", 0.5, 42).expect("think loop");
            assert_eq!(l.name, "think");
            assert!(l.frame_count() > 0);
        }
        // sheet fixture -> found and renamed to the state name
        let tmp = std::env::temp_dir().join(format!("dshpet-loop-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let img = image::RgbaImage::from_pixel(8, 8, image::Rgba([10, 20, 30, 255]));
        img.save(tmp.join("idle_loop.sheet.png")).unwrap();
        std::fs::write(
            tmp.join("idle_loop.sheet.json"),
            r#"{"width":8,"height":8,"frame_count":1,"frames_per_row":1}"#,
        )
        .unwrap();
        let a = load_loop_animation(&tmp, "idle", 0.5, 42).expect("loop found");
        assert_eq!(a.name, "idle");
        assert_eq!(a.frame_count(), 1);
        assert_eq!(a.durations_ms, vec![42]);
        // 8x8 scaled by 0.5 -> 4x4 = 16 px
        assert_eq!(a.frame(0).rgba.len(), 16 * 4);
        assert_eq!(&a.frame(0).rgba[..4], &[10, 20, 30, 255]);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn sheet_loads_minimal_manifest() {
        // The shipped sheet.jsons carry ONLY geometry now
        // (width/height/frame_count/frames_per_row); every frame plays for
        // the uniform configured frame_ms.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../resource");
        if !dir.join("idle.sheet.json").exists() {
            eprintln!("resource dir missing, skipping");
            return;
        }
        let a = load_sheet(&dir, "idle", 1.0, 42).expect("load idle sheet");
        assert!(a.frame_count() > 0);
        assert_eq!(a.durations_ms, vec![42; a.frame_count()]);
        // an explicit frame_ms override propagates to every frame
        let b = load_sheet(&dir, "idle", 1.0, 100).expect("load idle sheet");
        assert_eq!(b.durations_ms, vec![100; b.frame_count()]);
        // the animation loader is the sheet loader; missing sheet -> error
        let c = load_animation(&dir, "idle", 1.0, 42).expect("load idle");
        assert_eq!(c.frame_count(), a.frame_count());
        assert!(load_animation(&dir, "no_such_state", 1.0, 42).is_err());
    }

    #[test]
    fn alpha_bbox_scopes_to_visible_pixels() {
        // 6x4 canvas, opaque pixels only inside rows 1..3, cols 2..4
        let mut rgba = vec![0u8; 6 * 4 * 4];
        for y in 1..3 {
            for x in 2..4 {
                let i = (y * 6 + x) * 4;
                rgba[i] = 255;
                rgba[i + 1] = 0;
                rgba[i + 2] = 0;
                rgba[i + 3] = 255;
            }
        }
        let f = Frame { w: 6, h: 4, idx: Vec::new(), palette: Vec::new(), pal_alpha: Vec::new(), alpha: Vec::new(), rgba, bbox: None };
        assert_eq!(f.alpha_bbox(), Some((2, 1, 2, 2)));
        // fully transparent frame -> None
        let empty = Frame { w: 6, h: 4, idx: Vec::new(), palette: Vec::new(), pal_alpha: Vec::new(), alpha: Vec::new(), rgba: vec![0u8; 6 * 4 * 4], bbox: None };
        assert_eq!(empty.alpha_bbox(), None);
        // whole-canvas coverage
        let full = Frame { w: 2, h: 2, idx: Vec::new(), palette: Vec::new(), pal_alpha: Vec::new(), alpha: Vec::new(), rgba: vec![0xAA; 2 * 2 * 4], bbox: None };
        assert_eq!(full.alpha_bbox(), Some((0, 0, 2, 2)));
        // indexed frames bbox through the per-index alpha table
        let palette = vec![255, 0, 0, 0, 255, 0]; // idx0=red opaque, idx1=green
        let pal_alpha = vec![0u8, 255];
        let idx = vec![1u8, 1, 0, 0, 1, 1, 0, 0, 1, 1, 0, 0, 1, 1, 0, 0]; // 4x4
        let f = Frame { w: 4, h: 4, idx, palette, pal_alpha, alpha: Vec::new(), rgba: Vec::new(), bbox: None };
        assert_eq!(f.alpha_bbox(), Some((0, 0, 2, 4))); // opaque only in cols 0..2
        assert_eq!(f.pixel_alpha(5), 255); // idx 1 -> alpha 255
        assert_eq!(f.pixel_alpha(2), 0); // idx 0 -> alpha 0
    }

    #[test]
    fn partial_dt_accumulates() {
        let a = fake_anim(10, 100);
        let mut p = Player::new(&a, false, 300, None);
        assert_eq!(p.advance(&a, 30), 0);
        assert_eq!(p.advance(&a, 30), 0);
        assert_eq!(p.advance(&a, 40), 1); // boundary: 100ms elapsed
        assert_eq!(p.advance(&a, 10), 1); // 90ms left on frame 1
        assert_eq!(p.advance(&a, 90), 2);
    }

    /// Encode a small paletted PNG (2x2 cells, 3x1 grid) with a tRNS
    /// per-index alpha table, then check the indexed loader reproduces the
    /// exact RGBA pixels the `image` crate would decode.
    #[test]
    fn indexed_sheet_matches_rgba_decode() {
        let tmp = std::env::temp_dir().join(format!("dshpet-indexed-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        // palette: 0=red, 1=green, 2=blue, 3=white
        let palette: Vec<u8> = vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];
        // per-index alpha: red=0 (transparent), green=255, blue=128, white=77
        let trns: Vec<u8> = vec![0, 255, 128, 77];
        // 3 cells of 2x2 laid out 3 per row (1 row): indices
        let cells: [Vec<u8>; 3] = [
            vec![0, 0, 0, 0], // fully transparent
            vec![1, 2, 2, 1], // green/blue checker
            vec![3, 3, 3, 3], // white
        ];
        let mut raw = Vec::new();
        for c in &cells {
            raw.extend_from_slice(c);
        }
        let file = std::fs::File::create(tmp.join("test.sheet.png")).unwrap();
        let mut enc = png::Encoder::new(file, 6, 2);
        enc.set_color(png::ColorType::Indexed);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_palette(palette.clone());
        enc.set_trns(trns.clone());
        let mut writer = enc.write_header().unwrap();
        writer.write_image_data(&raw).unwrap();
        drop(writer);
        std::fs::write(
            tmp.join("test.sheet.json"),
            r#"{"width":2,"height":2,"frame_count":3,"frames_per_row":3}"#,
        )
        .unwrap();

        let anim = load_sheet(&tmp, "test", 1.0, 42).expect("indexed sheet loads");
        assert_eq!(anim.frame_count(), 3);
        assert!(anim.frame(0).rgba.is_empty(), "indexed path must be used at scale 1.0");
        assert_eq!(anim.frame(0).idx.len(), 4);
        assert_eq!(anim.frame(0).palette, palette);
        assert_eq!(anim.frame(0).pal_alpha, trns);

        // reference: what the RGBA path (image crate) decodes
        let ref_img = image::open(tmp.join("test.sheet.png")).unwrap().to_rgba8();
        for ci in 0..3usize {
            let f = anim.frame(ci);
            let sx = (ci % 3) * 2;
            // the expected pixel indices come from the actual row-major sheet
            // layout (cells span both rows of the 6x2 image), NOT from the
            // concatenated `cells` arrays used for encoding
            let mut expected = Vec::new();
            for row in 0..2usize {
                for col in 0..2usize {
                    expected.push(raw[row * 6 + sx + col]);
                }
            }
            assert_eq!(&f.idx, &expected, "cell {ci} index plane");
            for (p, &idx) in expected.iter().enumerate() {
                let (px, py) = (p % 2, p / 2);
                let refpix = ref_img.get_pixel((sx + px) as u32, py as u32).0;
                let pi = idx as usize;
                assert_eq!(
                    [f.palette[pi * 3], f.palette[pi * 3 + 1], f.palette[pi * 3 + 2], f.pal_alpha[pi]],
                    refpix,
                    "cell {ci} pixel {p} must match the RGBA decode"
                );
            }
        }
        // frame 0 bbox: opaque only in row 1 (blue/128 + green/255);
        // frame 2 (white) fully opaque -> (0,0,2,2)
        assert_eq!(anim.frame(0).alpha_bbox(), Some((0, 1, 2, 1)));
        assert_eq!(anim.frame(2).alpha_bbox(), Some((0, 0, 2, 2)));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn rgba_sheet_is_quantized_at_native_scale() {
        // an RGBA (color-type-6) sheet at scale 1.0 takes the compact path:
        // colors quantized to a palette, per-pixel alpha kept EXACT
        let tmp = std::env::temp_dir().join(format!("dshpet-rgba-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([10, 20, 30, 255]));
        img.save(tmp.join("t.sheet.png")).unwrap();
        std::fs::write(
            tmp.join("t.sheet.json"),
            r#"{"width":4,"height":4,"frame_count":1,"frames_per_row":1}"#,
        )
        .unwrap();
        let a = load_sheet(&tmp, "t", 1.0, 42).expect("rgba sheet loads");
        let f = a.frame(0);
        assert!(f.rgba.is_empty(), "native-scale RGBA sheets must be compacted");
        assert_eq!(f.idx.len(), 16);
        assert_eq!(f.alpha.len(), 16, "per-pixel alpha plane");
        assert_eq!(f.palette.len(), 3, "one unique color -> 1-entry palette");
        assert_eq!(&f.palette[..3], &[10, 20, 30]);
        assert!(f.alpha.iter().all(|&a| a == 255), "alpha stays exact");
        assert_eq!(f.alpha_bbox(), Some((0, 0, 4, 4)));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn rgba_sheet_median_cut_stays_bounded_and_alpha_exact() {
        // >256 unique colors: median cut keeps the palette ≤256 and the
        // per-pixel alpha plane byte-identical to the source
        let tmp = std::env::temp_dir().join(format!("dshpet-quant-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        // 64x64 gradient = 4096 unique colors, alpha varies per row
        let (w, h) = (64u32, 64u32);
        let mut img = image::RgbaImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                img.put_pixel(x, y, image::Rgba([x as u8, y as u8, (x + y) as u8, y as u8]));
            }
        }
        img.save(tmp.join("g.sheet.png")).unwrap();
        std::fs::write(
            tmp.join("g.sheet.json"),
            r#"{"width":64,"height":64,"frame_count":1,"frames_per_row":1}"#,
        )
        .unwrap();
        let a = load_sheet(&tmp, "g", 1.0, 42).expect("gradient sheet loads");
        let f = a.frame(0);
        assert!(f.rgba.is_empty());
        assert_eq!(f.idx.len(), (w * h) as usize);
        assert_eq!(f.alpha.len(), (w * h) as usize);
        assert!(f.palette.len() / 3 <= 256, "palette bounded: {}", f.palette.len() / 3);
        // alpha plane must equal the source exactly
        for (i, p) in img.pixels().enumerate() {
            assert_eq!(f.alpha[i], p.0[3], "alpha must be lossless");
        }
        // every palette index is valid
        let entries = f.palette.len() / 3;
        assert!(f.idx.iter().all(|&i| (i as usize) < entries));
        std::fs::remove_dir_all(&tmp).ok();
    }
}
