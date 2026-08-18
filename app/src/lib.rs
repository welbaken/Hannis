//! dshpet core: platform-independent logic (state machine, animation playback,
//! config, connectors). GUI lives in `gui` (Windows only).

pub mod anim;
pub mod bubble_text;
pub mod config;
pub mod connectors;
pub mod http;
pub mod state;

pub use state::{Mode, PetState, Snapshot, StateEvent, TurnEndReason};
