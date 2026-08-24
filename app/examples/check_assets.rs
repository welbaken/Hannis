//! Ground-truth decode check for shipped sprite sheets using the app's own
//! loader (dshpet::anim::load_animation / load_loop_animation), mirroring
//! what app/src/gui/mod.rs does at startup.
//! Usage: cargo run --example check_assets -- <dir>
use dshpet::anim::{load_animation, load_loop_animation};
use std::collections::BTreeSet;
use std::path::Path;
use std::time::Instant;

fn main() {
    let dir = std::env::args().nth(1).expect("usage: check_assets <dir>");
    let dir = Path::new(&dir);
    let states = ["idle", "working", "think", "attention", "done", "fail", "move", "lucky"];
    let mut any_fail = false;
    for state in states {
        let t0 = Instant::now();
        let path = dir.join(format!("{state}.sheet.json"));
        if !path.exists() {
            println!("{state:12} MISSING {path:?}");
            continue;
        }
        match load_animation(dir, state, 1.0, 42) {
            Ok(a) => {
                let f0 = a.frame(0);
                let dims: BTreeSet<(u32, u32)> = a.frames.iter().map(|f| (f.w, f.h)).collect();
                let mem = a.frames.iter().map(|f| f.rgba.len()).sum::<usize>();
                let mem_mb = mem as f64 / 1048576.0;
                // 调色板(compact)帧没有直接 rgba,统一走 pixel_alpha 统计
                let n = (f0.w as usize) * (f0.h as usize);
                let alpha0 = (0..n).filter(|&i| f0.pixel_alpha(i) == 0).count();
                let opaque = (0..n).filter(|&i| f0.pixel_alpha(i) == 255).count();
                let corner = [f0.pixel_alpha(0), f0.pixel_alpha(f0.w as usize - 1)];
                println!(
                    "{state:12} OK  frames={:3} frame0={}x{} dims_set={dims:?} total_ms={:5} mem={:.0}MB decode={:.2}s",
                    a.frame_count(), f0.w, f0.h, a.total_ms(), mem_mb, t0.elapsed().as_secs_f64()
                );
                println!(
                    "{state:12}     alpha: fully_transparent={} opaque={} of {n} px  frame0_corner_alpha={corner:?}",
                    alpha0, opaque
                );
                println!(
                    "{state:12}     durations: uniform {} ms (frame_ms)",
                    a.durations_ms.first().copied().unwrap_or(0)
                );
                let loop_anim = load_loop_animation(dir, state, 1.0, 42);
                println!("{state:12}     _loop asset: {}", if loop_anim.is_some() { "present" } else { "none -> tail-loop fallback" });
            }
            Err(e) => {
                println!("{state:12} FAIL {e}");
                any_fail = true;
            }
        }
    }
    println!("\n{}", if any_fail { "RESULT: at least one asset FAILED" } else { "RESULT: all present assets decode OK" });
}
