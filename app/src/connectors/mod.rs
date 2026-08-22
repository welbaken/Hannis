//! Connectors: AgentSource implementations (design doc §2/§5/§7).
//! Each source spawns its own thread(s) and emits StateEvent into a shared
//! channel. Health flips are emitted as SourceHealth on transitions only.

pub mod dsh;
pub mod hermes;
pub mod lua;

use crate::state::StateEvent;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;
use std::sync::Arc;

pub fn stop_flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

/// Sleep in small slices so `stop` can interrupt promptly.
pub fn sleep_interruptible(ms: u64, stop: &AtomicBool) {
    let mut left = ms;
    while left > 0 && !stop.load(std::sync::atomic::Ordering::Relaxed) {
        let step = left.min(50);
        std::thread::sleep(std::time::Duration::from_millis(step));
        left -= step;
    }
}

/// Small helper: send an event, silently dropping when the channel is gone.
pub fn send(tx: &Sender<StateEvent>, ev: StateEvent) {
    let _ = tx.send(ev);
}
