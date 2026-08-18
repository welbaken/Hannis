//! Animation loading (webp direct or split frames + manifest) and playback
//! scheduling: non-idle states play the full pass once, then loop the tail
//! (~1s); idle loops the full animation (plan §6).

use image::codecs::webp::WebPDecoder;
use image::imageops::FilterType;
use image::{AnimationDecoder, ImageDecoder, RgbaImage};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Frame {
    pub w: u32,
    pub h: u32,
    /// Straight RGBA8.
    pub rgba: Vec<u8>,
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

/// Load an animation for `state` from `resource_dir`, following plan §11:
/// 1) sprite sheet `resource/<state>.sheet.{png,json}` (single-file decode,
///    zero per-frame decode cost at startup);
/// 2) legacy split frames `resource/<state>/manifest.json` + frame_%03d.png;
/// 3) plain `<state>.webp`. Frames are downscaled to `scale` (1.0 = native).
pub fn load_animation(resource_dir: &Path, state: &str, scale: f32, use_split: &str) -> std::io::Result<Animation> {
    let sheet_path = resource_dir.join(format!("{state}.sheet.json"));
    let sheet_ok = matches!(use_split, "true") || (matches!(use_split, "auto") && sheet_path.exists());
    if sheet_ok {
        if let Ok(anim) = load_sheet(resource_dir, state, scale) {
            return Ok(anim);
        }
        if matches!(use_split, "true") {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("split frames requested but sheet load failed for {state}"),
            ));
        }
    }
    let state_dir = resource_dir.join(state);
    let manifest_path = state_dir.join("manifest.json");
    let split_ok = matches!(use_split, "true") || (matches!(use_split, "auto") && manifest_path.exists());
    if split_ok {
        if let Ok(anim) = load_split(&state_dir, scale) {
            return Ok(anim);
        }
        if matches!(use_split, "true") {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("split frames requested but manifest load failed for {state}"),
            ));
        }
    }
    load_webp(&resource_dir.join(format!("{state}.webp")), scale)
}

/// Load the optional separate loop animation for `state` (plan: play the
/// action webp once, then loop `<state>_loop.webp`). Looks for the sprite
/// sheet `resource/<state>_loop.sheet.{png,json}` first, then legacy split
/// frames in `resource/<state>_loop/`, then the webp file. Returns None when
/// none exists (caller falls back to tail-looping the action).
pub fn load_loop_animation(
    resource_dir: &Path,
    state: &str,
    scale: f32,
    use_split: &str,
) -> Option<Animation> {
    let mut anim = None;
    // 1) sprite sheet
    let sheet_path = resource_dir.join(format!("{state}_loop.sheet.json"));
    let sheet_ok = matches!(use_split, "true")
        || (matches!(use_split, "auto") && sheet_path.exists());
    if sheet_ok {
        anim = load_sheet(resource_dir, &format!("{state}_loop"), scale).ok();
    }
    // 2) legacy split frames
    if anim.is_none() {
        let split_dir = resource_dir.join(format!("{state}_loop"));
        let split_ok = matches!(use_split, "true")
            || (matches!(use_split, "auto") && split_dir.join("manifest.json").exists());
        if split_ok {
            anim = load_split(&split_dir, scale).ok();
        }
    }
    // 3) webp
    if anim.is_none() {
        let path = resource_dir.join(format!("{state}_loop.webp"));
        if path.exists() {
            anim = load_webp(&path, scale).ok();
        }
    }
    if let Some(mut a) = anim {
        a.name = state.to_string(); // display name = state asset name
        Some(a)
    } else {
        None
    }
}

/// Decode an animated webp into RGBA frames (pure-Rust image crate).
pub fn load_webp(path: &Path, scale: f32) -> std::io::Result<Animation> {
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let file = std::fs::File::open(path)?;
    let mut reader = image::ImageReader::new(std::io::BufReader::new(file));
    reader.set_format(image::ImageFormat::WebP);
    let reader = reader.into_inner();
    let decoder = WebPDecoder::new(reader)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;

    let (nw, nh) = (decoder.dimensions().0, decoder.dimensions().1);
    let total_bytes = decoder.total_bytes() as usize;
    let (tw, th) = scaled_dims(nw, nh, scale);

    let mut frames = Vec::new();
    let mut durations_ms = Vec::new();
    if decoder.has_animation() {
        let it = decoder.into_frames();
        for f in it {
            let f = f.map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
            let (num, den) = f.delay().numer_denom_ms();
            durations_ms.push(if den == 0 { 0 } else { (num as u64 / den as u64).max(1) as u32 });
            frames.push(downscale(&f.into_buffer(), tw, th));
        }
    } else {
        let mut buf = vec![0u8; total_bytes];
        decoder
            .read_image(&mut buf)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        durations_ms.push(0);
        let (w, h) = (nw, nh);
        let rgba = if total_bytes == (nw * nh * 3) as usize {
            let img = RgbaImage::from_raw(w, h, buf).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "rgb size mismatch")
            })?;
            let mut out = RgbaImage::new(w, h);
            for (x, y, p) in img.enumerate_pixels() {
                out.put_pixel(x, y, image::Rgba([p[0], p[1], p[2], 255]));
            }
            out
        } else {
            RgbaImage::from_raw(w, h, buf).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "rgba size mismatch")
            })?
        };
        frames.push(downscale(&rgba, tw, th));
    }
    if frames.is_empty() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "no frames"));
    }
    // Guard: unknown/zero durations -> default 41.67ms (24fps asset norm)
    if durations_ms.iter().all(|&d| d == 0) {
        durations_ms = vec![42; frames.len()];
    }
    Ok(Animation { name, frames, durations_ms })
}

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub state: String,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub frame_count: usize,
    #[serde(default)]
    pub durations_ms: Vec<u32>,
    /// Sprite-sheet layout: frames per row (1 = single column / legacy split).
    #[serde(default)]
    pub frames_per_row: usize,
}

/// Load frames from a sprite sheet: resource/<state>.sheet.png + .sheet.json.
/// Frames are laid out row-major in a grid of `frames_per_row` columns,
/// each cell `<width>x<height>` (see tools/split_webp.py / make_sheets.js).
pub fn load_sheet(resource_dir: &Path, state: &str, scale: f32) -> std::io::Result<Animation> {
    let json_path = resource_dir.join(format!("{state}.sheet.json"));
    let png_path = resource_dir.join(format!("{state}.sheet.png"));
    let manifest: Manifest = {
        let s = std::fs::read_to_string(&json_path)?;
        serde_json::from_str(&s)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?
    };
    let n = if manifest.frame_count > 0 {
        manifest.frame_count
    } else {
        manifest.durations_ms.len()
    };
    let (fw, fh) = (manifest.width, manifest.height);
    let fpr = manifest.frames_per_row.max(1) as u32;
    if fw == 0 || fh == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("bad sheet cell size {fw}x{fh} in {json_path:?}"),
        ));
    }
    let rows = (n as u32 + fpr - 1) / fpr;
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
    let (w, h) = scaled_dims(fw, fh, scale);
    let mut frames = Vec::with_capacity(n);
    let mut durations_ms = manifest.durations_ms.clone();
    for i in 0..n {
        let iu = i as u32;
        let x = (iu % fpr) * fw;
        let y = (iu / fpr) * fh;
        let cell = image::imageops::crop(&mut img, x, y, fw, fh).to_image();
        frames.push(if w == fw && h == fh {
            Frame { w, h, rgba: cell.into_raw() }
        } else {
            downscale(&cell, w, h)
        });
        if durations_ms.len() < n {
            durations_ms.push(42);
        }
    }
    if frames.is_empty() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "no frames"));
    }
    Ok(Animation { name: state.to_string(), frames, durations_ms })
}

/// Load split frames: resource/<state>/frame_%03d.png + manifest.json.
pub fn load_split(state_dir: &Path, scale: f32) -> std::io::Result<Animation> {
    let manifest: Manifest = {
        let s = std::fs::read_to_string(state_dir.join("manifest.json"))?;
        serde_json::from_str(&s)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?
    };
    let n = if manifest.frame_count > 0 {
        manifest.frame_count
    } else {
        manifest.durations_ms.len()
    };
    let mut frames = Vec::new();
    let mut durations_ms = manifest.durations_ms.clone();
    for i in 0..n {
        let p = state_dir.join(format!("frame_{i:03}.png"));
        let img = image::open(&p)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("{p:?}: {e}")))?;
        let rgba = img.to_rgba8();
        let (w, h) = scaled_dims(rgba.width(), rgba.height(), scale);
        frames.push(if w == rgba.width() && h == rgba.height() {
            Frame { w, h, rgba: rgba.into_raw() }
        } else {
            downscale(&rgba, w, h)
        });
        if durations_ms.len() < n {
            durations_ms.push(42);
        }
    }
    if frames.is_empty() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "no frames"));
    }
    Ok(Animation { name: manifest.state, frames, durations_ms })
}

fn scaled_dims(w: u32, h: u32, scale: f32) -> (u32, u32) {
    let s = scale.clamp(0.05, 4.0);
    ((w as f32 * s).round().max(1.0) as u32, (h as f32 * s).round().max(1.0) as u32)
}

fn downscale(img: &image::RgbaImage, w: u32, h: u32) -> Frame {
    if w == img.width() && h == img.height() {
        return Frame { w, h, rgba: img.as_raw().clone() };
    }
    let scaled = image::imageops::resize(img, w, h, FilterType::Triangle);
    Frame { w, h, rgba: scaled.into_raw() }
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
                .map(|i| Frame { w: 4, h: 4, rgba: vec![i as u8; 16] })
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
    fn loop_animation_split_and_fallback() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../resource");
        if !dir.join("idle.webp").exists() {
            eprintln!("resource dir missing, skipping");
            return;
        }
        // no loop file -> None
        assert!(load_loop_animation(&dir, "idle", 0.5, "auto").is_none());
        // split fixture -> found and renamed to the state name
        let tmp = std::env::temp_dir().join(format!("dshpet-loop-test-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("idle_loop")).unwrap();
        let img = image::RgbaImage::from_pixel(8, 8, image::Rgba([10, 20, 30, 255]));
        img.save(tmp.join("idle_loop/frame_000.png")).unwrap();
        std::fs::write(
            tmp.join("idle_loop/manifest.json"),
            r#"{"state":"idle_loop","width":8,"height":8,"frame_count":1,"durations_ms":[42]}"#,
        )
        .unwrap();
        let a = load_loop_animation(&tmp, "idle", 0.5, "auto").expect("loop found");
        assert_eq!(a.name, "idle");
        assert_eq!(a.frame_count(), 1);
        // 8x8 scaled by 0.5 -> 4x4 = 16 px
        assert_eq!(a.frame(0).rgba.len(), 16 * 4);
        assert_eq!(&a.frame(0).rgba[..4], &[10, 20, 30, 255]);
        std::fs::remove_dir_all(&tmp).ok();
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

    #[test]
    fn sheet_roundtrip_matches_webp() {
        // sheet loader must produce frames matching the webp reference decode.
        // webp 源已从 resource/ 移除后(sheet 为唯一素材),本测试自动跳过。
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../resource");
        if !dir.join("idle.sheet.json").exists() || !dir.join("idle.webp").exists() {
            eprintln!("sheet/webp fixtures missing, skipping");
            return;
        }
        let a_native = load_webp(&dir.join("idle.webp"), 1.0).expect("decode idle.webp");
        let a_half = load_webp(&dir.join("idle.webp"), 0.5).expect("decode idle.webp");
        let b = load_sheet(&dir, "idle", 1.0).expect("load sheet");
        assert_eq!(b.frame_count(), a_native.frame_count());
        assert_eq!(b.durations_ms, a_native.durations_ms);
        // 当前素材是 256 色量化 sheet:只在可见像素(alpha>0)上比较。
        // 实测分布:99% 可见像素有 ±1~2 抖差,max ~114,>64 的约占 0.8%。
        let stats = |x: &[u8], y: &[u8]| -> (u32, u32, u32, u32) {
            let (mut n, mut maxd, mut bad, mut visible) = (0u32, 0u32, 0u32, 0u32);
            for (a, b) in x.chunks(4).zip(y.chunks(4)) {
                if b[3] == 0 { continue; }
                visible += 1;
                let mut d = 0;
                for c in 0..4 {
                    let dc = (a[c] as i32 - b[c] as i32).unsigned_abs();
                    if dc > d { d = dc; }
                }
                if d > 0 { n += 1; }
                if d > maxd { maxd = d; }
                if d > 64 { bad += 1; }
            }
            (n, maxd, bad, visible)
        };
        let (n0, max0, bad0, vis0) = stats(&b.frame(0).rgba, &a_native.frame(0).rgba);
        let last = a_native.frame_count() - 1;
        let (nl, maxl, badl, visl) = stats(&b.frame(last).rgba, &a_native.frame(last).rgba);
        eprintln!("sheet roundtrip(quant): frame0 {n0}/{vis0} px diff (max {max0}, >64: {bad0}); frame{last} {nl}/{visl} px diff (max {maxl}, >64: {badl})");
        assert!(max0 <= 200 && maxl <= 200, "unexpected large divergence");
        assert!(bad0 * 50 < vis0.max(1) && badl * 50 < visl.max(1),
            "too many badly divergent pixels (>=2% of visible)");
        let bh = load_sheet(&dir, "idle", 0.5).expect("load scaled sheet");
        assert_eq!(bh.frame_count(), a_half.frame_count());
        let (nh, maxh, badh, vish) = stats(&bh.frame(0).rgba, &a_half.frame(0).rgba);
        eprintln!("scaled sheet roundtrip(quant): {nh}/{vish} px diff (max {maxh}, >64: {badh})");
        assert!(maxh <= 200 && badh * 50 < vish.max(1));
        // loop sheet is picked up by the loop loader and renamed to the state
        assert!(load_loop_animation(&dir, "idle", 0.5, "auto").is_none()); // no idle_loop
        if dir.join("think_loop.sheet.json").exists() {
            let l = load_loop_animation(&dir, "think", 0.5, "auto").expect("think loop");
            assert_eq!(l.name, "think");
            assert!(l.frame_count() > 0);
        }
    }

    #[test]
    fn split_roundtrip_matches_webp() {
        // verify the manifest/split-frame loader path end-to-end (plan §11).
        // Expectations derive from the CURRENT asset so a legitimate webp
        // update (frame count/timing) cannot rot this test.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../resource");
        if !dir.join("idle.webp").exists() {
            eprintln!("resource dir missing, skipping");
            return;
        }
        let a_native = load_webp(&dir.join("idle.webp"), 1.0).expect("decode idle.webp");
        let a_half = load_webp(&dir.join("idle.webp"), 0.5).expect("decode idle.webp");
        let tmp = std::env::temp_dir().join(format!("dshpet-split-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        for (i, f) in a_native.frames.iter().enumerate() {
            let img = image::RgbaImage::from_raw(f.w, f.h, f.rgba.clone()).unwrap();
            img.save(tmp.join(format!("frame_{i:03}.png"))).unwrap();
        }
        let n = a_native.frame_count();
        let (fw, fh) = (a_native.frame(0).w, a_native.frame(0).h);
        let manifest = format!(
            r#"{{"state":"idle","width":{fw},"height":{fh},"frame_count":{n},"durations_ms":{},"tail":{{"start":{},"end":{}}}}}"#,
            serde_json::to_string(&a_native.durations_ms).unwrap(),
            n.saturating_sub(24),
            n
        );
        std::fs::write(tmp.join("manifest.json"), manifest).unwrap();
        let b = load_split(&tmp, 0.5).expect("load split frames");
        assert_eq!(b.frame_count(), a_half.frame_count());
        assert_eq!(b.frame(0).rgba, a_half.frame(0).rgba);
        assert_eq!(b.durations_ms, a_half.durations_ms);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn load_real_webp_metadata() {
        // Structural sanity on the shipped idle.webp. Frame count / timing /
        // resolution are asset properties and may change across asset updates,
        // so assert self-consistency instead of hardcoded values.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../resource");
        if !dir.join("idle.webp").exists() {
            eprintln!("resource dir missing, skipping");
            return;
        }
        let a = load_webp(&dir.join("idle.webp"), 0.5).expect("decode idle.webp");
        assert!(a.frame_count() > 0);
        assert_eq!(a.frame_count(), a.frames.len());
        assert_eq!(a.total_ms(), a.durations_ms.iter().map(|&d| d as u64).sum::<u64>());
        // per-frame durations are positive and sane (1ms..2s)
        assert!(a.durations_ms.iter().all(|&d| (1..=2000).contains(&d)));
        let f = a.frame(0);
        assert!(f.w > 0 && f.h > 0);
        // all frames share the canvas size
        assert!(a.frames.iter().all(|x| x.w == f.w && x.h == f.h));
        // 0.5 scale halves the native resolution (guard against off-by-one)
        let native = load_webp(&dir.join("idle.webp"), 1.0).expect("decode idle.webp");
        assert!((native.frame(0).w as i64 - f.w as i64 * 2).abs() <= 1);
        assert!((native.frame(0).h as i64 - f.h as i64 * 2).abs() <= 1);
        // RGBA content sanity: has transparent pixels (alpha channel worked)
        assert!(f.rgba.iter().step_by(4).any(|&a| a == 0));
    }
}
