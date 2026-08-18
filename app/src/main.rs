#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

//! dshpet entry: Windows -> GUI app; other platforms -> headless demo/driver
//! (used to validate connectors against a live DSH from a dev machine).

#[cfg(target_os = "windows")]
mod gui;
#[cfg(not(target_os = "windows"))]
mod headless;

fn main() {
    #[cfg(target_os = "windows")]
    gui::run();
    #[cfg(not(target_os = "windows"))]
    headless::run();
}
