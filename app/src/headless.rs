//! Headless driver (non-Windows): runs connectors against live DSH/Hermes and
//! prints state transitions. Also `--self-test` validates asset decoding.

use dshpet::config::Config;
use dshpet::connectors::{dsh::DshConnector, hermes::HermesConnector, stop_flag};
use dshpet::state::{Mode, PetState, Snapshot, StateEvent};
use std::path::Path;
use std::sync::mpsc::channel;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub fn run() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--self-test") {
        self_test();
        return;
    }
    if std::env::var("DSH_PET_DEBUG").map(|v| v == "1").unwrap_or(false) {
        debug_run();
        return;
    }
    let cfg = Config::load(Path::new("config.json"));
    println!(
        "config: dsh={} hermes_db={:?} scripts={}",
        cfg.dsh_url(),
        cfg.hermes_db_path(),
        cfg.scripts.len()
    );
    drive(&cfg);
}

/// Debug mode: print every StateEvent plus per-session state changes, so
/// streaming issues can be diagnosed live (DSH_PET_DEBUG=1).
fn debug_run() {
    use dshpet::state::Source;
    let cfg = Config::load(Path::new("config.json"));
    let (tx, rx) = channel::<StateEvent>();
    let stop: Arc<AtomicBool> = stop_flag();
    DshConnector { url: cfg.dsh_url(), poll_ms: cfg.dsh.poll_ms, history_ms: cfg.dsh.history_ms }
        .spawn(tx.clone(), stop.clone());
    if let Some(db) = cfg.hermes_db_path() {
        HermesConnector {
            db_path: db,
            poll_ms_active: cfg.hermes.poll_ms_active,
            poll_ms_idle: cfg.hermes.poll_ms_idle,
        }
        .spawn(tx.clone(), stop.clone());
    }
    // 用户 Lua 脚本(开放接口):每脚本一线程 + 独立 Lua state
    for (i, sc) in cfg.scripts.iter().enumerate() {
        if sc.file.trim().is_empty() {
            eprintln!("[lua] scripts[{i}] has empty file, skipped");
            continue;
        }
        dshpet::connectors::lua::make(i as u16, sc.clone(), None)
            .spawn(tx.clone(), stop.clone());
    }
    let mut pet = PetState::new(cfg.windows.done_sec * 1000, cfg.windows.fail_sec * 1000);
    pet.set_celebrate_ms(cfg.windows.celebrate_sec * 1000);
    loop {
        let ev = match rx.recv() {
            Ok(ev) => ev,
            Err(_) => break,
        };
        pet.now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        match &ev {
            StateEvent::LiveText { session_id, reasoning, text, .. } => {
                println!(
                    "[debug] LiveText {} reasoning+{} text+{}",
                    &session_id[..session_id.len().min(12)],
                    reasoning.as_ref().map(|r| r.chars().count()).unwrap_or(0),
                    text.as_ref().map(|t| t.chars().count()).unwrap_or(0)
                );
            }
            StateEvent::TurnStarted { session_id, .. } => {
                println!("[debug] TurnStarted {}", &session_id[..session_id.len().min(12)]);
            }
            StateEvent::TurnEnded { session_id, reason, .. } => {
                println!("[debug] TurnEnded {:?} {}", reason, &session_id[..session_id.len().min(12)]);
            }
            StateEvent::ToolStarted { session_id, name, .. } => {
                println!("[debug] ToolStarted {} {}", &session_id[..session_id.len().min(12)], name);
            }
            StateEvent::ToolEnded { session_id, name, .. } => {
                println!("[debug] ToolEnded {} {}", &session_id[..session_id.len().min(12)], name);
            }
            StateEvent::Poll { source: Source::Dsh, items, .. } => {
                let running: Vec<&str> = items.iter().filter(|i| i.running).map(|i| i.session_id.as_str()).collect();
                println!("[debug] DSH poll running={} total={}", running.len(), items.len());
            }
            _ => {}
        }
        pet.apply(ev);
    }
}

fn drive(cfg: &Config) {
    let (tx, rx) = channel::<StateEvent>();
    let stop: Arc<AtomicBool> = stop_flag();

    DshConnector { url: cfg.dsh_url(), poll_ms: cfg.dsh.poll_ms, history_ms: cfg.dsh.history_ms }.spawn(tx.clone(), stop.clone());
    if let Some(db) = cfg.hermes_db_path() {
        HermesConnector {
            db_path: db,
            poll_ms_active: cfg.hermes.poll_ms_active,
            poll_ms_idle: cfg.hermes.poll_ms_idle,
        }
        .spawn(tx.clone(), stop.clone());
    } else {
        println!("hermes db path unresolvable -> hermes source disabled");
    }
    // 用户 Lua 脚本(开放接口):每脚本一线程 + 独立 Lua state
    for (i, sc) in cfg.scripts.iter().enumerate() {
        if sc.file.trim().is_empty() {
            eprintln!("[lua] scripts[{i}] has empty file, skipped");
            continue;
        }
        dshpet::connectors::lua::make(i as u16, sc.clone(), None)
            .spawn(tx.clone(), stop.clone());
    }

    let mut pet = PetState::new(cfg.windows.done_sec * 1000, cfg.windows.fail_sec * 1000);
    pet.set_celebrate_ms(cfg.windows.celebrate_sec * 1000);
    let mut last_mode: Option<Mode> = None;
    let mut last_source: Option<dshpet::state::Source> = None;
    loop {
        let ev = match rx.recv() {
            Ok(ev) => ev,
            Err(_) => break,
        };
        pet.now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        pet.apply(ev);
        let snap = pet.snapshot();
        if snap.mode != last_mode.unwrap_or(Mode::Idle) {
            print_snapshot(&snap);
            last_mode = Some(snap.mode);
        }
        let src = pet.select_bubble_source();
        if src != last_source {
            println!("[bubble-source] {:?}", src);
            last_source = src;
        }
    }
}

fn print_snapshot(s: &Snapshot) {
    let scripts = s
        .sources
        .iter()
        .filter(|(src, _)| matches!(src, dshpet::state::Source::Script(_)))
        .count();
    println!(
        "=== MODE {:?} (sources: dsh={} hermes={} scripts={}) ===",
        s.mode,
        s.sources.get(&dshpet::state::Source::Dsh).copied().unwrap_or(false),
        s.sources.get(&dshpet::state::Source::Hermes).copied().unwrap_or(false),
        scripts,
    );
    if s.queue_len > 0 {
        println!("  queue: {} pending", s.queue_len);
    }
    for w in &s.working {
        println!("  working: [{}] {} tool={:?}", w.source.label(), w.title, w.tool);
        if !w.live.reasoning.is_empty() {
            println!("    thinking: {}", truncate(&w.live.reasoning, 120));
        }
        if !w.live.text.is_empty() {
            println!("    text: {}", truncate(&w.live.text, 120));
        }
    }
    for t in &s.thinking {
        println!("  thinking: [{}] {}", t.source.label(), t.title);
    }
    for d in &s.done {
        println!("  done: [{}] {}", d.source.label(), d.title);
    }
    for f in &s.failed {
        println!("  failed: [{}] {}", f.source.label(), f.title);
    }
    for (sid, tool, _) in &s.pending_approvals {
        println!("  approval: session {sid} tool {tool}");
    }
    for (sid, text, _) in &s.pending_questions {
        println!("  question: session {sid}: {text}");
    }
}

fn truncate(s: &str, n: usize) -> String {
    let v: Vec<char> = s.chars().collect();
    if v.len() <= n {
        s.to_string()
    } else {
        let mut t: String = v[..n].iter().collect();
        t.push('…');
        t
    }
}

fn self_test() {
    println!("== asset self-test ==");
    let dir = Path::new("resource");
    let mut ok = true;
    for name in ["idle", "working", "think", "attention", "done", "fail", "move"] {
        // sheets are the only asset format now (resource/<name>.sheet.*)
        match dshpet::anim::load_animation(&dir, name, 0.5, 42) {
            Ok(a) => {
                println!(
                    "  {name}: {} frames, {}ms, tail_start@{}",
                    a.frame_count(),
                    a.total_ms(),
                    a.tail_start(1000, None)
                );
            }
            Err(e) => {
                println!("  {name}: FAIL {e}");
                ok = false;
            }
        }
    }
    let _ = &mut Ordering::Relaxed; // (unused import guard)
    if ok {
        println!("self-test OK");
    } else {
        std::process::exit(1);
    }
}
